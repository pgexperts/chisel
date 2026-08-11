// freemap.rs — Bitmap-based free page tracking (layer 4: page-type-specific logic).
//
// Role in the system: operates on raw [u8; PAGE_SIZE] buffers representing a
// FreeMap page. Each bit represents one page in the database file.
//   1 = free (available for reuse), 0 = in use.
//
// Nothing in this module decides WHICH page id is handed out. That decision
// belongs to the freemap tree (freemap_tree.rs), which descends its radix spine
// to a leaf and then calls in here only to inspect or flip one already-named
// bit. The production allocation path (ISSUES.md R2) runs:
//
//   `cow_alloc` (transaction/freemap.rs)
//     -> `FreeMapTree::allocate_first`
//        -> `scan_from` / `scan_node` -> `FreeMap::first_free_bit_from`  (FIND)
//        -> `FreeMapTree::clear_bit` -> `cow_descend` -> `FreeMap::clear_bit`
//                                                                       (CLAIM)
//
// Data-page allocation AND the handle-table / membership-index COW paths all
// enter through `cow_alloc`, so they prefer a page freed by a prior committed
// transaction and extend the file only when the tree has no free id to hand
// out. Reclamation happens during commit via `FreemapRecycle::persist`, which
// marks `txn_freed_pages` free in a COW of the committed freemap tree BEFORE
// the new snapshot is durable (I18 ordering) so allocation cannot reuse a page
// the last-durable superblock still references. The handle table and membership
// index both feed their COW-superseded pages into `txn_freed_pages` and
// allocate through `cow_alloc`, so they reach a bounded steady-state page count
// rather than growing one page per mutation. Overflow pages still extend
// directly (call `PageCache::new_page`), but their frees do feed the freemap on
// commit, so a later data/handle-table allocation can reclaim them.
//
// On-disk layout of a freemap page:
//   byte 0        : PageType tag (0x04 = FreeMap)
//   byte 1        : page format version (I31; PAGE_FORMAT_VERSION_CURRENT today)
//   bytes 2..8    : reserved / zeroed (type-specific region, unused)
//   bytes 8..16   : RESERVED for future common-header fields (I31 common
//                   reserved region, 64 bits, universally zero across all
//                   non-superblock page types)
//   bytes 16..8184: bitmap body (PAGE_BODY_SIZE bytes = 8168 bytes = 65344 bits)
//   bytes 8184..8192: XXH3 checksum (stamped by caller after mutation)
//
// One freemap page therefore tracks up to PAGE_BODY_SIZE * 8 pages
// (~512 MB of data at 8 KB pages). The multi-page freemap (freemap_tree.rs)
// composes many of these leaf pages into a COW radix tree.
//
// COW discipline: this module is pure bitmap manipulation on a buffer the caller
// owns. Callers are responsible for obtaining a writable COW copy (via
// PageCache::new_page / get_mut) before mutating, and for re-stamping the
// checksum after mutation. Nothing here touches the PageCache.
//
// Endianness: the bitmap is a flat byte array; bit_position() uses the
// convention (byte_idx = page_id / 8, bit_idx = page_id % 8) with LSB = lowest
// page_id within a byte. `first_free_bit_from` relies on this via
// `trailing_zeros`.

use crate::page::{PageType, DATA_PAGE_HEADER_SIZE, PAGE_BODY_SIZE, PAGE_SIZE};

/// Offset where the bitmap data starts within the page.
//
// Reusing DATA_PAGE_HEADER_SIZE here is a deliberate layout choice: freemap
// pages share the same 16-byte reserved header prefix as data pages so that
// page-type dispatch and header inspection stay uniform across page kinds.
// If DATA_PAGE_HEADER_SIZE ever changes, the on-disk freemap format changes
// with it — this is an intentional coupling, not an accident.
const BITMAP_OFFSET: usize = DATA_PAGE_HEADER_SIZE;

pub struct FreeMap;

// Every item below has a live PRODUCTION caller, which is why no blanket
// `#[allow(dead_code)]` sits on this impl any more. The allow used to cover the
// whole block, and what it hid was two genuinely dead functions —
// `allocate_first` and `capacity`, deleted for issue #115 — sitting under a
// note that named them as the production allocator path. Without the allow, the
// compiler flags the next item that loses its last caller instead.
//
//   * `first_free_bit_from` + `clear_bit` <- `FreeMapTree::allocate_first`
//     (find the lowest free id in a leaf, then claim exactly that bit).
//   * `mark_free` <- `FreeMapTree::mark_free`, reached from commit's
//     `FreemapRecycle::persist` and from the defrag orphan sweep.
//   * `is_free` <- `FreeMapTree::is_free` <- `FreemapRecycle::reclaim_orphans`.
//     PRODUCTION, not test-only: the orphan sweep calls it to confirm a dead
//     freemap-typed page is genuinely free before reclaiming it, so weakening
//     the predicate would let the sweep double-reclaim a live page. (The note
//     that stood here had this backwards, and cited `src/transaction.rs` — a
//     file that no longer exists.)
//   * `init_page` <- `FreeMapTree::create`, `cow_descend`'s absent-leaf
//     materialization, and `page`'s page-type construction.
//
// Bounds: every accessor here is TOTAL. `bit_position` maps any u64 to a
// (byte, bit) pair and each accessor range-checks `byte_idx < PAGE_BODY_SIZE`,
// so an id past this leaf's `PAGE_BODY_SIZE * 8` reach is ignored by the
// mutators and reads as "not free". Treat that as a crash guard, NOT a contract
// to lean on: silently dropping a write is tolerable only because such an id
// never belonged to this leaf in the first place. Choosing the leaf is
// `freemap_tree.rs`'s job, and its `cow_descend` rejects an out-of-reach id
// outright (FREEMAP-6, issue #115) rather than letting it through to a leaf
// where `id % LEAF_CAPACITY` would alias onto some other page's bit.
impl FreeMap {
    /// Initialize a page buffer as an empty freemap (all bits 0 = all in use).
    //
    // Starting state is "nothing is free". Callers must explicitly `mark_free`
    // pages as they are released. This avoids the bootstrap hazard of an empty
    // freemap accidentally claiming live pages as reusable.
    //
    // Mutates `buf` in place. Does NOT stamp the checksum — caller must call
    // page::stamp_checksum before writing the page back through the cache.
    pub fn init_page(buf: &mut [u8; PAGE_SIZE]) {
        buf.fill(0);
        buf[0] = PageType::FreeMap as u8;
        // I31: per-page version byte at position 1 (same offset as
        // data_page and overflow). Written explicitly even though
        // buf.fill(0) already zeroed it.
        buf[1] = crate::page::current_version(crate::page::PageType::FreeMap);
    }

    /// Check if a page is marked free in the bitmap.
    //
    // Returns false for out-of-range page_ids. This is safe because an
    // out-of-range page cannot have been `mark_free`'d through this API.
    pub fn is_free(buf: &[u8; PAGE_SIZE], page_id: u64) -> bool {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx >= PAGE_BODY_SIZE {
            return false;
        }
        (buf[BITMAP_OFFSET + byte_idx] >> bit_idx) & 1 == 1
    }

    /// Mark a page as free (set bit to 1).
    //
    // Silently ignores out-of-range page_ids rather than panicking; the caller
    // (allocator) is expected to have already validated bounds. Mutates in
    // place; caller must re-stamp the checksum.
    pub fn mark_free(buf: &mut [u8; PAGE_SIZE], page_id: u64) {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx < PAGE_BODY_SIZE {
            buf[BITMAP_OFFSET + byte_idx] |= 1 << bit_idx;
        }
    }

    /// Return the lowest free page id that is `>= lo`, or `None` if every free
    /// bit in this leaf is below `lo`.
    ///
    /// The non-mutating leaf finder the freemap tree's scan composes with COW:
    /// `scan_node` calls this with a leaf-local lower bound derived from the
    /// caller's hint, so the scan resumes mid-page without rescanning an already-
    /// exhausted prefix. Pass `lo = 0` to find the absolute lowest free bit.
    ///
    /// The starting byte is masked so that bits below `lo` within that byte are
    /// ignored; subsequent bytes scan whole. `trailing_zeros` finds the
    /// lowest-set bit within the first non-zero byte, which is exactly
    /// `bit_position`'s LSB-first convention.
    pub fn first_free_bit_from(buf: &[u8; PAGE_SIZE], lo: u64) -> Option<u64> {
        let start_byte = (lo / 8) as usize;
        if start_byte >= PAGE_BODY_SIZE {
            return None;
        }
        let start_bit = (lo % 8) as u32;
        for byte_idx in start_byte..PAGE_BODY_SIZE {
            let mut byte = buf[BITMAP_OFFSET + byte_idx];
            // In the first byte, clear the bits below `lo` so a free id we've
            // already passed cannot be re-reported.
            if byte_idx == start_byte {
                byte &= !((1u8 << start_bit).wrapping_sub(1));
            }
            if byte != 0 {
                return Some((byte_idx * 8 + byte.trailing_zeros() as usize) as u64);
            }
        }
        None
    }

    /// Clear one specific bit (mark the page as in-use).
    ///
    /// The inverse of `mark_free` for a single id. `FreeMapTree::allocate_first`
    /// uses this to CLAIM the id it found: by that point the tree has already
    /// decided *which* id to hand out (it descended the spine, then
    /// `first_free_bit_from` picked the bit), so the leaf only needs its one
    /// named bit cleared. Silently ignores out-of-range ids and does not
    /// re-stamp the checksum, matching `mark_free`'s contract.
    pub fn clear_bit(buf: &mut [u8; PAGE_SIZE], page_id: u64) {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx < PAGE_BODY_SIZE {
            buf[BITMAP_OFFSET + byte_idx] &= !(1 << bit_idx);
        }
    }

    // Maps a page_id to (byte_index_within_bitmap, bit_index_within_byte).
    // LSB-first within a byte: page_id 0 = bit 0 of byte 0, page_id 7 = bit 7
    // of byte 0, page_id 8 = bit 0 of byte 1. `first_free_bit_from` depends on
    // this ordering via `trailing_zeros`.
    //
    // The returned byte index is relative to the bitmap body; every accessor
    // re-adds BITMAP_OFFSET. That offset is what keeps even page_id 0 from
    // aliasing the type tag / version header (bytes 0..16), so no in-range id
    // can corrupt the header by setting or clearing a bit.
    fn bit_position(page_id: u64) -> (usize, usize) {
        let page_id = page_id as usize;
        (page_id / 8, page_id % 8)
    }
}

#[cfg(test)]
mod tests {
    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──
    //
    // Freemap had no prior in-module test mod; this is a fresh one. All
    // tests are pure bitmap manipulation — no PageCache, no I/O.
    use super::*;

    #[test]
    fn first_free_bit_from_returns_lowest_at_or_above_lo() {
        let mut buf = [0u8; PAGE_SIZE];
        FreeMap::init_page(&mut buf);
        FreeMap::mark_free(&mut buf, 12);
        FreeMap::mark_free(&mut buf, 40);
        FreeMap::mark_free(&mut buf, 300);
        // lo below the lowest free bit: returns the lowest free id.
        assert_eq!(FreeMap::first_free_bit_from(&buf, 0), Some(12));
        // lo exactly on a free bit: returns it.
        assert_eq!(FreeMap::first_free_bit_from(&buf, 12), Some(12));
        // lo just past a free bit: skips it for the next one.
        assert_eq!(FreeMap::first_free_bit_from(&buf, 13), Some(40));
        // lo within the same byte as a free bit but above it.
        assert_eq!(FreeMap::first_free_bit_from(&buf, 41), Some(300));
    }

    #[test]
    fn first_free_bit_from_is_none_when_all_below_lo() {
        let mut buf = [0u8; PAGE_SIZE];
        FreeMap::init_page(&mut buf);
        FreeMap::mark_free(&mut buf, 5);
        FreeMap::mark_free(&mut buf, 100);
        // Every free bit is below `lo` -> None.
        assert_eq!(FreeMap::first_free_bit_from(&buf, 101), None);
        // `lo` past capacity is also None (and does not panic).
        assert_eq!(
            FreeMap::first_free_bit_from(&buf, PAGE_BODY_SIZE as u64 * 8),
            None
        );
    }

    #[test]
    fn clear_bit_clears_named_id_and_leaves_others() {
        let mut buf = [0u8; PAGE_SIZE];
        FreeMap::init_page(&mut buf);
        FreeMap::mark_free(&mut buf, 7);
        FreeMap::mark_free(&mut buf, 8);
        FreeMap::mark_free(&mut buf, 9);
        FreeMap::clear_bit(&mut buf, 8);
        assert!(FreeMap::is_free(&buf, 7), "7 must remain free");
        assert!(!FreeMap::is_free(&buf, 8), "8 must be cleared");
        assert!(FreeMap::is_free(&buf, 9), "9 must remain free");
    }

    // I71 (ISSUES.md, 2026-05-22): property test — for any page_id
    // within the freemap's capacity, mark_free followed by is_free
    // returns true. Cheap shrinking via proptest catches off-by-one
    // errors at byte/bit boundaries (0, 7, 8, 9, capacity-1) without
    // hand-writing each.
    proptest::proptest! {
        #[test]
        fn prop_mark_free_then_is_free(page_id in 0u64..(PAGE_BODY_SIZE as u64 * 8)) {
            let mut buf = [0u8; PAGE_SIZE];
            FreeMap::init_page(&mut buf);
            // Sanity: an unmarked id is in-use.
            assert!(!FreeMap::is_free(&buf, page_id));
            FreeMap::mark_free(&mut buf, page_id);
            assert!(FreeMap::is_free(&buf, page_id),
                "page_id {} should be free after mark_free", page_id);
        }
    }
}
