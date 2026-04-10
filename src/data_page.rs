// data_page.rs — Slotted page for packing multiple values.
// Layout: [Header 16B] [Slot Dir →] [Free Space] [← Data] [Checksum 8B]
// The slot directory grows forward from the header; value data grows backward
// from the checksum. When they meet, the page is full.

use crate::page::{PageType, PAGE_SIZE, CHECKSUM_OFFSET, DATA_PAGE_HEADER_SIZE};

const SLOT_ENTRY_SIZE: usize = 6; // offset(2) + length(2) + flags(2)
const SLOT_FLAG_LIVE: u16 = 0x0001;
const SLOT_FLAG_DEAD: u16 = 0x0000;

pub struct DataPage;

impl DataPage {
    /// Initialize a page buffer as an empty data page.
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
    pub fn slot_count(buf: &[u8; PAGE_SIZE]) -> u16 {
        u16::from_le_bytes(buf[2..4].try_into().unwrap())
    }

    /// Available contiguous free space in the page.
    pub fn free_space(buf: &[u8; PAGE_SIZE]) -> usize {
        let free_start = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
        let free_end = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
        if free_end > free_start {
            free_end - free_start
        } else {
            0
        }
    }

    /// Insert a value into the page. Returns the slot index, or None if the page is full.
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
        let slot_offset = free_start;
        buf[slot_offset..slot_offset + 2].copy_from_slice(&(data_offset as u16).to_le_bytes());
        buf[slot_offset + 2..slot_offset + 4]
            .copy_from_slice(&(value.len() as u16).to_le_bytes());
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
    pub fn update(
        buf: &mut [u8; PAGE_SIZE],
        slot: u16,
        value: &[u8],
    ) -> std::result::Result<(), ()> {
        if slot >= Self::slot_count(buf) {
            return Err(());
        }
        let (old_offset, old_length, flags) = Self::read_slot_entry(buf, slot);
        if flags != SLOT_FLAG_LIVE {
            return Err(());
        }

        if value.len() <= old_length {
            buf[old_offset..old_offset + value.len()].copy_from_slice(value);
            Self::write_slot_length(buf, slot, value.len() as u16);
            Ok(())
        } else {
            let free_end = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
            let free_start = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
            let available = if free_end > free_start {
                free_end - free_start
            } else {
                0
            };
            if available < value.len() {
                return Err(());
            }
            let new_offset = free_end - value.len();
            buf[new_offset..new_offset + value.len()].copy_from_slice(value);
            Self::write_slot_offset(buf, slot, new_offset as u16);
            Self::write_slot_length(buf, slot, value.len() as u16);
            buf[6..8].copy_from_slice(&(new_offset as u16).to_le_bytes());
            Ok(())
        }
    }

    /// Mark a slot as dead. The data space becomes a hole (reclaimed by compact).
    pub fn delete(buf: &mut [u8; PAGE_SIZE], slot: u16) {
        if slot < Self::slot_count(buf) {
            Self::write_slot_flags(buf, slot, SLOT_FLAG_DEAD);
        }
    }

    /// Compact the page: remove dead slots, pack surviving data contiguously,
    /// and rebuild the slot directory. Returns a mapping of (old_slot → new_slot).
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

        let txn_counter_bytes: [u8; 8] = buf[8..16].try_into().unwrap();
        Self::init_page(buf);
        buf[8..16].copy_from_slice(&txn_counter_bytes);

        let mut mapping = Vec::new();
        for (old_slot, data) in &live_entries {
            let new_slot = Self::insert(buf, data).unwrap();
            mapping.push((*old_slot, new_slot));
        }

        mapping
    }

    /// Total occupied bytes (live data + slot directory), for computing occupancy.
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
