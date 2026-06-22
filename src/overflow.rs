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
//   footprint — which equals COMMON_HEADER_SIZE (= 16: bytes 0..8
//   type-specific + 8..16 reserved). The overflow page adds its own two
//   u64 fields (total_length, next_page) AFTER that common 16-byte header.
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
// not touch the freemap (that's the caller's responsibility, and see ARCHITECTURE.md
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
            // Zero the whole buffer first: new_page() returns uninitialized
            // bytes, and the trailing payload region on the final page (the
            // bytes past `chunk`) is never explicitly written below. Zeroing
            // makes those slack bytes deterministic so the stamped checksum is
            // reproducible on re-read, and clears the I31 reserved header
            // regions (bytes 2..8 and 8..16) that nothing else touches here.
            buf.fill(0);
            buf[0] = PageType::Overflow as u8;
            // I31: per-page version at byte 1. Explicit for the same
            // reason as data_page::init_page — intent visible, future
            // CURRENT bumps flow through here automatically.
            buf[1] = page::current_version(page::PageType::Overflow);
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
            // Reject a wrong-type page before trusting its header. The cache
            // verifies only the XXH3 checksum, which catches bit-flips but NOT
            // a checksum-valid page of the WRONG type reached via a stale/buggy
            // handle entry. Without this, a Data/FreeMap/HandleTable page would
            // be parsed as overflow metadata (bytes 16..24 as total_length,
            // 24..32 as next_page). data_page::validate_header defends exactly
            // this for data pages; overflow must too (review 2026-06-22).
            if buf[0] != PageType::Overflow as u8 {
                return Err(ChiselError::CorruptPage {
                    page_id: first_page,
                });
            }
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
            // Validate every page in the walk, not just the first: a mid-chain
            // next_page pointer could (under corruption) reference a non-overflow
            // page. See the first-page check above for the full rationale.
            if buf[0] != PageType::Overflow as u8 {
                return Err(ChiselError::CorruptPage {
                    page_id: current_page,
                });
            }
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

    /// Walk an overflow chain and return all page IDs in it. Returns the list
    /// of page IDs in the chain.
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
    pub fn collect_chain_pages(cache: &mut PageCache, first_page: u64) -> Result<Vec<u64>> {
        let total_length = {
            let buf = cache.get(first_page)?;
            // Same wrong-type guard as `read` (review 2026-06-22): the checksum
            // does not certify the page TYPE, so a stale handle pointing at a
            // checksum-valid non-overflow page must surface as CorruptPage, not
            // enumerate a bogus chain.
            if buf[0] != PageType::Overflow as u8 {
                return Err(ChiselError::CorruptPage {
                    page_id: first_page,
                });
            }
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
            if buf[0] != PageType::Overflow as u8 {
                return Err(ChiselError::CorruptPage {
                    page_id: current_page,
                });
            }
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

        match Overflow::collect_chain_pages(&mut cache, first) {
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

    // ── Migrated 2026-05-22 from tests/overflow.rs (I35 reshape) ──
    //
    // These three originally lived in tests/overflow.rs and exercised
    // Overflow against a PageCache built from the integration-test
    // Backing enum (file + memory). The dual-backing coverage was
    // belt-and-suspenders: Overflow operates one layer above PageIo
    // and is backing-agnostic. Migrating to file-backed src/ unit
    // tests is sufficient; the memory path is still exercised
    // end-to-end via tests/in_memory.rs through the public API.
    #[test]
    fn write_and_read_value_smaller_than_threshold() {
        // 4000 bytes would normally stay inline (MAX_INLINE_VALUE is
        // larger). Routing it through Overflow::write directly verifies
        // the chain machinery handles single-page chains even when the
        // caller could have packed inline — useful as a contract test
        // for callers that pre-emptively route to overflow on some
        // sizing heuristic of their own.
        let mut cache = make_cache();
        let value = vec![0xAB; 4000];
        let first_page = Overflow::write(&mut cache, &value).unwrap();
        let read_back = Overflow::read(&mut cache, first_page).unwrap();
        assert_eq!(read_back, value);
    }

    #[test]
    fn write_and_read_multi_page_value() {
        // 20000 bytes spans multiple OVERFLOW_PAYLOAD pages and
        // exercises the chain walk during read.
        let mut cache = make_cache();
        let value = vec![0xCD; 20000];
        let first_page = Overflow::write(&mut cache, &value).unwrap();
        let read_back = Overflow::read(&mut cache, first_page).unwrap();
        assert_eq!(read_back, value);
    }

    #[test]
    fn delete_returns_every_chain_page() {
        // delete must return the ids of every page it freed so the
        // caller can merge them into txn_freed_pages. For a 20000-byte
        // value the chain is at least 3 pages.
        let mut cache = make_cache();
        let value = vec![0xEF; 20000];
        let first_page = Overflow::write(&mut cache, &value).unwrap();
        let freed = Overflow::collect_chain_pages(&mut cache, first_page).unwrap();
        assert!(freed.len() >= 3);
    }

    // Review 2026-06-22: read/delete must reject a checksum-valid page of the
    // WRONG type (reached via a stale/buggy handle entry) instead of parsing its
    // bytes as overflow metadata. Mirrors data_page::corruption_tests, which
    // defends the same failure mode for data pages.
    #[test]
    fn read_and_delete_reject_wrong_page_type_at_first_page() {
        let mut cache = make_cache();
        // A checksum-valid non-overflow page carrying a HUGE total_length in
        // bytes 16..24. The first-page guard must reject it BEFORE the code
        // trusts that length — read() would otherwise reach
        // `Vec::with_capacity(total_length)` (a speculative huge allocation)
        // before the per-page loop guard fires. Using u64::MAX makes the test a
        // true discriminator: without the first-page guard, read aborts on the
        // allocation rather than returning a clean CorruptPage (review 2026-06-22).
        let id = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(id).unwrap();
            buf.fill(0);
            buf[0] = PageType::Data as u8;
            buf[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
            page::stamp_checksum(buf);
        }
        assert!(
            matches!(Overflow::read(&mut cache, id), Err(ChiselError::CorruptPage { page_id }) if page_id == id),
            "read must reject a wrong-type first page before trusting its total_length"
        );
        assert!(
            matches!(Overflow::collect_chain_pages(&mut cache, id), Err(ChiselError::CorruptPage { page_id }) if page_id == id),
            "collect_chain_pages must reject a wrong-type first page"
        );
    }

    #[test]
    fn read_rejects_wrong_page_type_mid_chain() {
        let mut cache = make_cache();
        let (first, _value) = write_three_page_chain(&mut cache);
        // Re-type the SECOND page to a non-overflow page, keeping the checksum
        // valid so only the type guard (not the checksum) can catch it.
        let second = read_next_page(&mut cache, first);
        assert_ne!(second, 0, "a three-page chain must have a second page");
        {
            let buf = cache.get_mut(second).unwrap();
            buf[0] = PageType::Data as u8;
            page::stamp_checksum(buf);
        }
        assert!(
            matches!(Overflow::read(&mut cache, first), Err(ChiselError::CorruptPage { page_id }) if page_id == second),
            "read must reject a wrong-type page mid-walk"
        );
    }
}
