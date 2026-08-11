// freemap_tree.rs — Multi-page freemap: a copy-on-write radix tree of bitmap
// leaves (layer 4: page-type-specific logic, alongside handle_table.rs and
// membership_index.rs).
//
// Role in the system: removes the single-page freemap's ~512 MB reclamation
// ceiling (one FreeMap page tracks only PAGE_BODY_SIZE*8 = 65,344 page ids;
// past that, mark_free silently no-ops and freed pages leak — a 2026-06-22
// fresh-eyes finding). The tree generalizes the freemap to
// LEAF_CAPACITY * PTRS_PER_INTERIOR^depth ids, so depth grows logarithmically
// with database size. Depth 0 IS today's format: a single FreeMap leaf pointed
// at directly by the root, so existing databases load unchanged.
// See docs/specs/2026-06-22-multi-page-freemap-design.md.
//
//   Leaf:     PageType::FreeMap (0x04) — a bitmap page, 1 bit = 1 page id,
//             unchanged byte-for-byte from the single-page format.
//   Interior: PageType::FreeMapInterior (0x07) — up to PTRS_PER_INTERIOR u64
//             child pointers (8-byte LE, from DATA_PAGE_HEADER_SIZE). A zero
//             child pointer = absent subtree = "that whole sub-range is all in
//             use" (lazy/sparse, mirroring the other two radixes).
//
// TERMINATION INVARIANT (critical): all structural page allocation — every
// interior/leaf COW copy AND every newly-materialized node — goes through the
// caller-supplied `extend` closure (in production `PageCache::new_page`, which
// extends the file; in tests `&mut |c| c.new_page()`), NEVER through the
// freemap itself. This breaks the "the free-list needs a free page to record
// free pages" recursion: a freemap mutation can never reuse a freemap-tracked
// page, so it always terminates. The freemap NEVER allocates from itself.
//
// COW discipline: every mutation rewrites the root + the spine down to the
// touched leaf into FRESH pages (via `extend`) and pushes each superseded OLD
// page id onto `pending_superseded`. The committed tree stays intact until the
// superblock flips; the integration unit drains `pending_superseded` into the
// freed list at commit. Growth (`grow`) reparents the old root as child 0 — the
// old root is NOT superseded (it lives on under the new root), so grow queues
// nothing.
//
// Page-type validation on descent: reading any tree page validates buf[0] is
// the expected type (FreeMap or FreeMapInterior); a checksum-valid wrong-type
// page (reached via a stale/corrupt child pointer) surfaces as
// ChiselError::CorruptPage, mirroring the overflow/data-page hardening.

use crate::error::{ChiselError, Result};
use crate::freemap::FreeMap;
use crate::page::{
    self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_BODY_SIZE, PAGE_SIZE,
};
use crate::page_cache::PageCache;

// One leaf bitmap covers PAGE_BODY_SIZE*8 = 65,344 page ids (≈ 2^16).
const LEAF_CAPACITY: u64 = (PAGE_BODY_SIZE * 8) as u64;

// Child pointers per interior page: same fan-out and layout as the membership
// interior. 1021 = (8184 - 16) / 8 ≈ 2^10.
const PTR_SIZE: usize = 8;
const PTRS_PER_INTERIOR: usize = (CHECKSUM_OFFSET - DATA_PAGE_HEADER_SIZE) / PTR_SIZE; // 1021

// Maximum valid tree depth. capacity(depth) = LEAF_CAPACITY * PTRS_PER_INTERIOR^depth.
// The leaf fans out to 2^16 and each level multiplies by ~2^10, so depth 5
// already covers 2^16 * 2^50 = 2^66 > u64::MAX page ids — the bound is 5
// (distinct from the membership tree's 6, which has no 2^16 leaf factor). A
// spine or stored depth claiming deeper is corrupt. capacity() saturates, but
// saturation only prevents arithmetic overflow — on its own it rejects nothing
// and does NOT make a bad on-disk depth fail closed. The actual rejection is
// the `freemap_depth > MAX_DEPTH` gate in `open_existing`, which is what keeps
// `scan_node`'s per-level recursion and `cow_descend`'s O(depth^2) loop from
// being driven by a hostile superblock. `pub(crate)` so that gate can name the
// constant instead of repeating the literal.
pub(crate) const MAX_DEPTH: u32 = 5;

fn read_child(buf: &[u8; PAGE_SIZE], index: usize) -> u64 {
    let off = DATA_PAGE_HEADER_SIZE + index * PTR_SIZE;
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_child(buf: &mut [u8; PAGE_SIZE], index: usize, value: u64) {
    let off = DATA_PAGE_HEADER_SIZE + index * PTR_SIZE;
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// Initialize a fresh zeroed interior page in `buf` and stamp it. A zeroed
/// child array reads as "all subtrees absent", the sparse-tree base case.
fn init_interior(buf: &mut [u8; PAGE_SIZE]) {
    buf.fill(0);
    buf[0] = PageType::FreeMapInterior as u8;
    buf[1] = page::current_version(PageType::FreeMapInterior); // version at byte 1
    page::stamp_checksum(buf);
}

/// Validate a freemap tree page's type byte before trusting its contents. The
/// cache verifies only the XXH3 checksum (catching bit-flips), NOT a
/// checksum-valid page of the WRONG type reached via a corrupt child pointer.
/// Mirrors overflow::read / data_page::validate_header.
fn check_type(buf: &[u8; PAGE_SIZE], expected: PageType, page_id: u64) -> Result<()> {
    if buf[0] != expected as u8 {
        return Err(ChiselError::CorruptPage { page_id });
    }
    Ok(())
}

/// A copy-on-write radix tree of bitmap leaves tracking free page ids. The
/// caller (the integration unit / transaction layer) owns `root` and `depth`,
/// threading them through the superblock; `pending_superseded` accumulates the
/// old page ids this tree's mutations have COW-superseded, for the caller to
/// drain into the freed list at commit.
///
/// COW-ONCE-PER-SESSION: a `FreeMapTree` handle represents one transaction's
/// working view. The FIRST mutation that touches a committed page COWs it (fresh
/// `extend` copy, old id queued on `pending_superseded`); SUBSEQUENT mutations
/// that re-touch a page THIS handle already created mutate it IN PLACE — no new
/// `extend`, no new supersede. Without this, `persist_freemap`'s loop over many
/// frees (all landing in the same depth-0 leaf) would re-COW the leaf once per
/// id, extending and superseding a fresh page each time and turning the
/// committed-free reclamation into unbounded file growth. The `session_owned`
/// set records every page this handle materialized or COW'd this transaction;
/// it is the in-memory "already dirty this txn" marker the spec's cost model
/// ("first touch this txn ... subsequent frees in the same leaf are in-place")
/// assumes. It is transient working state, never serialized.
pub(crate) struct FreeMapTree {
    pub root: u64,
    pub depth: u32,
    pub pending_superseded: Vec<u64>,
    // Pages this transaction has extended/COW'd (so a re-touch is in-place, not
    // a re-COW). FxHashSet: trusted local page ids, no DoS surface.
    //
    // The integration layer rebuilds a fresh `FreeMapTree` handle from
    // `{root, depth}` at EVERY allocation site within one transaction (data-page
    // alloc, each handle-table / membership COW, persist_freemap). Each of those
    // re-touches the same freemap leaf, so the dedup set must SURVIVE across
    // those handles or every site would re-COW the leaf. The manager therefore
    // owns the set for the whole transaction and swaps it into each transient
    // handle via `from_roots`, reading it back out after. `pub(crate)` so the
    // manager can move it in and out; `from_roots`/`create` seed it.
    pub(crate) session_owned: rustc_hash::FxHashSet<u64>,
}

impl FreeMapTree {
    /// Reconstruct a tree handle from a committed `(root, depth)` (open path).
    /// Starts with an empty `pending_superseded` — nothing has been superseded
    /// in the new transaction yet — and an empty `session_owned` set (no page is
    /// this-txn-dirty until the first mutation COWs one).
    pub fn from_roots(root: u64, depth: u32) -> FreeMapTree {
        FreeMapTree {
            root,
            depth,
            pending_superseded: Vec::new(),
            session_owned: rustc_hash::FxHashSet::default(),
        }
    }

    /// Create a fresh depth-0 tree: one zeroed FreeMap leaf (all bits 0 = all in
    /// use, the bootstrap-safe starting state). The leaf is allocated via
    /// `extend` like every structural page.
    pub fn create(
        cache: &mut PageCache,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<FreeMapTree> {
        let root = extend(cache)?;
        let buf = cache.get_mut(root)?;
        FreeMap::init_page(buf);
        page::stamp_checksum(buf);
        // The leaf was just extended by THIS handle, so it is in-place-mutable
        // for the rest of the transaction.
        let mut session_owned = rustc_hash::FxHashSet::default();
        session_owned.insert(root);
        Ok(FreeMapTree {
            root,
            depth: 0,
            pending_superseded: Vec::new(),
            session_owned,
        })
    }

    // capacity()/child_span() SATURATE (not `*=`): a valid tree never exceeds
    // MAX_DEPTH, but a corrupt-but-checksummed superblock could supply an
    // out-of-range depth. Saturating to u64::MAX keeps the descent's range guard
    // correct (every id is "in range" only when capacity truly covers u64)
    // instead of a debug panic / release wrap. Mirrors RadixU64::capacity.
    fn capacity(&self) -> u64 {
        let mut cap = LEAF_CAPACITY;
        for _ in 0..self.depth {
            cap = cap.saturating_mul(PTRS_PER_INTERIOR as u64);
        }
        cap
    }

    // Page ids covered by ONE child pointer at interior `level` (1..=depth).
    // Level 1's children are leaves, each covering LEAF_CAPACITY; each deeper
    // level multiplies by PTRS_PER_INTERIOR. saturating, same rationale as above.
    fn child_span(&self, level: u32) -> u64 {
        let mut span = LEAF_CAPACITY;
        for _ in 1..level {
            span = span.saturating_mul(PTRS_PER_INTERIOR as u64);
        }
        span
    }

    // Reject an out-of-range `depth` before any traversal begins. `open_existing`
    // already gates `freemap_depth > MAX_DEPTH` at the trust boundary, so this is
    // defense in depth — it matches what HandleTable::recover_depth and
    // RadixU64/MembershipIndex already do at every entry point, and it means a
    // future path that builds a tree from roots WITHOUT passing through the open
    // gate (a savepoint rewind, a rollback to a restored root) cannot resurrect
    // the hazard.
    //
    // It must run BEFORE the first `cache.get`, not merely before the recursion:
    // the traversals happen to fail type validation first for some root shapes,
    // which is luck, not a guarantee. A FreeMapInterior root with a
    // self-referential child pointer passes every type check and is precisely the
    // shape that drives scan_node into a stack overflow and cow_descend into its
    // O(depth^2) page-materializing loop.
    //
    // Note the saturating arithmetic above does NOT stand in for this: saturation
    // keeps `capacity()`/`child_span()` from overflowing, but a saturated span
    // simply yields child index 0 — it rejects nothing.
    fn check_depth(&self) -> Result<()> {
        if self.depth > MAX_DEPTH {
            return Err(ChiselError::CorruptPage { page_id: self.root });
        }
        Ok(())
    }

    /// Descend (read-only) to the leaf covering `id`. Returns the leaf page id,
    /// or `None` if `id` is past the tree's reach OR any subtree on the path is
    /// absent (a zero child pointer = "that whole range is all in use"). Every
    /// page read is type-validated.
    fn find_leaf(&self, cache: &mut PageCache, id: u64) -> Result<Option<u64>> {
        self.check_depth()?;
        // Reject ids past the tree's reach. When capacity saturated to u64::MAX
        // (depth where the real capacity exceeds u64), every id is in reach, so
        // skip the guard — otherwise id == u64::MAX would be wrongly rejected.
        let cap = self.capacity();
        if cap != u64::MAX && id >= cap {
            return Ok(None);
        }
        if self.depth == 0 {
            check_type(cache.get(self.root)?, PageType::FreeMap, self.root)?;
            return Ok(Some(self.root));
        }
        let mut current = self.root;
        let mut remaining = id;
        for level in (1..=self.depth).rev() {
            let buf = cache.get(current)?;
            check_type(buf, PageType::FreeMapInterior, current)?;
            let span = self.child_span(level);
            let child_idx = (remaining / span) as usize;
            // Defense-in-depth: a validated depth keeps child_idx in bounds, but
            // guard the slot index so a corrupt one reads as "absent" rather than
            // slicing past the page.
            if child_idx >= PTRS_PER_INTERIOR {
                return Ok(None);
            }
            let child = read_child(buf, child_idx);
            if child == 0 {
                return Ok(None); // absent subtree => all-in-use
            }
            remaining %= span;
            current = child;
        }
        check_type(cache.get(current)?, PageType::FreeMap, current)?;
        Ok(Some(current))
    }

    /// Is `id` currently free? Absent subtree (or past reach) reads as in-use.
    ///
    /// Read-only query: production allocation/free goes through `allocate_first`
    /// / `mark_free_growing`. The production consumer of this accessor is
    /// `FreemapRecycle::reclaim_orphans` (the defrag orphan-sweep), which calls
    /// it to confirm a freemap-typed dead page is genuinely free before
    /// reclaiming it; the in-crate tests and the transaction layer's
    /// C1-invariant assertions are the other (`#[cfg(test)]`) consumers.
    pub fn is_free(&self, cache: &mut PageCache, id: u64) -> Result<bool> {
        let Some(leaf) = self.find_leaf(cache, id)? else {
            return Ok(false);
        };
        Ok(FreeMap::is_free(cache.get(leaf)?, id % LEAF_CAPACITY))
    }

    /// Add a level: a fresh interior root with the old root reparented as child
    /// 0. depth += 1. No-op at MAX_DEPTH (the tree already covers all of u64).
    ///
    /// The old root is reparented, NOT cloned, so it is NOT superseded and pushes
    /// nothing onto `pending_superseded` — `grow` allocates via `extend` but frees
    /// nothing. (Mirrors RadixU64::grow.)
    pub fn grow(
        &mut self,
        cache: &mut PageCache,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            return Ok(());
        }
        let new_root = extend(cache)?;
        let buf = cache.get_mut(new_root)?;
        init_interior(buf);
        write_child(buf, 0, self.root); // old root becomes child 0
        page::stamp_checksum(buf);
        // Freshly extended by this handle => in-place-mutable hereafter.
        self.session_owned.insert(new_root);
        self.root = new_root;
        self.depth += 1;
        Ok(())
    }

    /// COW-rewrite the root + spine down to `id`'s leaf — materializing absent
    /// children along the way, pushing every superseded OLD page onto
    /// `pending_superseded`, stamping each interior after its child pointer is
    /// written — then apply `leaf_op(leaf_buf, id % LEAF_CAPACITY)` to the COW'd
    /// leaf and stamp it. The single tested spine implementation shared by both
    /// `mark_free` (leaf_op = set bit) and `clear_bit` (leaf_op = clear bit), so
    /// set and clear can never diverge.
    ///
    /// An `id` past the tree's reach is REJECTED as `CorruptPage`, never
    /// silently aliased. Callers still grow first when they may exceed capacity
    /// (`mark_free_growing`); the guard is what turns forgetting to into a loud
    /// failure rather than a write to the wrong page.
    fn cow_descend(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
        leaf_op: &mut dyn FnMut(&mut [u8; PAGE_SIZE], u64),
    ) -> Result<()> {
        self.check_depth()?;
        // FREEMAP-6 (issue #115): bound the WRITE path exactly as `find_leaf`
        // bounds the read path. Never write a bit this tree would refuse to
        // read — an unbounded id here is not a no-op, it is a write at the
        // WRONG id. At depth 0 the descent loop below never runs and the leaf op
        // lands on `id % LEAF_CAPACITY`, so `mark_free(LEAF_CAPACITY + 7)` would
        // free LIVE page 7 and the next `allocate_first` would hand it out —
        // while `is_free(LEAF_CAPACITY + 7)` still answers "in use", hiding the
        // damage at the id that was written and inflicting it on the id that was
        // not. `CorruptPage` (fatal) matches `check_depth` above: this cannot be
        // reached by correct code, so an occurrence means either the caller or
        // the stored depth is untrustworthy and no further writing is safe.
        //
        // Same saturation caveat as `find_leaf`: at a depth whose true capacity
        // exceeds u64 every id is in reach, so skip the guard rather than
        // rejecting id == u64::MAX.
        let cap = self.capacity();
        if cap != u64::MAX && id >= cap {
            return Err(ChiselError::CorruptPage { page_id: self.root });
        }
        // COW the root first; the new root replaces self.root and the old one is
        // superseded. (grow has already ensured depth covers id, if needed.)
        // The root's position type follows depth exactly as the read paths see it:
        // an interior at depth > 0, the lone leaf at depth 0.
        let root_expected = if self.depth > 0 {
            PageType::FreeMapInterior
        } else {
            PageType::FreeMap
        };
        let new_root = self.cow_node(cache, self.root, root_expected, extend)?;
        self.root = new_root;

        // Walk interiors, COWing each child (materializing absent ones) and
        // rewriting the parent's pointer to the new child. `current` is the
        // already-COW'd (or session-owned) page we are editing in place this
        // frame.
        let mut current = new_root;
        let mut remaining = id;
        for level in (1..=self.depth).rev() {
            let span = self.child_span(level);
            let child_idx = (remaining / span) as usize;
            // Defense in depth, the write-path counterpart to `find_leaf`'s
            // slot guard — and, like that one, not reachable by arithmetic: a
            // `check_depth`-validated depth plus the range guard above already
            // bound `child_idx` below PTRS_PER_INTERIOR at every level, and the
            // one depth where `capacity()` saturates (5) still divides by a
            // span just under 2^56, so the quotient tops out at 259 —
            // `mark_free_growing(u64::MAX)` reaches exactly that, legally, and
            // 259 is far below PTRS_PER_INTERIOR. It is here
            // because the write path has strictly more to lose than the read
            // path: `find_leaf` can answer "absent", whereas
            // `read_child`/`write_child` index `DATA_PAGE_HEADER_SIZE + index *
            // PTR_SIZE` with no bound of their own — slot 1021 would silently
            // overwrite the checksum field and 1022+ would panic on the slice.
            // Fatal rather than operational is doubly right here: by this point
            // the root has already been COW'd and `self.root` reassigned, so
            // there is no clean state left to hand back to a caller.
            if child_idx >= PTRS_PER_INTERIOR {
                return Err(ChiselError::CorruptPage { page_id: current });
            }
            let child = read_child(cache.get(current)?, child_idx);
            // A child below interior `level` is a leaf at level 1 and an interior
            // at deeper levels — the same position type the read paths validate.
            let child_expected = if level == 1 {
                PageType::FreeMap
            } else {
                PageType::FreeMapInterior
            };
            let new_child = if child == 0 {
                // Absent subtree: materialize a fresh node of the expected kind.
                // This is structural allocation — extend-only.
                let id_new = extend(cache)?;
                let buf = cache.get_mut(id_new)?;
                if child_expected == PageType::FreeMap {
                    FreeMap::init_page(buf);
                    page::stamp_checksum(buf);
                } else {
                    init_interior(buf);
                }
                self.session_owned.insert(id_new);
                id_new
            } else {
                // Present subtree: COW it (validating its position type), unless
                // it is already session-owned (cow_node returns it unchanged and
                // queues nothing).
                self.cow_node(cache, child, child_expected, extend)?
            };
            // Rewrite the parent's pointer to the new child and re-stamp. When
            // the child was session-owned and unchanged this write is a no-op on
            // the pointer value but harmless (and the re-stamp is required only
            // if `current` itself changed, which it did at least once on the
            // first touch). `current` is always session-owned here, so this is
            // an in-place edit.
            let buf = cache.get_mut(current)?;
            write_child(buf, child_idx, new_child);
            page::stamp_checksum(buf);
            remaining %= span;
            current = new_child;
        }

        // `current` is now the COW'd (or freshly materialized) leaf. Apply the
        // bit op and re-stamp.
        //
        // Defense-in-depth: this check_type can never fire under correct code —
        // a COW'd leaf was already position-validated in cow_node before copying,
        // and copy_page preserves buf[0]; a freshly materialized leaf has buf[0]
        // set explicitly by FreeMap::init_page. The primary leaf-type guard is in
        // cow_node; this one exists so a future caller that bypasses the COW path
        // cannot silently corrupt the wrong page type.
        let buf = cache.get_mut(current)?;
        check_type(buf, PageType::FreeMap, current)?;
        leaf_op(buf, remaining % LEAF_CAPACITY);
        page::stamp_checksum(buf);
        Ok(())
    }

    /// Return the page to edit in place for `page_id`'s position, COWing it if
    /// (and only if) it is not already owned by this transaction's handle.
    ///
    /// - If `page_id` is `session_owned` (this handle already extended or COW'd
    ///   it this transaction), return it UNCHANGED — no `extend`, no supersede.
    ///   This is what makes a second mutation to the same leaf/spine an in-place
    ///   edit, the property `persist_freemap`'s multi-free loop relies on.
    /// - Otherwise it is a committed page: validate its POSITION type, `extend`
    ///   a fresh copy, record the new id as session-owned, queue the old id on
    ///   `pending_superseded`, and return the new id.
    ///
    /// `expected` is the type the descent level says must live here (interior vs
    /// leaf). Validating against it — rather than self-classifying from buf[0] —
    /// makes the COW spine reject a checksum-valid but POSITION-wrong page (e.g. a
    /// FreeMap leaf reached where an interior is expected): 0x04 and 0x07 are both
    /// valid tree types, so a self-classifying check could not catch that, but the
    /// read paths (find_leaf/scan_node) already reject it. This closes that gap so
    /// mark_free/mark_free_growing — which reach the spine without a prior
    /// position-validating read — fail closed identically.
    fn cow_node(
        &mut self,
        cache: &mut PageCache,
        page_id: u64,
        expected: PageType,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<u64> {
        // Already this-txn-dirty: edit in place. (No type re-validation needed —
        // it was validated when first COW'd/materialized this session.)
        if self.session_owned.contains(&page_id) {
            return Ok(page_id);
        }
        // Validate before copying so a corrupt wrong-type page on the spine
        // fails closed rather than propagating into a fresh COW page.
        check_type(cache.get(page_id)?, expected, page_id)?;
        let new_id = extend(cache)?;
        cache.copy_page(page_id, new_id)?;
        self.session_owned.insert(new_id);
        self.pending_superseded.push(page_id);
        Ok(new_id)
    }

    /// Mark `id` free (set its bit), COWing the spine and materializing any
    /// absent nodes. `id` must already be in range; `mark_free_growing` is the
    /// entry point that guarantees that, and it is the only one the manager
    /// uses.
    ///
    /// Module-private ON PURPOSE (FREEMAP-6, issue #115). This was `pub` on a
    /// `pub(crate)` type, so a new caller could reach it having forgotten to
    /// grow, and `cow_descend` now rejects that as `CorruptPage` — a loud
    /// failure, but still a failure a caller had to hit at runtime. Privacy is
    /// what keeps the mistake from compiling in the first place; the guard is
    /// the backstop for the paths privacy cannot cover (a corrupt stored
    /// depth).
    fn mark_free(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<()> {
        self.cow_descend(cache, id, extend, &mut |buf, bit| {
            FreeMap::mark_free(buf, bit)
        })
    }

    /// Clear `id`'s bit (mark it in-use again), COWing the spine. Used by
    /// `allocate_first` to claim the id it found.
    ///
    /// Precondition: the leaf covering `id` is present. The only caller,
    /// `allocate_first`, satisfies this: it clears an id that `scan_from` just
    /// proved free, so the leaf exists. A future caller clearing an id in an
    /// ABSENT subtree would cause `cow_descend` to materialize a fresh leaf and
    /// queue a superseded-page entry — correct for the bitmap, but wasteful (a
    /// new page is allocated for a leaf that was never written, and
    /// `pending_superseded` grows). Avoid clearing ids in absent subtrees.
    fn clear_bit(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<()> {
        self.cow_descend(cache, id, extend, &mut |buf, bit| {
            FreeMap::clear_bit(buf, bit)
        })
    }

    /// Grow until `id` is in range, then `mark_free` it. The manager-facing entry
    /// point: a page id only ever needs a freemap bit when it is *freed*, so
    /// freeing an id beyond capacity is the sole growth trigger (an
    /// allocated-and-never-freed page needs no leaf — absent subtree = in-use).
    pub fn mark_free_growing(
        &mut self,
        cache: &mut PageCache,
        id: u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<()> {
        // Depth-check FIRST. The `while` condition below evaluates `capacity()`
        // before it evaluates `self.depth < MAX_DEPTH`, and `capacity()` loops
        // `depth` times — so a forged depth of u32::MAX spins ~4.3e9 saturating
        // multiplies (tens of seconds) before the guard inside `mark_free` ever
        // runs. It does eventually fail closed, but "eventually" is not what the
        // other entry points promise, and this is the manager-facing one.
        self.check_depth()?;
        // Grow until capacity covers id. MAX_DEPTH covers all of u64, so the loop
        // terminates; grow() no-ops at MAX_DEPTH and capacity() saturates to
        // u64::MAX, breaking the loop.
        while self.capacity() <= id && self.depth < MAX_DEPTH {
            self.grow(cache, extend)?;
        }
        self.mark_free(cache, id, extend)
    }

    /// Find the lowest free id at/above `*hint`, clear its bit (COW), advance
    /// `*hint` to it, and return `Some(found_id)`. `None` if the tree holds no
    /// free id at/above the hint.
    ///
    /// The scan skips absent subtrees (a zero child = all-in-use) and walks
    /// left-to-right, so it returns the true lowest free id >= hint. Clearing
    /// goes through `clear_bit` -> `cow_descend`, so the claim is a normal COW.
    /// `*hint` is advanced to the found id (not past it): the next call resumes
    /// here, and the just-cleared id will be skipped because its bit is now 0.
    pub fn allocate_first(
        &mut self,
        cache: &mut PageCache,
        hint: &mut u64,
        extend: &mut dyn FnMut(&mut PageCache) -> Result<u64>,
    ) -> Result<Option<u64>> {
        let Some(found) = self.scan_from(cache, *hint)? else {
            return Ok(None);
        };
        self.clear_bit(cache, found, extend)?;
        *hint = found;
        Ok(Some(found))
    }

    /// Read-only left-to-right scan for the lowest free id >= `lo`. Skips absent
    /// subtrees. Returns the global id (not leaf-local). Every page is type-
    /// validated on the way down.
    fn scan_from(&self, cache: &mut PageCache, lo: u64) -> Result<Option<u64>> {
        self.check_depth()?;
        self.scan_node(cache, self.root, self.depth, 0, lo)
    }

    // Recursive scan. `base` is the global id of this subtree's first covered id;
    // `lo` is the global lower bound. At a leaf, scan bits >= max(lo, base) - base
    // via first_free_bit_from. At an interior, walk children left-to-right,
    // skipping those whose covered range ends at/below `lo` or that are absent.
    fn scan_node(
        &self,
        cache: &mut PageCache,
        page_id: u64,
        level: u32,
        base: u64,
        lo: u64,
    ) -> Result<Option<u64>> {
        if level == 0 {
            let buf = cache.get(page_id)?;
            check_type(buf, PageType::FreeMap, page_id)?;
            // Leaf-local lower bound: ids below `base` are out of this leaf;
            // below `lo` are excluded by the hint.
            let local_lo = lo.saturating_sub(base);
            return Ok(FreeMap::first_free_bit_from(buf, local_lo).map(|b| base + b));
        }
        let buf = cache.get(page_id)?;
        check_type(buf, PageType::FreeMapInterior, page_id)?;
        let span = self.child_span(level);
        // Collect children first so the read borrow is released before recursing.
        let children: Vec<(usize, u64)> = (0..PTRS_PER_INTERIOR)
            .map(|i| (i, read_child(buf, i)))
            .filter(|(_, c)| *c != 0)
            .collect();
        for (i, child) in children {
            let child_base = base.saturating_add((i as u64).saturating_mul(span));
            // Skip a child whose entire covered range is below `lo`.
            let child_end = child_base.saturating_add(span);
            if child_end <= lo {
                continue;
            }
            if let Some(found) = self.scan_node(cache, child, level - 1, child_base, lo)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Collect every page id the live tree physically occupies (root + every
    /// present interior + every materialized leaf). Mirrors
    /// `HandleTable::collect_page_ids`. Returns an empty set when the tree is
    /// unmaterialized (`root == PAGE_ID_NONE`).
    ///
    /// Two consumers: the structural-recycle pin-tests (assert the LIVE tree and
    /// the dead-page reuse pool stay disjoint) and the defrag orphan-sweep
    /// (`reclaim_freemap_orphans`), which subtracts this set from the
    /// freemap-typed pages on disk to find pages a crash orphaned. Because the
    /// sweep acts on the result (marks the complement free), the walk
    /// type-validates every page — a leaf must be `FreeMap`, an interior
    /// `FreeMapInterior` — so a checksum-valid wrong-type page reached via a
    /// corrupt child pointer surfaces as `CorruptPage` rather than silently
    /// shrinking the reachable set (which would then reclaim a still-live page).
    pub(crate) fn reachable_pages(
        &self,
        cache: &mut PageCache,
    ) -> Result<rustc_hash::FxHashSet<u64>> {
        self.check_depth()?;
        let mut set = rustc_hash::FxHashSet::default();
        if self.root != crate::page::PAGE_ID_NONE {
            self.collect_reachable(cache, self.root, self.depth, &mut set)?;
        }
        Ok(set)
    }

    fn collect_reachable(
        &self,
        cache: &mut PageCache,
        page: u64,
        level: u32,
        set: &mut rustc_hash::FxHashSet<u64>,
    ) -> Result<()> {
        set.insert(page);
        if level == 0 {
            // Leaf: validate the position type, no children to recurse into.
            check_type(cache.get(page)?, PageType::FreeMap, page)?;
            return Ok(());
        }
        check_type(cache.get(page)?, PageType::FreeMapInterior, page)?;
        // Materialize child pointers before recursing (the recursive call needs
        // &mut PageCache, invalidating the borrow on `page`).
        let children: Vec<u64> = {
            let buf = cache.get(page)?;
            (0..PTRS_PER_INTERIOR)
                .map(|i| read_child(buf, i))
                .filter(|c| *c != 0)
                .collect()
        };
        for child in children {
            self.collect_reachable(cache, child, level - 1, set)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_cache::PageCache;

    // Reserve pages 0/1 so new_page() never returns the zero-child sentinel.
    // Pass a larger max_pages for tests that materialize many pages (deep trees,
    // large proptest sequences) so COW spines do not thrash against a small cap.
    fn make_cache(max_pages: usize) -> PageCache {
        let file = tempfile::NamedTempFile::new().unwrap();
        let io = crate::page_io::PageIo::open(file.path(), false).unwrap();
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

    fn extend(c: &mut PageCache) -> Result<u64> {
        c.new_page()
    }

    #[test]
    fn depth0_is_free_round_trip() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        assert_eq!(t.depth, 0);
        t.mark_free(&mut c, 5, &mut extend).unwrap();
        assert!(t.is_free(&mut c, 5).unwrap());
        assert!(!t.is_free(&mut c, 6).unwrap());
    }

    #[test]
    fn mark_free_materializes_absent_subtree_at_depth1() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        t.grow(&mut c, &mut extend).unwrap();
        assert_eq!(t.depth, 1);
        // Both ids live in child 1 (LEAF_CAPACITY..2*LEAF_CAPACITY), which is
        // absent until the first mark_free materializes its leaf.
        let id_a = LEAF_CAPACITY + 42;
        let id_b = LEAF_CAPACITY + 99;
        t.mark_free(&mut c, id_a, &mut extend).unwrap();
        assert!(t.is_free(&mut c, id_a).unwrap());
        t.mark_free(&mut c, id_b, &mut extend).unwrap();
        assert!(t.is_free(&mut c, id_b).unwrap());
        // The first child's range (child 0) stays absent => in-use.
        assert!(!t.is_free(&mut c, 7).unwrap());
    }

    #[test]
    fn mark_free_structural_alloc_is_extend_only() {
        let mut c = make_cache(256);
        // Spy: count how many times `extend` is invoked. Materializing a leaf in
        // an absent subtree must call it >= 1 (proves structural COW draws from
        // extend, never from the freemap itself). A Cell lets the closure share
        // the counter without holding an exclusive borrow across the reads below.
        let count = std::cell::Cell::new(0usize);
        let mut spy = |cache: &mut PageCache| -> Result<u64> {
            count.set(count.get() + 1);
            cache.new_page()
        };
        let mut t = FreeMapTree::create(&mut c, &mut spy).unwrap();
        t.grow(&mut c, &mut spy).unwrap();
        let before = count.get();
        t.mark_free(&mut c, LEAF_CAPACITY + 7, &mut spy).unwrap();
        assert!(
            count.get() > before,
            "materializing an absent leaf must allocate via extend"
        );
    }

    #[test]
    fn grow_increases_depth_and_preserves_existing_frees() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        t.mark_free(&mut c, 7, &mut extend).unwrap();
        t.grow(&mut c, &mut extend).unwrap();
        assert_eq!(t.depth, 1);
        // The old leaf is now child 0, so id 7 is still free.
        assert!(t.is_free(&mut c, 7).unwrap());
        // A child-1 id whose subtree is absent reads in-use.
        assert!(!t.is_free(&mut c, LEAF_CAPACITY + 3).unwrap());
    }

    #[test]
    fn mark_free_growing_grows_to_reach_high_id() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        let id = LEAF_CAPACITY * 3 + 17;
        t.mark_free_growing(&mut c, id, &mut extend).unwrap();
        assert!(t.depth >= 1, "high id must grow the tree");
        assert!(t.is_free(&mut c, id).unwrap());
    }

    #[test]
    fn allocate_first_returns_lowest_free_and_clears_it() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        t.mark_free_growing(&mut c, 500, &mut extend).unwrap();
        t.mark_free_growing(&mut c, 9, &mut extend).unwrap();
        t.mark_free_growing(&mut c, LEAF_CAPACITY + 4, &mut extend)
            .unwrap();

        let mut hint = 0u64;
        assert_eq!(
            t.allocate_first(&mut c, &mut hint, &mut extend).unwrap(),
            Some(9)
        );
        assert_eq!(
            t.allocate_first(&mut c, &mut hint, &mut extend).unwrap(),
            Some(500)
        );
        assert_eq!(
            t.allocate_first(&mut c, &mut hint, &mut extend).unwrap(),
            Some(LEAF_CAPACITY + 4)
        );
        assert_eq!(
            t.allocate_first(&mut c, &mut hint, &mut extend).unwrap(),
            None
        );

        // Each allocated id now reads not-free.
        assert!(!t.is_free(&mut c, 9).unwrap());
        assert!(!t.is_free(&mut c, 500).unwrap());
        assert!(!t.is_free(&mut c, LEAF_CAPACITY + 4).unwrap());
    }

    // Descent must reject a checksum-valid WRONG-type page reached as an
    // interior child (a corrupt/stale child pointer) as CorruptPage rather than
    // parsing its bytes as a freemap leaf. Mirrors the overflow/data-page
    // hardening: the cache verifies only the XXH3 checksum, not the type tag.
    #[test]
    fn descent_rejects_wrong_type_child_as_corrupt_page() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        t.grow(&mut c, &mut extend).unwrap(); // depth 1: root is an interior
                                              // Materialize child 0's leaf by freeing an id in its range.
        t.mark_free(&mut c, 5, &mut extend).unwrap();
        // Corrupt child 0's leaf into a (checksum-valid) Data page.
        let leaf = read_child(c.get(t.root).unwrap(), 0);
        {
            let buf = c.get_mut(leaf).unwrap();
            buf[0] = PageType::Data as u8;
            page::stamp_checksum(buf);
        }
        let err = match t.is_free(&mut c, 5) {
            Ok(v) => panic!("is_free accepted a wrong-type leaf: {v}"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ChiselError::CorruptPage { .. }),
            "expected CorruptPage, got {err:?}"
        );
    }

    // The COW spine must reject a checksum-valid but POSITION-wrong page exactly
    // like the read paths. Here an INTERIOR child is corrupted into a FreeMap
    // LEAF (0x04) — a perfectly valid TREE type, so cow_node's old self-
    // classifying check (which only asked "is buf[0] one of the two tree types?")
    // would have accepted it and copied a leaf where an interior belongs. Only the
    // positional check (expected = FreeMapInterior at this descent level) catches
    // it. mark_free reaches the spine without a prior position-validating read, so
    // this is the exposed path that needed the hardening.
    #[test]
    fn mark_free_rejects_wrong_position_page_on_cow_spine() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        // Depth 2: root (level 2) -> interior children (level 1's parents) -> leaves.
        t.grow(&mut c, &mut extend).unwrap();
        t.grow(&mut c, &mut extend).unwrap();
        assert_eq!(t.depth, 2);
        // Materialize the spine for an id, creating the level-2 child interior and
        // its leaf. id 5 lives under root child 0 (an INTERIOR), then leaf child 0.
        t.mark_free(&mut c, 5, &mut extend).unwrap();
        let interior_child = read_child(c.get(t.root).unwrap(), 0);
        // Corrupt that interior into a checksum-valid FreeMap LEAF: right checksum,
        // valid tree type byte, WRONG position (a leaf where an interior is
        // expected). FreeMap::init_page stamps a leaf type byte; re-stamp the
        // checksum so the cache accepts it.
        {
            let buf = c.get_mut(interior_child).unwrap();
            FreeMap::init_page(buf);
            page::stamp_checksum(buf);
        }
        // Reach the corrupted interior through a FRESH handle (empty
        // session-owned set) — the production scenario, where a new transaction
        // descends into a COMMITTED, now-corrupt page via from_roots. The COW
        // short-circuit only skips re-validation for pages THIS handle already
        // COW'd this transaction (freshly written, single-writer, so they cannot
        // rot between two touches); a committed page reached fresh is always
        // re-validated. Re-reading on the original `t` (which COW'd
        // `interior_child` itself this session) would correctly treat it as
        // in-place and NOT re-validate, so the from_roots handle is what models
        // the corruption-detection path.
        let mut t2 = FreeMapTree::from_roots(t.root, t.depth);
        let err = match t2.mark_free(&mut c, 5, &mut extend) {
            Ok(()) => panic!("mark_free accepted a position-wrong interior child"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ChiselError::CorruptPage { page_id } if page_id == interior_child),
            "expected CorruptPage {{ page_id: {interior_child} }}, got {err:?}"
        );
    }

    // reachable_pages must return exactly the live tree's pages: the root
    // interior plus every materialized leaf. Forcing depth 1 and freeing ids in
    // two different child ranges materializes two leaves, so the set is
    // {root, leaf_a, leaf_b} — count 3.
    #[test]
    fn reachable_pages_collects_every_node() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        t.grow(&mut c, &mut extend).unwrap(); // depth 1: root is an interior
        assert_eq!(t.depth, 1);
        // id 5 lives in child 0's leaf; LEAF_CAPACITY + 7 in child 1's leaf —
        // two distinct materialized leaves under the one root interior.
        t.mark_free(&mut c, 5, &mut extend).unwrap();
        t.mark_free(&mut c, LEAF_CAPACITY + 7, &mut extend).unwrap();
        let reachable = t.reachable_pages(&mut c).unwrap();
        assert!(reachable.contains(&t.root), "root must be reachable");
        assert_eq!(
            reachable.len(),
            3,
            "1 root-interior + 2 materialized leaves (depth 1)"
        );
        // The superblock page (0) is never part of the tree.
        for id in reachable.iter() {
            assert!(*id != 0, "the superblock page must never be a tree page");
        }
    }

    // Oracle proptest — the correctness backbone. Random (is_free_op, id)
    // sequence over a range that crosses the depth-0 -> depth-1+ boundary
    // (LEAF_CAPACITY*1200 forces depth >= 2). A BTreeSet models the free set:
    //   free  -> mark_free_growing + oracle.insert
    //   alloc -> allocate_first must equal the oracle's min, then remove it.
    // `pending_superseded` is cleared after every op to mirror the manager's
    // commit-time drain (so it never grows unbounded and the tree handle stays
    // the live working state). After the sequence, every remaining oracle id
    // must read is_free.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        #[test]
        fn prop_tree_matches_oracle(
            ops in proptest::collection::vec(
                (proptest::bool::ANY, 0u64..(LEAF_CAPACITY * 1200)),
                1..120usize,
            )
        ) {
            let mut c = make_cache(1_000_000);
            let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
            let mut oracle: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
            let mut hint = 0u64;

            for (is_free_op, id) in ops {
                if is_free_op {
                    t.mark_free_growing(&mut c, id, &mut extend).unwrap();
                    oracle.insert(id);
                    // A free at an id below the current hint must be reachable by
                    // the next alloc, so pull the hint back to cover it. (Mirrors
                    // a manager that lowers its hint when a lower id is freed.)
                    hint = hint.min(id);
                } else {
                    // The hint must never run ahead of the true lowest free id,
                    // else scan_from would skip it — assert the test maintains
                    // that so the oracle genuinely constrains the tree (and a tree
                    // bug cannot hide behind a sloppy hint).
                    if let Some(&min) = oracle.iter().next() {
                        proptest::prop_assert!(
                            hint <= min,
                            "test hint {} ran ahead of oracle min {}", hint, min
                        );
                    }
                    let got = t.allocate_first(&mut c, &mut hint, &mut extend).unwrap();
                    let expected = oracle.iter().next().copied();
                    proptest::prop_assert_eq!(got, expected);
                    if let Some(min) = expected {
                        oracle.remove(&min);
                    }
                }
                t.pending_superseded.clear();
            }

            for id in &oracle {
                proptest::prop_assert!(
                    t.is_free(&mut c, *id).unwrap(),
                    "remaining oracle id {} must read free", id
                );
            }
        }
    }

    // FREEMAP-6 (issue #115). The WRITE path must refuse an id it cannot reach,
    // because the alternative is not "no effect" — it is an effect at the WRONG
    // id. At depth 0 `cow_descend`'s descent loop never runs, so the leaf op
    // lands on `id % LEAF_CAPACITY`: marking `LEAF_CAPACITY + 7` free used to
    // set the free bit for page 7 — a LIVE page the next `allocate_first` hands
    // straight out, i.e. a double allocation. The read path already refuses the
    // id (`find_leaf`'s `id >= capacity()` guard), so the damage was invisible
    // at the id you wrote and lethal at the id you did not.
    #[test]
    fn an_out_of_reach_id_is_rejected_by_the_write_path_not_aliased() {
        let mut c = make_cache(256);
        let mut t = FreeMapTree::create(&mut c, &mut extend).unwrap();
        assert_eq!(t.depth, 0, "the aliasing case is the depth-0 tree");
        let out_of_reach = LEAF_CAPACITY + 7;

        // The read path already refuses the id.
        assert!(
            !t.is_free(&mut c, out_of_reach).unwrap(),
            "precondition: the read path reports an out-of-reach id as in-use"
        );

        // So the write path must refuse it too — never write a bit this tree
        // would refuse to read.
        let err = match t.mark_free(&mut c, out_of_reach, &mut extend) {
            Ok(()) => panic!("the write path accepted an out-of-reach id"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ChiselError::CorruptPage { .. }),
            "expected CorruptPage, got {err:?}"
        );

        // The corruption itself: page 7 is live and must not have been freed by
        // a write aimed at LEAF_CAPACITY + 7.
        assert!(
            !t.is_free(&mut c, 7).unwrap(),
            "aliased write: freeing LEAF_CAPACITY + 7 set the free bit for LIVE \
             page 7, which the next allocate_first would hand out"
        );
    }

    // Defense in depth for the SECURITY-SWEEP-3 hazard. `open_existing` rejects
    // an over-deep `freemap_depth` at the trust boundary; this pins the tree's
    // own entry-point guards, so a path that builds a tree from roots without
    // passing through that gate cannot reach the recursion.
    //
    // The root here is a page id that was never allocated, which is what makes
    // the test meaningful: the ONLY way to get a typed error rather than a
    // read failure is for check_depth to run before the first cache.get. That
    // also keeps the test terminating — an unguarded traversal at this depth is
    // a stack overflow, not a failure the harness could report.
    #[test]
    fn an_over_deep_depth_is_rejected_before_any_page_is_read() {
        let mut c = make_cache(256);
        let phantom_root = 4_242;
        let bad_depth = MAX_DEPTH + 1;

        let assert_rejected = |label: &str, r: Result<()>| {
            match r {
                Err(ChiselError::CorruptPage { page_id }) => assert_eq!(
                    page_id, phantom_root,
                    "{label}: the guard must blame the tree root it refused to descend"
                ),
                other => panic!("{label}: expected CorruptPage, got {other:?}"),
            };
        };

        // Read paths.
        let t = FreeMapTree::from_roots(phantom_root, bad_depth);
        assert_rejected("is_free", t.is_free(&mut c, 7).map(|_| ()));
        assert_rejected("reachable_pages", t.reachable_pages(&mut c).map(|_| ()));

        // Allocation path (scan_from), and the commit-path writer (cow_descend).
        let mut t = FreeMapTree::from_roots(phantom_root, bad_depth);
        let mut hint = 0u64;
        assert_rejected(
            "allocate_first",
            t.allocate_first(&mut c, &mut hint, &mut extend).map(|_| ()),
        );
        assert_rejected("mark_free", t.mark_free(&mut c, 7, &mut extend));
        // The manager-facing entry point, and the one the first version of this
        // guard missed. Its `while` condition evaluates `capacity()` — which
        // loops `depth` times — BEFORE it evaluates `depth < MAX_DEPTH`, so at
        // u32::MAX it spun ~4.3e9 saturating multiplies before the guard inside
        // mark_free fired. It reached the same verdict, but only after tens of
        // seconds; a bounded-but-absurd delay is not what the other entry points
        // promise. `u32::MAX` rather than `MAX_DEPTH + 1` is deliberate: at the
        // small over-bound the missing check is invisible.
        let mut t = FreeMapTree::from_roots(phantom_root, u32::MAX);
        assert_rejected(
            "mark_free_growing",
            t.mark_free_growing(&mut c, 7, &mut extend),
        );

        // A depth AT the bound is legal and must still be accepted — the guard
        // is `>`, not `>=`. It fails on the phantom page instead, which proves
        // it got past check_depth to the read.
        let t = FreeMapTree::from_roots(phantom_root, MAX_DEPTH);
        assert!(
            !matches!(
                t.is_free(&mut c, 7),
                Err(ChiselError::CorruptPage { page_id }) if page_id == phantom_root
            ),
            "MAX_DEPTH itself must not be rejected by the depth guard"
        );
    }
}
