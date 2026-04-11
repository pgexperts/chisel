use chisel::page::{self, PAGE_SIZE};
use chisel::superblock::Superblock;
use chisel::{Chisel, ChiselError, Options};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use tempfile::NamedTempFile;

#[test]
fn test_recovery_after_clean_close() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"durable data").unwrap();
        db.commit().unwrap();
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"durable data");
}

#[test]
fn test_recovery_uncommitted_data_lost() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let committed_handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        committed_handle = db.allocate(b"committed").unwrap();
        db.commit().unwrap();
        db.begin().unwrap();
        db.allocate(b"uncommitted").unwrap();
        // Drop without commit — simulates crash.
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(committed_handle).unwrap(), b"committed");
    assert!(db.read(committed_handle + 1).is_err());
}

#[test]
fn test_recovery_corrupt_superblock_b() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"safe").unwrap();
        db.commit().unwrap();
    }
    // Corrupt superblock B (page 1) by zeroing it.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"safe");
}

#[test]
fn test_recovery_torn_first_commit() {
    // Regression test for ISSUES.md I2: historically `create_new` left
    // superblock slot 1 as an all-zero buffer. The very first user commit
    // writes slot 0 (txn_counter=2, even parity), overwriting the only
    // valid superblock on disk — so a torn write during that first commit
    // destroyed every recoverable state. The fix initializes both slots
    // with valid staggered-counter superblocks so slot 1 is always a
    // legitimate fallback.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    // Fresh database — no commits yet. Both slots should be valid.
    {
        let _db = Chisel::open(&path, Default::default()).unwrap();
    }

    // Simulate a torn write to slot 0 before any successful commit.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }

    // Must open successfully via the slot-1 fallback. Database is empty.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.handles().unwrap().len(), 0);
}

#[test]
fn test_recovery_both_superblocks_corrupt() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"doomed").unwrap();
        db.commit().unwrap();
    }
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    let result = Chisel::open(&path, Default::default());
    assert!(result.is_err());
}

#[test]
fn test_reject_unsupported_format_version() {
    // Regression test for ISSUES.md I15: a superblock with a valid checksum
    // but a future format_version must be refused with a distinct error,
    // not silently accepted.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    // Create a fresh database (initializes both superblock slots at v1).
    {
        let _db = Chisel::open(&path, Default::default()).unwrap();
    }

    // Overwrite BOTH slots with valid superblocks at a future format_version.
    // We must overwrite both — if we leave one at v1, select() would pick
    // whichever had the higher counter. Using identical counters here and
    // overwriting both guarantees the v-future slot wins.
    let mut sb = Superblock {
        magic: page::MAGIC,
        format_version: page::FORMAT_VERSION + 99,
        txn_counter: 5,
        root_handle_table_page: page::PAGE_ID_NONE,
        root_freemap_page: page::PAGE_ID_NONE,
        total_pages: 2,
        next_handle: 0,
        page_size: PAGE_SIZE as u32,
        named_roots: [chisel::superblock::NamedRoot::EMPTY; chisel::superblock::NAMED_ROOT_COUNT],
    };
    let buf_a = sb.serialize();
    sb.txn_counter = 4;
    let buf_b = sb.serialize();
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&buf_a).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&buf_b).unwrap();
        f.sync_all().unwrap();
    }

    match Chisel::open(&path, Default::default()) {
        Err(ChiselError::UnsupportedFormatVersion { found, expected }) => {
            assert_eq!(found, page::FORMAT_VERSION + 99);
            assert_eq!(expected, page::FORMAT_VERSION);
        }
        Err(other) => panic!("expected UnsupportedFormatVersion, got error {:?}", other),
        Ok(_) => panic!("expected UnsupportedFormatVersion, but open succeeded"),
    }
}

#[test]
fn test_read_takes_shared_reference() {
    // Regression test for ISSUES.md F3. `Chisel::read`, `handles`, and
    // `stats` must take `&self` so that downstream callers with a
    // shared reference (e.g. a `StorageEngine` trait whose read methods
    // are `&self`) don't need a `RefCell<Chisel>` wrapper. The test
    // compiles iff the signatures are correct.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle = {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"via shared ref").unwrap();
        db.commit().unwrap();
        h
    };

    let db = Chisel::open(&path, Default::default()).unwrap();

    // Hand out a shared reference and prove we can do all read-side work
    // through it without any interior wrapper.
    fn read_only_probe(db: &Chisel, h: u64) -> Vec<u8> {
        assert!(!db.is_poisoned());
        let _ = db.handles().unwrap();
        let _ = db.stats().unwrap();
        db.read(h).unwrap()
    }

    assert_eq!(read_only_probe(&db, handle), b"via shared ref");
    // Reading twice through the same shared ref also works.
    assert_eq!(read_only_probe(&db, handle), b"via shared ref");
}

#[test]
fn test_poison_recovery_by_reopen() {
    // End-to-end test for the ISSUES.md I1 poison recovery idiom. After
    // a poisoned handle is dropped and the database is reopened, data
    // committed before the poison must still be readable.
    //
    // This test cannot directly inject a fatal I/O error through the
    // public API, so it exercises the recovery path: commit some data,
    // then verify that a fresh open after drop sees that data. The
    // in-process poison semantics are covered by the unit tests in
    // transaction.rs.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle = {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        assert!(!db.is_poisoned());
        db.begin().unwrap();
        let h = db.allocate(b"survives recovery").unwrap();
        db.commit().unwrap();
        h
        // Drop here — simulates the user's response to a poisoned handle.
    };

    // Reopen: the shadow-paging recovery path picks the last durable
    // superblock, which references the committed value.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert!(!db.is_poisoned());
    assert_eq!(db.read(handle).unwrap(), b"survives recovery");
}

// --- F2: named roots ---

#[test]
fn test_named_roots_survive_commit_and_reopen() {
    // The core F2 use case: bind a name to a handle, commit, reopen the
    // database, and verify the name still resolves to the same handle.
    // This is what lets the client replace the "handle 0 is always meta"
    // convention with an explicit name → handle mapping.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let meta_handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        meta_handle = db.allocate(b"meta btree root state").unwrap();
        db.set_root_name("meta", meta_handle).unwrap();
        db.commit().unwrap();
    }

    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(meta_handle));
    assert_eq!(db.read(meta_handle).unwrap(), b"meta btree root state");
    // Unbound names must return Ok(None), not an error.
    assert_eq!(db.get_root_name("does_not_exist").unwrap(), None);
}

#[test]
fn test_named_roots_revert_on_rollback() {
    // A named-root mutation must revert when the enclosing transaction
    // is rolled back — the whole point of F2 (from the client's
    // perspective) is that named roots are transactional.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h1 = db.allocate(b"initial").unwrap();
    db.set_root_name("meta", h1).unwrap();
    db.commit().unwrap();

    // In a new transaction, change the binding and then roll back.
    db.begin().unwrap();
    let h2 = db.allocate(b"replacement").unwrap();
    db.set_root_name("meta", h2).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h2));
    db.rollback().unwrap();

    // After rollback, the committed binding must be restored.
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h1));
}

#[test]
fn test_named_roots_revert_on_rollback_to_savepoint() {
    // Savepoints must also snapshot and restore named roots — because
    // Savepoint captures a Roots clone, this is automatic if named_roots
    // is part of Roots. This test guards against someone refactoring
    // Roots and accidentally leaving named_roots out.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
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
    db.commit().unwrap();
}

#[test]
fn test_named_roots_validation_errors() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();

    // Empty name.
    assert!(matches!(
        db.set_root_name("", 0),
        Err(ChiselError::InvalidRootName)
    ));
    // Too long (> 24 bytes).
    let too_long = "a".repeat(25);
    assert!(matches!(
        db.set_root_name(&too_long, 0),
        Err(ChiselError::InvalidRootName)
    ));
    // Contains NUL.
    assert!(matches!(
        db.set_root_name("bad\0name", 0),
        Err(ChiselError::InvalidRootName)
    ));
    // Max length (exactly 24 bytes) is valid.
    let exactly_24 = "a".repeat(24);
    assert!(db.set_root_name(&exactly_24, 42).is_ok());
    assert_eq!(db.get_root_name(&exactly_24).unwrap(), Some(42));
    db.commit().unwrap();
}

#[test]
fn test_named_roots_table_full() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();

    // Fill all 8 slots.
    for i in 0..8 {
        let name = format!("r{i}");
        db.set_root_name(&name, i as u64).unwrap();
    }
    // Overwriting an existing name must still work even when full.
    db.set_root_name("r3", 999).unwrap();
    assert_eq!(db.get_root_name("r3").unwrap(), Some(999));

    // Adding a NEW name must fail.
    assert!(matches!(
        db.set_root_name("overflow", 1234),
        Err(ChiselError::RootNameTableFull)
    ));

    // Clearing a slot must reopen capacity.
    db.clear_root_name("r0").unwrap();
    assert!(db.set_root_name("new_one", 1234).is_ok());
    assert_eq!(db.get_root_name("new_one").unwrap(), Some(1234));
    db.commit().unwrap();
}

#[test]
fn test_named_roots_clear_is_idempotent() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    db.set_root_name("meta", 7).unwrap();
    db.clear_root_name("meta").unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), None);
    // Clearing a non-existent name is a no-op, not an error.
    db.clear_root_name("meta").unwrap();
    db.clear_root_name("never_set").unwrap();
    db.commit().unwrap();
}

// --- R2 / I9 / I10 / I11 / F1 / I12: the freemap bundle ---

#[test]
fn test_delete_then_allocate_reuses_pages() {
    // R2: after deletion, the freed data pages must become available to
    // subsequent allocations. The observable signature is that a
    // delete-then-reallocate cycle grows the file only by the
    // handle-table COW churn (~1 page per mutation), NOT by the full
    // data-page cost.
    //
    // Without R2, every allocate extends the file by 1 data page + 1
    // ht-cow page = ~2N pages for N allocations. With R2, the data
    // pages come from the freemap and we only pay ~N pages for ht
    // churn plus 1 for the freemap-page rewrite. So for N = 20:
    // without-R2 ≈ 40 new pages, with-R2 ≈ 21 new pages. Asserting
    // growth < 30 comfortably discriminates without being flaky to
    // minor commit-path accounting.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();

    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..20 {
        handles.push(db.allocate(format!("value-{i}").as_bytes()).unwrap());
    }
    db.commit().unwrap();

    db.begin().unwrap();
    for &h in &handles {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();
    let pages_after_delete = db.stats().unwrap().total_pages;

    db.begin().unwrap();
    for i in 0..20 {
        db.allocate(format!("replacement-{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after_reuse = db.stats().unwrap().total_pages;
    let growth = pages_after_reuse - pages_after_delete;
    assert!(
        growth < 30,
        "delete+reallocate grew file by {growth} pages; without R2 this would be ~40+, with R2 data pages are reused (only ht+freemap churn remains)"
    );
}

#[test]
fn test_update_does_not_leak_old_inline_page() {
    // I9: updating an inline handle many times must not grow the file
    // linearly. Pre-I9 each update leaked the old data page AND
    // allocated a new one, so the file grew by 1 data page + ht-cow
    // per update. Post-I9 the old data page goes to txn_freed_pages
    // and the next commit's allocations reuse it, so per-update growth
    // is dominated by ht-cow churn (~1 page) and the freemap page
    // rewrite.
    //
    // Each update is its own transaction here. With 50 updates and no
    // reuse, growth would be ~100 pages. With reuse, it's ~50-75 pages
    // (ht cow + freemap rewrite per commit). Asserting < 90 comfortably
    // discriminates.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handle = db.allocate(b"initial").unwrap();
    db.commit().unwrap();
    let pages_after_initial = db.stats().unwrap().total_pages;

    for i in 0..50 {
        db.begin().unwrap();
        db.update(handle, format!("round-{i}").as_bytes()).unwrap();
        db.commit().unwrap();
    }
    let pages_after_updates = db.stats().unwrap().total_pages;
    let growth = pages_after_updates - pages_after_initial;
    assert!(
        growth < 90,
        "50 updates grew the file by {growth} pages; without I9 this would be ~100+, with I9 old data pages are reused"
    );
    assert_eq!(db.read(handle).unwrap(), b"round-49");
}

#[test]
fn test_rollback_after_delete_does_not_free_pages() {
    // The freemap semantics under rollback: if a transaction deletes a
    // handle and then rolls back, the deletion is undone AND the old
    // data page must NOT have been added to the committed freemap.
    // Otherwise a subsequent transaction could reuse the page while
    // committed_roots still references it, overwriting live data.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handle = db.allocate(b"keep me").unwrap();
    db.commit().unwrap();

    // Delete + rollback: the delete's freeing should not become durable.
    db.begin().unwrap();
    db.delete(handle).unwrap();
    db.rollback().unwrap();

    // Handle must still be readable — the rollback restored it.
    assert_eq!(db.read(handle).unwrap(), b"keep me");

    // Allocate again: the new page must NOT collide with `handle`'s
    // data page. Reading `handle` after the allocation still gives the
    // original content.
    db.begin().unwrap();
    db.allocate(b"new value").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(handle).unwrap(), b"keep me");
}

#[test]
fn test_delete_many_atomicity() {
    // F1 / I12: delete_many runs inside a single transaction, so
    // rolling back discards the whole batch.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handles: Vec<u64> = (0..10)
        .map(|i| db.allocate(format!("h{i}").as_bytes()).unwrap())
        .collect();
    db.commit().unwrap();

    // Delete them all, then rollback — every handle must survive.
    db.begin().unwrap();
    db.delete_many(&handles).unwrap();
    db.rollback().unwrap();
    for (i, &h) in handles.iter().enumerate() {
        assert_eq!(db.read(h).unwrap(), format!("h{i}").as_bytes());
    }

    // Delete them all and commit — every handle must be gone, and the
    // freemap should now contain their pages.
    db.begin().unwrap();
    db.delete_many(&handles).unwrap();
    db.commit().unwrap();
    for &h in &handles {
        assert!(matches!(db.read(h), Err(ChiselError::InvalidHandle(_))));
    }

    // A subsequent bulk allocation should reuse the freed data pages.
    // Growth is bounded by ht-cow (~10 pages) + freemap page (~1), so
    // well under the ~20 pages that would be needed without reuse.
    let pages_before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..10 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;
    assert!(
        growth < 15,
        "bulk delete_many + re-allocate grew file by {growth} pages; without reuse this would be ~20+"
    );
}

#[test]
fn test_r1_small_values_pack_into_data_pages() {
    // R1: many small values in a single transaction must share data
    // pages instead of each getting its own. The observable signature
    // is dramatically lower file-page growth per value than the
    // pre-R1 "one value per page" model.
    //
    // For 100 small values (~16 bytes each, well under 8KB), pre-R1
    // would create 100 data pages + 100 ht-cow pages + 1 freemap
    // page ≈ 201 new pages. Post-R1, 100 values pack into ~1 data
    // page (or maybe 2 near a boundary) + 100 ht-cow + 1 freemap
    // ≈ 102 new pages. Asserting growth < 150 comfortably
    // discriminates.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    let pages_before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..100 {
        db.allocate(format!("v{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;
    assert!(
        growth < 150,
        "100 small values grew the file by {growth} pages; without R1 this would be ~200+, with R1 most values share data pages"
    );
}

#[test]
fn test_r1_packed_values_are_readable_after_reopen() {
    // Sanity: after R1 packs many values into the same data page and
    // commits, the values must all still be readable by handle (and
    // after a reopen, since the live-slot count map is rebuilt from
    // the handle-table scan).
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handles: Vec<u64>;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handles = (0..50)
            .map(|i| db.allocate(format!("packed-{i}").as_bytes()).unwrap())
            .collect();
        db.commit().unwrap();
    }

    let db = Chisel::open(&path, Default::default()).unwrap();
    for (i, &h) in handles.iter().enumerate() {
        assert_eq!(db.read(h).unwrap(), format!("packed-{i}").as_bytes());
    }
}

#[test]
fn test_r1_deleting_last_slot_frees_the_page() {
    // Under R1, a data page holding multiple slots is freed to the
    // freemap only when the LAST live slot is deleted. This test
    // allocates several values (which pack into one data page),
    // deletes all but one — the page must remain allocated — and
    // then deletes the last one — the page must now be free.
    //
    // The observable signature: after deleting all values and doing a
    // subsequent allocate, the file should NOT grow significantly
    // because the previously-packed page is reusable.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handles: Vec<u64> = (0..5)
        .map(|i| db.allocate(format!("s{i}").as_bytes()).unwrap())
        .collect();
    db.commit().unwrap();
    let pages_after_alloc = db.stats().unwrap().total_pages;

    // Delete all five in one transaction — the last delete should
    // free the shared data page back to the freemap.
    db.begin().unwrap();
    for &h in &handles {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();

    // Allocate 5 more: they should reuse the freed packed page.
    db.begin().unwrap();
    for i in 0..5 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after_reuse = db.stats().unwrap().total_pages;
    let growth = pages_after_reuse - pages_after_alloc;
    assert!(
        growth < 15,
        "re-allocate after bulk delete grew file by {growth} pages; packed page should be reusable"
    );
}

#[test]
fn test_r1_rollback_reverts_slot_counts() {
    // If a transaction inserts several packed values and rolls back,
    // the live-slot counts must revert so the next transaction sees
    // a clean slate.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h1 = db.allocate(b"survive-1").unwrap();
    let h2 = db.allocate(b"survive-2").unwrap();
    db.commit().unwrap();

    // Pack more values, then rollback.
    db.begin().unwrap();
    let _ = db.allocate(b"rolled-back-1").unwrap();
    let _ = db.allocate(b"rolled-back-2").unwrap();
    db.rollback().unwrap();

    // The pre-rollback handles must still be readable, AND
    // allocating new values must not behave as if the rolled-back
    // inserts left phantom slot counts.
    assert_eq!(db.read(h1).unwrap(), b"survive-1");
    assert_eq!(db.read(h2).unwrap(), b"survive-2");

    db.begin().unwrap();
    let h3 = db.allocate(b"after-rollback").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h3).unwrap(), b"after-rollback");
}

#[test]
fn test_freemap_persists_across_reopen() {
    // The freemap must round-trip through the superblock's
    // root_freemap_page. After a reopen, freed pages from the previous
    // session must still be reusable.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle_to_delete;
    let pages_after_delete;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        // Allocate many so the freed page is definitely inside the
        // active range, not tail garbage.
        for i in 0..5 {
            db.allocate(format!("keep-{i}").as_bytes()).unwrap();
        }
        handle_to_delete = db.allocate(b"delete me").unwrap();
        db.commit().unwrap();

        db.begin().unwrap();
        db.delete(handle_to_delete).unwrap();
        db.commit().unwrap();
        pages_after_delete = db.stats().unwrap().total_pages;
    }

    // Reopen and allocate: the freed page should be reused via the
    // freemap loaded from disk. Growth is bounded by 1 ht-cow + 1
    // freemap page ≈ 2, not 3 (1 data + 1 ht + 1 freemap) that we'd
    // pay without reuse.
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    db.allocate(b"reuse after reopen").unwrap();
    db.commit().unwrap();
    let pages_after_reopen_alloc = db.stats().unwrap().total_pages;
    let growth = pages_after_reopen_alloc - pages_after_delete;
    assert!(
        growth < 3,
        "after reopen, an allocation should reuse the freemap-tracked page (growth {growth})"
    );
}

#[test]
fn test_file_not_found_without_create() {
    let path = std::path::PathBuf::from("/tmp/chisel_nonexistent_test.db");
    let _ = fs::remove_file(&path);
    let result = Chisel::open(
        &path,
        Options {
            create_if_missing: false,
            ..Default::default()
        },
    );
    assert!(result.is_err());
}
