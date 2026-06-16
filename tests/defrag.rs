use chisel::ChiselError;

mod common;
use common::{open_chisel, Backing};

fn test_defrag_without_active_txn_errors_immediately_body(b: &Backing) {
    // defrag() mutates through `update`, which requires an active
    // transaction. Calling defrag on an idle handle must fail
    // eagerly with `NoActiveTransaction` rather than reading from
    // the committed snapshot and then failing partway through on
    // the first update.
    let mut db = open_chisel(b);

    db.begin().unwrap();
    for i in 0..4 {
        db.allocate(&[i as u8; 32]).unwrap();
    }
    db.commit().unwrap();

    // NO begin() here: the next call is defrag on an idle handle.
    let err = db.defrag(Default::default()).unwrap_err();
    assert!(
        matches!(err, ChiselError::NoActiveTransaction),
        "expected NoActiveTransaction, got {err:?}"
    );
}

dual_backing_test!(
    test_defrag_without_active_txn_errors_immediately,
    test_defrag_without_active_txn_errors_immediately_body
);

fn test_defrag_reclaims_space_after_deletes_body(b: &Backing) {
    let mut db = open_chisel(b);

    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..50 {
        handles.push(db.allocate(&[i as u8; 200]).unwrap());
    }
    db.commit().unwrap();

    db.begin().unwrap();
    for &h in &handles[5..] {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();

    db.begin().unwrap();
    let result = db.defrag(Default::default()).unwrap();
    db.commit().unwrap();

    // Defrag relocates surviving values between data pages and rewrites their
    // handle-table entries — assert the BYTES survive, not just that the handle
    // still resolves. handles[i] was allocated as [i; 200].
    for (i, &h) in handles[..5].iter().enumerate() {
        assert_eq!(
            db.read(h).unwrap(),
            vec![i as u8; 200],
            "handle {h} (index {i}) lost or corrupted its value across defrag"
        );
    }
    assert!(result.pages_freed > 0);
}

dual_backing_test!(
    test_defrag_reclaims_space_after_deletes,
    test_defrag_reclaims_space_after_deletes_body
);

fn test_defrag_preserves_all_data_body(b: &Backing) {
    let mut db = open_chisel(b);

    db.begin().unwrap();
    let h1 = db.allocate(b"alpha").unwrap();
    let h2 = db.allocate(b"beta").unwrap();
    let h3 = db.allocate(b"gamma").unwrap();
    db.commit().unwrap();

    db.begin().unwrap();
    db.defrag(Default::default()).unwrap();
    db.commit().unwrap();

    assert_eq!(db.read(h1).unwrap(), b"alpha");
    assert_eq!(db.read(h2).unwrap(), b"beta");
    assert_eq!(db.read(h3).unwrap(), b"gamma");
}

dual_backing_test!(
    test_defrag_preserves_all_data,
    test_defrag_preserves_all_data_body
);

fn test_defrag_skips_dense_pages_body(b: &Backing) {
    // R3: a database whose data pages are all dense should have
    // nothing to do during defrag. `values_moved` should be zero
    // because the sweep identifies no sparse pages.
    let mut db = open_chisel(b);

    // Pack many values in one transaction — they all go into a
    // fresh dense cursor page.
    db.begin().unwrap();
    for i in 0..50 {
        db.allocate(format!("dense-{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();

    db.begin().unwrap();
    let result = db.defrag(Default::default()).unwrap();
    db.commit().unwrap();

    assert_eq!(
        result.values_moved, 0,
        "dense pages should not be touched by selective defrag"
    );
    assert_eq!(result.pages_freed, 0);
    assert_eq!(result.pages_examined, 0);
}

dual_backing_test!(
    test_defrag_skips_dense_pages,
    test_defrag_skips_dense_pages_body
);

fn test_defrag_stats_are_page_accurate_body(b: &Backing) {
    // I17: `pages_examined` should count unique sparse pages touched,
    // not values moved. `pages_freed` should count pages that were
    // present at the start of the sweep and are gone at the end.
    let mut db = open_chisel(b);

    // Seed enough values to spread across multiple data pages.
    // 200-byte values pack ~39 per 8 KB page, so 100 values land
    // on ~3 data pages.
    db.begin().unwrap();
    let handles: Vec<u64> = (0..100)
        .map(|i| db.allocate(&[i as u8; 200]).unwrap())
        .collect();
    db.commit().unwrap();

    // Delete all but a handful, leaving the pages very sparse.
    db.begin().unwrap();
    for &h in &handles[3..] {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();

    db.begin().unwrap();
    let result = db.defrag(Default::default()).unwrap();
    db.commit().unwrap();

    // pages_examined should be the number of distinct sparse pages
    // we relocated from — at most a handful, and definitely NOT 97
    // (which would be the old per-value count for 97 deletes).
    assert!(
        result.pages_examined > 0 && result.pages_examined < 10,
        "pages_examined should be unique sparse pages (got {})",
        result.pages_examined
    );
    // pages_freed should be non-zero — the sparse pages the sweep
    // relocated out of are now gone.
    assert!(
        result.pages_freed > 0,
        "expected pages_freed > 0, got {}",
        result.pages_freed
    );
    // Surviving handles still readable.
    for (i, &h) in handles[..3].iter().enumerate() {
        assert_eq!(db.read(h).unwrap(), [i as u8; 200]);
    }
}

dual_backing_test!(
    test_defrag_stats_are_page_accurate,
    test_defrag_stats_are_page_accurate_body
);

fn test_defrag_respects_max_pages_body(b: &Backing) {
    // `max_pages` caps how much work defrag does in one call so
    // large databases can be incrementally compacted. With a cap of
    // 2, defrag should relocate at most 2 values.
    use chisel::DefragOptions;
    let mut db = open_chisel(b);

    db.begin().unwrap();
    let handles: Vec<u64> = (0..50)
        .map(|i| db.allocate(&[i as u8; 200]).unwrap())
        .collect();
    db.commit().unwrap();

    db.begin().unwrap();
    for &h in &handles[2..] {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();

    db.begin().unwrap();
    let result = db
        .defrag(DefragOptions::default().sparse_threshold(0.25).max_pages(2))
        .unwrap();
    db.commit().unwrap();

    assert!(
        result.values_moved <= 2,
        "max_pages=2 should cap values_moved to 2, got {}",
        result.values_moved
    );
    // Surviving data is still readable.
    for (i, &h) in handles[..2].iter().enumerate() {
        assert_eq!(db.read(h).unwrap(), [i as u8; 200]);
    }
}

dual_backing_test!(
    test_defrag_respects_max_pages,
    test_defrag_respects_max_pages_body
);
