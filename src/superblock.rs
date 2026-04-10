// superblock.rs — Superblock layout, serialization, and dual-superblock selection.
// Two superblock copies alternate on each commit. On open, the one with the
// higher txn_counter and valid checksum is selected. This is the atomic
// commit mechanism — the entire transaction becomes visible when the new
// superblock is fsync'd.

use crate::page::{self, MAGIC, PAGE_SIZE};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub magic: u32,
    pub format_version: u32,
    pub txn_counter: u64,
    pub root_handle_table_page: u64,
    pub root_freemap_page: u64,
    pub total_pages: u64,
    pub next_handle: u64,
    pub page_size: u32,
}

impl Superblock {
    /// Serialize the superblock into a full page buffer with a trailing checksum.
    pub fn serialize(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        buf[8..16].copy_from_slice(&self.txn_counter.to_le_bytes());
        buf[16..24].copy_from_slice(&self.root_handle_table_page.to_le_bytes());
        buf[24..32].copy_from_slice(&self.root_freemap_page.to_le_bytes());
        buf[32..40].copy_from_slice(&self.total_pages.to_le_bytes());
        buf[40..48].copy_from_slice(&self.next_handle.to_le_bytes());
        buf[48..52].copy_from_slice(&self.page_size.to_le_bytes());
        // bytes 52..CHECKSUM_OFFSET are reserved (zeroed).
        page::stamp_checksum(&mut buf);
        buf
    }

    /// Deserialize from a page buffer. Returns None if the checksum is invalid
    /// or the magic number doesn't match.
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<Superblock> {
        if !page::verify_checksum(buf) {
            return None;
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return None;
        }
        Some(Superblock {
            magic,
            format_version: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            txn_counter: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            root_handle_table_page: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            root_freemap_page: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            total_pages: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            next_handle: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            page_size: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
        })
    }

    /// Select the active superblock from a slice of page buffers.
    /// Returns the one with the highest txn_counter that has a valid checksum.
    /// Returns None if all superblocks are corrupt.
    pub fn select(buffers: &[[u8; PAGE_SIZE]]) -> Option<Superblock> {
        buffers
            .iter()
            .filter_map(|buf| Superblock::deserialize(buf))
            .max_by_key(|sb| sb.txn_counter)
    }

    /// Create the initial superblock for a new, empty database.
    pub fn new_empty() -> Superblock {
        Superblock {
            magic: MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: 1,
            root_handle_table_page: page::PAGE_ID_NONE,
            root_freemap_page: page::PAGE_ID_NONE,
            total_pages: 2,
            next_handle: 0,
            page_size: PAGE_SIZE as u32,
        }
    }
}
