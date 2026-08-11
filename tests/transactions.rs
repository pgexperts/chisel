mod common;
use chisel::ChiselError;
use common::{open_chisel, Backing};

fn test_begin_allocate_commit_read_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"hello world").unwrap();
    db.commit().unwrap();
    let data = db.read(handle).unwrap();
    assert_eq!(data, b"hello world");
}

dual_backing_test!(
    test_begin_allocate_commit_read,
    test_begin_allocate_commit_read_body
);

fn test_rollback_discards_changes_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"doomed").unwrap();
    db.rollback().unwrap();
    assert!(
        matches!(db.read(handle), Err(ChiselError::InvalidHandle(_))),
        "rolled-back handle must be InvalidHandle"
    );
}

dual_backing_test!(
    test_rollback_discards_changes,
    test_rollback_discards_changes_body
);

fn test_update_preserves_handle_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"original").unwrap();
    db.commit().unwrap();
    db.begin().unwrap();
    db.update(handle, b"updated value").unwrap();
    db.commit().unwrap();
    let data = db.read(handle).unwrap();
    assert_eq!(data, b"updated value");
}

dual_backing_test!(
    test_update_preserves_handle,
    test_update_preserves_handle_body
);

fn test_delete_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handle = db.allocate(b"gone soon").unwrap();
    db.commit().unwrap();
    db.begin().unwrap();
    db.delete(handle).unwrap();
    db.commit().unwrap();
    assert!(
        matches!(db.read(handle), Err(ChiselError::InvalidHandle(_))),
        "deleted handle must be InvalidHandle"
    );
}

dual_backing_test!(test_delete, test_delete_body);

fn test_savepoint_rollback_to_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"kept").unwrap();
    db.savepoint("alpha").unwrap();
    let h2 = db.allocate(b"discarded").unwrap();
    db.rollback_to("alpha").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"kept");
    assert!(
        matches!(db.read(h2), Err(ChiselError::InvalidHandle(_))),
        "handle rolled back via savepoint must be InvalidHandle"
    );
}

dual_backing_test!(test_savepoint_rollback_to, test_savepoint_rollback_to_body);

fn test_savepoint_release_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"first").unwrap();
    db.savepoint("alpha").unwrap();
    let h2 = db.allocate(b"second").unwrap();
    db.release("alpha").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"first");
    assert_eq!(db.read(h2).unwrap(), b"second");
}

dual_backing_test!(test_savepoint_release, test_savepoint_release_body);

fn test_savepoint_rollback_preserves_savepoint_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    db.savepoint("retry").unwrap();
    let _h1 = db.allocate(b"attempt 1").unwrap();
    db.rollback_to("retry").unwrap();
    let h2 = db.allocate(b"attempt 2").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h2).unwrap(), b"attempt 2");
}

dual_backing_test!(
    test_savepoint_rollback_preserves_savepoint,
    test_savepoint_rollback_preserves_savepoint_body
);

fn test_nested_savepoints_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"base").unwrap();
    db.savepoint("alpha").unwrap();
    let h2 = db.allocate(b"in alpha").unwrap();
    db.savepoint("beta").unwrap();
    let h3 = db.allocate(b"in beta").unwrap();
    db.rollback_to("alpha").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"base");
    assert!(
        matches!(db.read(h2), Err(ChiselError::InvalidHandle(_))),
        "h2 rolled back via alpha savepoint must be InvalidHandle"
    );
    assert!(
        matches!(db.read(h3), Err(ChiselError::InvalidHandle(_))),
        "h3 rolled back via alpha savepoint must be InvalidHandle"
    );
}

dual_backing_test!(test_nested_savepoints, test_nested_savepoints_body);

// Note: test_reopen_preserves_data (which exercised TransactionManager /
// PageCache / PageIo directly to verify the open_existing path)
// migrated 2026-05-22 to src/transaction.rs as `reopen_preserves_committed_data`
// under the I35 pub→pub(crate) reshape. The Chisel-public-API equivalent
// (open path + commit + reopen + read) is covered by `test_chisel_reopen`
// in tests/basic_ops.rs.

// A handle's "never reused" guarantee covers COMMITTED handles only. The
// counter that mints them (`next_handle`) lives in the roots snapshot, so
// every rewind path restores it and the id is handed out again — for
// different bytes, with nothing in the entry to distinguish the two.
//
// This is correct behaviour (a rolled-back allocate never happened), but it
// is the exact thing ARCHITECTURE.md's "Handle stability" section and
// `Chisel::allocate`'s rustdoc now warn callers about, so it is pinned here.
// The crash-recovery arm of the same property is pinned by
// `src/recovery_tests.rs`; this covers the two in-process rewind paths.
fn test_uncommitted_handle_is_reminted_body(b: &Backing) {
    let mut db = open_chisel(b);

    // Rewind path 1: rollback.
    db.begin().unwrap();
    let h = db.allocate(b"rolled-back").unwrap();
    db.rollback().unwrap();

    db.begin().unwrap();
    let h2 = db.allocate(b"different-bytes").unwrap();
    db.commit().unwrap();

    assert_eq!(
        h, h2,
        "next_handle rewinds with the roots snapshot, so a rolled-back id is re-minted"
    );
    assert_eq!(
        db.read(h).unwrap(),
        b"different-bytes",
        "the re-minted handle resolves to the NEW value — a caller holding the old \
         id reads unrelated data with no error"
    );

    // Rewind path 2: rollback_to a savepoint.
    db.begin().unwrap();
    db.savepoint("sp").unwrap();
    let h3 = db.allocate(b"inside-savepoint").unwrap();
    db.rollback_to("sp").unwrap();
    let h4 = db.allocate(b"after-rollback-to").unwrap();
    db.commit().unwrap();

    assert_eq!(
        h3, h4,
        "rollback_to restores the savepoint's roots, next_handle included"
    );
    assert_eq!(db.read(h3).unwrap(), b"after-rollback-to");
}

dual_backing_test!(
    test_uncommitted_handle_is_reminted,
    test_uncommitted_handle_is_reminted_body
);

// TRIPWIRE — not a regression test. This passes on main today and must keep
// passing; it exists to FAIL if anyone gives the handle table a
// `session_owned`-style COW dedup (HANDLES-INDEX-2, issue #112) without
// handling savepoints.
//
// The hazard it pins: `rollback_to` cannot undo an IN-PLACE write.
// `savepoint` captures `watermark = cache.next_page_id()` plus a clone of the
// current roots; `rollback_to` does `cache.truncate(watermark)` and restores
// those roots. Truncate drops only entries with `id >= watermark`, so a page
// allocated BEFORE the savepoint keeps whatever was written into it AFTER.
//
// With a naive dedup, the second allocate below would recognise the
// handle-table root as one this transaction already COW'd (page id below the
// watermark) and write the new entry straight into it. Restoring the roots
// then restores the same page id, so the entry survives: `handles()` reports
// two, and a `commit()` here would make the phantom durable, pointing at a
// data slot whose live-slot count `packer.restore` just rewound.
//
// `next_handle` also rewinds, so the NEXT allocate would overwrite the
// phantom slot and mask the bug — which is why this asserts on `handles()`
// immediately after `rollback_to` and allocates nothing further.
fn rollback_to_savepoint_undoes_a_later_allocate_in_the_same_transaction_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let kept = db.allocate(b"before savepoint").unwrap();
    db.savepoint("sp").unwrap();
    let discarded = db.allocate(b"after savepoint").unwrap();
    db.rollback_to("sp").unwrap();

    let live = db.handles().unwrap();
    assert_eq!(
        live.len(),
        1,
        "rollback_to must leave exactly the pre-savepoint handle in the table; got {live:?}"
    );
    assert_eq!(live[0], kept, "the surviving handle must be the kept one");
    assert!(
        matches!(db.read(discarded), Err(ChiselError::InvalidHandle(_))),
        "the post-savepoint handle must not resolve"
    );

    // ...and the phantom must not be reachable through a commit either.
    db.commit().unwrap();
    assert_eq!(db.handles().unwrap().len(), 1);
    assert_eq!(db.read(kept).unwrap(), b"before savepoint");
}

dual_backing_test!(
    rollback_to_savepoint_undoes_a_later_allocate_in_the_same_transaction,
    rollback_to_savepoint_undoes_a_later_allocate_in_the_same_transaction_body
);
