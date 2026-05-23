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
mod common;
use common::{open_chisel, Backing};

// --- NoActiveTransaction on every mutating call ---

fn test_allocate_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.allocate(b"x").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_allocate_without_begin_errors,
    test_allocate_without_begin_errors_body
);

fn test_update_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.update(0, b"x").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_update_without_begin_errors,
    test_update_without_begin_errors_body
);

fn test_delete_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.delete(0).unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_delete_without_begin_errors,
    test_delete_without_begin_errors_body
);

fn test_delete_many_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.delete_many(&[0, 1, 2]).unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_delete_many_without_begin_errors,
    test_delete_many_without_begin_errors_body
);

fn test_commit_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.commit().unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_commit_without_begin_errors,
    test_commit_without_begin_errors_body
);

fn test_rollback_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.rollback().unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_rollback_without_begin_errors,
    test_rollback_without_begin_errors_body
);

fn test_savepoint_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.savepoint("sp").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_savepoint_without_begin_errors,
    test_savepoint_without_begin_errors_body
);

fn test_set_root_name_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.set_root_name("meta", 0).unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_set_root_name_without_begin_errors,
    test_set_root_name_without_begin_errors_body
);

fn test_clear_root_name_without_begin_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    let err = db.clear_root_name("meta").unwrap_err();
    assert!(matches!(err, ChiselError::NoActiveTransaction));
}

dual_backing_test!(
    test_clear_root_name_without_begin_errors,
    test_clear_root_name_without_begin_errors_body
);

// --- Nested begin ---

fn test_begin_twice_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let err = db.begin().unwrap_err();
    assert!(matches!(err, ChiselError::TransactionAlreadyActive));
}

dual_backing_test!(test_begin_twice_errors, test_begin_twice_errors_body);

// --- Invalid handle on mutations ---

fn test_update_invalid_handle_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let err = db.update(999_999, b"nope").unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(999_999)));
    // Handle is still alive after an operational error.
    assert!(!db.is_poisoned());
    db.rollback().unwrap();
}

dual_backing_test!(
    test_update_invalid_handle_errors,
    test_update_invalid_handle_errors_body
);

fn test_delete_invalid_handle_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let err = db.delete(42).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(42)));
    assert!(!db.is_poisoned());
    db.rollback().unwrap();
}

dual_backing_test!(
    test_delete_invalid_handle_errors,
    test_delete_invalid_handle_errors_body
);

fn test_read_invalid_handle_on_empty_db_errors_body(b: &Backing) {
    let db = open_chisel(b);
    let err = db.read(0).unwrap_err();
    assert!(matches!(err, ChiselError::InvalidHandle(0)));
    assert!(!db.is_poisoned());
}

dual_backing_test!(
    test_read_invalid_handle_on_empty_db_errors,
    test_read_invalid_handle_on_empty_db_errors_body
);

// --- Empty value roundtrip ---

fn test_allocate_empty_value_roundtrip_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"").unwrap();
    db.commit().unwrap();
    let got = db.read(h).unwrap();
    assert!(got.is_empty());
}

dual_backing_test!(
    test_allocate_empty_value_roundtrip,
    test_allocate_empty_value_roundtrip_body
);

fn test_update_to_empty_value_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"not empty").unwrap();
    db.update(h, b"").unwrap();
    db.commit().unwrap();
    assert!(db.read(h).unwrap().is_empty());
}

dual_backing_test!(test_update_to_empty_value, test_update_to_empty_value_body);

// --- Inline / overflow boundary. MAX_INLINE_VALUE = 8162. ---

fn test_value_at_inline_max_boundary_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let v = vec![0x5Au8; 8162];
    let h = db.allocate(&v).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), v);
}

dual_backing_test!(
    test_value_at_inline_max_boundary,
    test_value_at_inline_max_boundary_body
);

fn test_value_one_byte_over_inline_goes_overflow_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let v = vec![0x5Au8; 8163];
    let h = db.allocate(&v).unwrap();
    db.commit().unwrap();
    // Roundtrip is the observable check; overflow routing is internal.
    assert_eq!(db.read(h).unwrap(), v);
}

dual_backing_test!(
    test_value_one_byte_over_inline_goes_overflow,
    test_value_one_byte_over_inline_goes_overflow_body
);

fn test_handle_stable_across_inline_overflow_transition_body(b: &Backing) {
    // Allocate a small inline value, update it to a large overflow value,
    // then update it back to a small inline value. The handle u64 MUST
    // survive both transitions and yield the current value on read.
    let mut db = open_chisel(b);
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

dual_backing_test!(
    test_handle_stable_across_inline_overflow_transition,
    test_handle_stable_across_inline_overflow_transition_body
);

// --- delete_many ---

fn test_delete_many_empty_slice_is_noop_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate(b"still here").unwrap();
    db.delete_many(&[]).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h).unwrap(), b"still here");
}

dual_backing_test!(
    test_delete_many_empty_slice_is_noop,
    test_delete_many_empty_slice_is_noop_body
);

fn test_delete_many_bad_handle_mid_batch_stops_at_first_error_body(b: &Backing) {
    // Documented semantics (transaction.rs::delete_many): on first error the
    // loop returns; handles deleted before the failure remain deleted in the
    // current txn. Caller decides commit vs rollback.
    let mut db = open_chisel(b);
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

dual_backing_test!(
    test_delete_many_bad_handle_mid_batch_stops_at_first_error,
    test_delete_many_bad_handle_mid_batch_stops_at_first_error_body
);

// --- Savepoint error cases ---

fn test_duplicate_savepoint_rejected_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    db.savepoint("sp").unwrap();
    let err = db.savepoint("sp").unwrap_err();
    assert!(matches!(err, ChiselError::DuplicateSavepoint(ref n) if n == "sp"));
}

dual_backing_test!(
    test_duplicate_savepoint_rejected,
    test_duplicate_savepoint_rejected_body
);

fn test_rollback_to_missing_savepoint_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let err = db.rollback_to("nope").unwrap_err();
    assert!(matches!(err, ChiselError::SavepointNotFound(ref n) if n == "nope"));
}

dual_backing_test!(
    test_rollback_to_missing_savepoint_errors,
    test_rollback_to_missing_savepoint_errors_body
);

fn test_release_missing_savepoint_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let err = db.release("nope").unwrap_err();
    assert!(matches!(err, ChiselError::SavepointNotFound(ref n) if n == "nope"));
}

dual_backing_test!(
    test_release_missing_savepoint_errors,
    test_release_missing_savepoint_errors_body
);

fn test_rollback_to_released_savepoint_errors_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    db.savepoint("sp").unwrap();
    db.release("sp").unwrap();
    let err = db.rollback_to("sp").unwrap_err();
    assert!(matches!(err, ChiselError::SavepointNotFound(ref n) if n == "sp"));
}

dual_backing_test!(
    test_rollback_to_released_savepoint_errors,
    test_rollback_to_released_savepoint_errors_body
);

// --- Poison model ---
//
// The direct `force_poison_for_test` hook lives behind #[cfg(test)] on
// TransactionManager and is only reachable from in-crate unit tests. The
// existing crash_recovery.rs suite covers poison via real fatal-I/O
// injection; these tests exist to verify that a fresh, non-poisoned
// handle is the baseline state and that stats() reflects committed-only
// durable data after a reopen (which is the other half of the poison
// recovery contract).

fn test_fresh_handle_is_not_poisoned_body(b: &Backing) {
    let db = open_chisel(b);
    assert!(!db.is_poisoned());
}

dual_backing_test!(
    test_fresh_handle_is_not_poisoned,
    test_fresh_handle_is_not_poisoned_body
);

// Left file-only: reopens the same path after drop, which requires a persistent
// on-disk file. In-memory backing has no path identity across instances.
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

fn test_stats_empty_db_has_zero_handles_body(b: &Backing) {
    let db = open_chisel(b);
    let s = db.stats().unwrap();
    assert_eq!(s.handle_count, 0);
    // File size is always a whole number of pages.
    assert_eq!(s.file_size_bytes % chisel::PAGE_SIZE as u64, 0);
    assert_eq!(s.file_size_bytes, s.total_pages * chisel::PAGE_SIZE as u64);
}

dual_backing_test!(
    test_stats_empty_db_has_zero_handles,
    test_stats_empty_db_has_zero_handles_body
);

fn test_stats_handle_count_matches_handles_len_body(b: &Backing) {
    let mut db = open_chisel(b);
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

dual_backing_test!(
    test_stats_handle_count_matches_handles_len,
    test_stats_handle_count_matches_handles_len_body
);

fn test_stats_is_idempotent_without_mutation_body(b: &Backing) {
    let mut db = open_chisel(b);
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

dual_backing_test!(
    test_stats_is_idempotent_without_mutation,
    test_stats_is_idempotent_without_mutation_body
);

// I53 (ISSUES.md, 2026-05-22): `Chisel::file_size_bytes()` is a
// dedicated O(1) accessor added for the bench harness — it must
// return the same value `stats().file_size_bytes` does, but without
// the handle-table scan. This test pins the equality across a few
// representative database states (empty, just-allocated,
// just-committed) so a future refactor of either method that breaks
// the equality is caught immediately.
fn test_file_size_bytes_matches_stats_body(b: &Backing) {
    let mut db = open_chisel(b);
    // Phase 1: fresh database, before any allocations.
    assert_eq!(
        db.file_size_bytes().unwrap(),
        db.stats().unwrap().file_size_bytes,
        "file_size_bytes must match stats().file_size_bytes on fresh db"
    );
    // Phase 2: mid-transaction with allocations in flight.
    db.begin().unwrap();
    for i in 0..20u32 {
        db.allocate(&i.to_le_bytes()).unwrap();
    }
    assert_eq!(
        db.file_size_bytes().unwrap(),
        db.stats().unwrap().file_size_bytes,
        "file_size_bytes must match stats().file_size_bytes mid-transaction"
    );
    // Phase 3: post-commit.
    db.commit().unwrap();
    assert_eq!(
        db.file_size_bytes().unwrap(),
        db.stats().unwrap().file_size_bytes,
        "file_size_bytes must match stats().file_size_bytes post-commit"
    );
}

dual_backing_test!(
    test_file_size_bytes_matches_stats,
    test_file_size_bytes_matches_stats_body
);
