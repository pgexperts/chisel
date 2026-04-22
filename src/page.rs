// page.rs — Foundation layer (layer 1). Defines the on-disk page format:
// page size, type tags, header sizes, magic/version constants, and the
// checksum primitives every other page module relies on.
//
// Invariant: every page on disk is exactly PAGE_SIZE bytes and ends with an
// 8-byte little-endian XXH3 checksum computed over bytes 0..CHECKSUM_OFFSET.
// PageCache validates this checksum on every disk LOAD (cache miss); cache
// hits skip revalidation because the in-memory bytes are trusted between
// writes. A mismatch at load time is fatal (ChecksumMismatch).
// On-disk format is little-endian by convention (we assume LE hosts and
// explicitly use to_le_bytes/from_le_bytes for portability if that changes).

use xxhash_rust::xxh3::xxh3_64;

// 8 KiB pages: small enough to keep per-page I/O cheap and the cache working
// set fine-grained, large enough to amortize header overhead. Changing this
// is a format break — FORMAT_VERSION must bump.
pub const PAGE_SIZE: usize = 8192;
pub const CHECKSUM_SIZE: usize = 8;
// Checksum lives at the very end of the page so that the entire header+body
// region (bytes 0..CHECKSUM_OFFSET) is a single contiguous hashable slice.
pub const CHECKSUM_OFFSET: usize = PAGE_SIZE - CHECKSUM_SIZE; // 8184

// Common page header (first 12 bytes) is shared by non-superblock pages so
// that PageCache can identify a page's type without knowing its concrete
// module. The superblock uses its own layout and does not carry this header.
pub const COMMON_HEADER_SIZE: usize = 12;
// Data pages carry an extended 16-byte header (common header + slot-array
// metadata). PAGE_BODY_SIZE is the space available to slot payloads after
// subtracting that header and the trailing checksum.
pub const DATA_PAGE_HEADER_SIZE: usize = 16;
pub const PAGE_BODY_SIZE: usize = PAGE_SIZE - DATA_PAGE_HEADER_SIZE - CHECKSUM_SIZE; // 8168

// "CHSL" in ASCII, stored little-endian so it appears as C-H-S-L when you
// hexdump the first 4 bytes of the file. Used to reject non-Chisel files
// before we even look at the checksum.
pub const MAGIC: u32 = 0x4348534C; // "CHSL"

// On-disk format version: byte-packed u32 with upper 16 bits = MAJOR,
// lower 16 bits = MINOR. Gates at open time on MAJOR only; same-major
// files are read-compatible regardless of minor (additive-only layout
// changes within a major are the invariant that makes this safe).
// Write safety across minors is a separate concern — a binary at minor
// M opening a file at minor M' > M can read but not safely write
// without clobbering fields it doesn't know about; this check is
// deferred until the first 1.1 release (at which point the gate grows
// a "newer minor ⇒ refuse writes" arm). See ISSUES.md I29.
//
// Pre-1.0 files (format_version = 1 or 2 in the old flat scheme) have
// major byte = 0 and are rejected with UnsupportedFormatVersion — a
// clean break, since there are no production DBs yet.
pub const FORMAT_MAJOR_VERSION: u16 = 1;
pub const FORMAT_MINOR_VERSION: u16 = 0;

/// Pack a (major, minor) pair into the on-disk u32 format version.
pub const fn pack_format_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | (minor as u32)
}

/// Extract the major-version byte pair from a packed format_version.
pub const fn format_major(version: u32) -> u16 {
    (version >> 16) as u16
}

/// Extract the minor-version byte pair from a packed format_version.
pub const fn format_minor(version: u32) -> u16 {
    (version & 0xFFFF) as u16
}

pub const FORMAT_VERSION: u32 = pack_format_version(FORMAT_MAJOR_VERSION, FORMAT_MINOR_VERSION);

/// Sentinel value meaning "not yet allocated" for root page pointers
/// (e.g. an empty database has no handle-table or freemap root yet).
/// u64::MAX is used because 0 is a legitimate page id.
pub const PAGE_ID_NONE: u64 = u64::MAX;

/// On-disk page type tag, stored as a single byte in the common header.
/// Discriminants are explicit and stable — changing them is a format break.
/// 0x00 is intentionally reserved so a zeroed/uninitialized page cannot be
/// mistaken for a valid type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    HandleTable = 0x01,
    Data = 0x02,
    Overflow = 0x03,
    FreeMap = 0x04,
}

impl PageType {
    /// Parse a type byte. Returns None for unknown/reserved values; callers
    /// should treat that as corruption since the checksum has typically
    /// already been validated by the time we look at the type tag.
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

// XXH3 was chosen over CRC32C for throughput on modern CPUs. It is a
// non-cryptographic hash — sufficient for detecting disk corruption, NOT
// a defense against adversarial tampering.

/// Compute the XXH3 checksum for a page buffer (over bytes 0..CHECKSUM_OFFSET).
/// The checksum region itself is excluded so that stamp/verify are symmetric.
pub fn compute_checksum(buf: &[u8; PAGE_SIZE]) -> u64 {
    xxh3_64(&buf[..CHECKSUM_OFFSET])
}

/// Write the checksum into the last 8 bytes of the page buffer.
/// Must be called after every mutation and before the page is handed to
/// page_io for writing — otherwise the next read will see a stale checksum
/// and treat the page as corrupt.
pub fn stamp_checksum(buf: &mut [u8; PAGE_SIZE]) {
    let cksum = compute_checksum(buf);
    buf[CHECKSUM_OFFSET..].copy_from_slice(&cksum.to_le_bytes());
}

/// Verify the checksum in the last 8 bytes matches the computed checksum.
/// PageCache calls this on every read; callers should not need to invoke it
/// directly. A `false` result must be reported as ChecksumMismatch (fatal).
pub fn verify_checksum(buf: &[u8; PAGE_SIZE]) -> bool {
    // try_into().unwrap() is infallible: the slice length is a compile-time
    // constant (CHECKSUM_SIZE = 8) matching u64's byte width.
    let stored = u64::from_le_bytes(buf[CHECKSUM_OFFSET..].try_into().unwrap());
    let computed = compute_checksum(buf);
    stored == computed
}
