// handle_table.rs — Layer 5 of the Chisel stack: the stable-handle indirection.
//
// Role: maps a stable u64 `handle` to the physical `(page_id, slot_index)`
// location of a value. Because values are located through this table, the
// storage engine is free to move values around (on update, overflow promotion,
// or defrag) without invalidating any handle the caller holds.
//
// Structure: a fixed-fanout radix tree over the integer handle.
//   - Leaf pages hold ENTRIES_PER_LEAF (= 510) 16-byte HandleEntry slots.
//   - Interior pages hold PTRS_PER_INTERIOR (= 1021) child page pointers.
//   - `depth` = 0 means the root itself is a leaf; depth grows when handles
//     overflow the current capacity (see `grow`).
//
// Addressing is arithmetic, not bit-masked: at each interior level we divide
// the remaining handle by the "child span" (how many handles live beneath one
// child) and recurse into that child index, passing `handle % child_span`
// downward. This keeps capacity exact (510 * 1021^depth) rather than rounding
// up to a power of two.
//
// Copy-on-write (per-module): this module implements its own COW on top of
// `PageCache::new_page()`. Every mutation path (`insert`, `delete`, `grow`)
// allocates fresh pages for every node it touches and returns a new root
// page ID; it NEVER writes to a page that was reachable from the
// previously-committed superblock. Invariants:
//
//   (I1) After `insert`/`delete`, the old root and every page reachable from
//        it are still byte-identical to what the previous commit sees. A
//        concurrent reader (or a crash) walking from the old superblock sees
//        a consistent pre-mutation tree.
//   (I2) The new root returned by a mutation must be stored in
//        `current_roots.handle_table_page` by the caller, and ultimately
//        written into the new superblock before commit fsync. Forgetting to
//        propagate the new root would silently lose the mutation.
//   (I3) Newly allocated COW pages are dirty in the cache and will be
//        flushed by `cache.flush()` in commit phase 1 BEFORE the superblock
//        swap in phase 2 — this is what makes shadow paging crash-safe.
//
// Handle allocation policy (enforced by transaction.rs, not this module):
// handles are monotonic from `next_handle` and never reused. Delete writes
// a tombstone entry (HandleFlags::Deleted) in place; the leaf slot is not
// freed. This keeps handles stable forever but means the tree only grows.

use crate::error::Result;
use crate::page::{
    self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_ID_NONE, PAGE_SIZE,
};
use crate::page_cache::PageCache;

// A HandleEntry on disk is {u64 page_id, u16 slot_index, u8 flags, 5 reserved}.
// Kept at 16 bytes both for alignment and to leave room for future fields
// (e.g. generation counter) without changing on-disk layout math.
const ENTRY_SIZE: usize = 16;
// 510 entries per leaf = (8192 - header - checksum) / 16. This is the branching
// factor at the bottom of the tree and also the radix divisor at level 0.
pub const ENTRIES_PER_LEAF: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / ENTRY_SIZE; // 510

const CHILD_PTR_SIZE: usize = 8;
// 1021 interior pointers per page — slightly asymmetric with leaves because
// child pointers are 8 bytes vs. 16-byte leaf entries. This asymmetry is
// intentional: using 1024 would waste 24 bytes; using exact division gets
// the maximum fan-out the page format allows.
const PTRS_PER_INTERIOR: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / CHILD_PTR_SIZE; // 1021

// Page flags byte (buf[1]): distinguishes leaf from interior. Stored in the
// page header so `open_existing` can walk the tree to recover depth without
// needing the depth to be persisted separately in the superblock. Currently
// forensic-only — no live code reads it (the depth walk uses child-pointer
// presence); kept because a hex-dump reader can tell leaf from interior at
// a glance.
//
// NOTE on per-page version byte (I31): handle-table pages keep the flag at
// byte 1 and put the I31 per-page format version at byte 2. Every other
// page type puts its version at byte 1; `page::page_format_version`
// dispatches on PageType to read from the right offset.
const FLAG_LEAF: u8 = 0x01;
// I50 (ISSUES.md, 2026-05-22): `pub(crate)` so transaction.rs's
// open_existing depth-walk can match against the named constant
// instead of a raw `0x02` literal. The on-disk flag value is part of
// the format and must stay in lockstep with FLAG_LEAF.
pub(crate) const FLAG_INTERIOR: u8 = 0x02;

/// Per-entry state tag. `Deleted` functions as a tombstone — the slot stays
/// allocated in the leaf, so the corresponding handle value is permanently
/// burned (never reused). `Overflow` signals that `page_id` points to the
/// first page of an overflow chain rather than a data page slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleFlags {
    Live,
    Deleted,
    Overflow,
}

impl HandleFlags {
    fn to_u8(self) -> u8 {
        match self {
            HandleFlags::Live => 0x01,
            HandleFlags::Deleted => 0x00,
            HandleFlags::Overflow => 0x02,
        }
    }
    // NOTE: 0x00 maps to Deleted. This is load-bearing: a freshly zeroed
    // leaf page reads every slot as Deleted, which is what allows `create_root`
    // and `grow` to simply zero-fill a page and have every entry behave as
    // "unused". The same property makes a zero child pointer in an interior
    // page unambiguous: 0 means "no child allocated yet".
    fn from_u8(v: u8) -> HandleFlags {
        match v {
            0x01 => HandleFlags::Live,
            0x02 => HandleFlags::Overflow,
            _ => HandleFlags::Deleted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleEntry {
    pub page_id: u64,
    pub slot_index: u16,
    pub flags: HandleFlags,
    /// Immutable client-supplied grouping tag; 0 = untagged. Stored in the
    /// entry's reserved bytes [11..15). See
    /// docs/specs/2026-06-02-chunk-tags-design.md.
    pub tag: u32,
}

/// `HandleTable` owns only the tree's depth; the actual pages live in the
/// `PageCache`, and the current root page ID lives in the transaction's
/// `Roots` struct (ultimately the superblock). Depth is not persisted
/// directly — `open_existing` re-derives it by walking the leftmost spine
/// (which is why `grow` always places the old root at child index 0).
pub struct HandleTable {
    depth: u32, // 0 = root is a leaf, 1 = one level of interior, etc.
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleTable {
    pub fn new() -> HandleTable {
        HandleTable { depth: 0 }
    }

    /// Create a new empty root leaf page. Returns its page ID.
    ///
    /// Invariant: called only when the committed tree has no root
    /// (`handle_table_page == PAGE_ID_NONE`). Resets depth to 0 — if called
    /// on an existing tree it would orphan every leaf beneath the old root.
    pub fn create_root(&mut self, cache: &mut PageCache) -> Result<u64> {
        let page_id = cache.new_page()?;
        // ISSUES.md I8: handle-table pages must never have id 0,
        // because 0 is the "no child allocated yet" sentinel in
        // interior nodes. A real database reserves pages 0 and 1 for
        // the superblock slots, so new_page() here cannot return 0 —
        // but the assertion makes the load-bearing invariant explicit
        // and catches misuse in tests or future refactors that might
        // change page allocation bootstrap.
        debug_assert_ne!(
            page_id, 0,
            "handle-table pages must not use page id 0 (reserved as the zero-child sentinel)"
        );
        let buf = cache.get_mut(page_id)?;
        buf.fill(0); // Zero-fill: every slot reads as Deleted (see HandleFlags::from_u8).
        buf[0] = PageType::HandleTable as u8;
        buf[1] = FLAG_LEAF;
        buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31
        page::stamp_checksum(buf);
        self.depth = 0;
        Ok(page_id)
    }

    /// Look up a handle. Returns None if the handle doesn't exist or is deleted.
    ///
    /// Read-only: walks the tree without touching any page. Safe to call on
    /// either `committed_roots` or `current_roots` — this is how
    /// transaction.rs serves reads from outside an active transaction.
    pub fn lookup(
        &self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
    ) -> Result<Option<HandleEntry>> {
        if root == PAGE_ID_NONE {
            return Ok(None);
        }
        let Some((leaf_page, index)) = self.find_leaf(cache, root, handle)? else {
            // find_leaf returned None: an interior child pointer was zero
            // mid-descent, meaning this handle's subtree was never allocated.
            // The handle is definitionally absent.
            return Ok(None);
        };
        let buf = cache.get(leaf_page)?;
        let entry = Self::read_entry(buf, index);
        if entry.flags == HandleFlags::Deleted {
            Ok(None)
        } else {
            Ok(Some(entry))
        }
    }

    /// Insert or update a handle entry. Returns the new root page ID (COW).
    ///
    /// The caller MUST store the returned page ID in the transaction's
    /// current roots — otherwise the mutation is effectively lost at commit
    /// time because the superblock will still point at the old root (see
    /// invariant I2 in the file header).
    pub fn insert(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
        entry: &HandleEntry,
    ) -> Result<u64> {
        let mut current_root = root;
        // Grow the tree upward until the handle fits. `grow` stacks a new
        // interior above the old root each time; the old root becomes child 0
        // of the new interior, which preserves addressability of all existing
        // handles (their addresses all fall within the first child's span).
        while handle >= self.capacity() {
            current_root = self.grow(cache, current_root)?;
        }
        self.insert_recursive(cache, current_root, handle, entry, self.depth)
    }

    /// Delete a handle: write a tombstone for it and return the previous
    /// entry in a single tree descent.
    ///
    /// Returns `(new_root, Some(entry))` if the handle had a Live or
    /// Overflow entry that was just tombstoned. Returns `(root, None)`
    /// — note unchanged root — if the handle was absent or already a
    /// tombstone; in those cases no COW is performed and the tree is
    /// not grown. This contrasts with the historical `insert(deleted)`
    /// implementation which always COWed and could grow the tree even
    /// for no-op deletes.
    ///
    /// Tombstone-write semantics are unchanged: the leaf entry stays
    /// at its fixed `(handle % ENTRIES_PER_LEAF)` position forever
    /// once written. This is why `next_handle` in the transaction
    /// layer is monotonic — reusing a deleted handle would be
    /// ambiguous against a stale reader.
    pub fn delete(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        handle: u64,
    ) -> Result<(u64, Option<HandleEntry>)> {
        // Empty tree: nothing to delete.
        if root == PAGE_ID_NONE {
            return Ok((root, None));
        }
        // I26-style guard: handle outside tree's reach is definitionally
        // absent. No tree growth (insert grows; delete doesn't).
        if handle >= self.capacity() {
            return Ok((root, None));
        }
        self.delete_recursive(cache, root, handle, self.depth)
    }

    /// Single-pass recursive descent that reads the existing entry at
    /// the leaf and writes the tombstone via COW on the way back up.
    /// If the leaf entry is already Deleted (or the subtree is absent
    /// via a zero child pointer), no COW is performed at any level —
    /// the original page IDs propagate back up unchanged.
    fn delete_recursive(
        &mut self,
        cache: &mut PageCache,
        page: u64,
        handle: u64,
        level: u32,
    ) -> Result<(u64, Option<HandleEntry>)> {
        if level == 0 {
            // Leaf: read the entry. Decide whether to write tombstone.
            let index = (handle as usize) % ENTRIES_PER_LEAF;
            let entry = {
                let buf = cache.get(page)?;
                Self::read_entry(buf, index)
            };
            match entry.flags {
                HandleFlags::Deleted => {
                    // Already tombstoned (or never written — read_entry
                    // on a zeroed slot decodes flag byte 0 which is not
                    // Live (1) or Overflow (2), so it presents as
                    // Deleted by elimination). No COW.
                    Ok((page, None))
                }
                HandleFlags::Live | HandleFlags::Overflow => {
                    // COW the leaf, write tombstone, return Some(entry).
                    // Pattern mirrors insert_recursive's leaf branch.
                    let new_leaf = cache.new_page()?;
                    debug_assert_ne!(new_leaf, 0); // I8
                    {
                        let buf_copy: [u8; PAGE_SIZE] = *cache.get(page)?;
                        let new_buf = cache.get_mut(new_leaf)?;
                        *new_buf = buf_copy;
                    }
                    let tombstone = HandleEntry {
                        page_id: 0,
                        slot_index: 0,
                        flags: HandleFlags::Deleted,
                        tag: 0,
                    };
                    {
                        let new_buf = cache.get_mut(new_leaf)?;
                        Self::write_entry(new_buf, index, &tombstone);
                        page::stamp_checksum(new_buf);
                    }
                    Ok((new_leaf, Some(entry)))
                }
            }
        } else {
            // Interior: descend the appropriate child.
            let child_span = self.span_at_level(level);
            let child_idx = (handle / child_span) as usize;
            let child_page = {
                let buf = cache.get(page)?;
                let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
                u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
            };
            if child_page == 0 {
                // Subtree never allocated — handle is definitionally
                // absent. No COW at any level above; return the
                // original page id unchanged.
                return Ok((page, None));
            }
            let (new_child, prev_entry) =
                self.delete_recursive(cache, child_page, handle % child_span, level - 1)?;
            if prev_entry.is_none() {
                // Recursion did not write a tombstone (subtree already
                // tombstoned or absent at the leaf). No COW at this
                // level either: the original `page` is still referenced
                // by the committed superblock and remains unmodified,
                // and we have no new child id to link in — returning
                // `(page, None)` preserves the COW invariant.
                return Ok((page, None));
            }
            // Recursion COWed below us. COW this interior page so it
            // points at the new child.
            let new_page = cache.new_page()?;
            debug_assert_ne!(new_page, 0); // I8
            {
                let buf_copy: [u8; PAGE_SIZE] = *cache.get(page)?;
                let new_buf = cache.get_mut(new_page)?;
                *new_buf = buf_copy;
            }
            {
                let new_buf = cache.get_mut(new_page)?;
                let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
                new_buf[offset..offset + 8].copy_from_slice(&new_child.to_le_bytes());
                page::stamp_checksum(new_buf);
            }
            Ok((new_page, prev_entry))
        }
    }

    /// Iterate over all live entries. Returns (handle, HandleEntry) pairs.
    pub fn iter_live(&self, cache: &mut PageCache, root: u64) -> Result<Vec<(u64, HandleEntry)>> {
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        self.iter_recursive(cache, root, 0, self.depth, &mut result)?;
        Ok(result)
    }

    /// Set the tree depth (used when loading from an existing file).
    ///
    /// Depth is not persisted in the superblock; `TransactionManager::
    /// open_existing` reconstructs it by walking the leftmost child spine
    /// and calls this. Correctness depends on `grow` always installing the
    /// old root at child index 0.
    pub fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    /// Rebuild the tree depth by walking the left spine from `root` until a leaf
    /// (page type byte != `FLAG_INTERIOR`). Returns 0 for an empty tree
    /// (`PAGE_ID_NONE`) or a leaf root. The in-memory `depth` is NOT carried in
    /// `Roots`, so the transaction layer re-derives it from the restored root both
    /// at open AND after a rollback rewinds to a shallower committed/savepoint root.
    pub fn recover_depth(cache: &mut PageCache, root: u64) -> Result<u32> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let mut depth = 0u32;
        let mut current = root;
        loop {
            let buf = cache.get(current)?;
            if buf[1] != FLAG_INTERIOR {
                break;
            }
            depth += 1;
            let child_offset = page::DATA_PAGE_HEADER_SIZE;
            let child = u64::from_le_bytes(buf[child_offset..child_offset + 8].try_into().unwrap());
            if child == 0 {
                break;
            }
            current = child;
        }
        Ok(depth)
    }

    /// `#[allow(dead_code)]`: companion to `set_depth`. Production
    /// `open_existing` reconstructs depth from the on-disk spine walk
    /// and stores it via `set_depth`, but no current reader queries it
    /// back. Kept for forensic / debug-print use and so the
    /// set_depth/depth pair stays symmetric.
    #[allow(dead_code)]
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Maximum handle value the tree can currently hold.
    ///
    /// = 510 * 1021^depth. At depth 0 the tree holds 510 handles; each level
    /// of growth multiplies by 1021, so depth 3 already addresses ~543M
    /// handles. In practice the tree rarely exceeds depth 2-3.
    fn capacity(&self) -> u64 {
        let mut cap = ENTRIES_PER_LEAF as u64;
        for _ in 0..self.depth {
            cap *= PTRS_PER_INTERIOR as u64;
        }
        cap
    }

    /// Add a new interior root above the current root, increasing depth by 1.
    ///
    /// Crucial design choice: the old root is installed at child index 0 of
    /// the new interior. Because the old tree's capacity equals exactly the
    /// span of one child at the new level, every pre-existing handle `h`
    /// satisfies `h / new_child_span == 0` and routes straight into child 0
    /// unchanged. This lets us grow without rewriting any existing node.
    ///
    /// COW note: we do NOT clone `old_root` here. We only allocate the new
    /// interior page and point it at the unchanged old root. The old root
    /// remains reachable from the previous superblock, preserving I1.
    fn grow(&mut self, cache: &mut PageCache, old_root: u64) -> Result<u64> {
        let new_root = cache.new_page()?;
        // I8: interior nodes use 0 as "no child allocated yet"; the
        // new root (which will itself be linked from other interior
        // pages on future growth) must not share that encoding.
        debug_assert_ne!(new_root, 0);
        let buf = cache.get_mut(new_root)?;
        buf.fill(0);
        buf[0] = PageType::HandleTable as u8;
        buf[1] = FLAG_INTERIOR;
        buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31
        buf[DATA_PAGE_HEADER_SIZE..DATA_PAGE_HEADER_SIZE + 8]
            .copy_from_slice(&old_root.to_le_bytes());
        page::stamp_checksum(buf);
        self.depth += 1;
        Ok(new_root)
    }

    // Core COW mutation. Every node on the path from root down to the target
    // leaf is cloned into a fresh page. The old path remains intact and
    // reachable from the previous superblock (invariant I1); the returned
    // page ID is the new clone at this level.
    //
    // Temporary stack copy: we `*old_buf` into a PAGE_SIZE array rather than
    // holding an immutable borrow across a `get_mut`, because `PageCache`
    // hands out exclusive references and we need to read the old page to
    // populate the new one. 8KB on the stack per level is cheap relative to
    // page I/O.
    fn insert_recursive(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        handle: u64,
        entry: &HandleEntry,
        level: u32,
    ) -> Result<u64> {
        // COW: copy the page.
        let new_page = cache.new_page()?;
        // I8: handle-table pages must never have id 0 (see
        // create_root for context).
        debug_assert_ne!(new_page, 0);
        {
            let old_buf = cache.get(page_id)?;
            let old_data: [u8; PAGE_SIZE] = *old_buf;
            let new_buf = cache.get_mut(new_page)?;
            new_buf.copy_from_slice(&old_data);
        }

        if level == 0 {
            // Leaf: the remaining handle bits directly index the slot.
            // `handle % 510` here is equivalent to "the low-order radix digit"
            // after all the higher levels have already divided it down.
            let index = (handle % ENTRIES_PER_LEAF as u64) as usize;
            let buf = cache.get_mut(new_page)?;
            Self::write_entry(buf, index, entry);
            page::stamp_checksum(buf);
            Ok(new_page)
        } else {
            // Interior: `child_span` is the number of handles one child
            // covers at this level. `handle / child_span` picks the child
            // slot; `handle % child_span` is what we pass down.
            let child_span = self.span_at_level(level);
            let child_idx = (handle / child_span) as usize;

            let child_page = {
                let buf = cache.get(new_page)?;
                let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
                u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
            };

            // Sparse allocation: interior pages don't pre-populate children.
            // A zero pointer means "no subtree allocated here yet"; we lazily
            // create one only when an insert touches that range. The newly
            // allocated child is already a fresh page, so it IS its own COW
            // clone — no further copy needed.
            let actual_child = if child_page == 0 {
                if level == 1 {
                    let leaf = cache.new_page()?;
                    debug_assert_ne!(leaf, 0); // I8
                    let buf = cache.get_mut(leaf)?;
                    buf.fill(0);
                    buf[0] = PageType::HandleTable as u8;
                    buf[1] = FLAG_LEAF;
                    buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31
                    page::stamp_checksum(buf);
                    leaf
                } else {
                    let interior = cache.new_page()?;
                    debug_assert_ne!(interior, 0); // I8
                    let buf = cache.get_mut(interior)?;
                    buf.fill(0);
                    buf[0] = PageType::HandleTable as u8;
                    buf[1] = FLAG_INTERIOR;
                    buf[2] = page::PAGE_FORMAT_VERSION_CURRENT; // I31
                    page::stamp_checksum(buf);
                    interior
                }
            } else {
                child_page
            };

            let new_child =
                self.insert_recursive(cache, actual_child, handle % child_span, entry, level - 1)?;

            // Patch the child pointer in our cloned interior page to point
            // at the new subtree, then re-stamp the checksum. This is the
            // only mutation to `new_page`, and `new_page` is a fresh COW
            // clone so mutating it does not violate I1.
            let buf = cache.get_mut(new_page)?;
            let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
            buf[offset..offset + 8].copy_from_slice(&new_child.to_le_bytes());
            page::stamp_checksum(buf);

            Ok(new_page)
        }
    }

    // Read-only tree descent for lookup. Mirrors the arithmetic in
    // `insert_recursive` but walks from level = depth down to 1 without
    // cloning. Returns `Some((leaf_page, slot_index))` if the descent
    // reaches a populated leaf, or `None` if any interior child pointer on
    // the path is zero — which means the subtree for this handle range was
    // never allocated and the handle is definitionally absent.
    //
    // HISTORY (ISSUES.md I6): an earlier version returned `Ok((page_id, 0))`
    // using the ORIGINAL root page id as a sentinel when it hit a zero child
    // pointer. `lookup` would then read slot 0 of the root (possibly an
    // interior page) as a `HandleEntry`. For small page ids this coincidentally
    // decoded as `Deleted` (the byte at offset 10 of an interior page's
    // child-pointer region is usually zero), but once any child pointer
    // referenced a page id with bit 16 set, that same byte could decode as
    // `Live` (0x01) or `Overflow` (0x02) and lookup would return a bogus
    // HandleEntry for a handle that does not exist. Returning `Option` makes
    // the "absent subtree" case explicit and type-safe.
    fn find_leaf(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        handle: u64,
    ) -> Result<Option<(u64, usize)>> {
        // I26: any handle outside the tree's reach is definitionally
        // absent — same answer as a zero child pointer mid-descent.
        // Without this guard the descent loop below would compute a
        // `child_idx >= PTRS_PER_INTERIOR` and read `buf[CHECKSUM_OFFSET..]`
        // (treating the page's checksum as a child pointer) or panic on
        // an out-of-bounds slice for very large handles. `insert`
        // pre-grows via `while handle >= capacity { grow() }` so it
        // cannot hit this path — only the lookup side (read / update /
        // delete) needs the guard. At depth 0 `handle % ENTRIES_PER_LEAF`
        // is already total, so the check is only needed when we actually
        // descend.
        if self.depth > 0 && handle >= self.capacity() {
            return Ok(None);
        }

        if self.depth == 0 {
            let index = (handle % ENTRIES_PER_LEAF as u64) as usize;
            return Ok(Some((page_id, index)));
        }

        let mut current = page_id;
        let mut remaining = handle;

        for level in (1..=self.depth).rev() {
            let child_span = self.span_at_level(level);
            let child_idx = (remaining / child_span) as usize;
            remaining %= child_span;

            let buf = cache.get(current)?;
            let offset = DATA_PAGE_HEADER_SIZE + child_idx * CHILD_PTR_SIZE;
            let child = u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
            if child == 0 {
                return Ok(None);
            }
            current = child;
        }

        let index = (remaining % ENTRIES_PER_LEAF as u64) as usize;
        Ok(Some((current, index)))
    }

    // Number of handles covered by a single child pointer at `level`.
    // Level 1 interior → each child is a leaf covering 510 handles.
    // Level 2 interior → each child is a level-1 interior covering 510*1021.
    // In general: 510 * 1021^(level - 1).
    fn span_at_level(&self, level: u32) -> u64 {
        let mut span = ENTRIES_PER_LEAF as u64;
        for _ in 1..level {
            span *= PTRS_PER_INTERIOR as u64;
        }
        span
    }

    // Depth-first enumeration of live entries. `base_handle` accumulates
    // the handle prefix contributed by the path taken so far, so that a
    // leaf at level 0 can reconstruct each entry's full u64 handle as
    // `base_handle + slot_index`. This mirrors the division performed on
    // the way down during insert/lookup.
    fn iter_recursive(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        base_handle: u64,
        level: u32,
        result: &mut Vec<(u64, HandleEntry)>,
    ) -> Result<()> {
        if level == 0 {
            let buf = cache.get(page_id)?;
            for i in 0..ENTRIES_PER_LEAF {
                let entry = Self::read_entry(buf, i);
                if entry.flags != HandleFlags::Deleted {
                    result.push((base_handle + i as u64, entry));
                }
            }
        } else {
            let child_span = self.span_at_level(level);
            // Materialize child pointers before recursing: the recursive
            // call takes `&mut PageCache` and would invalidate any live
            // borrow of the interior page's buffer.
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page_id)?;
                (0..PTRS_PER_INTERIOR)
                    .map(|i| {
                        let offset = DATA_PAGE_HEADER_SIZE + i * CHILD_PTR_SIZE;
                        let child = u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
                        (i, child)
                    })
                    .filter(|(_, child)| *child != 0)
                    .collect()
            };
            for (i, child) in children {
                self.iter_recursive(
                    cache,
                    child,
                    base_handle + (i as u64) * child_span,
                    level - 1,
                    result,
                )?;
            }
        }
        Ok(())
    }

    fn read_entry(buf: &[u8; PAGE_SIZE], index: usize) -> HandleEntry {
        let base = DATA_PAGE_HEADER_SIZE + index * ENTRY_SIZE;
        HandleEntry {
            page_id: u64::from_le_bytes(buf[base..base + 8].try_into().unwrap()),
            slot_index: u16::from_le_bytes(buf[base + 8..base + 10].try_into().unwrap()),
            flags: HandleFlags::from_u8(buf[base + 10]),
            tag: u32::from_le_bytes(buf[base + 11..base + 15].try_into().unwrap()),
        }
    }

    // On-disk layout per 16-byte entry:
    //   [0..8)   page_id (u64 LE)
    //   [8..10)  slot_index (u16 LE)
    //   [10]     flags (HandleFlags u8)
    //   [11..15) tag (u32 LE)
    //   [15]     reserved (always zeroed for forward compatibility)
    fn write_entry(buf: &mut [u8; PAGE_SIZE], index: usize, entry: &HandleEntry) {
        let base = DATA_PAGE_HEADER_SIZE + index * ENTRY_SIZE;
        buf[base..base + 8].copy_from_slice(&entry.page_id.to_le_bytes());
        buf[base + 8..base + 10].copy_from_slice(&entry.slot_index.to_le_bytes());
        buf[base + 10] = entry.flags.to_u8();
        buf[base + 11..base + 15].copy_from_slice(&entry.tag.to_le_bytes());
        buf[base + 15] = 0; // remaining reserved byte
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    fn make_cache() -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut cache = PageCache::new(
            io,
            1024 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        // Reserve pages 0 and 1 (the superblock slots in a real DB).
        // Without this, the first allocation hands out page id 0,
        // which collides with the I8 "zero child pointer" sentinel
        // in interior nodes and trips the debug_assert in
        // HandleTable::create_root.
        cache.set_next_page_id(2);
        cache
    }

    // Regression test for ISSUES.md I6. Constructs a depth-1 handle table,
    // then artificially sets byte 10 of slot 0 in the interior root to 0x01
    // (Live). This simulates the real-world case where an interior page's
    // child pointer has its byte-2 nonzero (i.e., some child's page id has
    // bit 16 set). Pre-fix, `find_leaf` would return (root, 0) on the
    // sparse-child path, `lookup` would read slot 0 of the root, and the
    // patched flags byte would cause a bogus `Live` HandleEntry to be
    // returned for a handle that does not exist. Post-fix, `find_leaf`
    // returns None and `lookup` correctly returns Ok(None).
    #[test]
    fn lookup_sparse_range_in_depth1_tree_returns_none() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root0 = ht.create_root(&mut cache).unwrap();

        let entry = HandleEntry {
            page_id: 42,
            slot_index: 7,
            flags: HandleFlags::Live,
            tag: 0,
        };
        // Insert handle 0 (fits in depth-0 root leaf).
        let root1 = ht.insert(&mut cache, root0, 0, &entry).unwrap();
        // Insert handle 510: forces grow() → depth becomes 1. The old leaf
        // becomes child 0 of the new interior root; child 1 is a fresh leaf
        // holding handle 510. Children 2..1021 are zero (sparse).
        let root2 = ht.insert(&mut cache, root1, 510, &entry).unwrap();
        assert_eq!(ht.depth(), 1);

        // Patch byte 10 of slot 0 in the interior root to 0x01. This byte
        // lives inside child 1's pointer region ([24..32]); overwriting it
        // corrupts that pointer, but this test never traverses child 1
        // after the patch. Re-stamp the checksum so cache.get() still
        // validates on re-read.
        {
            let buf = cache.get_mut(root2).unwrap();
            let flags_offset = DATA_PAGE_HEADER_SIZE + 10; // entry 0 flags
            buf[flags_offset] = HandleFlags::Live.to_u8();
            page::stamp_checksum(buf);
        }

        // Handle 510 * 2 = 1020 routes to interior child_idx = 2, which is
        // zero (never allocated). Pre-fix: find_leaf returns (root2, 0);
        // lookup reads slot 0 of the interior root; flags byte is now 0x01;
        // a bogus Live HandleEntry is returned. Post-fix: lookup returns None.
        let result = ht.lookup(&mut cache, root2, 1020).unwrap();
        assert_eq!(
            result, None,
            "sparse handle must report absent, not a bogus entry"
        );
    }

    #[test]
    fn delete_returns_some_for_live_entry() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root = ht.create_root(&mut cache).unwrap();

        // Insert a Live entry.
        let live_entry = HandleEntry {
            page_id: 42,
            slot_index: 7,
            flags: HandleFlags::Live,
            tag: 0,
        };
        let root_after_insert = ht.insert(&mut cache, root, 100, &live_entry).unwrap();

        // Delete it. Expect (new_root, Some(live_entry-equivalent)).
        let (new_root, prev_entry) = ht.delete(&mut cache, root_after_insert, 100).unwrap();

        assert_ne!(
            new_root, root_after_insert,
            "delete of a Live entry must COW the leaf"
        );
        let entry = prev_entry.expect("delete of a Live entry must return Some(entry)");
        assert_eq!(entry.page_id, 42);
        assert_eq!(entry.slot_index, 7);
        assert_eq!(entry.flags, HandleFlags::Live);
    }

    #[test]
    fn delete_returns_some_for_overflow_entry() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root = ht.create_root(&mut cache).unwrap();

        let overflow_entry = HandleEntry {
            page_id: 99,
            slot_index: 0,
            flags: HandleFlags::Overflow,
            tag: 0,
        };
        let root_after_insert = ht.insert(&mut cache, root, 200, &overflow_entry).unwrap();

        let (new_root, prev_entry) = ht.delete(&mut cache, root_after_insert, 200).unwrap();

        assert_ne!(
            new_root, root_after_insert,
            "delete of an Overflow entry must COW the leaf"
        );
        let entry = prev_entry.expect("delete of an Overflow entry must return Some(entry)");
        assert_eq!(entry.page_id, 99);
        assert_eq!(entry.flags, HandleFlags::Overflow);
    }

    #[test]
    fn delete_returns_none_for_already_deleted() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root = ht.create_root(&mut cache).unwrap();

        let live_entry = HandleEntry {
            page_id: 42,
            slot_index: 7,
            flags: HandleFlags::Live,
            tag: 0,
        };
        let root_after_insert = ht.insert(&mut cache, root, 100, &live_entry).unwrap();

        // First delete: returns Some(entry).
        let (root_after_first_delete, _) = ht.delete(&mut cache, root_after_insert, 100).unwrap();

        // Second delete: handle is now a tombstone. Expect (root, None) with NO COW.
        let (root_after_second_delete, prev_entry) =
            ht.delete(&mut cache, root_after_first_delete, 100).unwrap();

        assert_eq!(
            prev_entry, None,
            "delete of an already-tombstoned handle must return None"
        );
        assert_eq!(
            root_after_second_delete, root_after_first_delete,
            "no-op delete must not COW the tree (root unchanged)"
        );
    }

    #[test]
    fn delete_returns_none_for_absent_handle_in_existing_subtree() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root = ht.create_root(&mut cache).unwrap();

        // Insert handle 100 — this allocates the depth-0 leaf containing slots
        // 0..510. Handle 200 lands in the same leaf (slot 200) but was never
        // written, so its slot is zeroed (read_entry returns flags = 0 = treated
        // as Deleted by the leaf-level branch).
        let live_entry = HandleEntry {
            page_id: 42,
            slot_index: 7,
            flags: HandleFlags::Live,
            tag: 0,
        };
        let root_after_insert = ht.insert(&mut cache, root, 100, &live_entry).unwrap();

        let (new_root, prev_entry) = ht.delete(&mut cache, root_after_insert, 200).unwrap();

        assert_eq!(
            prev_entry, None,
            "delete of an absent slot must return None"
        );
        assert_eq!(
            new_root, root_after_insert,
            "delete of an absent slot must not COW the tree"
        );
    }

    #[test]
    fn delete_does_not_grow_tree_for_handle_beyond_capacity() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root = ht.create_root(&mut cache).unwrap();

        // Tree is at depth 0 (capacity = 510). u64::MAX is far beyond.
        assert_eq!(ht.depth(), 0);

        let (new_root, prev_entry) = ht.delete(&mut cache, root, u64::MAX).unwrap();

        assert_eq!(prev_entry, None);
        assert_eq!(
            new_root, root,
            "delete beyond capacity must not COW the tree"
        );
        assert_eq!(
            ht.depth(),
            0,
            "delete beyond capacity must NOT grow the tree (insert would have grown; delete must not)"
        );
    }

    // Regression test for ISSUES.md I26. `find_leaf` did not bounds-check
    // `child_idx` against `PTRS_PER_INTERIOR`: any `handle >= capacity()`
    // walked the offset calculation past the last valid child pointer. At
    // the first-overflow boundary (`child_idx == PTRS_PER_INTERIOR`, i.e.
    // handle == ENTRIES_PER_LEAF * PTRS_PER_INTERIOR at depth 1) the code
    // read `buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 8]` — the XXH3 checksum
    // bytes of the interior page — and treated that nonzero u64 as a
    // child page id. Descent then called `cache.get(checksum_as_id)`,
    // which failed fatally and poisoned the manager. At larger `child_idx`
    // the slice op panicked with out-of-bounds access. The contract
    // (from lookup's callers read()/update()/delete()) says an unknown
    // handle must surface as InvalidHandle, not as a poison or a crash.
    //
    // Post-fix: the capacity guard at the top of `find_leaf` short-circuits
    // to Ok(None), so any handle beyond the tree's current reach is
    // reported as definitionally absent — same answer as a zero child
    // pointer encountered mid-descent.
    #[test]
    fn lookup_handle_beyond_capacity_returns_none() {
        let mut cache = make_cache();
        let mut ht = HandleTable::new();
        let root0 = ht.create_root(&mut cache).unwrap();

        let entry = HandleEntry {
            page_id: 42,
            slot_index: 7,
            flags: HandleFlags::Live,
            tag: 0,
        };
        // Grow to depth=1: handle 0 fits in the initial leaf; the second
        // insert at ENTRIES_PER_LEAF forces `grow()`.
        let root1 = ht.insert(&mut cache, root0, 0, &entry).unwrap();
        let root2 = ht
            .insert(&mut cache, root1, ENTRIES_PER_LEAF as u64, &entry)
            .unwrap();
        assert_eq!(ht.depth(), 1);

        // Capacity at depth=1 is ENTRIES_PER_LEAF * PTRS_PER_INTERIOR
        // (= 520_710). The FIRST out-of-range handle makes child_idx
        // exactly PTRS_PER_INTERIOR, whose offset lands on CHECKSUM_OFFSET
        // — the classic pre-fix failure site that read the checksum as
        // a page id and poisoned the cache via `get()` on a bogus id.
        let first_over = (ENTRIES_PER_LEAF as u64) * (PTRS_PER_INTERIOR as u64);
        assert_eq!(
            ht.lookup(&mut cache, root2, first_over).unwrap(),
            None,
            "handle at exact capacity must be reported absent, not trigger a fatal cache.get"
        );

        // A much larger handle would, pre-fix, panic on the slice op
        // because child_idx * CHILD_PTR_SIZE overflows PAGE_SIZE. The
        // capacity guard must cover this path too — not just the neat
        // first-overflow boundary.
        assert_eq!(
            ht.lookup(&mut cache, root2, u64::MAX).unwrap(),
            None,
            "u64::MAX handle must not panic nor read past the page"
        );
    }

    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──
    //
    // Helper: handle-table tests need pages 0/1 reserved (the real
    // database puts the superblock there) so that new_page() never
    // returns 0, which would collide with the "no child allocated" zero
    // sentinel inside interior nodes (ISSUES.md I8).
    //
    // Distinct from `make_cache` above (1024-page fixed cap) because
    // the migrated tests need a parameterised cap — one of them uses a
    // 2048-page cache to fit the COW path of a two-level grow.
    fn ht_cache(max_pages: usize) -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut cache = PageCache::new(
            io,
            max_pages as u64 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        cache.set_next_page_id(2);
        cache
    }

    #[test]
    fn test_handle_table_insert_and_lookup() {
        let mut cache = ht_cache(64);
        let mut ht = HandleTable::new();
        let root = ht.create_root(&mut cache).unwrap();
        let entry = HandleEntry {
            page_id: 10,
            slot_index: 3,
            flags: HandleFlags::Live,
            tag: 0,
        };
        let new_root = ht.insert(&mut cache, root, 0, &entry).unwrap();
        let found = ht.lookup(&mut cache, new_root, 0).unwrap().unwrap();
        assert_eq!(found.page_id, 10);
        assert_eq!(found.slot_index, 3);
    }

    #[test]
    fn test_handle_table_multiple_entries() {
        let mut cache = ht_cache(64);
        let mut ht = HandleTable::new();
        let mut root = ht.create_root(&mut cache).unwrap();
        for i in 0..10u64 {
            let entry = HandleEntry {
                page_id: 100 + i,
                slot_index: i as u16,
                flags: HandleFlags::Live,
                tag: 0,
            };
            root = ht.insert(&mut cache, root, i, &entry).unwrap();
        }
        for i in 0..10u64 {
            let found = ht.lookup(&mut cache, root, i).unwrap().unwrap();
            assert_eq!(found.page_id, 100 + i);
            assert_eq!(found.slot_index, i as u16);
        }
    }

    #[test]
    fn test_handle_table_cow_returns_new_root() {
        let mut cache = ht_cache(64);
        let mut ht = HandleTable::new();
        let root1 = ht.create_root(&mut cache).unwrap();
        let entry = HandleEntry {
            page_id: 10,
            slot_index: 0,
            flags: HandleFlags::Live,
            tag: 0,
        };
        let root2 = ht.insert(&mut cache, root1, 0, &entry).unwrap();
        assert_ne!(root1, root2);
    }

    // The handle table's two-level growth requires COW-copying the path on
    // every insert, so 520 inserts allocate significantly more than 520
    // pages. max_pages=2048 gives comfortable room (was 256×8=2048 under
    // the old 8× HARD_CEILING_MULTIPLIER). With spillway_max_bytes=0 the
    // cap is now strict, so the helper needs the full 2048 budget.
    #[test]
    fn test_handle_table_grows_to_two_levels() {
        let mut cache = ht_cache(2048);
        let mut ht = HandleTable::new();
        let mut root = ht.create_root(&mut cache).unwrap();
        for i in 0..(ENTRIES_PER_LEAF as u64 + 10) {
            let entry = HandleEntry {
                page_id: i,
                slot_index: 0,
                flags: HandleFlags::Live,
                tag: 0,
            };
            root = ht.insert(&mut cache, root, i, &entry).unwrap();
        }
        for i in 0..(ENTRIES_PER_LEAF as u64 + 10) {
            let found = ht.lookup(&mut cache, root, i).unwrap().unwrap();
            assert_eq!(found.page_id, i);
        }
    }

    #[test]
    fn test_handle_table_delete() {
        let mut cache = ht_cache(64);
        let mut ht = HandleTable::new();
        let mut root = ht.create_root(&mut cache).unwrap();
        let entry = HandleEntry {
            page_id: 10,
            slot_index: 0,
            flags: HandleFlags::Live,
            tag: 0,
        };
        root = ht.insert(&mut cache, root, 0, &entry).unwrap();
        let (new_root, prev) = ht.delete(&mut cache, root, 0).unwrap();
        root = new_root;
        assert!(prev.is_some(), "delete of a live entry must return Some");
        let found = ht.lookup(&mut cache, root, 0).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn handle_entry_tag_round_trips_through_a_leaf_slot() {
        let mut buf = [0u8; PAGE_SIZE];
        let entry = HandleEntry {
            page_id: 42,
            slot_index: 7,
            flags: HandleFlags::Live,
            tag: 0xDEADBEEF,
        };
        HandleTable::write_entry(&mut buf, 3, &entry);
        let read = HandleTable::read_entry(&buf, 3);
        assert_eq!(read, entry);
        assert_eq!(read.tag, 0xDEADBEEF);
        let zeroed = HandleTable::read_entry(&[0u8; PAGE_SIZE], 0);
        assert_eq!(zeroed.tag, 0);
    }
}
