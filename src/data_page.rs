// data_page.rs — Slotted page for packing multiple values (layer 4: page-type logic).
//
// Role in the system: operates on raw [u8; PAGE_SIZE] buffers representing a
// Data page. This is the workhorse container for user values that fit within a
// single page; larger values spill into overflow.rs chains.
//
// Physical layout of a data page:
//   byte  0       : PageType tag (0x02 = Data)
//   byte  1       : reserved / padding (zeroed by init_page; no reader looks at it)
//   bytes 2..4    : slot_count (u16 LE) — number of slot dir entries (live + dead)
//   bytes 4..6    : free_start (u16 LE) — end of the slot directory, grows forward
//   bytes 6..8    : free_end   (u16 LE) — start of packed data region, grows backward
//   bytes 8..16   : reserved for a future per-page txn_counter (u64 LE) that
//                   would record "the transaction that last wrote this page".
//                   Currently ALLOCATED IN THE LAYOUT BUT NOT WRITTEN by any
//                   module — init_page zeroes these bytes and compact()
//                   faithfully preserves whatever's there across a rebuild.
//                   The layout slot exists so the field can be added without
//                   an on-disk format bump; no live code reads or writes it.
//   bytes 16..free_start : slot directory (6 bytes per entry)
//   bytes free_start..free_end : free hole (shrinks as slots and data are added)
//   bytes free_end..CHECKSUM_OFFSET : packed value data (grows backward)
//   bytes 8184..8192 : XXH3 checksum
//
// Slot directory entry (6 bytes, LE):
//   bytes 0..2: data offset within the page
//   bytes 2..4: data length
//   bytes 4..6: flags (SLOT_FLAG_LIVE = 0x0001, SLOT_FLAG_DEAD = 0x0000)
//
// Invariants:
//   * Slot indices are stable while the page is not compacted — the handle
//     table stores (page_id, slot_index) and relies on this.
//   * compact() invalidates slot indices and returns an (old→new) mapping so
//     callers can rewrite handle-table entries.
//   * Dead slots are NOT reused by insert(); space reclamation only happens
//     during compact().
//   * free_start <= free_end at all times; insert() fails rather than violating.
//   * All multi-byte integers are little-endian.
//
// Endianness: little-endian throughout. On-disk integers are always written via
// to_le_bytes / read via from_le_bytes.
//
// Mutation discipline: every public mutator operates in place on the buffer
// argument. None of them stamp the checksum — callers (transaction layer) are
// responsible for calling page::stamp_checksum before the page is flushed.
// init_page() does not preserve prior contents; compact() rebuilds the page
// but preserves bytes 8..16 (txn_counter).

use crate::page::{PageType, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE, PAGE_SIZE};

// Slot directory entry size — must match the layout documented above.
// Changing this is an on-disk format break.
const SLOT_ENTRY_SIZE: usize = 6; // offset(2) + length(2) + flags(2)
const SLOT_FLAG_LIVE: u16 = 0x0001;
const SLOT_FLAG_DEAD: u16 = 0x0000;

pub struct DataPage;

impl DataPage {
    /// Initialize a page buffer as an empty data page.
    //
    // Zeros the full buffer then sets the page type, free_start (= end of
    // header, where the slot directory begins) and free_end (= checksum
    // offset, where data packing begins growing backward). slot_count is left
    // implicitly at 0 via the fill.
    //
    // Does NOT preserve any existing bytes — compact() must save/restore the
    // txn_counter itself if it wants to keep that metadata.
    pub fn init_page(buf: &mut [u8; PAGE_SIZE]) {
        buf.fill(0);
        buf[0] = PageType::Data as u8;
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
    // NOT account for holes left by dead slots — those are reclaimed only by
    // compact(). saturating_sub defends against a corrupt header where
    // free_start > free_end; returning 0 forces the caller to fail insert
    // rather than underflowing.
    pub fn free_space(buf: &[u8; PAGE_SIZE]) -> usize {
        let free_start = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
        let free_end = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
        free_end.saturating_sub(free_start)
    }

    /// Insert a value into the page. Returns the slot index, or None if the page is full.
    //
    // Algorithm: append a new slot directory entry at free_start (growing
    // forward) and copy the value bytes to just below free_end (growing
    // backward). The two pointers converge into the central hole.
    //
    // The returned slot index is always the pre-insertion slot_count, making
    // indices monotonically increasing and stable for the page's lifetime
    // (until compact() renumbers them).
    //
    // Note (v1 simplification per CLAUDE.md): the transaction layer calls
    // PageCache::new_page() for every insert rather than scanning existing
    // pages for free slots. Intentional, not a bug — this function itself is
    // correct; it's just underutilized.
    pub fn insert(buf: &mut [u8; PAGE_SIZE], value: &[u8]) -> Option<u16> {
        let needed = SLOT_ENTRY_SIZE + value.len();
        if Self::free_space(buf) < needed {
            return None;
        }

        let slot_count = Self::slot_count(buf);
        let free_start = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
        let free_end = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;

        // Data grows backward from free_end.
        let data_offset = free_end - value.len();
        buf[data_offset..data_offset + value.len()].copy_from_slice(value);

        // Write slot directory entry at free_start.
        // The directory is append-only during inserts; dead slots retain their
        // positions and are only collapsed by compact().
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

    /// Read a value by slot index. Returns None if the slot is dead or out of range.
    //
    // Zero-copy: returns a slice borrowed from the page buffer. The borrow
    // keeps the buffer immutable for its lifetime, which is enforced by Rust's
    // borrow checker at the call site.
    pub fn read(buf: &[u8; PAGE_SIZE], slot: u16) -> Option<&[u8]> {
        if slot >= Self::slot_count(buf) {
            return None;
        }
        let (offset, length, flags) = Self::read_slot_entry(buf, slot);
        if flags != SLOT_FLAG_LIVE {
            return None;
        }
        Some(&buf[offset..offset + length])
    }

    /// Update a value in-place. If the new value fits in the old slot's space,
    /// it's written directly. If larger, the old space becomes a hole and new
    /// data is allocated from the free region.
    //
    // In-place path (fits): preserves the old offset, only the length changes.
    // This keeps data packing stable when values shrink.
    //
    // Relocate path (doesn't fit): the old bytes are abandoned in place and
    // become dead space until compact() runs. The new data is carved from the
    // central hole exactly like insert(), but free_start is NOT advanced —
    // only free_end moves — because we're reusing an existing slot entry, not
    // adding a new one.
    //
    // Returns false if the slot is dead/out-of-range, or if the relocate path
    // has insufficient free space. The page is left unmodified on failure.
    pub fn update(buf: &mut [u8; PAGE_SIZE], slot: u16, value: &[u8]) -> bool {
        if slot >= Self::slot_count(buf) {
            return false;
        }
        let (old_offset, old_length, flags) = Self::read_slot_entry(buf, slot);
        if flags != SLOT_FLAG_LIVE {
            return false;
        }

        if value.len() <= old_length {
            buf[old_offset..old_offset + value.len()].copy_from_slice(value);
            Self::write_slot_length(buf, slot, value.len() as u16);
            true
        } else {
            let free_end = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
            let free_start = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
            let available = free_end.saturating_sub(free_start);
            if available < value.len() {
                return false;
            }
            let new_offset = free_end - value.len();
            buf[new_offset..new_offset + value.len()].copy_from_slice(value);
            Self::write_slot_offset(buf, slot, new_offset as u16);
            Self::write_slot_length(buf, slot, value.len() as u16);
            // Only free_end advances: the slot entry already exists in the
            // directory, so free_start stays put.
            buf[6..8].copy_from_slice(&(new_offset as u16).to_le_bytes());
            true
        }
    }

    /// Mark a slot as dead. The data space becomes a hole (reclaimed by compact).
    //
    // Tombstone-style delete: the slot entry and its data bytes remain so that
    // live slot indices don't shift. Actual space reclamation is deferred to
    // compact(). This is required because external references (the handle
    // table) point at (page, slot) pairs.
    pub fn delete(buf: &mut [u8; PAGE_SIZE], slot: u16) {
        if slot < Self::slot_count(buf) {
            Self::write_slot_flags(buf, slot, SLOT_FLAG_DEAD);
        }
    }

    /// Compact the page: remove dead slots, pack surviving data contiguously,
    /// and rebuild the slot directory. Returns a mapping of (old_slot → new_slot).
    //
    // Strategy: copy all live values out to a temporary Vec, re-init the page,
    // and re-insert. This is simple but O(n) allocations and a full copy.
    //
    // The txn_counter (bytes 8..16) is preserved across the re-init because
    // init_page() zeroes the buffer — losing it would break any code tracking
    // "last modified by which transaction". Other header fields are
    // regenerated correctly by the reinsert loop.
    //
    // Returned mapping must be consumed by the caller (transaction layer) to
    // rewrite any handle-table entries that reference this page, since slot
    // indices will have changed. Live slots retain their original relative
    // order, but their indices are renumbered starting from 0.
    pub fn compact(buf: &mut [u8; PAGE_SIZE]) -> Vec<(u16, u16)> {
        let count = Self::slot_count(buf);
        let mut live_entries: Vec<(u16, Vec<u8>)> = Vec::new();

        for i in 0..count {
            let (offset, length, flags) = Self::read_slot_entry(buf, i);
            if flags == SLOT_FLAG_LIVE {
                let data = buf[offset..offset + length].to_vec();
                live_entries.push((i, data));
            }
        }

        // Save txn_counter across the re-init (init_page fills the whole buf).
        let txn_counter_bytes: [u8; 8] = buf[8..16].try_into().unwrap();
        Self::init_page(buf);
        buf[8..16].copy_from_slice(&txn_counter_bytes);

        let mut mapping = Vec::new();
        for (old_slot, data) in &live_entries {
            // unwrap is safe: the live data came from this same page, so it
            // must fit after removing dead space.
            let new_slot = Self::insert(buf, data).unwrap();
            mapping.push((*old_slot, new_slot));
        }

        mapping
    }

    /// Total occupied bytes (live data + slot directory), for computing occupancy.
    //
    // Used by defrag/stats to decide whether a page is sparse enough to merit
    // consolidation. Counts only live entries' payload plus the full slot
    // directory overhead for those live slots — dead slots' directory bytes
    // and orphaned data bytes are not included, matching "what would remain
    // after compact()".
    pub fn used_space(buf: &[u8; PAGE_SIZE]) -> usize {
        let count = Self::slot_count(buf);
        let mut data_bytes = 0usize;
        let mut live_slots = 0usize;
        for i in 0..count {
            let (_, length, flags) = Self::read_slot_entry(buf, i);
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
        let count = Self::slot_count(buf);
        let mut result = Vec::new();
        for i in 0..count {
            let (offset, length, flags) = Self::read_slot_entry(buf, i);
            if flags == SLOT_FLAG_LIVE {
                result.push((i, &buf[offset..offset + length]));
            }
        }
        result
    }

    // --- Private helpers ---
    //
    // These compute slot entry byte offsets from a slot index. The base
    // formula `DATA_PAGE_HEADER_SIZE + slot * SLOT_ENTRY_SIZE` encodes the
    // invariant that the slot directory is packed immediately after the
    // fixed header with no gaps. None of these helpers validate bounds; the
    // callers above must ensure `slot < slot_count(buf)`.

    fn read_slot_entry(buf: &[u8; PAGE_SIZE], slot: u16) -> (usize, usize, u16) {
        let base = DATA_PAGE_HEADER_SIZE + (slot as usize) * SLOT_ENTRY_SIZE;
        let offset = u16::from_le_bytes(buf[base..base + 2].try_into().unwrap()) as usize;
        let length = u16::from_le_bytes(buf[base + 2..base + 4].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(buf[base + 4..base + 6].try_into().unwrap());
        (offset, length, flags)
    }

    fn write_slot_offset(buf: &mut [u8; PAGE_SIZE], slot: u16, offset: u16) {
        let base = DATA_PAGE_HEADER_SIZE + (slot as usize) * SLOT_ENTRY_SIZE;
        buf[base..base + 2].copy_from_slice(&offset.to_le_bytes());
    }

    fn write_slot_length(buf: &mut [u8; PAGE_SIZE], slot: u16, length: u16) {
        let base = DATA_PAGE_HEADER_SIZE + (slot as usize) * SLOT_ENTRY_SIZE;
        buf[base + 2..base + 4].copy_from_slice(&length.to_le_bytes());
    }

    fn write_slot_flags(buf: &mut [u8; PAGE_SIZE], slot: u16, flags: u16) {
        let base = DATA_PAGE_HEADER_SIZE + (slot as usize) * SLOT_ENTRY_SIZE;
        buf[base + 4..base + 6].copy_from_slice(&flags.to_le_bytes());
    }
}
