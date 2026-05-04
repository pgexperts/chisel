// overflow.rs — Overflow page chains for values too large for a slotted page
// (layer 4: page-type-specific logic).
//
// Role in the system: when a value exceeds PAGE_BODY_SIZE it cannot be stored
// in a single DataPage slot. Instead it is split into a singly-linked chain of
// Overflow pages. The handle table stores the first page's id; readers follow
// `next_page` pointers until they hit a terminator. The chain is owned
// exclusively by one handle — overflow pages are NOT shared between values.
//
// Physical layout of an overflow page:
//   byte  0        : PageType tag (0x03 = Overflow)
//   byte  1        : page format version (I31;
//                    page::PAGE_FORMAT_VERSION_CURRENT today)
//   bytes 2..8     : reserved / zeroed for type-specific use.
//   bytes 8..16    : RESERVED for future common-header fields (I31; 64
//                    bits of headroom shared with data_page / freemap /
//                    handle_table). Universally zero today.
//
//   The layout is padded out to DATA_PAGE_HEADER_SIZE (= 16, from page.rs)
//   so data pages and overflow pages share the same 16-byte header
//   footprint. This is NOT the same as COMMON_HEADER_SIZE (= 12); the
//   overflow and data pages share the bigger number, the common header
//   is only the shared prefix.
//   bytes 16..24   : total_length (u64 LE) — full value size, REPEATED on every
//                    page in the chain so any page can answer "how big is this
//                    value?" without walking to the end
//   bytes 24..32   : next_page (u64 LE) — page_id of the next overflow page,
//                    or 0 to mark end-of-chain (0 is safe as a terminator
//                    because page 0 is always the superblock, never an
//                    overflow page)
//   bytes 32..8184 : payload (OVERFLOW_PAYLOAD = 8152 bytes)
//   bytes 8184..8192: XXH3 checksum
//
// Invariants:
//   * total_length is identical on every page in the chain.
//   * The last page has next_page == 0. All other pages have next_page != 0.
//   * sum of payload bytes across the chain == total_length. The final page
//     may carry fewer than OVERFLOW_PAYLOAD bytes of actual data; the trailing
//     bytes are undefined but included in the checksum.
//   * A chain is reachable from exactly one handle; delete() frees all pages.
//
// Endianness: little-endian for all multi-byte integers.
//
// COW / durability: write() allocates fresh pages via PageCache::new_page(),
// so under shadow paging the entire chain is new and never modifies existing
// on-disk pages. delete() just returns the list of page ids to free; it does
// not touch the freemap (that's the caller's responsibility, and see CLAUDE.md
// for the v1 note that the freemap is not yet wired into the allocator).

use crate::error::{ChiselError, Result};
use crate::page::{self, PageType, CHECKSUM_OFFSET};
use crate::page_cache::PageCache;

// Overflow page body layout (after common 16-byte header, before 8-byte checksum):
// bytes 16..24: total_length (u64) — full value size (repeated on every page)
// bytes 24..32: next_page (u64) — next overflow page, or 0 if last
// bytes 32..CHECKSUM_OFFSET: payload
const OVERFLOW_HEADER_END: usize = 32;
const OVERFLOW_PAYLOAD: usize = CHECKSUM_OFFSET - OVERFLOW_HEADER_END; // 8152

pub struct Overflow;

impl Overflow {
    /// Write a value as a chain of overflow pages. Returns the page ID of the first page.
    //
    // Two-pass construction: first allocate all page ids so each page can
    // know its successor's id before being written, then fill and stamp each
    // page. This avoids a second pass to patch next_page pointers.
    //
    // Each page is stamped individually via page::stamp_checksum because
    // new_page() returns an uninitialized buffer and overflow has no generic
    // "rebuild page" path that would stamp for us.
    //
    // ISSUES.md I13: rejects zero-length values with a clear panic. Without
    // this guard, div_ceil(0, OVERFLOW_PAYLOAD) produces num_pages == 0,
    // page_ids stays empty, and the final `Ok(page_ids[0])` crashes with
    // an index-out-of-bounds panic. Empty values must fit in a data page
    // slot and never reach this path — the assert turns a latent cryptic
    // crash into a clear caller-bug diagnostic. Changing this to a soft
    // error would require a new ChiselError variant for something that
    // is structurally a programming error, not a recoverable condition.
    pub fn write(cache: &mut PageCache, value: &[u8]) -> Result<u64> {
        assert!(
            !value.is_empty(),
            "Overflow::write requires a non-empty value; empty values must use a data page slot"
        );
        let total_length = value.len() as u64;
        let num_pages = value.len().div_ceil(OVERFLOW_PAYLOAD);

        let mut page_ids = Vec::with_capacity(num_pages);
        for _ in 0..num_pages {
            page_ids.push(cache.new_page()?);
        }

        for (i, &page_id) in page_ids.iter().enumerate() {
            let start = i * OVERFLOW_PAYLOAD;
            let end = std::cmp::min(start + OVERFLOW_PAYLOAD, value.len());
            let chunk = &value[start..end];

            // 0 is the end-of-chain sentinel; page 0 is the superblock so it
            // can never appear as a legitimate next_page value.
            let next_page = if i + 1 < page_ids.len() {
                page_ids[i + 1]
            } else {
                0
            };

            let buf = cache.get_mut(page_id)?;
            buf.fill(0);
            buf[0] = PageType::Overflow as u8;
            // I31: per-page version at byte 1. Explicit for the same
            // reason as data_page::init_page — intent visible, future
            // CURRENT bumps flow through here automatically.
            buf[1] = page::PAGE_FORMAT_VERSION_CURRENT;
            buf[16..24].copy_from_slice(&total_length.to_le_bytes());
            buf[24..32].copy_from_slice(&next_page.to_le_bytes());
            buf[OVERFLOW_HEADER_END..OVERFLOW_HEADER_END + chunk.len()].copy_from_slice(chunk);
            page::stamp_checksum(buf);
        }

        Ok(page_ids[0])
    }

    /// Read a complete value from an overflow chain.
    //
    // Reads total_length from the first page (any page would work — it's
    // replicated — but the first is already in hand) and pre-allocates a Vec
    // of exactly that size. Then walks next_page links, copying up to
    // OVERFLOW_PAYLOAD bytes per page or the remaining tail on the final
    // page.
    //
    // ISSUES.md I14 hardening:
    //   * Cycle detection: `total_length` bounds the maximum number of
    //     pages a well-formed chain can have (= div_ceil(total_length,
    //     OVERFLOW_PAYLOAD)). If the walk visits more pages than that,
    //     the chain has a cycle or a malformed next_page link, and we
    //     return `CorruptPage` instead of looping forever.
    //   * Bounded remaining: `result.len()` is compared to
    //     `total_length` via `saturating_sub`, so a chain whose
    //     concatenated payload would exceed total_length can't cause an
    //     arithmetic underflow. Reaching `remaining == 0` while the
    //     chain still has more pages is treated as corruption.
    //   * Short-chain detection: if the loop terminates before the Vec
    //     has received `total_length` bytes, the chain is shorter than
    //     its header advertises — also corruption.
    pub fn read(cache: &mut PageCache, first_page: u64) -> Result<Vec<u8>> {
        let total_length = {
            let buf = cache.get(first_page)?;
            u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize
        };
        // An empty value should never be stored as an overflow chain
        // (see I13 in write()); bail out rather than walking a
        // potentially-malformed pointer chain from a zero-size claim.
        if total_length == 0 {
            return Err(ChiselError::CorruptPage {
                page_id: first_page,
            });
        }
        let max_pages = total_length.div_ceil(OVERFLOW_PAYLOAD);
        let mut result = Vec::with_capacity(total_length);

        let mut current_page = first_page;
        let mut pages_visited = 0usize;
        loop {
            if pages_visited >= max_pages {
                // Walked past the chain's advertised length — cycle or
                // malformed next_page link.
                return Err(ChiselError::CorruptPage {
                    page_id: first_page,
                });
            }
            pages_visited += 1;

            let buf = cache.get(current_page)?;
            let next_page = u64::from_le_bytes(buf[24..32].try_into().unwrap());
            let remaining = total_length.saturating_sub(result.len());
            if remaining == 0 {
                // We've already copied total_length bytes but the chain
                // is not done. Structurally inconsistent.
                return Err(ChiselError::CorruptPage {
                    page_id: current_page,
                });
            }
            let chunk_len = std::cmp::min(remaining, OVERFLOW_PAYLOAD);
            result.extend_from_slice(&buf[OVERFLOW_HEADER_END..OVERFLOW_HEADER_END + chunk_len]);

            if next_page == 0 {
                break;
            }
            current_page = next_page;
        }

        if result.len() != total_length {
            // Chain ended before delivering all advertised bytes.
            return Err(ChiselError::CorruptPage {
                page_id: first_page,
            });
        }

        Ok(result)
    }

    /// Delete an overflow chain. Returns the list of page IDs freed.
    //
    // Walks the chain collecting page ids without touching any data. The
    // caller (transaction layer) is responsible for actually releasing the
    // pages — this function deliberately has no side effect on page state so
    // it's safe to call speculatively and discard the result.
    //
    // ISSUES.md I14: cycle detection. The first page's `total_length`
    // determines how many pages a well-formed chain can have, giving
    // us a bound for the walk. If the walk exceeds that bound, the
    // chain has a cycle or a malformed next_page link, and we return
    // `CorruptPage` rather than accumulating page ids forever.
    pub fn delete(cache: &mut PageCache, first_page: u64) -> Result<Vec<u64>> {
        let total_length = {
            let buf = cache.get(first_page)?;
            u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize
        };
        if total_length == 0 {
            return Err(ChiselError::CorruptPage {
                page_id: first_page,
            });
        }
        let max_pages = total_length.div_ceil(OVERFLOW_PAYLOAD);

        let mut freed = Vec::new();
        let mut current_page = first_page;
        loop {
            if freed.len() >= max_pages {
                return Err(ChiselError::CorruptPage {
                    page_id: first_page,
                });
            }
            let buf = cache.get(current_page)?;
            let next_page = u64::from_le_bytes(buf[24..32].try_into().unwrap());
            freed.push(current_page);
            if next_page == 0 {
                break;
            }
            current_page = next_page;
        }
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    fn make_cache() -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut cache = PageCache::new(
            io,
            64 * crate::page::PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        // Reserve page ids 0 and 1 the way a real database does
        // (they're superblock slots). This matters for cycle tests:
        // 0 is the end-of-chain sentinel, so if an overflow page
        // ever gets id 0, "next_page = 0" is ambiguous between
        // "end of chain" and "cycle back to first". Starting at 2
        // eliminates that ambiguity.
        cache.set_next_page_id(2);
        cache
    }

    // Write a 3-page overflow chain by picking a value length that
    // occupies exactly three payload slices. Handy for corruption
    // tests below.
    fn write_three_page_chain(cache: &mut PageCache) -> (u64, Vec<u8>) {
        let value_len = OVERFLOW_PAYLOAD * 2 + 100;
        let value: Vec<u8> = (0..value_len).map(|i| (i % 251) as u8).collect();
        let first = Overflow::write(cache, &value).unwrap();
        (first, value)
    }

    // Patch the `next_page` pointer on an overflow page to a new
    // target, restamping the checksum so subsequent reads don't fail
    // at the validation step.
    fn patch_next_page(cache: &mut PageCache, page_id: u64, new_next: u64) {
        let buf = cache.get_mut(page_id).unwrap();
        buf[24..32].copy_from_slice(&new_next.to_le_bytes());
        page::stamp_checksum(buf);
    }

    // Read the `next_page` pointer of a page so tests can discover
    // the chain ids without assuming any particular allocation order.
    fn read_next_page(cache: &mut PageCache, page_id: u64) -> u64 {
        let buf = cache.get(page_id).unwrap();
        u64::from_le_bytes(buf[24..32].try_into().unwrap())
    }

    // ISSUES.md I13: passing an empty value to Overflow::write is a
    // caller bug (empty values fit in a data page slot), and the old
    // implementation crashed with an index-out-of-bounds panic at
    // `page_ids[0]`. The guard turns that into a clear assertion.
    #[test]
    #[should_panic(expected = "non-empty value")]
    fn write_empty_value_panics() {
        let mut cache = make_cache();
        let _ = Overflow::write(&mut cache, &[]);
    }

    // I14: a cycle in the chain (last page's next_page pointing back
    // at an earlier page instead of 0) must be detected by read() and
    // reported as CorruptPage — not loop forever.
    #[test]
    fn read_detects_cycle_in_chain() {
        let mut cache = make_cache();
        let (first, _) = write_three_page_chain(&mut cache);
        let second = read_next_page(&mut cache, first);
        let third = read_next_page(&mut cache, second);
        // Create a cycle: last page now points at the second page
        // instead of terminating.
        patch_next_page(&mut cache, third, second);

        match Overflow::read(&mut cache, first) {
            Err(ChiselError::CorruptPage { page_id: _ }) => {}
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }

    // I14: the same cycle must be detected by delete(), which
    // otherwise would accumulate page ids indefinitely.
    #[test]
    fn delete_detects_cycle_in_chain() {
        let mut cache = make_cache();
        let (first, _) = write_three_page_chain(&mut cache);
        let second = read_next_page(&mut cache, first);
        let third = read_next_page(&mut cache, second);
        patch_next_page(&mut cache, third, first);

        match Overflow::delete(&mut cache, first) {
            Err(ChiselError::CorruptPage { page_id: _ }) => {}
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }

    // I14: a chain whose header claims a large total_length but
    // whose physical chain is shorter must be detected. We achieve
    // this by truncating a valid chain (patching an earlier page's
    // next_page to 0) while leaving total_length alone.
    #[test]
    fn read_detects_short_chain() {
        let mut cache = make_cache();
        let (first, _) = write_three_page_chain(&mut cache);
        // Truncate after the first page: now the chain claims
        // total_length = 2 * OVERFLOW_PAYLOAD + 100 but only
        // delivers OVERFLOW_PAYLOAD bytes.
        patch_next_page(&mut cache, first, 0);

        match Overflow::read(&mut cache, first) {
            Err(ChiselError::CorruptPage { page_id: _ }) => {}
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }

    // Sanity: the hardened read() still correctly returns a
    // well-formed chain's payload byte-for-byte.
    #[test]
    fn read_roundtrip_well_formed_chain() {
        let mut cache = make_cache();
        let (first, expected) = write_three_page_chain(&mut cache);
        let got = Overflow::read(&mut cache, first).unwrap();
        assert_eq!(got, expected);
    }
}
