// error_and_format.rs — Unit coverage for error.rs and the named-root
// validation path in transaction.rs.
//
// These are pure-types / pure-functions tests with no file I/O. They
// exist mostly to pin the contracts that higher-level tests implicitly
// depend on:
//   * is_fatal() classification of every ChiselError variant
//   * Display output is non-empty for every variant
//   * From<io::Error> wraps into the fatal IoError variant
//   * encode_root_name validation via the public set_root_name path
//     (covers empty, too-long, embedded NUL, and a happy-path max-length
//     name).
//
// Page-module tests (checksum, layout constants) moved into src/page.rs's
// test mod on 2026-05-22 as part of the I35 pub→pub(crate) reshape; they
// no longer needed the crate-public `chisel::page::*` re-exports.
//
// Named-root tests that exercise the public API through a live Chisel
// handle run under dual_backing_test! (file + memory). Tests relying on
// reopen/persistence remain file-only.

mod common;
use common::{open_chisel, Backing};

use chisel::{Chisel, ChiselError, Options, NAMED_ROOT_COUNT, NAMED_ROOT_NAME_LEN};
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
        ChiselError::CorruptSuperblock { defects: vec![] },
        ChiselError::FileSizeMismatch {
            expected: 0,
            actual: 0,
        },
        ChiselError::LockFailed,
        ChiselError::UnsupportedFormatVersion {
            // Arbitrary values: the test only checks that is_fatal()
            // classifies UnsupportedFormatVersion as fatal, not that
            // either field has a specific value. Using literals here
            // avoids reaching for the now-pub(crate) FORMAT_VERSION
            // constant (I35 reshape).
            found: 1,
            expected: 0x0001_0000,
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
        ChiselError::CorruptSuperblock { defects: vec![] },
        ChiselError::FileSizeMismatch {
            expected: 1,
            actual: 2,
        },
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
    let err = db.set_root_name("", chisel::Handle::from(0)).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidRootName));
}

dual_backing_test!(
    test_named_root_empty_name_rejected,
    test_named_root_empty_name_rejected_body
);

fn test_named_root_too_long_rejected_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    let long = "x".repeat(NAMED_ROOT_NAME_LEN + 1);
    let err = db
        .set_root_name(&long, chisel::Handle::from(0))
        .unwrap_err();
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
    db.set_root_name(&name, chisel::Handle::from(7)).unwrap();
    assert_eq!(
        db.get_root_name(&name).unwrap(),
        Some(chisel::Handle::from(7))
    );
}

dual_backing_test!(
    test_named_root_max_length_accepted,
    test_named_root_max_length_accepted_body
);

fn test_named_root_embedded_nul_rejected_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    let err = db
        .set_root_name("bad\0name", chisel::Handle::from(0))
        .unwrap_err();
    assert!(matches!(err, ChiselError::InvalidRootName));
}

dual_backing_test!(
    test_named_root_embedded_nul_rejected,
    test_named_root_embedded_nul_rejected_body
);

fn test_named_root_overwrite_reuses_slot_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    db.set_root_name("meta", chisel::Handle::from(1)).unwrap();
    db.set_root_name("meta", chisel::Handle::from(42)).unwrap();
    assert_eq!(
        db.get_root_name("meta").unwrap(),
        Some(chisel::Handle::from(42))
    );
}

dual_backing_test!(
    test_named_root_overwrite_reuses_slot,
    test_named_root_overwrite_reuses_slot_body
);

fn test_named_root_table_full_body(b: &Backing) {
    // There are NAMED_ROOT_COUNT slots; filling them all + 1 must return
    // RootNameTableFull.
    let mut db = open_with_txn_body(b);
    for i in 0..NAMED_ROOT_COUNT {
        db.set_root_name(&format!("n{i}"), chisel::Handle::from(i as u64))
            .unwrap();
    }
    let err = db
        .set_root_name("one_too_many", chisel::Handle::from(999))
        .unwrap_err();
    assert!(matches!(err, ChiselError::RootNameTableFull));
}

dual_backing_test!(test_named_root_table_full, test_named_root_table_full_body);

fn test_named_root_clear_then_reuse_slot_body(b: &Backing) {
    let mut db = open_with_txn_body(b);
    for i in 0..NAMED_ROOT_COUNT {
        db.set_root_name(&format!("n{i}"), chisel::Handle::from(i as u64))
            .unwrap();
    }
    db.clear_root_name("n0").unwrap();
    // Slot freed — new name should now fit.
    db.set_root_name("replacement", chisel::Handle::from(123))
        .unwrap();
    assert_eq!(
        db.get_root_name("replacement").unwrap(),
        Some(chisel::Handle::from(123))
    );
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
    db.set_root_name("meta", chisel::Handle::from(5)).unwrap();
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
        db.set_root_name("meta", chisel::Handle::from(9)).unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(&path, Options::default()).unwrap();
        assert_eq!(
            db.get_root_name("meta").unwrap(),
            Some(chisel::Handle::from(9))
        );
    }
}
