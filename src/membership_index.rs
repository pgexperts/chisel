//! Membership index for chunk tags: maps a `u32` tag to the set of handles that
//! carry it. Built from one generic copy-on-write radix (`RadixU64`: u64 key ->
//! u64 value, 0 = absent) used twice — an outer tree keyed by tag whose value
//! bit-packs `(inner_depth:6 | inner_root:58)`, and per-tag inner trees keyed by
//! handle storing `1` for "present". See docs/specs/2026-06-02-chunk-tags-design.md.
//!
//! Layer dependency: page + page_cache only (strictly below transaction.rs).
//! Like the handle table, this module returns the new root page id after a COW
//! mutation; all page dirtiness lives in `PageCache`, flushed at commit.

use crate::error::{ChiselError, Result};
use crate::page::{
    self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_ID_NONE, PAGE_SIZE,
};
use crate::page_cache::PageCache;

// Leaf values and interior child pointers are both 8-byte little-endian u64s,
// so one constant is the fan-out at every level. 1021 = (8184 - 16) / 8.
const SLOT_SIZE: usize = 8;
const SLOTS_PER_PAGE: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / SLOT_SIZE; // 1021

// Maximum valid radix depth. capacity(depth) = SLOTS_PER_PAGE^(depth+1); for
// SLOTS_PER_PAGE = 1021, 1021^6 ≈ 1.1e18 < u64::MAX < 1021^7, so a tree keyed by
// u64 values is never deeper than 6 (a key in (capacity(5), u64::MAX] forces one
// final grow to depth 6, whose capacity saturates to u64::MAX). Any spine or
// packed inner-depth claiming a deeper tree is corrupt — used by recover_depth's
// cap (which also bounds a cyclic spine) and the unpack_inner range check.
const MAX_DEPTH: u32 = 6;

fn read_slot(buf: &[u8; PAGE_SIZE], index: usize) -> u64 {
    let off = DATA_PAGE_HEADER_SIZE + index * SLOT_SIZE;
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_slot(buf: &mut [u8; PAGE_SIZE], index: usize, value: u64) {
    let off = DATA_PAGE_HEADER_SIZE + index * SLOT_SIZE;
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// Initialize a fresh zeroed radix page of the given type and stamp it.
fn init_page(cache: &mut PageCache, page_type: PageType) -> Result<u64> {
    let id = cache.new_page()?;
    debug_assert_ne!(id, 0, "membership-index pages must not use page id 0");
    let buf = cache.get_mut(id)?;
    buf.fill(0);
    buf[0] = page_type as u8;
    buf[1] = page::PAGE_FORMAT_VERSION_CURRENT; // version at byte 1 (no FLAG byte)
    page::stamp_checksum(buf);
    Ok(id)
}

/// A copy-on-write radix tree: `u64` key -> `u64` value, where `0` means absent
/// (a zeroed leaf reads as all-absent, mirroring the handle table's tombstone
/// trick). The caller owns `depth` and the root page id.
pub(crate) struct RadixU64 {
    pub depth: u32,
}

impl RadixU64 {
    #[allow(dead_code)] // used by tests; production builds RadixU64 { depth } directly
    pub fn new() -> RadixU64 {
        RadixU64 { depth: 0 }
    }

    /// Create a new empty root leaf. Returns its page id.
    pub fn create_root(&mut self, cache: &mut PageCache) -> Result<u64> {
        self.depth = 0;
        init_page(cache, PageType::MembershipLeaf)
    }

    // saturating_mul (not `*=`): a valid tree never exceeds MAX_DEPTH (capacity
    // fits u64), but a corrupt-but-checksummed page can supply an out-of-range
    // depth (via recover_depth on a bad spine, or unpack_inner's 6-bit field).
    // Saturating to u64::MAX keeps `key >= capacity()` in find_leaf correctly
    // true (rejecting the descent) instead of a debug panic / release wrap.
    fn capacity(&self) -> u64 {
        let mut cap = SLOTS_PER_PAGE as u64;
        for _ in 0..self.depth {
            cap = cap.saturating_mul(SLOTS_PER_PAGE as u64);
        }
        cap
    }

    fn span_at_level(&self, level: u32) -> u64 {
        let mut span = SLOTS_PER_PAGE as u64;
        for _ in 1..level {
            span = span.saturating_mul(SLOTS_PER_PAGE as u64);
        }
        span
    }

    fn find_leaf(
        &self,
        cache: &mut PageCache,
        root: u64,
        key: u64,
    ) -> Result<Option<(u64, usize)>> {
        // Reject keys past the tree's reach (I26). When capacity() SATURATED to
        // u64::MAX (depth 6, where the real capacity exceeds u64::MAX), every u64
        // key is in reach, so the guard is skipped — otherwise the single key
        // u64::MAX would be wrongly reported absent. The child_idx bound below
        // still protects the descent.
        let cap = self.capacity();
        if self.depth > 0 && cap != u64::MAX && key >= cap {
            return Ok(None);
        }
        if self.depth == 0 {
            return Ok(Some((root, (key % SLOTS_PER_PAGE as u64) as usize)));
        }
        let mut current = root;
        let mut remaining = key;
        for level in (1..=self.depth).rev() {
            let span = self.span_at_level(level);
            let child_idx = (remaining / span) as usize;
            // Defense-in-depth: a validated depth (≤ MAX_DEPTH) guarantees
            // child_idx < SLOTS_PER_PAGE, but guard the slot index directly so a
            // bad index reads as "absent" rather than slicing past the page
            // (read_slot would index buf[16 + child_idx*8 ..] out of bounds).
            if child_idx >= SLOTS_PER_PAGE {
                return Ok(None);
            }
            remaining %= span;
            let child = read_slot(cache.get(current)?, child_idx);
            if child == 0 {
                return Ok(None);
            }
            current = child;
        }
        Ok(Some((
            current,
            (remaining % SLOTS_PER_PAGE as u64) as usize,
        )))
    }

    /// Return the value for `key`, or `0` if absent.
    pub fn lookup(&self, cache: &mut PageCache, root: u64, key: u64) -> Result<u64> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let Some((leaf, idx)) = self.find_leaf(cache, root, key)? else {
            return Ok(0);
        };
        Ok(read_slot(cache.get(leaf)?, idx))
    }

    // `old_root` is reparented as child 0 (not cloned), so it is NOT
    // superseded and contributes nothing to the freed list — `grow` allocates
    // via the freemap-aware `alloc` but frees nothing.
    fn grow(
        &mut self,
        cache: &mut PageCache,
        old_root: u64,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<u64> {
        let new_root = alloc(cache)?;
        debug_assert_ne!(new_root, 0);
        let buf = cache.get_mut(new_root)?;
        buf.fill(0);
        buf[0] = PageType::MembershipInterior as u8;
        buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
        write_slot(buf, 0, old_root); // old root becomes child 0
        page::stamp_checksum(buf);
        self.depth += 1;
        Ok(new_root)
    }

    /// Insert `value` (must be non-zero) at `key`. Returns the new root.
    /// `alloc` is the transaction layer's freemap-aware page allocator (reuses
    /// prior-transaction freed pages before extending); `freed` collects the
    /// page ids this call supersedes so the caller can return them to the
    /// freemap on commit. Threading both is what keeps the membership index at
    /// a bounded steady-state page count under churn.
    pub fn insert(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        key: u64,
        value: u64,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        debug_assert_ne!(value, 0, "0 is the absent sentinel; cannot be stored");
        let mut current_root = root;
        while key >= self.capacity() {
            current_root = self.grow(cache, current_root, alloc)?;
        }
        self.insert_recursive(cache, current_root, key, value, self.depth, alloc, freed)
    }

    // Args are the recursion state plus the two reclamation channels
    // (`alloc`/`freed`), which travel together at every level.
    #[allow(clippy::too_many_arguments)]
    fn insert_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        key: u64,
        value: u64,
        level: u32,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        let new_page = alloc(cache)?;
        debug_assert_ne!(new_page, 0);
        {
            let old: [u8; PAGE_SIZE] = *cache.get(page)?;
            cache.get_mut(new_page)?.copy_from_slice(&old);
        }
        // `page` is superseded by `new_page`; queue it for reclamation.
        freed.push(page);
        if level == 0 {
            let idx = (key % SLOTS_PER_PAGE as u64) as usize;
            let buf = cache.get_mut(new_page)?;
            write_slot(buf, idx, value);
            page::stamp_checksum(buf);
            Ok(new_page)
        } else {
            let span = self.span_at_level(level);
            let child_idx = (key / span) as usize;
            let child = read_slot(cache.get(new_page)?, child_idx);
            let actual_child = if child == 0 {
                let pt = if level == 1 {
                    PageType::MembershipLeaf
                } else {
                    PageType::MembershipInterior
                };
                // A fresh child here is immediately re-COWed by the recursive
                // call below (it becomes that frame's superseded `page` and is
                // pushed to `freed` there); benign, first-touch only. Allocated
                // via the freemap-aware `alloc` like every other COW page.
                let id = alloc(cache)?;
                let buf = cache.get_mut(id)?;
                buf.fill(0);
                buf[0] = pt as u8;
                buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
                page::stamp_checksum(buf);
                id
            } else {
                child
            };
            let new_child = self.insert_recursive(
                cache,
                actual_child,
                key % span,
                value,
                level - 1,
                alloc,
                freed,
            )?;
            let buf = cache.get_mut(new_page)?;
            write_slot(buf, child_idx, new_child);
            page::stamp_checksum(buf);
            Ok(new_page)
        }
    }

    /// Set `key` to absent. Returns `(new_root, prev_value)`; `prev_value == 0`
    /// means it was already absent and no COW happened.
    pub fn delete(
        &mut self,
        cache: &mut PageCache,
        root: u64,
        key: u64,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        freed: &mut Vec<u64>,
    ) -> Result<(u64, u64)> {
        if root == PAGE_ID_NONE || key >= self.capacity() {
            return Ok((root, 0));
        }
        self.delete_recursive(cache, root, key, self.depth, alloc, freed)
    }

    fn delete_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        key: u64,
        level: u32,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        freed: &mut Vec<u64>,
    ) -> Result<(u64, u64)> {
        if level == 0 {
            let idx = (key % SLOTS_PER_PAGE as u64) as usize;
            let prev = read_slot(cache.get(page)?, idx);
            if prev == 0 {
                return Ok((page, 0));
            }
            let new_leaf = alloc(cache)?;
            debug_assert_ne!(new_leaf, 0);
            {
                let old: [u8; PAGE_SIZE] = *cache.get(page)?;
                *cache.get_mut(new_leaf)? = old;
            }
            {
                let buf = cache.get_mut(new_leaf)?;
                write_slot(buf, idx, 0);
                page::stamp_checksum(buf);
            }
            // Old leaf superseded by `new_leaf`; queue it.
            freed.push(page);
            Ok((new_leaf, prev))
        } else {
            let span = self.span_at_level(level);
            let child_idx = (key / span) as usize;
            let child = read_slot(cache.get(page)?, child_idx);
            if child == 0 {
                return Ok((page, 0));
            }
            let (new_child, prev) =
                self.delete_recursive(cache, child, key % span, level - 1, alloc, freed)?;
            if prev == 0 {
                return Ok((page, 0));
            }
            let new_page = alloc(cache)?;
            debug_assert_ne!(new_page, 0);
            {
                let old: [u8; PAGE_SIZE] = *cache.get(page)?;
                *cache.get_mut(new_page)? = old;
            }
            {
                let buf = cache.get_mut(new_page)?;
                write_slot(buf, child_idx, new_child);
                page::stamp_checksum(buf);
            }
            // Old interior `page` superseded by `new_page`; queue it.
            freed.push(page);
            Ok((new_page, prev))
        }
    }

    /// Enumerate all `(key, value)` pairs with a non-zero value. The walk is a
    /// deterministic function of the tree's structure: for a fixed tree it
    /// returns the same pairs in the same order on every call, which is what
    /// backs the public within-session iteration-stability contract on
    /// `Chisel::handles_with_tag`. The order itself (currently ascending key) is
    /// unspecified and must not be relied upon.
    pub fn iter(&self, cache: &mut PageCache, root: u64) -> Result<Vec<(u64, u64)>> {
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.iter_recursive(cache, root, 0, self.depth, &mut out)?;
        Ok(out)
    }

    fn iter_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        base: u64,
        level: u32,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                let v = read_slot(buf, i);
                if v != 0 {
                    out.push((base.saturating_add(i as u64), v));
                }
            }
        } else {
            let span = self.span_at_level(level);
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| (i, read_slot(buf, i)))
                    .filter(|(_, c)| *c != 0)
                    .collect()
            };
            for (i, child) in children {
                // saturating: with a validated depth this never saturates, but a
                // bad depth makes `span` saturate to u64::MAX and `i as u64 * span`
                // would overflow-panic — saturate the prefix accumulation instead.
                let next_base = base.saturating_add((i as u64).saturating_mul(span));
                self.iter_recursive(cache, child, next_base, level - 1, out)?;
            }
        }
        Ok(())
    }

    /// Like `iter` but stops after collecting `limit` pairs — touches at most
    /// ~`limit` leaves' worth of work, giving the caller a bounded-time pass.
    pub fn iter_bounded(
        &self,
        cache: &mut PageCache,
        root: u64,
        limit: usize,
    ) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        if root == PAGE_ID_NONE || limit == 0 {
            return Ok(out);
        }
        self.iter_bounded_recursive(cache, root, 0, self.depth, limit, &mut out)?;
        Ok(out)
    }

    fn iter_bounded_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        base: u64,
        level: u32,
        limit: usize,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        if out.len() >= limit {
            return Ok(());
        }
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                if out.len() >= limit {
                    break;
                }
                let v = read_slot(buf, i);
                if v != 0 {
                    out.push((base.saturating_add(i as u64), v));
                }
            }
        } else {
            let span = self.span_at_level(level);
            let children: Vec<(usize, u64)> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| (i, read_slot(buf, i)))
                    .filter(|(_, c)| *c != 0)
                    .collect()
            };
            for (i, child) in children {
                if out.len() >= limit {
                    break;
                }
                // saturating: matches iter_recursive — a corrupt depth of exactly
                // MAX_DEPTH (which passes the unpack_inner check) makes `span`
                // large enough that `i as u64 * span` overflow-panics for a
                // non-zero child slot i >= 17. Saturate the prefix instead.
                let next_base = base.saturating_add((i as u64).saturating_mul(span));
                self.iter_bounded_recursive(cache, child, next_base, level - 1, limit, out)?;
            }
        }
        Ok(())
    }

    /// Early-exit emptiness check: `true` if any key has a non-zero value.
    /// Cheap when the tree is non-empty (stops at the first hit); `O(tree)` only
    /// when it is empty.
    pub fn any_present(&self, cache: &mut PageCache, root: u64) -> Result<bool> {
        if root == PAGE_ID_NONE {
            return Ok(false);
        }
        self.any_recursive(cache, root, self.depth)
    }

    fn any_recursive(&self, cache: &mut PageCache, page: u64, level: u32) -> Result<bool> {
        if level == 0 {
            let buf = cache.get(page)?;
            for i in 0..SLOTS_PER_PAGE {
                if read_slot(buf, i) != 0 {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            let children: Vec<u64> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| read_slot(buf, i))
                    .filter(|c| *c != 0)
                    .collect()
            };
            for child in children {
                if self.any_recursive(cache, child, level - 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    /// Recover a tree's depth by walking the leftmost spine from `root` (mirrors
    /// the handle-table open-time depth recovery; relies on `grow` always
    /// installing the old root at child 0).
    pub fn recover_depth(cache: &mut PageCache, root: u64) -> Result<u32> {
        if root == PAGE_ID_NONE {
            return Ok(0);
        }
        let mut depth = 0u32;
        let mut current = root;
        loop {
            let buf = cache.get(current)?;
            if buf[0] != PageType::MembershipInterior as u8 {
                break;
            }
            depth += 1;
            // Cap the walk at MAX_DEPTH. A valid spine is never deeper; a corrupt
            // (but checksum-valid) spine that is over-deep OR cyclic (a child-0
            // cycle revisits interior pages and grows depth without bound) is
            // caught here as a typed CorruptPage instead of an infinite loop.
            // This is the cycle guard too — no visited-set needed.
            if depth > MAX_DEPTH {
                return Err(ChiselError::CorruptPage { page_id: current });
            }
            let child = read_slot(buf, 0);
            if child == 0 {
                break;
            }
            current = child;
        }
        Ok(depth)
    }
}

// The outer tree's value bit-packs the inner tree's (depth, root): depth in the
// top 6 bits (max radix depth for u64 keys is < 7), root in the low 58 bits.
// Page ids never approach 2^58 (2^58 * 8 KiB ~= 2.3 ZiB), so this is lossless.
const INNER_ROOT_BITS: u32 = 58;
const INNER_ROOT_MASK: u64 = (1u64 << INNER_ROOT_BITS) - 1;

fn pack_inner(root: u64, depth: u32) -> u64 {
    debug_assert!(root <= INNER_ROOT_MASK, "page id exceeds 2^58");
    debug_assert!(
        depth < (1 << (64 - INNER_ROOT_BITS)),
        "inner depth too large"
    );
    ((depth as u64) << INNER_ROOT_BITS) | root
}

fn unpack_inner(packed: u64) -> (u64, u32) {
    (packed & INNER_ROOT_MASK, (packed >> INNER_ROOT_BITS) as u32)
}

/// Progress report from a bounded `delete_with_tag` pass. Produced ONLY on the
/// success path: a mid-pass error returns `Err` and no `TagDropProgress`, so
/// the handles dropped before the failure are not reported (the per-delete
/// state stays consistent — see `Chisel::delete_with_tag` — only the reporting
/// is lost).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TagDropProgress {
    /// Handles removed from the index in this pass (the engine deletes their
    /// chunks). May be fewer than `max` if the tag emptied first.
    pub deleted: Vec<u64>,
    /// `true` if the tag has no remaining members after this pass.
    pub complete: bool,
}

/// Two-level membership index: tag -> {handles}. Owns the outer tree's depth;
/// inner depths ride the outer values via `pack_inner`. The caller (transaction
/// layer) owns the outer root and threads it through `Roots`/superblock.
pub(crate) struct MembershipIndex {
    outer_depth: u32,
}

impl MembershipIndex {
    pub fn new() -> MembershipIndex {
        MembershipIndex { outer_depth: 0 }
    }

    /// Restore the outer depth on open (the engine calls `RadixU64::recover_depth`
    /// on the index root and passes it here).
    ///
    /// This is the ONLY depth recovered at open. Per-tag inner depths are never
    /// reconstructed separately: each inner tree's depth is self-describing,
    /// `pack_inner`'d into its outer-leaf value and read back via `unpack_inner`
    /// on every operation. That is why one `u32` suffices to restore a two-level
    /// structure.
    pub fn set_outer_depth(&mut self, depth: u32) {
        self.outer_depth = depth;
    }

    /// Insert `(tag, handle)`. `tag` must be non-zero (0 = untagged is filtered
    /// by the caller). Returns the new outer root.
    pub fn insert(
        &mut self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        freed: &mut Vec<u64>,
    ) -> Result<u64> {
        let mut outer = RadixU64 {
            depth: self.outer_depth,
        };
        let root = if outer_root == PAGE_ID_NONE {
            outer.create_root(cache)?
        } else {
            outer_root
        };
        let (mut inner_root, inner_depth) = unpack_inner(outer.lookup(cache, root, tag as u64)?);
        // A corrupt outer-leaf packed value can claim an out-of-range inner
        // depth (6-bit field). Reject it as a typed CorruptPage before building
        // a bogus-depth descent. (inner_root == 0 means the tag is absent — a
        // fresh inner tree at depth 0, validated by construction.)
        if inner_root != 0 && inner_depth > MAX_DEPTH {
            return Err(ChiselError::CorruptPage {
                page_id: inner_root,
            });
        }
        let mut inner = RadixU64 { depth: inner_depth };
        if inner_root == 0 {
            inner_root = inner.create_root(cache)?;
        }
        let new_inner_root = inner.insert(cache, inner_root, handle, 1, alloc, freed)?;
        let packed = pack_inner(new_inner_root, inner.depth);
        let new_outer_root = outer.insert(cache, root, tag as u64, packed, alloc, freed)?;
        self.outer_depth = outer.depth;
        Ok(new_outer_root)
    }

    /// Remove `(tag, handle)`. Returns `(new_outer_root, was_present)`. Reclaims
    /// the tag's outer entry when its last member is removed (cheap: the
    /// emptiness check early-exits unless the tag truly emptied).
    pub fn remove(
        &mut self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
        alloc: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        freed: &mut Vec<u64>,
    ) -> Result<(u64, bool)> {
        if outer_root == PAGE_ID_NONE {
            return Ok((outer_root, false));
        }
        let mut outer = RadixU64 {
            depth: self.outer_depth,
        };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        // Reject a corrupt out-of-range inner depth as a typed CorruptPage
        // (skip when the tag is absent: inner_root == 0).
        if inner_root != 0 && inner_depth > MAX_DEPTH {
            return Err(ChiselError::CorruptPage {
                page_id: inner_root,
            });
        }
        if inner_root == 0 {
            return Ok((outer_root, false));
        }
        let mut inner = RadixU64 { depth: inner_depth };
        let (new_inner_root, prev) = inner.delete(cache, inner_root, handle, alloc, freed)?;
        if prev == 0 {
            return Ok((outer_root, false));
        }
        let new_outer_root = if inner.any_present(cache, new_inner_root)? {
            outer.insert(
                cache,
                outer_root,
                tag as u64,
                pack_inner(new_inner_root, inner.depth),
                alloc,
                freed,
            )?
        } else {
            // The tag's last member is gone: drop the outer entry. NOTE: the
            // now-orphaned inner tree (`new_inner_root` and its pages) is NOT
            // reclaimed here — that is the separate emptied-subtree compaction
            // concern, out of scope for COW-supersession reclamation.
            let (r, _) = outer.delete(cache, outer_root, tag as u64, alloc, freed)?;
            r
        };
        self.outer_depth = outer.depth;
        Ok((new_outer_root, true))
    }

    #[allow(dead_code)] // index-completeness + tests; production reads tags via handle_table.lookup
    pub fn contains(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
    ) -> Result<bool> {
        if outer_root == PAGE_ID_NONE {
            return Ok(false);
        }
        let outer = RadixU64 {
            depth: self.outer_depth,
        };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        // Reject a corrupt out-of-range inner depth as a typed CorruptPage
        // (skip when the tag is absent: inner_root == 0).
        if inner_root != 0 && inner_depth > MAX_DEPTH {
            return Err(ChiselError::CorruptPage {
                page_id: inner_root,
            });
        }
        if inner_root == 0 {
            return Ok(false);
        }
        let inner = RadixU64 { depth: inner_depth };
        Ok(inner.lookup(cache, inner_root, handle)? != 0)
    }

    pub fn handles_for_tag(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
    ) -> Result<Vec<u64>> {
        if outer_root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let outer = RadixU64 {
            depth: self.outer_depth,
        };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        // Reject a corrupt out-of-range inner depth as a typed CorruptPage
        // (skip when the tag is absent: inner_root == 0).
        if inner_root != 0 && inner_depth > MAX_DEPTH {
            return Err(ChiselError::CorruptPage {
                page_id: inner_root,
            });
        }
        if inner_root == 0 {
            return Ok(Vec::new());
        }
        let inner = RadixU64 { depth: inner_depth };
        Ok(inner
            .iter(cache, inner_root)?
            .into_iter()
            .map(|(h, _)| h)
            .collect())
    }

    /// Return at most `limit` handles of `tag` (bounded enumeration). The engine
    /// loops `delete` over these so each `delete_with_tag` pass is bounded-time;
    /// `complete` is derived by the engine (it requests `max + 1` and checks the
    /// count). The full drop (members + chunks) lives in the engine because
    /// deleting a member's chunk is the engine's responsibility, not the index's.
    pub fn handles_for_tag_bounded(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        limit: usize,
    ) -> Result<Vec<u64>> {
        if outer_root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let outer = RadixU64 {
            depth: self.outer_depth,
        };
        let (inner_root, inner_depth) = unpack_inner(outer.lookup(cache, outer_root, tag as u64)?);
        // Reject a corrupt out-of-range inner depth as a typed CorruptPage
        // (skip when the tag is absent: inner_root == 0).
        if inner_root != 0 && inner_depth > MAX_DEPTH {
            return Err(ChiselError::CorruptPage {
                page_id: inner_root,
            });
        }
        if inner_root == 0 {
            return Ok(Vec::new());
        }
        let inner = RadixU64 { depth: inner_depth };
        Ok(inner
            .iter_bounded(cache, inner_root, limit)?
            .into_iter()
            .map(|(h, _)| h)
            .collect())
    }
}

// Test-only convenience wrappers (extend-only allocator, discarded freed list):
// the unit tests exercise radix-tree shape and two-level composition, not
// freemap reclamation.
#[cfg(test)]
impl RadixU64 {
    fn insert_t(&mut self, cache: &mut PageCache, root: u64, key: u64, value: u64) -> Result<u64> {
        self.insert(
            cache,
            root,
            key,
            value,
            &mut |c| c.new_page(),
            &mut Vec::new(),
        )
    }
    fn delete_t(&mut self, cache: &mut PageCache, root: u64, key: u64) -> Result<(u64, u64)> {
        self.delete(cache, root, key, &mut |c| c.new_page(), &mut Vec::new())
    }

    /// Test-only: collect every page id in this radix spine reachable from `root`.
    pub(crate) fn collect_page_ids(
        &self,
        cache: &mut PageCache,
        root: u64,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        if root == PAGE_ID_NONE {
            return Ok(());
        }
        self.collect_recursive(cache, root, self.depth, out)
    }

    fn collect_recursive(
        &self,
        cache: &mut PageCache,
        page: u64,
        level: u32,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        out.push(page);
        if level > 0 {
            let children: Vec<u64> = {
                let buf = cache.get(page)?;
                (0..SLOTS_PER_PAGE)
                    .map(|i| read_slot(buf, i))
                    .filter(|c| *c != 0)
                    .collect()
            };
            for child in children {
                self.collect_recursive(cache, child, level - 1, out)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl MembershipIndex {
    fn insert_t(
        &mut self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
    ) -> Result<u64> {
        self.insert(
            cache,
            outer_root,
            tag,
            handle,
            &mut |c| c.new_page(),
            &mut Vec::new(),
        )
    }
    fn remove_t(
        &mut self,
        cache: &mut PageCache,
        outer_root: u64,
        tag: u32,
        handle: u64,
    ) -> Result<(u64, bool)> {
        self.remove(
            cache,
            outer_root,
            tag,
            handle,
            &mut |c| c.new_page(),
            &mut Vec::new(),
        )
    }

    /// Test-only: collect every page id reachable from the outer root — the
    /// outer (tag) tree spine PLUS every per-tag inner (handle) tree spine.
    pub(crate) fn collect_page_ids(
        &self,
        cache: &mut PageCache,
        outer_root: u64,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        if outer_root == PAGE_ID_NONE {
            return Ok(());
        }
        let outer = RadixU64 {
            depth: self.outer_depth,
        };
        outer.collect_page_ids(cache, outer_root, out)?;
        // Each outer leaf value packs an inner tree's (depth, root); walk each.
        for (_tag, packed) in outer.iter(cache, outer_root)? {
            let (inner_root, inner_depth) = unpack_inner(packed);
            if inner_root != 0 {
                let inner = RadixU64 { depth: inner_depth };
                inner.collect_page_ids(cache, inner_root, out)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    // Same fixture shape as handle_table.rs tests: reserve pages 0/1 so
    // new_page() never returns the zero-child sentinel.
    fn cache(max_pages: usize) -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut c = PageCache::new(
            io,
            max_pages as u64 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        c.set_next_page_id(2);
        c
    }

    #[test]
    fn insert_lookup_delete_single_level() {
        let mut c = cache(64);
        let mut t = RadixU64::new();
        let root = t.create_root(&mut c).unwrap();
        assert_eq!(t.lookup(&mut c, root, 5).unwrap(), 0);
        let r = t.insert_t(&mut c, root, 5, 99).unwrap();
        assert_eq!(t.lookup(&mut c, r, 5).unwrap(), 99);
        assert!(t.any_present(&mut c, r).unwrap());
        let (r2, prev) = t.delete_t(&mut c, r, 5).unwrap();
        assert_eq!(prev, 99);
        assert_eq!(t.lookup(&mut c, r2, 5).unwrap(), 0);
        assert!(!t.any_present(&mut c, r2).unwrap());
    }

    // --- Corrupt-input robustness (deepdive review finding #3) ---------------
    // A corrupt-but-checksummed page is only stopped by the XXH3 checksum on
    // load; its page-type / slot / packed-value bytes are NOT validated. These
    // tests pin that such input yields a typed CorruptPage (or a bounded,
    // non-panicking result) rather than a panic (OOB slice), a hang (cyclic
    // recover_depth spine), or an arithmetic overflow.

    // capacity()/span_at_level() must SATURATE on an out-of-range depth (the
    // unpack_inner 6-bit field can be up to 63) instead of overflowing — a
    // debug-build multiply panic, or a release wrap to a small value that would
    // defeat find_leaf's `key >= capacity()` bounds guard.
    #[test]
    fn capacity_and_span_saturate_on_out_of_range_depth() {
        let r = RadixU64 { depth: 30 };
        assert_eq!(r.capacity(), u64::MAX);
        assert_eq!(r.span_at_level(30), u64::MAX);
    }

    // recover_depth must reject a corrupt (checksum-valid) interior spine whose
    // child-0 forms a cycle — pre-fix this loops forever (open-time hang); the
    // depth cap returns a typed CorruptPage instead.
    #[test]
    fn recover_depth_rejects_cyclic_spine() {
        let mut c = cache(64);
        let id = c.new_page().unwrap();
        {
            let buf = c.get_mut(id).unwrap();
            buf.fill(0);
            buf[0] = PageType::MembershipInterior as u8;
            buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
            write_slot(buf, 0, id); // child-0 -> self: a cycle
            page::stamp_checksum(buf);
        }
        let err = match RadixU64::recover_depth(&mut c, id) {
            Ok(d) => panic!("recover_depth accepted a cyclic spine (depth {d})"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ChiselError::CorruptPage { .. }),
            "expected CorruptPage, got {err:?}"
        );
    }

    // A corrupt outer-leaf packed value can claim an out-of-range inner-tree
    // depth (the field is 6 bits, 0..=63). The MembershipIndex methods must
    // reject it as a typed CorruptPage rather than descending a bogus-depth
    // inner tree (which lands on a wrong page id and surfaces a misleading
    // ChecksumMismatch — or, without the capacity saturation, would panic).
    #[test]
    fn corrupt_inner_depth_is_corrupt_page() {
        let mut c = cache(64);
        let mut idx = MembershipIndex::new();
        let root = idx.insert_t(&mut c, PAGE_ID_NONE, 7, 5).unwrap();
        // Outer tree is depth 0 (one tag): `root` is the outer leaf, and outer
        // slot (7 % 1021 = 7) packs tag 7's inner (depth, root).
        {
            let buf = c.get_mut(root).unwrap();
            let packed = read_slot(buf, 7);
            assert_ne!(packed, 0, "tag 7 should occupy outer slot 7");
            // Overwrite the 6-bit depth field with 30 (> MAX_DEPTH).
            let corrupt = (packed & INNER_ROOT_MASK) | (30u64 << INNER_ROOT_BITS);
            write_slot(buf, 7, corrupt);
            page::stamp_checksum(buf);
        }
        let err = match idx.handles_for_tag(&mut c, root, 7) {
            Ok(hs) => panic!("handles_for_tag accepted a corrupt inner depth: {hs:?}"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ChiselError::CorruptPage { .. }),
            "expected CorruptPage, got {err:?}"
        );
    }

    // iter_bounded's prefix multiply must saturate like iter's: a corrupt inner
    // depth of exactly MAX_DEPTH PASSES the unpack_inner check (6 is not > 6),
    // and a non-zero child at slot i >= 17 makes `i * span_at_level(6)` overflow
    // without saturation. Must not panic (it descends depth-bounded levels).
    #[test]
    fn iter_bounded_saturates_prefix_on_corrupt_max_depth() {
        let mut c = cache(64);
        let id = c.new_page().unwrap();
        {
            let buf = c.get_mut(id).unwrap();
            buf.fill(0);
            buf[0] = PageType::MembershipInterior as u8;
            buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
            write_slot(buf, 20, id); // non-zero child at slot 20 (>= 17)
            page::stamp_checksum(buf);
        }
        let t = RadixU64 { depth: MAX_DEPTH };
        // Pre-fix: `20 * span_at_level(6)` overflow-panics. Post-fix: saturates.
        let _ = t.iter_bounded(&mut c, id, 8).unwrap();
    }

    #[test]
    fn grows_and_iterates_across_levels() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        for k in [0u64, 1, 1021, 2000, 1_000_000] {
            root = t.insert_t(&mut c, root, k, k + 7).unwrap();
        }
        assert!(t.depth >= 1, "tree should have grown");
        for k in [0u64, 1, 1021, 2000, 1_000_000] {
            assert_eq!(t.lookup(&mut c, root, k).unwrap(), k + 7);
        }
        let mut pairs = t.iter(&mut c, root).unwrap();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (0, 7),
                (1, 8),
                (1021, 1028),
                (2000, 2007),
                (1_000_000, 1_000_007)
            ]
        );
    }

    #[test]
    fn iter_bounded_caps_collection() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        for k in 0..50u64 {
            root = t.insert_t(&mut c, root, k, k + 1).unwrap();
        }
        assert_eq!(t.iter_bounded(&mut c, root, 10).unwrap().len(), 10);
        assert_eq!(t.iter_bounded(&mut c, root, 100).unwrap().len(), 50);
    }

    #[test]
    fn delete_absent_is_noop() {
        let mut c = cache(64);
        let mut t = RadixU64::new();
        let root = t.create_root(&mut c).unwrap();
        let (r, prev) = t.delete_t(&mut c, root, 42).unwrap();
        assert_eq!(prev, 0);
        assert_eq!(r, root, "no COW for an absent key");
    }

    #[test]
    fn recover_depth_matches_after_grow() {
        let mut c = cache(8192);
        let mut t = RadixU64::new();
        let mut root = t.create_root(&mut c).unwrap();
        root = t.insert_t(&mut c, root, 5_000_000, 1).unwrap();
        let recovered = RadixU64::recover_depth(&mut c, root).unwrap();
        assert_eq!(recovered, t.depth);
    }

    #[test]
    fn membership_insert_contains_remove() {
        let mut c = cache(8192);
        let mut idx = MembershipIndex::new();
        let mut root = PAGE_ID_NONE;
        root = idx.insert_t(&mut c, root, 7, 100).unwrap();
        root = idx.insert_t(&mut c, root, 7, 200).unwrap();
        root = idx.insert_t(&mut c, root, 9, 300).unwrap();
        assert!(idx.contains(&mut c, root, 7, 100).unwrap());
        assert!(idx.contains(&mut c, root, 7, 200).unwrap());
        assert!(!idx.contains(&mut c, root, 7, 300).unwrap());
        let mut h7 = idx.handles_for_tag(&mut c, root, 7).unwrap();
        h7.sort();
        assert_eq!(h7, vec![100, 200]);
        assert_eq!(idx.handles_for_tag(&mut c, root, 9).unwrap(), vec![300]);

        let (root2, removed) = idx.remove_t(&mut c, root, 7, 100).unwrap();
        assert!(removed);
        assert!(!idx.contains(&mut c, root2, 7, 100).unwrap());
        assert!(idx.contains(&mut c, root2, 7, 200).unwrap());
        let (_root3, removed_again) = idx.remove_t(&mut c, root2, 7, 100).unwrap();
        assert!(!removed_again, "removing an absent member reports false");
    }

    #[test]
    fn handles_for_tag_bounded_caps_results() {
        let mut c = cache(16384);
        let mut idx = MembershipIndex::new();
        let mut root = PAGE_ID_NONE;
        for h in 0..10u64 {
            root = idx.insert_t(&mut c, root, 3, 1000 + h).unwrap();
        }
        assert_eq!(
            idx.handles_for_tag_bounded(&mut c, root, 3, 4)
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            idx.handles_for_tag_bounded(&mut c, root, 3, 100)
                .unwrap()
                .len(),
            10
        );
        assert_eq!(
            idx.handles_for_tag_bounded(&mut c, root, 3, 5)
                .unwrap()
                .len(),
            5
        );
    }

    #[test]
    fn inner_grow_roundtrip() {
        // >1021 handles under one tag splits the inner tree past depth 0; the
        // post-grow depth must round-trip through pack_inner so readers descend
        // the right number of levels. The other membership tests stay at depth 0,
        // where a stale-vs-fresh inner-depth bug is invisible.
        let mut c = cache(1_000_000);
        let mut idx = MembershipIndex::new();
        let mut root = PAGE_ID_NONE;
        let n = 1100u64;
        for h in 0..n {
            root = idx.insert_t(&mut c, root, 7, h).unwrap();
        }
        let mut got = idx.handles_for_tag(&mut c, root, 7).unwrap();
        got.sort();
        assert_eq!(got, (0..n).collect::<Vec<_>>());
        for h in 0..n {
            let (r, removed) = idx.remove_t(&mut c, root, 7, h).unwrap();
            root = r;
            assert!(removed);
        }
        assert!(idx.handles_for_tag(&mut c, root, 7).unwrap().is_empty());
    }
}
