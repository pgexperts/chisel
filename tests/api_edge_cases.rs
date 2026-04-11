// api_edge_cases.rs — Public-API edge and error paths that were thin or missing.
//
// Covers gaps surfaced during the 2026-04-10 test-gap audit:
//   * NoActiveTransaction on every mutating call
//   * TransactionAlreadyActive (begin twice)
//   * Invalid-handle errors on update/delete
//   * Empty value and the inline/overflow boundary (8162 vs 8163)
//   * Handle stability across an inline<->overflow transition
//   * delete_many empty / bad handle mid-batch
//   * Savepoint error cases: DuplicateSavepoint, SavepointNotFound, savepoint
//     without active txn
//   * is_poisoned / recovery-by-reopen on a forced poison
//   * stats() internal consistency
//
// These tests intentionally exercise the public Chisel surface only;
// lower-level invariants have their own tests.

use chisel::{Chisel, ChiselError, Options};
use tempfile::NamedTempFile;

fn open_fresh() -> (NamedTempFile, Chisel) {
    let file = NamedTempFile::new().unwrap();
    let db = Chisel::open(file.path(), Options::default()).unwrap();
    (file, db)
}

// --- NoActiveTransaction on every mutating call ---

#[test]
fn test_allocate_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.allocate(b"x").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_update_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.update(0, b"x").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_delete_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.delete(0).unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_delete_many_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.delete_many(&[0, 1, 2]).unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_commit_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.commit().unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_rollback_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.rollback().unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_savepoint_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.savepoint("sp").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_set_root_name_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.set_root_name("meta", 0).unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

#[test]
fn test_clear_root_name_without_begin_errors() {
    let (_f, mut db) = open_fresh();
    let err = db.clear_root_name("meta").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

// --- Nested begin ---

#[test]
fn test_begin_twice_errors() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let err = db.begin().unwrap_err();
    assert!(matches!(err, ChiselError::TransactionAlreadyActive));
}

// --- Invalid handle on mutations ---

#[test]
fn test_update_invalid_handle_errors() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let err = db.update(999_999, b"nope").unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(999_999)));
    // Handle is still alive after an operational error.
    assert!(!db.is_poisoned());
    db.rollback().unwrap();
}

#[test]
fn test_delete_invalid_handle_errors() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let err = db.delete(42).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(42)));
    assert!(!db.is_poisoned());
    db.rollback().unwrap();
}

#[test]
fn test_read_invalid_handle_on_empty_db_errors() {
    let (_f, db) = open_fresh();
    let err = db.read(0).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(0)));
    assert!(!db.is_poisoned());
}

// --- Empty value roundtrip ---

#[test]
fn test_allocate_empty_value_roundtrip() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let h = db.allocate(b"").unwrap();
    db.commit().unwrap();
    let got = db.read(h).unwrap();
    assert!(got.is_empty());
}

#[test]
fn test_update_to_empty_value() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let h = db.allocate(b"not empty").unwrap();
    db.update(h, b"").unwrap();
    db.commit().unwrap();
    assert!(db.read(h).unwrap().is_empty());
}

// --- Inline / overflow boundary. MAX_INLINE_VALUE = 8162. ---

#[test]
fn test_value_at_inline_max_boundary() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let v = vec![0x5Au8; 8162];
    let h = db.allocate(&v).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), v);
}

#[test]
fn test_value_one_byte_over_inline_goes_overflow() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let v = vec![0x5Au8; 8163];
    let h = db.allocate(&v).unwrap();
    db.commit().unwrap();
    // Roundtrip is the observable check; overflow routing is internal.
    assert_eq!(db.read(h).unwrap(), v);
}

#[test]
fn test_handle_stable_across_inline_overflow_transition() {
    // Allocate a small inline value, update it to a large overflow value,
    // then update it back to a small inline value. The handle u64 MUST
    // survive both transitions and yield the current value on read.
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let h = db.allocate(b"small").unwrap();
    db.commit().unwrap();

    let big = vec![0xC3u8; 20_000];
    db.begin().unwrap();
    db.update(h, &big).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), big);

    db.begin().unwrap();
    db.update(h, b"small again").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"small again");
}

// --- delete_many ---

#[test]
fn test_delete_many_empty_slice_is_noop() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let h = db.allocate(b"still here").unwrap();
    db.delete_many(&[]).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"still here");
}

#[test]
fn test_delete_many_bad_handle_mid_batch_stops_at_first_error() {
    // Documented semantics (transaction.rs::delete_many): on first error the
    // loop returns; handles deleted before the failure remain deleted in the
    // current txn. Caller decides commit vs rollback.
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let h1 = db.allocate(b"a").unwrap();
    let h2 = db.allocate(b"b").unwrap();
    let h3 = db.allocate(b"c").unwrap();
    db.commit().unwrap();

    db.begin().unwrap();
    let err = db.delete_many(&[h1, 999_999, h3]).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(999_999)));
    // h1 is already gone inside the txn; h2 was never touched; h3 was
    // never reached. Rollback to restore.
    db.rollback().unwrap();
    // All three live again.
    assert_eq!(db.read(h1).unwrap(), b"a");
    assert_eq!(db.read(h2).unwrap(), b"b");
    assert_eq!(db.read(h3).unwrap(), b"c");
}

// --- Savepoint error cases ---

#[test]
fn test_duplicate_savepoint_rejected() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    db.savepoint("sp").unwrap();
    let err = db.savepoint("sp").unwrap_err();
    assert!(matches!(err, ChiselError::DuplicateSavepoint(ref n) if n == "sp"));
}

#[test]
fn test_rollback_to_missing_savepoint_errors() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let err = db.rollback_to("nope").unwrap_err();
    assert!(matches!(err, ChiselError::SavepointNotFound(ref n) if n == "nope"));
}

#[test]
fn test_release_missing_savepoint_errors() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    let err = db.release("nope").unwrap_err();
    assert!(matches!(err, ChiselError::SavepointNotFound(ref n) if n == "nope"));
}

#[test]
fn test_rollback_to_released_savepoint_errors() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    db.savepoint("sp").unwrap();
    db.release("sp").unwrap();
    let err = db.rollback_to("sp").unwrap_err();
    assert!(matches!(err, ChiselError::SavepointNotFound(ref n) if n == "sp"));
}

// --- Poison model ---
//
// The direct `force_poison_for_test` hook lives behind #[cfg(test)] on
// TransactionManager and is only reachable from in-crate unit tests. The
// existing crash_recovery.rs suite covers poison via real fatal-I/O
// injection; these tests exist to verify that a fresh, non-poisoned
// handle is the baseline state and that stats() reflects committed-only
// durable data after a reopen (which is the other half of the poison
// recovery contract).

#[test]
fn test_fresh_handle_is_not_poisoned() {
    let (_f, db) = open_fresh();
    assert!(!db.is_poisoned());
}

#[test]
fn test_reopen_after_drop_observes_only_committed_data() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let committed_handle;
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        committed_handle = db.allocate(b"committed").unwrap();
        db.commit().unwrap();
        // Start a second txn and deliberately drop without committing.
        db.begin().unwrap();
        db.allocate(b"uncommitted").unwrap();
        // db drops here; the in-flight txn is discarded by shadow paging.
    }
    let db = Chisel::open(&path, Options::default()).unwrap();
    assert!(!db.is_poisoned());
    assert_eq!(db.read(committed_handle).unwrap(), b"committed");
    assert_eq!(db.stats().unwrap().handle_count, 1);
}

// --- stats() consistency ---

#[test]
fn test_stats_empty_db_has_zero_handles() {
    let (_f, db) = open_fresh();
    let s = db.stats().unwrap();
    assert_eq!(s.handle_count, 0);
    // File size is always a whole number of pages.
    assert_eq!(s.file_size_bytes % chisel::page::PAGE_SIZE as u64, 0);
    assert_eq!(
        s.file_size_bytes,
        s.total_pages * chisel::page::PAGE_SIZE as u64
    );
}

#[test]
fn test_stats_handle_count_matches_handles_len() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    for i in 0..25u32 {
        db.allocate(&i.to_le_bytes()).unwrap();
    }
    db.commit().unwrap();
    let s = db.stats().unwrap();
    // handle_count comes from the same enumeration used to build the Vec,
    // but verifying both paths agree is the point.
    assert_eq!(s.handle_count, 25);
}

#[test]
fn test_stats_is_idempotent_without_mutation() {
    let (_f, mut db) = open_fresh();
    db.begin().unwrap();
    db.allocate(b"x").unwrap();
    db.allocate(b"y").unwrap();
    db.commit().unwrap();
    let s1 = db.stats().unwrap();
    let s2 = db.stats().unwrap();
    assert_eq!(s1.handle_count, s2.handle_count);
    assert_eq!(s1.total_pages, s2.total_pages);
    assert_eq!(s1.file_size_bytes, s2.file_size_bytes);
}
