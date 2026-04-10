// freemap.rs — Bitmap-based free page tracking.
// Each bit represents one page in the database file. 1 = free, 0 = in use.
// The bitmap occupies the body of a freemap page (PAGE_BODY_SIZE bytes),
// covering up to PAGE_BODY_SIZE * 8 pages (~512MB at 8KB page size).
//
// The freemap page itself is COW'd. Callers are responsible for COW mechanics;
// this module only provides bitmap operations on a raw page buffer.

use crate::page::{PageType, DATA_PAGE_HEADER_SIZE, PAGE_BODY_SIZE, PAGE_SIZE};

/// Offset where the bitmap data starts within the page.
const BITMAP_OFFSET: usize = DATA_PAGE_HEADER_SIZE;

pub struct FreeMap;

impl FreeMap {
    /// Maximum number of pages one freemap page can track.
    pub fn capacity() -> usize {
        PAGE_BODY_SIZE * 8
    }

    /// Initialize a page buffer as an empty freemap (all bits 0 = all in use).
    pub fn init_page(buf: &mut [u8; PAGE_SIZE]) {
        buf.fill(0);
        buf[0] = PageType::FreeMap as u8;
    }

    /// Check if a page is marked free in the bitmap.
    pub fn is_free(buf: &[u8; PAGE_SIZE], page_id: u64) -> bool {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx >= PAGE_BODY_SIZE {
            return false;
        }
        (buf[BITMAP_OFFSET + byte_idx] >> bit_idx) & 1 == 1
    }

    /// Mark a page as free (set bit to 1).
    pub fn mark_free(buf: &mut [u8; PAGE_SIZE], page_id: u64) {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx < PAGE_BODY_SIZE {
            buf[BITMAP_OFFSET + byte_idx] |= 1 << bit_idx;
        }
    }

    /// Mark a page as in-use (clear bit to 0).
    pub fn mark_used(buf: &mut [u8; PAGE_SIZE], page_id: u64) {
        let (byte_idx, bit_idx) = Self::bit_position(page_id);
        if byte_idx < PAGE_BODY_SIZE {
            buf[BITMAP_OFFSET + byte_idx] &= !(1 << bit_idx);
        }
    }

    /// Allocate the first free page. Clears its bit and returns the page ID.
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

    /// Allocate a free page near `target`. Searches outward from target,
    /// then falls back to allocate_first.
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

    fn bit_position(page_id: u64) -> (usize, usize) {
        let page_id = page_id as usize;
        (page_id / 8, page_id % 8)
    }
}
