// page.rs — Foundation layer (layer 1). Defines the on-disk page format:
// page size, type tags, header sizes, magic/format-version constants, and
// the checksum primitives every other page module relies on.
//
// Invariant: every page on disk is exactly PAGE_SIZE bytes and ends with an
// 8-byte little-endian XXH3 checksum computed over bytes 0..CHECKSUM_OFFSET.
// PageCache validates this checksum on every disk LOAD (cache miss); cache
// hits skip revalidation because the in-memory bytes are trusted between
// writes. A mismatch at load time is fatal (ChecksumMismatch).
// On-disk format is little-endian by convention (we assume LE hosts and
// explicitly use to_le_bytes/from_le_bytes for portability if that changes).
//
// Versioning is two-tiered. At the FILE level, the superblock carries a
// packed MAJOR/MINOR `format_version` (I29); the open-time gate rejects
// a file whose MAJOR doesn't match the binary's. At the PAGE level, each
// non-superblock page carries a one-byte per-page format version (I31)
// that lets individual page layouts evolve within a MAJOR without a
// file-wide bump — the foundation for lazy per-page upgrade. See the
// PAGE_FORMAT_VERSION_CURRENT block below for the storage convention.

use xxhash_rust::xxh3::xxh3_64;

// 8 KiB pages: small enough to keep per-page I/O cheap and the cache working
// set fine-grained, large enough to amortize header overhead. Changing this
// is a format break — FORMAT_VERSION must bump.
pub const PAGE_SIZE: usize = 8192;
pub const CHECKSUM_SIZE: usize = 8;
// Checksum lives at the very end of the page so that the entire header+body
// region (bytes 0..CHECKSUM_OFFSET) is a single contiguous hashable slice.
pub const CHECKSUM_OFFSET: usize = PAGE_SIZE - CHECKSUM_SIZE; // 8184

// Common page header (first 16 bytes) is shared by non-superblock pages: bytes
// 0..8 are page-type-specific (type tag at byte 0, per-page version at byte 1 —
// or byte 2 for HandleTable — plus any type header fields) and bytes 8..16 are
// the I31 reserved common-header region (COMMON_RESERVED_OFFSET..+LEN).
// PageCache identifies a page's type from byte 0 without knowing the concrete
// module. The superblock uses its own layout and does NOT carry this header (it
// stores its txn_counter in bytes 8..16).
//
// `#[allow(dead_code)]`: documents the layout commitment that every page-type
// module's `init_page` agrees with — the single source of truth even though no
// current call site reads it. It MUST equal DATA_PAGE_HEADER_SIZE and
// COMMON_RESERVED_OFFSET + COMMON_RESERVED_LEN; the const-asserts below lock
// that. I133 (2026-06-21): this was 12 — a stale value predating the I31 8..16
// reservation that contradicted its own sibling constants and every comment;
// corrected to 16. A workflow confirmed no page type stores data in 8..16, so
// the reserved region is genuine and the common header genuinely runs to 16.
#[allow(dead_code)]
pub const COMMON_HEADER_SIZE: usize = 16;
// Data pages carry an extended 16-byte header (common header + slot-array
// metadata). PAGE_BODY_SIZE is the space available to slot payloads after
// subtracting that header and the trailing checksum.
pub const DATA_PAGE_HEADER_SIZE: usize = 16;
pub const PAGE_BODY_SIZE: usize = PAGE_SIZE - DATA_PAGE_HEADER_SIZE - CHECKSUM_SIZE; // 8168

// Per-page format version (ISSUES.md I31). Each non-superblock page
// carries a one-byte version that lets future binaries read older
// page layouts without a MAJOR-format bump — the basis for lazy
// per-page upgrade. Version 0 is reserved for "the layout as of the
// I31 commit" (byte values pre-this-change happen to be zero already,
// which is why introducing this byte does not require breaking
// existing files).
//
// Storage offset is per-type:
//   - Data, Overflow, FreeMap: byte 1 (was unused / padding)
//   - HandleTable:             byte 2 (byte 1 holds FLAG_LEAF/INTERIOR)
//
// Bytes 8..16 of every non-superblock page are RESERVED for future
// common-header fields (64 bits of headroom). Today they are
// universally zero. Adding a field there in the future will bump
// the relevant page type's per-page version, not the superblock's
// MAJOR.
pub const PAGE_FORMAT_VERSION_CURRENT: u8 = 0;
// I31 (ISSUES.md): reserved-region constants describe the 8-byte
// common-header headroom every non-superblock page leaves for future
// fields. No live code reads them today; they exist so that whenever a
// new common field IS added (bumping the relevant per-page version),
// the offset comes from one authoritative spot, not a fresh literal.
#[allow(dead_code)]
pub const COMMON_RESERVED_OFFSET: usize = 8;
#[allow(dead_code)]
pub const COMMON_RESERVED_LEN: usize = 8;

// I133 (ISSUES.md, 2026-06-21): lock the header-size constants so the 12-vs-16
// drift cannot recur. The common header spans bytes 0..16 (0..8 type-specific,
// 8..16 reserved), so it must equal both the reserved region's end and the
// data-page header — whose slot directory begins at byte 16, leaving no fields
// in the reserved tail. The per-page version byte (1, or 2 for HandleTable)
// sits below the reserved window. These are compile-time checks: a future edit
// that desyncs the constants fails to build.
const _: () = assert!(COMMON_HEADER_SIZE == COMMON_RESERVED_OFFSET + COMMON_RESERVED_LEN);
const _: () = assert!(COMMON_HEADER_SIZE == DATA_PAGE_HEADER_SIZE);
const _: () = assert!(2 < COMMON_RESERVED_OFFSET);

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
pub const FORMAT_MINOR_VERSION: u16 = 1;

/// MAJOR version stamped into an ENCRYPTED database's superblock. The bump from
/// 1 → 2 hard-rejects old binaries (which gate on FORMAT_MAJOR_VERSION == 1).
pub const FORMAT_MAJOR_VERSION_ENCRYPTED: u16 = 2;

/// Pack the encrypted-DB format version: MAJOR=2, MINOR=current. Used by
/// `create_new` when a key is supplied, and by Task 2.4's open-time gate.
pub fn format_version_encrypted() -> u32 {
    pack_format_version(FORMAT_MAJOR_VERSION_ENCRYPTED, FORMAT_MINOR_VERSION)
}

/// Pack a (major, minor) pair into the on-disk u32 format version.
pub const fn pack_format_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | (minor as u32)
}

/// Extract the major-version byte pair from a packed format_version.
pub const fn format_major(version: u32) -> u16 {
    (version >> 16) as u16
}

/// Extract the minor-version pair from a packed `format_version`. Companion to
/// `format_major`; used by the open-time I29 write-gate — a binary whose
/// MINOR is below the file's must not write the file (it would drop fields it
/// doesn't know). See ISSUES.md I29.
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
    MembershipInterior = 0x05,
    MembershipLeaf = 0x06,
    // Interior node of the multi-page radix freemap tree (COW radix of bitmap
    // leaves). Introduced for the multi-page freemap feature; 0x07 is the next
    // slot after MembershipLeaf. Leaf pages reuse PageType::FreeMap (0x04).
    // No non-test constructor yet — the freemap tree implementation unit is
    // the production caller.
    #[allow(dead_code)]
    FreeMapInterior = 0x07,
}

/// The per-page format version a freshly-initialized page of `page_type`
/// stamps — the single source of truth for the write side (every `init_page`
/// site calls this). Returns 0 for every type today. When a page type's layout
/// gains a version-requiring field, ONLY that type's arm changes; others stay
/// put (per-type format evolution — see docs/specs/2026-06-21-per-page-format-versioning-design.md). The match is exhaustive on purpose — adding
/// a `PageType` variant forces a decision here rather than defaulting silently.
/// See ISSUES.md I31.
pub const fn current_version(page_type: PageType) -> u8 {
    match page_type {
        PageType::HandleTable
        | PageType::Data
        | PageType::Overflow
        | PageType::FreeMap
        | PageType::MembershipInterior
        | PageType::MembershipLeaf
        | PageType::FreeMapInterior => PAGE_FORMAT_VERSION_CURRENT,
    }
}

/// Read the per-page format version stamped in `buf`. Dispatches the byte
/// offset on the page-type tag at byte 0: HandleTable keeps byte 1 for its
/// leaf/interior flag and stores the version at byte 2; every other
/// non-superblock page stores it at byte 1. A reader that must distinguish
/// layouts (an additive field where zero is a legitimate value) branches on
/// this: `if page_format_version(buf) >= K { read field } else { default }`.
/// See ISSUES.md I31 and docs/specs/2026-06-21-per-page-format-versioning-design.md.
// `#[allow(dead_code)]`: forward-looking API seam — read-side version gates
// (I31 follow-on) are not yet wired in, so no call site exists today.
#[allow(dead_code)]
pub fn page_format_version(buf: &[u8; PAGE_SIZE]) -> u8 {
    if buf[0] == PageType::HandleTable as u8 {
        buf[2]
    } else {
        buf[1]
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

#[cfg(test)]
mod tests {
    use super::*;

    // A freshly-constructed page of any type — that is, one that has
    // passed through its module's init_page / create_root — must stamp
    // `PAGE_FORMAT_VERSION_CURRENT` into its version byte. Data and
    // FreeMap pages both keep that byte at position 1 (HandleTable's is
    // at byte 2, but it inits through a cache-aware path, below). Today
    // CURRENT is 0, so a pre-I31 page (all header bytes zero by accident)
    // also reports 0; the test still pins the invariant for future bumps.
    //
    // Lives here rather than in each page-type test module because the
    // intent is the cross-cutting convention, not any one type's init.
    #[test]
    fn fresh_pages_report_current_version() {
        // Data — version byte at offset 1.
        let mut buf = [0u8; PAGE_SIZE];
        crate::data_page::DataPage::init_page(&mut buf);
        assert_eq!(buf[1], PAGE_FORMAT_VERSION_CURRENT);

        // FreeMap — version byte at offset 1.
        let mut buf = [0u8; PAGE_SIZE];
        crate::freemap::FreeMap::init_page(&mut buf);
        assert_eq!(buf[1], PAGE_FORMAT_VERSION_CURRENT);

        // Overflow and HandleTable init through their cache-aware paths,
        // not a free-standing helper, so exercising them here would pull
        // PageCache into a pure-byte test. They are covered end-to-end
        // by the existing integration tests instead.
    }

    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──

    #[test]
    fn test_checksum_roundtrip() {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0x42;
        buf[100] = 0xFF;
        stamp_checksum(&mut buf);
        assert!(verify_checksum(&buf));
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0x42;
        stamp_checksum(&mut buf);
        buf[50] = 0xAA;
        assert!(!verify_checksum(&buf));
    }

    #[test]
    fn test_checksum_detects_torn_write() {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0x42;
        stamp_checksum(&mut buf);
        buf[PAGE_SIZE - 2] = 0;
        buf[PAGE_SIZE - 1] = 0;
        assert!(!verify_checksum(&buf));
    }

    // ── Migrated 2026-05-22 from tests/error_and_format.rs (I35 reshape) ──

    #[test]
    fn test_checksum_stamp_is_deterministic() {
        let mut a = [0u8; PAGE_SIZE];
        let mut b = [0u8; PAGE_SIZE];
        a[42] = 0xAB;
        b[42] = 0xAB;
        stamp_checksum(&mut a);
        stamp_checksum(&mut b);
        assert_eq!(a[CHECKSUM_OFFSET..], b[CHECKSUM_OFFSET..]);
    }

    #[test]
    fn test_checksum_detects_every_single_byte_flip_at_sampled_offsets() {
        // Full 8192-byte sweep is overkill in unit tests; sample a handful
        // of offsets including the very first and last non-checksum bytes.
        let mut base = [0u8; PAGE_SIZE];
        for (i, b) in base.iter_mut().enumerate().take(CHECKSUM_OFFSET) {
            *b = (i & 0xFF) as u8;
        }
        stamp_checksum(&mut base);
        assert!(verify_checksum(&base));

        let offsets = [0usize, 1, 127, 4096, CHECKSUM_OFFSET - 1];
        for &off in &offsets {
            let mut buf = base;
            buf[off] ^= 0x01;
            assert!(
                !verify_checksum(&buf),
                "bit flip at offset {off} undetected",
            );
        }
    }

    #[test]
    fn test_checksum_detects_flip_in_checksum_region() {
        // Flipping a bit in the stored checksum itself must also fail
        // verification — the check is "computed == stored", not just
        // "hash(body) stable".
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0xFF;
        stamp_checksum(&mut buf);
        buf[CHECKSUM_OFFSET] ^= 0x80;
        assert!(!verify_checksum(&buf));
    }

    #[test]
    fn test_compute_checksum_ignores_trailing_bytes() {
        // compute_checksum hashes 0..CHECKSUM_OFFSET only; the last 8 bytes
        // must not affect the result.
        let mut a = [0u8; PAGE_SIZE];
        let mut b = [0u8; PAGE_SIZE];
        a[5] = 0x11;
        b[5] = 0x11;
        b[PAGE_SIZE - 1] = 0xFF;
        assert_eq!(compute_checksum(&a), compute_checksum(&b));
    }

    #[test]
    fn test_page_layout_constants_are_self_consistent() {
        // Pin the relationships between constants so a refactor of one
        // doesn't silently desync the others.
        assert_eq!(CHECKSUM_OFFSET + 8, PAGE_SIZE);
        assert_eq!(PAGE_BODY_SIZE + 16 + 8, PAGE_SIZE); // header + body + checksum
        assert_eq!(MAGIC, 0x4348534C);
    }

    #[test]
    fn page_format_version_dispatches_offset_on_page_type() {
        // HandleTable: version at byte 2 (byte 1 holds the leaf/interior flag).
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = PageType::HandleTable as u8;
        buf[1] = 0x01; // FLAG_LEAF — must be ignored by the version reader
        buf[2] = 7;
        assert_eq!(page_format_version(&buf), 7);

        // Every other non-superblock type: version at byte 1.
        for pt in [
            PageType::Data,
            PageType::Overflow,
            PageType::FreeMap,
            PageType::FreeMapInterior,
            PageType::MembershipInterior,
            PageType::MembershipLeaf,
        ] {
            let mut buf = [0u8; PAGE_SIZE];
            buf[0] = pt as u8;
            buf[1] = 5;
            buf[2] = 99; // type-specific byte must not leak into the version
            assert_eq!(page_format_version(&buf), 5, "type {pt:?}");
        }
    }

    #[test]
    fn current_version_is_zero_for_all_types_today() {
        for pt in [
            PageType::HandleTable,
            PageType::Data,
            PageType::Overflow,
            PageType::FreeMap,
            PageType::FreeMapInterior,
            PageType::MembershipInterior,
            PageType::MembershipLeaf,
        ] {
            assert_eq!(current_version(pt), 0, "type {pt:?}");
        }
    }

    #[test]
    fn freemap_interior_type_tag_and_version() {
        assert_eq!(PageType::FreeMapInterior as u8, 0x07);
        assert_eq!(
            current_version(PageType::FreeMapInterior),
            PAGE_FORMAT_VERSION_CURRENT
        );
    }

    #[test]
    fn format_minor_extracts_low_16_bits() {
        let v = pack_format_version(3, 42);
        assert_eq!(format_major(v), 3);
        assert_eq!(format_minor(v), 42);
        assert_eq!(format_minor(FORMAT_VERSION), FORMAT_MINOR_VERSION);
    }
}
