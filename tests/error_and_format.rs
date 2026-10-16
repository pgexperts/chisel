// error_and_format.rs — Public-API coverage for src/error.rs and the
// named-root table in src/transaction/named_roots.rs.
//
// They exist mostly to pin the contracts that higher-level tests
// implicitly depend on:
//   * is_fatal() and Display smoke tests over a sample of ChiselError
//     variants, exercised through the public surface. These lists are
//     NOT exhaustive and cannot be made so here: ChiselError is
//     #[non_exhaustive], so an integration test cannot write an
//     exhaustive match over it. The every-variant guard lives in-crate,
//     in src/error.rs: `is_fatal_matches_documented_classification_for_every_variant`
//     and `variant_table_holds_each_variant_once_with_nonempty_display`.
//   * From<io::Error> wraps into the fatal IoError variant
//   * named-root behaviour through a live handle: encode_root_name
//     validation via set_root_name (empty, too-long, embedded NUL, and a
//     happy-path max-length name), table-full and slot reuse, clear,
//     transactional revert on rollback and rollback_to, and persistence
//     across reopen.
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

// --- is_fatal classification (smoke tests; see header for the exhaustive guard) ---

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
fn test_error_display_is_non_empty_for_sampled_variants() {
    // Smoke test that Display doesn't render "" (which would break
    // downstream logging) for a sample of variants, through the public
    // API. The list is hand-maintained and does not track new variants;
    // every-variant coverage is the in-crate
    // `variant_table_holds_each_variant_once_with_nonempty_display`.
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
    // Rebinding an EXISTING name must still succeed on a full table: it
    // reuses that name's slot rather than claiming a new one.
    db.set_root_name("n3", chisel::Handle::from(999)).unwrap();
    assert_eq!(
        db.get_root_name("n3").unwrap(),
        Some(chisel::Handle::from(999))
    );
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
    // A name that WAS bound becomes missing once cleared, so clearing it
    // again is also a no-op rather than an error (clear is idempotent).
    db.set_root_name("meta", chisel::Handle::from(7)).unwrap();
    db.clear_root_name("meta").unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), None);
    db.clear_root_name("meta").unwrap();
    db.commit().unwrap();
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

    // Rollback must also RESTORE a committed binding, not merely drop a
    // new one: rebinding a committed name and rolling back brings the
    // committed handle back.
    db.begin().unwrap();
    let h1 = db.allocate(b"initial").unwrap();
    db.set_root_name("meta", h1).unwrap();
    db.commit().unwrap();

    db.begin().unwrap();
    let h2 = db.allocate(b"replacement").unwrap();
    db.set_root_name("meta", h2).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h2));
    db.rollback().unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h1));
}

dual_backing_test!(
    test_named_root_rollback_reverts_set,
    test_named_root_rollback_reverts_set_body
);

fn test_named_root_rollback_to_savepoint_reverts_body(b: &Backing) {
    // Savepoints must snapshot and restore named roots as well as values.
    // Today that falls out of Savepoint capturing a whole Roots clone with
    // named_roots inside it; this guards against a refactor of Roots that
    // leaves named_roots out of the snapshot. Both a rebind ("meta") and a
    // new binding ("index") made after the savepoint must revert.
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"a").unwrap();
    db.set_root_name("meta", h1).unwrap();
    db.savepoint("sp").unwrap();

    let h2 = db.allocate(b"b").unwrap();
    db.set_root_name("meta", h2).unwrap();
    db.set_root_name("index", h2).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h2));
    assert_eq!(db.get_root_name("index").unwrap(), Some(h2));

    db.rollback_to("sp").unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h1));
    assert_eq!(db.get_root_name("index").unwrap(), None);

    // The reverted state is what commits. "index" also released its slot:
    // the remaining NAMED_ROOT_COUNT - 1 slots (all but "meta") are free.
    db.commit().unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h1));
    assert_eq!(db.get_root_name("index").unwrap(), None);
    db.begin().unwrap();
    for i in 0..NAMED_ROOT_COUNT - 1 {
        db.set_root_name(&format!("n{i}"), chisel::Handle::from(i as u64))
            .unwrap();
    }
    assert!(matches!(
        db.set_root_name("one_too_many", chisel::Handle::from(0)),
        Err(ChiselError::RootNameTableFull)
    ));
}

dual_backing_test!(
    test_named_root_rollback_to_savepoint_reverts,
    test_named_root_rollback_to_savepoint_reverts_body
);

// FILE-ONLY: reopens the same on-disk path after drop to test persistence;
// memory mode has no persistent backing to reopen.
#[test]
fn test_named_root_survives_commit_and_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    // Bind the name to a real allocated value so the reopen check covers
    // the full use case: the name resolves AND the handle it resolves to
    // still reads back the committed bytes.
    let meta_handle;
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        meta_handle = db.allocate(b"meta btree root state").unwrap();
        db.set_root_name("meta", meta_handle).unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(&path, Options::default()).unwrap();
        assert_eq!(db.get_root_name("meta").unwrap(), Some(meta_handle));
        assert_eq!(db.read(meta_handle).unwrap(), b"meta btree root state");
        // An unbound name is Ok(None) after reopen, not an error.
        assert_eq!(db.get_root_name("does_not_exist").unwrap(), None);
    }
}
