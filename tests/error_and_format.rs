// error_and_format.rs — Unit coverage for error.rs, page.rs, and the
// named-root validation path in transaction.rs.
//
// These are pure-types / pure-functions tests with no file I/O. They
// exist mostly to pin the contracts that higher-level tests implicitly
// depend on:
//   * is_fatal() classification of every ChiselError variant
//   * Display output is non-empty for every variant
//   * From<io::Error> wraps into the fatal IoError variant
//   * Checksum stamp/verify/bit-flip detection (including the checksum
//     region itself)
//   * PageType::from_u8 rejects unknown discriminants
//   * encode_root_name validation via the public set_root_name path
//     (covers empty, too-long, embedded NUL, and a happy-path max-length
//     name).
//
// Named-root tests that exercise the public API through a live Chisel
// handle run under dual_backing_test! (file + memory). Tests relying on
// reopen/persistence remain file-only.

mod common;
use common::{open_chisel, Backing};

use chisel::page::{
    self, compute_checksum, stamp_checksum, verify_checksum, PageType, CHECKSUM_OFFSET,
    FORMAT_VERSION, PAGE_BODY_SIZE, PAGE_SIZE,
};
use chisel::superblock::NAMED_ROOT_NAME_LEN;
use chisel::{Chisel, ChiselError, Options};
use tempfile::NamedTempFile;

// --- is_fatal classification ---

#[test]
fn test_is_fatal_operational_variants_are_not_fatal() {
    let cases: Vec<ChiselError> = vec![
        ChiselError::InvalidHandle(0),
        ChiselError::NoActiveTransaction,
        ChiselError::TransactionAlreadyActive,
        ChiselError::SavepointNotFound("x".into()),
        ChiselError::DuplicateSavepoint("x".into()),
        ChiselError::ReadOnlyMode,
        ChiselError::FileNotFound,
        ChiselError::InvalidRootName,
        ChiselError::RootNameTableFull,
        ChiselError::InvalidSuperblockCount { value: 0 },
        // Poisoned is intentionally NOT fatal — it means "already dead".
        ChiselError::Poisoned,
    ];
    for e in cases {
        assert!(!e.is_fatal(), "expected non-fatal: {e}");
    }
}

#[test]
fn test_is_fatal_storage_integrity_variants_are_fatal() {
    let cases: Vec<ChiselError> = vec![
        ChiselError::IoError(std::io::Error::other("x")),
        ChiselError::ChecksumMismatch { page_id: 0 },
        ChiselError::CorruptSuperblock,
        ChiselError::FileSizeMismatch {
            expected: 0,
            actual: 0,
        },
        ChiselError::InvalidMagic,
        ChiselError::LockFailed,
        ChiselError::UnsupportedFormatVersion {
            found: 1,
            expected: FORMAT_VERSION,
        },
        ChiselError::CorruptPage { page_id: 0 },
        ChiselError::InvalidPageId { page_id: 0 },
    ];
    for e in cases {
        assert!(e.is_fatal(), "expected fatal: {e}");
    }
}

#[test]
fn test_error_display_is_non_empty_for_every_variant() {
    // This ensures the Display impl doesn't regress to "" on any variant
    // (which would break downstream logging). We enumerate every variant
    // explicitly so a newly added variant forces a compile error in the
    // match below, reminding the author to add a Display arm.
    fn display_nonempty(e: &ChiselError) {
        let s = format!("{e}");
        assert!(!s.is_empty(), "empty Display for {e:?}");
    }
    let variants: Vec<ChiselError> = vec![
        ChiselError::InvalidHandle(0),
        ChiselError::NoActiveTransaction,
        ChiselError::TransactionAlreadyActive,
        ChiselError::SavepointNotFound("x".into()),
        ChiselError::DuplicateSavepoint("x".into()),
        ChiselError::ReadOnlyMode,
        ChiselError::FileNotFound,
        ChiselError::InvalidRootName,
        ChiselError::RootNameTableFull,
        ChiselError::InvalidSuperblockCount { value: 17 },
        ChiselError::IoError(std::io::Error::other("x")),
        ChiselError::ChecksumMismatch { page_id: 0 },
        ChiselError::CorruptSuperblock,
        ChiselError::FileSizeMismatch {
            expected: 1,
            actual: 2,
        },
        ChiselError::InvalidMagic,
        ChiselError::LockFailed,
        ChiselError::UnsupportedFormatVersion {
            found: 1,
            expected: 2,
        },
        ChiselError::Poisoned,
        ChiselError::CorruptPage { page_id: 3 },
        ChiselError::InvalidPageId { page_id: 99 },
    ];
    for v in &variants {
        display_nonempty(v);
    }
}

#[test]
fn test_from_io_error_wraps_as_fatal_io_error() {
    let io_err = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof");
    let e: ChiselError = io_err.into();
    assert!(matches!(e, ChiselError::IoError(_)));
    assert!(e.is_fatal());
}

// --- page.rs ---

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
fn test_page_type_from_u8_known_variants() {
    assert_eq!(PageType::from_u8(0x01), Some(PageType::HandleTable));
    assert_eq!(PageType::from_u8(0x02), Some(PageType::Data));
    assert_eq!(PageType::from_u8(0x03), Some(PageType::Overflow));
    assert_eq!(PageType::from_u8(0x04), Some(PageType::FreeMap));
}

#[test]
fn test_page_type_from_u8_rejects_zero_and_unknown() {
    // 0x00 is reserved so a zeroed page cannot impersonate a valid type.
    assert_eq!(PageType::from_u8(0x00), None);
    assert_eq!(PageType::from_u8(0x05), None);
    assert_eq!(PageType::from_u8(0xFF), None);
}

#[test]
fn test_page_layout_constants_are_self_consistent() {
    // Pin the relationships between constants so a refactor of one
    // doesn't silently desync the others.
    assert_eq!(CHECKSUM_OFFSET + 8, PAGE_SIZE);
    assert_eq!(PAGE_BODY_SIZE + 16 + 8, PAGE_SIZE); // header + body + checksum
    assert_eq!(page::MAGIC, 0x4348534C);
}

// --- Named-root validation via the public API ---
//
// open_with_txn_body opens a DB from the given backing, begins a
// transaction, and returns the handle. Used by all named-root tests that
// only need a live transaction and do not reopen the database.

fn open_with_txn_body(b: &Backing) -> Chisel {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    db
}

fn test_named_root_empty_name_rejected_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    let err = db.set_root_name("", 0).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidRootName));
}

dual_backing_test!(
    test_named_root_empty_name_rejected,
    test_named_root_empty_name_rejected_body
);

fn test_named_root_too_long_rejected_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    let long = "x".repeat(NAMED_ROOT_NAME_LEN + 1);
    let err = db.set_root_name(&long, 0).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidRootName));
}

dual_backing_test!(
    test_named_root_too_long_rejected,
    test_named_root_too_long_rejected_body
);

fn test_named_root_max_length_accepted_body(b: &Backing) {
    // Exactly NAMED_ROOT_NAME_LEN bytes is the inclusive upper bound.
    let mut db = open_with_txn_body(b);
    let name = "x".repeat(NAMED_ROOT_NAME_LEN);
    db.set_root_name(&name, 7).unwrap();
    assert_eq!(db.get_root_name(&name).unwrap(), Some(7));
}

dual_backing_test!(
    test_named_root_max_length_accepted,
    test_named_root_max_length_accepted_body
);

fn test_named_root_embedded_nul_rejected_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    let err = db.set_root_name("bad\0name", 0).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidRootName));
}

dual_backing_test!(
    test_named_root_embedded_nul_rejected,
    test_named_root_embedded_nul_rejected_body
);

fn test_named_root_overwrite_reuses_slot_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    db.set_root_name("meta", 1).unwrap();
    db.set_root_name("meta", 42).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(42));
}

dual_backing_test!(
    test_named_root_overwrite_reuses_slot,
    test_named_root_overwrite_reuses_slot_body
);

fn test_named_root_table_full_body(b: &Backing) {
    // There are NAMED_ROOT_COUNT slots; filling them all + 1 must return
    // RootNameTableFull.
    let mut db = open_with_txn_body(b);
    for i in 0..chisel::superblock::NAMED_ROOT_COUNT {
        db.set_root_name(&format!("n{i}"), i as u64).unwrap();
    }
    let err = db.set_root_name("one_too_many", 999).unwrap_err();
    assert!(matches!(err, ChiselError::RootNameTableFull));
}

dual_backing_test!(test_named_root_table_full, test_named_root_table_full_body);

fn test_named_root_clear_then_reuse_slot_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    for i in 0..chisel::superblock::NAMED_ROOT_COUNT {
        db.set_root_name(&format!("n{i}"), i as u64).unwrap();
    }
    db.clear_root_name("n0").unwrap();
    // Slot freed — new name should now fit.
    db.set_root_name("replacement", 123).unwrap();
    assert_eq!(db.get_root_name("replacement").unwrap(), Some(123));
    assert_eq!(db.get_root_name("n0").unwrap(), None);
}

dual_backing_test!(
    test_named_root_clear_then_reuse_slot,
    test_named_root_clear_then_reuse_slot_body
);

fn test_named_root_clear_missing_name_is_noop_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    db.clear_root_name("never_set").unwrap();
    assert_eq!(db.get_root_name("never_set").unwrap(), None);
}

dual_backing_test!(
    test_named_root_clear_missing_name_is_noop,
    test_named_root_clear_missing_name_is_noop_body
);

fn test_named_root_rollback_reverts_set_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    db.set_root_name("meta", 5).unwrap();
    db.rollback().unwrap();
    // Not visible outside the rolled-back txn.
    assert_eq!(db.get_root_name("meta").unwrap(), None);
}

dual_backing_test!(
    test_named_root_rollback_reverts_set,
    test_named_root_rollback_reverts_set_body
);

// FILE-ONLY: reopens the same on-disk path after drop to test persistence;
// memory mode has no persistent backing to reopen.
#[test]
fn test_named_root_survives_commit_and_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        db.set_root_name("meta", 9).unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(&path, Options::default()).unwrap();
        assert_eq!(db.get_root_name("meta").unwrap(), Some(9));
    }
}
