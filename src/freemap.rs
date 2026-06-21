// freemap.rs — Bitmap-based free page tracking (layer 4: page-type-specific logic).
//
// Role in the system: operates on raw [u8; PAGE_SIZE] buffers representing a
// FreeMap page. Each bit represents one page in the database file.
//   1 = free (available for reuse), 0 = in use.
//
// The freemap is wired into page allocation (ISSUES.md R2) via the shared
// `cow_alloc` helper in transaction.rs: data-page allocation AND the
// handle-table / membership-index COW paths prefer `FreeMap::allocate_first`
// (reusing a page freed by a prior committed transaction) and fall back to
// extending the file only when the freemap has no free id to hand out.
// Reclamation happens during commit via `persist_freemap`, which merges
// `txn_freed_pages` into `current_freemap` BEFORE writing the new freemap
// snapshot (I18 ordering) so allocation cannot reuse a page the last-durable
// superblock still references. The handle table and membership index both feed
// their COW-superseded pages into `txn_freed_pages` and allocate through
// `cow_alloc`, so they reach a bounded steady-state page count rather than
// growing one page per mutation. Overflow pages still extend directly (call
// `PageCache::new_page`), but their frees do feed the freemap on commit, so a
// later data/handle-table allocation can reclaim them.
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
// (~512 MB of data at 8 KB pages). A multi-page freemap is not yet implemented.
//
// COW discipline: this module is pure bitmap manipulation on a buffer the caller
// owns. Callers are responsible for obtaining a writable COW copy (via
// PageCache::new_page / get_mut) before mutating, and for re-stamping the
// checksum after mutation. Nothing here touches the PageCache.
//
// Endianness: the bitmap is a flat byte array; bit_position() uses the
// convention (byte_idx = page_id / 8, bit_idx = page_id % 8) with LSB = lowest
// page_id within a byte. `allocate_first` relies on this via `trailing_zeros`.

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

// I35 reshape note: capacity and is_free are reached only from src-tests
// today (capacity from src/freemap.rs's own tests; is_free from
// src/transaction.rs's I27/I28 regression tests). The production allocator
// path uses `allocate_first` + `mark_free`. They stay behind
// `#[allow(dead_code)]` rather than being deleted: is_free is the natural
// predicate any future health-check tool would call, and capacity is its
// companion bound.
#[allow(dead_code)]
impl FreeMap {
    /// Maximum number of pages one freemap page can track.
    //
    // Invariant: any page_id passed to is_free/mark_free/allocate_first
    // must be < capacity(). Out-of-range IDs are silently ignored by the
    // mutators and treated as "not free" by is_free — this is defensive, not
    // a contract to rely on.
    pub fn capacity() -> usize {
        PAGE_BODY_SIZE * 8
    }

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

    /// Allocate the first free page. Clears its bit and returns the page ID.
    //
    // Linear scan; returns the lowest free page_id. `trailing_zeros` finds the
    // lowest-set bit within the first non-zero byte, which matches the
    // bit_position() convention (LSB = lowest page_id within a byte).
    //
    // Allocation combines two effects: (1) locate a free page, (2) flip its
    // bit to "in use" in the same pass — callers get an already-claimed id.
    pub fn allocate_first(buf: &mut [u8; PAGE_SIZE]) -> Option<u64> {
        for byte_idx in 0..PAGE_BODY_SIZE {
            let byte = buf[BITMAP_OFFSET + byte_idx];
            if byte != 0 {
                let bit_idx = byte.trailing_zeros() as usize;
                let page_id = (byte_idx * 8 + bit_idx) as u64;
                buf[BITMAP_OFFSET + byte_idx] &= !(1 << bit_idx);
                return Some(page_id);
            }
        }
        None
    }

    // Maps a page_id to (byte_index_within_bitmap, bit_index_within_byte).
    // LSB-first within a byte: page_id 0 = bit 0 of byte 0, page_id 7 = bit 7
    // of byte 0, page_id 8 = bit 0 of byte 1. `allocate_first` depends on
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
    fn test_freemap_allocate_first_free() {
        let mut buf = [0u8; PAGE_SIZE];
        FreeMap::init_page(&mut buf);
        FreeMap::mark_free(&mut buf, 50);
        FreeMap::mark_free(&mut buf, 200);
        let alloc = FreeMap::allocate_first(&mut buf);
        assert_eq!(alloc, Some(50));
        let alloc2 = FreeMap::allocate_first(&mut buf);
        assert_eq!(alloc2, Some(200));
        let alloc3 = FreeMap::allocate_first(&mut buf);
        assert_eq!(alloc3, None);
    }

    #[test]
    fn test_freemap_capacity() {
        assert_eq!(FreeMap::capacity(), PAGE_BODY_SIZE * 8);
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
