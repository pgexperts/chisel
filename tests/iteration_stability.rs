//! Within-session iteration-stability contract for `handles()` and
//! `handles_with_tag()` (see docs/specs/2026-06-04-stable-chunk-iteration-design.md).
//!
//! These are *differential* tests: a "repeatable, order-unspecified" guarantee
//! cannot be checked from a single scan, so each test scans, churns state the
//! contract permits to change (reads, an `update`, cache eviction, a rolled-back
//! transaction/savepoint), scans again, and asserts the two scans are
//! byte-identical. They assert repeatability + contents, NEVER a specific order
//! — asserting order would silently strengthen the contract beyond what the spec
//! promises.
//!
//! Deliberately NOT tested: order across close+reopen or across defrag/compact.
//! The single-session scope makes those non-guarantees; a test there would lock
//! behavior the engine is explicitly free to change.

mod common;

use chisel::{Handle, Options, Tag};
use common::{open_chisel, open_chisel_with, Backing};

fn back_to_back_handles_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let mut expected = vec![
        db.allocate(b"untagged-a").unwrap(),
        db.allocate_tagged(b"t42-a", Tag::new(42).unwrap()).unwrap(),
        db.allocate(b"untagged-b").unwrap(),
        db.allocate_tagged(b"t7-a", Tag::new(7).unwrap()).unwrap(),
        db.allocate_tagged(b"t42-b", Tag::new(42).unwrap()).unwrap(),
    ];
    db.commit().unwrap();

    // The contract: two scans with no mutation between them are byte-identical.
    let first = db.handles().unwrap();
    let second = db.handles().unwrap();
    assert_eq!(first, second, "repeated handles() must be identical");

    // Sanity that the scan covers the real data — order-normalized, so this
    // never asserts a particular order.
    let mut got = first.clone();
    got.sort_unstable();
    expected.sort_unstable();
    assert_eq!(got, expected, "handles() must return exactly the live set");

    db.close().unwrap();
}
dual_backing_test!(back_to_back_handles, back_to_back_handles_body);

fn back_to_back_handles_with_tag_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let t42_a = db.allocate_tagged(b"t42-a", Tag::new(42).unwrap()).unwrap();
    let _t7 = db.allocate_tagged(b"t7", Tag::new(7).unwrap()).unwrap();
    let t42_b = db.allocate_tagged(b"t42-b", Tag::new(42).unwrap()).unwrap();
    let _untagged = db.allocate(b"untagged").unwrap();
    db.commit().unwrap();

    let first = db.handles_with_tag(Tag::new(42).unwrap()).unwrap();
    let second = db.handles_with_tag(Tag::new(42).unwrap()).unwrap();
    assert_eq!(
        first, second,
        "repeated handles_with_tag() must be identical"
    );

    let mut got = first.clone();
    got.sort_unstable();
    let mut expected = vec![t42_a, t42_b];
    expected.sort_unstable();
    assert_eq!(
        got, expected,
        "handles_with_tag(42) must return exactly tag 42's members"
    );

    db.close().unwrap();
}
dual_backing_test!(
    back_to_back_handles_with_tag,
    back_to_back_handles_with_tag_body
);

fn reads_between_scans_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let handles: Vec<Handle> = (0..16)
        .map(|i| db.allocate(format!("row-{i}").as_bytes()).unwrap())
        .collect();
    db.commit().unwrap();

    let first = db.handles().unwrap();

    // Reads take &self and must not perturb a later scan — including a read of
    // the most recently allocated handle.
    for &h in &handles {
        let _ = db.read(h).unwrap();
    }
    let _ = db.read(*handles.last().unwrap()).unwrap();

    let second = db.handles().unwrap();
    assert_eq!(
        first, second,
        "reads must not perturb handles() order or contents"
    );

    db.close().unwrap();
}
dual_backing_test!(reads_between_scans, reads_between_scans_body);

fn update_between_scans_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h_small = db.allocate(b"small").unwrap();
    let h_tagged = db
        .allocate_tagged(b"tagged-small", Tag::new(42).unwrap())
        .unwrap();
    let h_other = db.allocate(b"other").unwrap();
    db.commit().unwrap();

    let first_all = db.handles().unwrap();
    let first_t42 = db.handles_with_tag(Tag::new(42).unwrap()).unwrap();

    // `update` preserves the handle (and the immutable tag), so the live set —
    // and each handle's radix position — is unchanged. Grow one value past
    // MAX_INLINE_VALUE (8162 bytes) to force relocation to an overflow chain,
    // the most disruptive update path.
    db.begin().unwrap();
    db.update(h_small, &[0xABu8; 9000]).unwrap();
    db.update(h_tagged, &[0xCDu8; 9000]).unwrap();
    db.update(h_other, b"still-small").unwrap();
    db.commit().unwrap();

    let second_all = db.handles().unwrap();
    let second_t42 = db.handles_with_tag(Tag::new(42).unwrap()).unwrap();
    assert_eq!(first_all, second_all, "update must not perturb handles()");
    assert_eq!(
        first_t42, second_t42,
        "update must not perturb handles_with_tag()"
    );

    db.close().unwrap();
}
dual_backing_test!(update_between_scans, update_between_scans_body);

fn cache_eviction_between_scans_body(b: &Backing) {
    // A small cache with the spillway LEFT ENABLED (so the bulk insert
    // succeeds). The point: force the handle-table page(s) out of the LRU
    // between the two scans and prove the reload reproduces the scan exactly —
    // i.e. the order is structural, not cache-residency-dependent.
    const N: usize = 300;
    let opts = Options::default().cache_max_bytes(32 * 8192); // 32 pages
    let mut db = open_chisel_with(b, opts);

    db.begin().unwrap();
    // ~4 KiB values → ~two per data page → ~150 data pages, far exceeding the
    // 32-page cache, so reading them all evicts the handle-table page(s).
    for i in 0..N {
        let mut v = vec![0u8; 4000];
        v[0] = i as u8;
        db.allocate(&v).unwrap();
    }
    db.commit().unwrap();

    let first = db.handles().unwrap();
    assert_eq!(first.len(), N, "baseline scan should see every chunk");

    // Churn the cache: reading every value loads ~150 distinct data pages
    // through a 32-page cache, evicting the handle-table page(s).
    for &h in &first {
        let _ = db.read(h).unwrap();
    }

    let second = db.handles().unwrap();
    assert_eq!(first, second, "cache eviction must not perturb handles()");

    db.close().unwrap();
}
dual_backing_test!(
    cache_eviction_between_scans,
    cache_eviction_between_scans_body
);

// Allocating this many same-tag chunks inside a transaction grows BOTH radix
// trees past a level: > 510 entries grows the handle-table leaf, and > 1021
// same-tag members grows that tag's inner membership tree. Rolling back must
// then restore the roots AND re-derive tree depth (the I99/C1 invariant), or a
// later scan would mis-descend and mis-enumerate.
const GROW: usize = 1100;

fn rolled_back_transaction_body(b: &Backing) {
    const TAG: u32 = 99;
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let mut baseline_tag = vec![
        db.allocate_tagged(b"base-1", Tag::new(TAG).unwrap())
            .unwrap(),
        db.allocate_tagged(b"base-2", Tag::new(TAG).unwrap())
            .unwrap(),
        db.allocate_tagged(b"base-3", Tag::new(TAG).unwrap())
            .unwrap(),
    ];
    let _base_untagged = db.allocate(b"base-untagged").unwrap();
    db.commit().unwrap();

    let baseline_all = db.handles().unwrap();
    let baseline_t = db.handles_with_tag(Tag::new(TAG).unwrap()).unwrap();

    db.begin().unwrap();
    for i in 0..GROW {
        db.allocate_tagged(format!("ephemeral-{i}").as_bytes(), Tag::new(TAG).unwrap())
            .unwrap();
    }
    db.rollback().unwrap();

    // After rollback both scans must reproduce the committed baseline exactly.
    assert_eq!(
        db.handles().unwrap(),
        baseline_all,
        "rollback must restore handles()"
    );
    assert_eq!(
        db.handles_with_tag(Tag::new(TAG).unwrap()).unwrap(),
        baseline_t,
        "rollback must restore handles_with_tag()"
    );

    // Order-normalized: the tag retains exactly its three committed members.
    let mut got = db.handles_with_tag(Tag::new(TAG).unwrap()).unwrap();
    got.sort_unstable();
    baseline_tag.sort_unstable();
    assert_eq!(got, baseline_tag);

    db.close().unwrap();
}
dual_backing_test!(rolled_back_transaction, rolled_back_transaction_body);

fn savepoint_rollback_body(b: &Backing) {
    const TAG: u32 = 123;
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let mut baseline_tag = vec![
        db.allocate_tagged(b"keep-1", Tag::new(TAG).unwrap())
            .unwrap(),
        db.allocate_tagged(b"keep-2", Tag::new(TAG).unwrap())
            .unwrap(),
    ];
    db.commit().unwrap();

    let baseline_all = db.handles().unwrap();
    let baseline_t = db.handles_with_tag(Tag::new(TAG).unwrap()).unwrap();

    // Grow both trees inside a savepoint, then roll the savepoint back. Like a
    // full rollback, rollback_to must re-derive depth (I99/C1).
    db.begin().unwrap();
    db.savepoint("grow").unwrap();
    for i in 0..GROW {
        db.allocate_tagged(format!("ephemeral-{i}").as_bytes(), Tag::new(TAG).unwrap())
            .unwrap();
    }
    db.rollback_to("grow").unwrap();
    db.commit().unwrap();

    assert_eq!(
        db.handles().unwrap(),
        baseline_all,
        "savepoint rollback must restore handles()"
    );
    assert_eq!(
        db.handles_with_tag(Tag::new(TAG).unwrap()).unwrap(),
        baseline_t,
        "savepoint rollback must restore handles_with_tag()"
    );

    let mut got = db.handles_with_tag(Tag::new(TAG).unwrap()).unwrap();
    got.sort_unstable();
    baseline_tag.sort_unstable();
    assert_eq!(got, baseline_tag);

    db.close().unwrap();
}
dual_backing_test!(savepoint_rollback, savepoint_rollback_body);
