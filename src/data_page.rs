// data_page.rs — Slotted page for packing multiple values (layer 4: page-type logic).
//
// Role in the system: operates on raw [u8; PAGE_SIZE] buffers representing a
// Data page. This is the workhorse container for user values that fit within a
// single page; larger values spill into overflow.rs chains.
//
// Physical layout of a data page:
//   byte  0       : PageType tag (0x02 = Data)
//   byte  1       : page format version (per-page versioning, I31;
//                   `page::PAGE_FORMAT_VERSION_CURRENT` today)
//   bytes 2..4    : slot_count (u16 LE) — number of slot dir entries (live + dead)
//   bytes 4..6    : free_start (u16 LE) — end of the slot directory, grows forward
//   bytes 6..8    : free_end   (u16 LE) — start of packed data region, grows backward
//   bytes 8..16   : RESERVED for future common-header fields (I31 reserved
//                   region, 64 bits; universally zero across all page types
//                   today). A future common field added here bumps the
//                   affected page type's per-page version, not the
//                   superblock's MAJOR. No live code reads or writes these
//                   bytes; init_page zeroes them.
//   bytes 16..free_start : slot directory (6 bytes per entry)
//   bytes free_start..free_end : free hole (shrinks as slots and data are added)
//   bytes free_end..CHECKSUM_OFFSET : packed value data (grows backward)
//   bytes 8184..8192 : XXH3 checksum
//
// Slot directory entry (6 bytes, LE):
//   bytes 0..2: data offset within the page
//   bytes 2..4: data length
//   bytes 4..6: flags (SLOT_FLAG_LIVE = 0x0001)
//
// Invariants:
//   * Slot indices are stable for the lifetime of the page — the handle
//     table stores (page_id, slot_index) and relies on this.
//   * Dead slots are NOT reused by insert(); the transaction layer frees
//     whole pages rather than compacting individual pages.
//   * free_start <= free_end at all times; insert() fails rather than violating.
//   * All multi-byte integers are little-endian.
//
// Endianness: little-endian throughout. On-disk integers are always written via
// to_le_bytes / read via from_le_bytes.
//
// Mutation discipline: every public mutator operates in place on the buffer
// argument. None of them stamp the checksum — callers (transaction layer) are
// responsible for calling page::stamp_checksum before the page is flushed.
// init_page() does not preserve prior contents.

use crate::page::{self, PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_SIZE};

// Slot directory entry size — must match the layout documented above.
// Changing this is an on-disk format break.
// pub(crate) so transaction.rs can derive MAX_INLINE_VALUE from it (I117),
// making that constant impossible to drift out of sync with the slot layout.
pub(crate) const SLOT_ENTRY_SIZE: usize = 6; // offset(2) + length(2) + flags(2)
const SLOT_FLAG_LIVE: u16 = 0x0001;

pub struct DataPage;

// `free_space`, `used_space`, and `iter_live` are called only from
// cfg(test) today; `#[allow(dead_code)]` suppresses the lib-build warning.
#[allow(dead_code)]
impl DataPage {
    /// Initialize a page buffer as an empty data page.
    //
    // Zeros the full buffer then sets the page type, free_start (= end of
    // header, where the slot directory begins) and free_end (= checksum
    // offset, where data packing begins growing backward). slot_count is left
    // implicitly at 0 via the fill.
    //
    // Does NOT preserve any existing bytes.
    pub fn init_page(buf: &mut [u8; PAGE_SIZE]) {
        buf.fill(0);
        buf[0] = PageType::Data as u8;
        // I31: per-page version byte at position 1 for Data pages.
        // Written explicitly (even though buf.fill(0) already set it) so
        // the layout intent is visible and future `CURRENT` bumps flow
        // through here automatically.
        buf[1] = page::current_version(page::PageType::Data);
        // slot_count = 0 (bytes 2..4 already zero)
        // free_start = DATA_PAGE_HEADER_SIZE (end of header = start of slot dir area)
        let free_start = DATA_PAGE_HEADER_SIZE as u16;
        buf[4..6].copy_from_slice(&free_start.to_le_bytes());
        // free_end = CHECKSUM_OFFSET (start of data region, growing backward)
        let free_end = CHECKSUM_OFFSET as u16;
        buf[6..8].copy_from_slice(&free_end.to_le_bytes());
    }

    /// Number of slots (live + dead) in the page.
    //
    // Dead slots are counted here because slot indices are positional: the Nth
    // entry in the directory is always at the same offset, regardless of
    // liveness. Callers wanting only live entries should iterate and filter.
    pub fn slot_count(buf: &[u8; PAGE_SIZE]) -> u16 {
        u16::from_le_bytes(buf[2..4].try_into().unwrap())
    }

    /// Available contiguous free space in the page.
    //
    // Reflects only the central hole between slot dir and data region. Does
    // NOT account for holes left by dead slots (the transaction layer frees
    // whole pages rather than compacting individual pages). Returns 0 if the
    // header is structurally invalid (a corrupt page with free_start > free_end,
    // or free_end past the checksum region, etc.) so downstream mutators refuse
    // to operate on it. The more precise validity check is `validate_header` —
    // callers that care about the distinction between "full" and "corrupt"
    // should use that.
    pub fn free_space(buf: &[u8; PAGE_SIZE]) -> usize {
        match Self::validate_header(buf) {
            Some((free_start, free_end, _)) => free_end - free_start,
            None => 0,
        }
    }

    /// Validate the page header. Returns `(free_start, free_end, slot_count)`
    /// if every header byte is self-consistent, otherwise None.
    ///
    /// Used as the gatekeeper for every mutating or iterating path: a None
    /// result means the page's bytes are inconsistent with the Data format
    /// (wrong page-type tag, header pointers out of bounds, slot directory
    /// overlapping the data region, etc.). The checksum layer above us
    /// catches bit-flips; this function catches the separate failure mode
    /// where we're handed a checksum-valid page that was never a valid
    /// Data page to begin with — for example, one reached via a stale
    /// handle-table entry pointing at a freemap or overflow page. Without
    /// this check, indexing the directory or the payload region with
    /// untrusted u16 values would panic.
    fn validate_header(buf: &[u8; PAGE_SIZE]) -> Option<(usize, usize, u16)> {
        if buf[0] != PageType::Data as u8 {
            return None;
        }
        let slot_count = u16::from_le_bytes(buf[2..4].try_into().unwrap());
        let free_start = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
        let free_end = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
        if free_start < DATA_PAGE_HEADER_SIZE {
            return None;
        }
        if free_end > CHECKSUM_OFFSET {
            return None;
        }
        if free_start > free_end {
            return None;
        }
        // Slot directory must fit entirely below free_start: header area
        // (0..DATA_PAGE_HEADER_SIZE) then `slot_count` entries of 6 bytes
        // each. If these overflow past free_start, the slot directory and
        // the free hole would overlap.
        let dir_end = DATA_PAGE_HEADER_SIZE
            .checked_add((slot_count as usize).checked_mul(SLOT_ENTRY_SIZE)?)?;
        if dir_end > free_start {
            return None;
        }
        Some((free_start, free_end, slot_count))
    }

    /// Insert a value into the page. Returns the slot index, or None if the page is full.
    //
    // Algorithm: append a new slot directory entry at free_start (growing
    // forward) and copy the value bytes to just below free_end (growing
    // backward). The two pointers converge into the central hole.
    //
    // The returned slot index is always the pre-insertion slot_count, making
    // indices monotonically increasing and stable for the page's lifetime.
    //
    // The multi-slot machinery here is load-bearing, not spare capacity. Under
    // R1 packing the transaction layer keeps an insert cursor — a data page
    // allocated earlier in the SAME transaction that still has room — and every
    // value is appended to it through this function. `PageCache::new_page` is
    // called only when the cursor fills (this returns None) or when packing is
    // disabled because a savepoint is active. See `transaction::packing`.
    //
    // This comment previously said the opposite: that the transaction layer
    // allocated a fresh page per insert and that this function was "correct,
    // just underutilized". Do not simplify the slot-directory append path on
    // that basis — it now runs for every non-first insert in a transaction.
    pub fn insert(buf: &mut [u8; PAGE_SIZE], value: &[u8]) -> Option<u16> {
        let (free_start, free_end, slot_count) = Self::validate_header(buf)?;
        let needed = SLOT_ENTRY_SIZE + value.len();
        if free_end - free_start < needed {
            return None;
        }
        // slot_count is bounded by validate_header: the full directory
        // (including the new entry) still fits below free_start.

        // Data grows backward from free_end.
        let data_offset = free_end - value.len();
        buf[data_offset..data_offset + value.len()].copy_from_slice(value);

        // Write slot directory entry at free_start.
        // The directory is append-only during inserts; dead slots retain their
        // positions.
        let slot_offset = free_start;
        buf[slot_offset..slot_offset + 2].copy_from_slice(&(data_offset as u16).to_le_bytes());
        buf[slot_offset + 2..slot_offset + 4].copy_from_slice(&(value.len() as u16).to_le_bytes());
        buf[slot_offset + 4..slot_offset + 6].copy_from_slice(&SLOT_FLAG_LIVE.to_le_bytes());

        // Update header.
        let new_slot_count = slot_count + 1;
        buf[2..4].copy_from_slice(&new_slot_count.to_le_bytes());
        let new_free_start = (free_start + SLOT_ENTRY_SIZE) as u16;
        buf[4..6].copy_from_slice(&new_free_start.to_le_bytes());
        let new_free_end = data_offset as u16;
        buf[6..8].copy_from_slice(&new_free_end.to_le_bytes());

        Some(slot_count) // slot index = old count
    }

    /// Read a value by slot index. Returns None if the slot is dead, out of
    /// range, or the page header / slot entry is structurally invalid.
    //
    // Zero-copy: returns a slice borrowed from the page buffer. The borrow
    // keeps the buffer immutable for its lifetime, which is enforced by Rust's
    // borrow checker at the call site.
    //
    // A None return does NOT distinguish "legitimately dead" from "corrupt
    // page". Callers that know a slot *should* be live (e.g., the handle
    // table points at it) should treat a None as CorruptPage; see the
    // transaction-layer read path for that upgrade.
    pub fn read(buf: &[u8; PAGE_SIZE], slot: u16) -> Option<&[u8]> {
        let (_, _, slot_count) = Self::validate_header(buf)?;
        if slot >= slot_count {
            return None;
        }
        let (offset, length, flags) = Self::read_slot_entry(buf, slot)?;
        if flags != SLOT_FLAG_LIVE {
            return None;
        }
        Some(&buf[offset..offset + length])
    }

    /// Total occupied bytes (live data + slot directory), for computing occupancy.
    //
    // Used by defrag/stats to decide whether a page is sparse enough to merit
    // consolidation. Counts only live entries' payload plus the full slot
    // directory overhead for those live slots — dead slots' directory bytes
    // and orphaned data bytes are not included.
    pub fn used_space(buf: &[u8; PAGE_SIZE]) -> usize {
        // Corrupt header ⇒ occupancy is meaningless; returning 0 keeps
        // defrag's density math defined without pretending the page has
        // usable free space.
        let Some((_, _, count)) = Self::validate_header(buf) else {
            return 0;
        };
        let mut data_bytes = 0usize;
        let mut live_slots = 0usize;
        for i in 0..count {
            let Some((_, length, flags)) = Self::read_slot_entry(buf, i) else {
                return 0;
            };
            if flags == SLOT_FLAG_LIVE {
                data_bytes += length;
                live_slots += 1;
            }
        }
        live_slots * SLOT_ENTRY_SIZE + data_bytes
    }

    /// Iterate over all live slots, yielding (slot_index, data_slice).
    //
    // Slot indices in the result are the CURRENT indices (matching what read()
    // would accept), not dense 0..N. This lets callers preserve identity
    // across the iteration — important for defrag, which rewrites handle
    // table entries.
    pub fn iter_live(buf: &[u8; PAGE_SIZE]) -> Vec<(u16, &[u8])> {
        // Corrupt header ⇒ no slots are reported; see used_space for the
        // same reasoning.
        let Some((_, _, count)) = Self::validate_header(buf) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for i in 0..count {
            let Some((offset, length, flags)) = Self::read_slot_entry(buf, i) else {
                return Vec::new();
            };
            if flags == SLOT_FLAG_LIVE {
                result.push((i, &buf[offset..offset + length]));
            }
        }
        result
    }

    // --- Private helpers ---
    //
    // `read_slot_entry` returns Option so every caller is forced to decide
    // what a structurally-broken slot entry means. Whenever a slot index
    // comes from the on-disk file (not a freshly-produced index), route it
    // through `read_slot_entry` rather than indexing directly.

    /// Parse a single slot directory entry. Returns None if the slot
    /// base or the referenced payload range would fall outside the
    /// page body.
    ///
    /// Callers may have already validated `slot < slot_count` via
    /// `validate_header`, but this function still re-checks the
    /// physical bounds so it remains safe to call on an untrusted
    /// slot index in isolation.
    fn read_slot_entry(buf: &[u8; PAGE_SIZE], slot: u16) -> Option<(usize, usize, u16)> {
        let base =
            DATA_PAGE_HEADER_SIZE.checked_add((slot as usize).checked_mul(SLOT_ENTRY_SIZE)?)?;
        if base.checked_add(SLOT_ENTRY_SIZE)? > CHECKSUM_OFFSET {
            return None;
        }
        let offset = u16::from_le_bytes(buf[base..base + 2].try_into().unwrap()) as usize;
        let length = u16::from_le_bytes(buf[base + 2..base + 4].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(buf[base + 4..base + 6].try_into().unwrap());
        // Payload must live entirely within the page body (offset + length <=
        // CHECKSUM_OFFSET) and must not overlap the fixed common header
        // (offset >= DATA_PAGE_HEADER_SIZE). Note: this does NOT verify that
        // the payload lies beyond the slot directory (offset >= free_start) —
        // a checksum-valid but corrupt page could carry a slot whose offset
        // aliases the directory area; only the directory boundary (above) is
        // enforced here. Dead slots are still subject to these bounds, so we
        // check unconditionally even when the caller may later discard the entry.
        if offset < DATA_PAGE_HEADER_SIZE {
            return None;
        }
        let end = offset.checked_add(length)?;
        if end > CHECKSUM_OFFSET {
            return None;
        }
        Some((offset, length, flags))
    }
}

#[cfg(test)]
mod corruption_tests {
    use super::*;

    // Each of these tests constructs a checksum-VALID page whose header
    // or slot directory has out-of-band values, then verifies that every
    // public reader returns a graceful empty/false rather than panicking.
    // This is the defence-in-depth layer that catches "wrong page type
    // reached via a stale pointer" — the page-cache checksum layer can't
    // detect that because the bytes were written correctly for what they
    // were; they're just not a Data page.

    fn fresh_page() -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        DataPage::init_page(&mut buf);
        buf
    }

    // I133 (ISSUES.md, 2026-06-21): the 8..16 common-reserved region must stay
    // zero on a Data page — the data-page format uses none of it (the slot
    // directory starts at byte 16 = DATA_PAGE_HEADER_SIZE). If future code ever
    // writes a field there, this trips so the "8..16 is reserved everywhere"
    // assumption (which COMMON_HEADER_SIZE == 16 encodes) is revisited
    // deliberately rather than silently broken.
    #[test]
    fn reserved_common_header_bytes_stay_zero() {
        let mut buf = fresh_page();
        assert_eq!(
            &buf[8..16],
            &[0u8; 8],
            "init_page must zero the reserved 8..16"
        );
        DataPage::insert(&mut buf, b"hello world").unwrap();
        assert_eq!(
            &buf[8..16],
            &[0u8; 8],
            "insert must not write into the reserved 8..16"
        );
        assert_eq!(
            DataPage::slot_count(&buf),
            1,
            "the value should have landed in slot 0"
        );
    }

    #[test]
    fn read_returns_none_on_corrupt_slot_count() {
        let mut buf = fresh_page();
        // slot_count = 0xFFFF: far past the directory region.
        buf[2..4].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(DataPage::read(&buf, 0).is_none());
        assert!(DataPage::read(&buf, 1365).is_none());
        assert!(DataPage::read(&buf, 0xFFFE).is_none());
    }

    #[test]
    fn read_returns_none_on_out_of_range_offset_length() {
        let mut buf = fresh_page();
        // Construct a directory with one entry whose offset+length
        // runs past CHECKSUM_OFFSET.
        buf[2..4].copy_from_slice(&1u16.to_le_bytes());
        let free_start = (DATA_PAGE_HEADER_SIZE + SLOT_ENTRY_SIZE) as u16;
        buf[4..6].copy_from_slice(&free_start.to_le_bytes());
        let base = DATA_PAGE_HEADER_SIZE;
        // offset = 8000, length = 500 → end = 8500 > CHECKSUM_OFFSET (8184)
        buf[base..base + 2].copy_from_slice(&8000u16.to_le_bytes());
        buf[base + 2..base + 4].copy_from_slice(&500u16.to_le_bytes());
        buf[base + 4..base + 6].copy_from_slice(&SLOT_FLAG_LIVE.to_le_bytes());
        assert!(DataPage::read(&buf, 0).is_none());
    }

    #[test]
    fn read_returns_none_on_offset_below_header() {
        let mut buf = fresh_page();
        buf[2..4].copy_from_slice(&1u16.to_le_bytes());
        buf[4..6]
            .copy_from_slice(&((DATA_PAGE_HEADER_SIZE + SLOT_ENTRY_SIZE) as u16).to_le_bytes());
        // offset = 4 sits inside the fixed header.
        let base = DATA_PAGE_HEADER_SIZE;
        buf[base..base + 2].copy_from_slice(&4u16.to_le_bytes());
        buf[base + 2..base + 4].copy_from_slice(&1u16.to_le_bytes());
        buf[base + 4..base + 6].copy_from_slice(&SLOT_FLAG_LIVE.to_le_bytes());
        assert!(DataPage::read(&buf, 0).is_none());
    }

    #[test]
    fn read_returns_none_on_free_end_past_page_body() {
        let mut buf = fresh_page();
        // free_end past CHECKSUM_OFFSET
        buf[6..8].copy_from_slice(&(CHECKSUM_OFFSET as u16 + 100).to_le_bytes());
        assert!(DataPage::read(&buf, 0).is_none());
        assert_eq!(DataPage::free_space(&buf), 0);
    }

    #[test]
    fn read_returns_none_on_free_start_less_than_header() {
        let mut buf = fresh_page();
        buf[4..6].copy_from_slice(&4u16.to_le_bytes());
        assert!(DataPage::read(&buf, 0).is_none());
    }

    #[test]
    fn read_returns_none_on_wrong_page_type() {
        let mut buf = fresh_page();
        buf[0] = 0xFF; // not PageType::Data
        assert!(DataPage::read(&buf, 0).is_none());
        assert_eq!(DataPage::used_space(&buf), 0);
        assert_eq!(DataPage::iter_live(&buf).len(), 0);
    }

    #[test]
    fn insert_refuses_corrupt_header() {
        let mut buf = fresh_page();
        buf[4..6].copy_from_slice(&(CHECKSUM_OFFSET as u16 + 1).to_le_bytes());
        assert!(DataPage::insert(&mut buf, b"x").is_none());
    }

    #[test]
    fn iter_live_and_used_space_survive_oversized_slot_count() {
        let mut buf = fresh_page();
        DataPage::insert(&mut buf, b"abc").unwrap();
        // Inflate slot_count so the directory "extends" past the data
        // region. validate_header must reject this.
        buf[2..4].copy_from_slice(&2000u16.to_le_bytes());
        assert_eq!(DataPage::iter_live(&buf).len(), 0);
        assert_eq!(DataPage::used_space(&buf), 0);
    }

    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──
    //
    // Lives in `corruption_tests` despite not being corruption-focused —
    // data_page.rs has only one #[cfg(test)] mod, and splitting that for
    // a naming nit is unnecessary churn.

    #[test]
    fn test_data_page_insert_and_read() {
        let mut buf = [0u8; PAGE_SIZE];
        DataPage::init_page(&mut buf);
        let slot = DataPage::insert(&mut buf, b"hello world").unwrap();
        let data = DataPage::read(&buf, slot).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn test_data_page_multiple_inserts() {
        let mut buf = [0u8; PAGE_SIZE];
        DataPage::init_page(&mut buf);
        let s0 = DataPage::insert(&mut buf, b"aaa").unwrap();
        let s1 = DataPage::insert(&mut buf, b"bbb").unwrap();
        let s2 = DataPage::insert(&mut buf, b"ccc").unwrap();
        assert_eq!(DataPage::read(&buf, s0).unwrap(), b"aaa");
        assert_eq!(DataPage::read(&buf, s1).unwrap(), b"bbb");
        assert_eq!(DataPage::read(&buf, s2).unwrap(), b"ccc");
        assert_eq!(DataPage::slot_count(&buf), 3);
    }

    #[test]
    fn test_data_page_full() {
        let mut buf = [0u8; PAGE_SIZE];
        DataPage::init_page(&mut buf);
        let big = vec![0xABu8; 2000];
        let mut count = 0;
        while DataPage::insert(&mut buf, &big).is_some() {
            count += 1;
        }
        assert!(count >= 3);
        assert!(count <= 4);
    }

    #[test]
    fn test_data_page_max_value() {
        let mut buf = [0u8; PAGE_SIZE];
        DataPage::init_page(&mut buf);
        let max_val = vec![0xCD; 8162];
        let slot = DataPage::insert(&mut buf, &max_val);
        assert!(slot.is_some());
        assert_eq!(DataPage::read(&buf, slot.unwrap()).unwrap().len(), 8162);
    }

    // I71 (ISSUES.md, 2026-05-22): property test — DataPage::insert
    // then DataPage::read round-trips byte content for any value at
    // or below the inline maximum (PAGE_BODY_SIZE - SLOT_ENTRY_SIZE
    // = 8168 - 6 = 8162). proptest shrinks failures down to minimal
    // counterexamples: an off-by-one in slot layout would reproduce
    // at e.g. value.len() == 1 rather than burying it in an 8000-byte
    // dump.
    proptest::proptest! {
        #[test]
        fn prop_insert_read_roundtrip(value in proptest::collection::vec(0u8..=255, 0..=8162)) {
            let mut buf = [0u8; PAGE_SIZE];
            DataPage::init_page(&mut buf);
            let slot = DataPage::insert(&mut buf, &value)
                .expect("insert must succeed for value sized within the inline max");
            let read_back = DataPage::read(&buf, slot)
                .expect("read must return the value at its own slot");
            assert_eq!(read_back, value.as_slice(),
                "insert then read must preserve bytes exactly");
        }
    }
}
