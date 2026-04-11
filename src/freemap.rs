// freemap.rs — Bitmap-based free page tracking (layer 4: page-type-specific logic).
//
// Role in the system: operates on raw [u8; PAGE_SIZE] buffers representing a
// FreeMap page. Each bit represents one page in the database file.
//   1 = free (available for reuse), 0 = in use.
//
// NOTE (v1 simplification, per CLAUDE.md): this module is BUILT BUT NOT WIRED IN.
// `PageCache::new_page()` currently always extends the file rather than consulting
// the freemap, so freed pages are never actually reused. The bitmap logic here is
// correct and ready; the allocator just doesn't call it yet. Do not assume calls
// to mark_free/mark_used occur during normal transactional writes.
//
// On-disk layout of a freemap page:
//   byte 0        : PageType tag (0x04 = FreeMap)
//   bytes 1..16   : reserved (common/data-page header region; zeroed)
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

impl FreeMap {
    /// Maximum number of pages one freemap page can track.
    //
    // Invariant: any page_id passed to is_free/mark_free/mark_used/allocate_*
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

    /// Mark a page as in-use (clear bit to 0).
    //
    // Counterpart to mark_free. Same out-of-range handling and same checksum
    // re-stamping requirement.
    pub fn mark_used(buf: &mut [u8; PAGE_SIZE], page_id: u64) {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx < PAGE_BODY_SIZE {
            buf[BITMAP_OFFSET + byte_idx] &= !(1 << bit_idx);
        }
    }

    /// Allocate the first free page. Clears its bit and returns the page ID.
    //
    // Linear scan; returns the lowest free page_id. `trailing_zeros` finds the
    // lowest-set bit within the first non-zero byte, which matches the
    // bit_position() convention (LSB = lowest page_id within a byte).
    //
    // Allocation combines two effects: (1) locate a free page, (2) atomically
    // flip its bit to "in use". Callers must not separately call mark_used.
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

    /// Allocate a free page near `target`, searching outward with an
    /// expanding radius. Returns None only if the whole bitmap has no
    /// free bits.
    //
    // Locality hint for reducing fragmentation: when COW'ing a page, the
    // allocator can request a replacement near the original so related pages
    // stay clustered on disk. The radius loop is O(n) worst-case but typically
    // short because the target is usually close to a free slot.
    //
    // ISSUES.md C2: the earlier doc comment said "then falls back to
    // allocate_first", but the implementation is a single radius scan
    // that returns None if no free bit exists anywhere in the bitmap —
    // there is no separate fallback call. The two strategies are
    // behaviorally equivalent (both find the only free page if one
    // exists), but the phrasing was misleading. Fixed to describe what
    // actually happens.
    pub fn allocate_near(buf: &mut [u8; PAGE_SIZE], target: u64) -> Option<u64> {
        let target = target as usize;
        let max_page = PAGE_BODY_SIZE * 8;

        // Search outward from target in expanding radius.
        for radius in 0..max_page {
            if target + radius < max_page {
                let page_id = (target + radius) as u64;
                if Self::is_free(buf, page_id) {
                    Self::mark_used(buf, page_id);
                    return Some(page_id);
                }
            }
            // Guard `target >= radius` prevents underflow when searching below
            // target. `radius > 0` avoids re-testing the target itself (already
            // handled by the forward branch at radius == 0).
            if radius > 0 && target >= radius {
                let page_id = (target - radius) as u64;
                if Self::is_free(buf, page_id) {
                    Self::mark_used(buf, page_id);
                    return Some(page_id);
                }
            }
        }
        None
    }

    // Maps a page_id to (byte_index_within_bitmap, bit_index_within_byte).
    // LSB-first within a byte: page_id 0 = bit 0 of byte 0, page_id 7 = bit 7
    // of byte 0, page_id 8 = bit 0 of byte 1. `allocate_first` depends on
    // this ordering via `trailing_zeros`.
    fn bit_position(page_id: u64) -> (usize, usize) {
        let page_id = page_id as usize;
        (page_id / 8, page_id % 8)
    }
}
