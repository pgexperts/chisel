// page.rs — Page-level constants, type tags, common header serialization, and checksum.
// Every page is PAGE_SIZE bytes. The last 8 bytes are always an XXH3 checksum
// covering bytes 0..CHECKSUM_OFFSET.

use xxhash_rust::xxh3::xxh3_64;

pub const PAGE_SIZE: usize = 8192;
pub const CHECKSUM_SIZE: usize = 8;
pub const CHECKSUM_OFFSET: usize = PAGE_SIZE - CHECKSUM_SIZE; // 8184

// Common page header occupies the first 12 bytes of non-superblock pages.
pub const COMMON_HEADER_SIZE: usize = 12;
// Usable body: PAGE_SIZE - data page header (16) - checksum (8)
pub const DATA_PAGE_HEADER_SIZE: usize = 16;
pub const PAGE_BODY_SIZE: usize = PAGE_SIZE - DATA_PAGE_HEADER_SIZE - CHECKSUM_SIZE; // 8168

pub const MAGIC: u32 = 0x4348534C; // "CHSL"
pub const FORMAT_VERSION: u32 = 1;

/// Sentinel value meaning "not yet allocated" for root page pointers.
pub const PAGE_ID_NONE: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    HandleTable = 0x01,
    Data = 0x02,
    Overflow = 0x03,
    FreeMap = 0x04,
}

impl PageType {
    pub fn from_u8(v: u8) -> Option<PageType> {
        match v {
            0x01 => Some(PageType::HandleTable),
            0x02 => Some(PageType::Data),
            0x03 => Some(PageType::Overflow),
            0x04 => Some(PageType::FreeMap),
            _ => None,
        }
    }
}

/// Compute the XXH3 checksum for a page buffer (over bytes 0..CHECKSUM_OFFSET).
pub fn compute_checksum(buf: &[u8; PAGE_SIZE]) -> u64 {
    xxh3_64(&buf[..CHECKSUM_OFFSET])
}

/// Write the checksum into the last 8 bytes of the page buffer.
pub fn stamp_checksum(buf: &mut [u8; PAGE_SIZE]) {
    let cksum = compute_checksum(buf);
    buf[CHECKSUM_OFFSET..].copy_from_slice(&cksum.to_le_bytes());
}

/// Verify the checksum in the last 8 bytes matches the computed checksum.
pub fn verify_checksum(buf: &[u8; PAGE_SIZE]) -> bool {
    let stored = u64::from_le_bytes(buf[CHECKSUM_OFFSET..].try_into().unwrap());
    let computed = compute_checksum(buf);
    stored == computed
}
