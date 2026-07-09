// Regression tests for handle-table / membership-index COW page reclamation.
//
// Before the fix, every handle-table mutation (allocate / update / delete /
// set_client_byte / allocate_tagged) COWed its root->leaf spine via the
// monotonic `PageCache::new_page` allocator and abandoned the superseded
// pages forever: they were never returned to the freemap, and even when
// they were, the COW allocator never consulted the freemap. The database
// file therefore grew by ~1 page per mutation without bound, even when the
// logical data size was constant.
//
// These tests pin the invariant that a workload with a constant live set
// reaches a bounded steady-state page count rather than growing linearly
// with the number of operations.

use chisel::{Chisel, Options};
use std::collections::HashMap;

// Repeated same-handle, same-size updates must not grow the file without
// bound. Each update COWs the handle-table leaf and relocates the value;
// with reclamation the superseded pages are reused, so total_pages reaches
// a small steady state instead of climbing ~1 per update.
#[test]
fn repeated_update_of_same_handle_reaches_bounded_page_count() {
    let mut db = Chisel::open_in_memory_with_options(Options::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"a-small-constant-value").unwrap();
    db.commit().unwrap();

    let pages_before = db.stats().unwrap().total_pages;

    const N: u64 = 2000;
    for _ in 0..N {
        db.begin().unwrap();
        db.update(h, b"a-small-constant-value").unwrap();
        db.commit().unwrap();
    }

    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;

    // Pre-fix this was ~N (one leaked page per update). Post-fix the file
    // reaches a small steady state. A generous bound that still fails hard
    // on a linear leak:
    assert!(
        growth < 50,
        "file grew by {growth} pages over {N} constant-size updates \
         (pages_before={pages_before}, pages_after={pages_after}); \
         expected a bounded steady state, not a per-op leak"
    );

    // The value must still be intact after all that churn.
    assert_eq!(db.read(h).unwrap(), b"a-small-constant-value");
}

// `set_client_byte` is the purest handle-table mutation: it COWs only the
// handle-table leaf and touches no data page at all. Pre-fix it leaked
// exactly one page per call with nothing to ever reclaim it (no data
// allocation to drain the freemap). It is the sharpest test of freemap-aware
// COW allocation.
#[test]
fn repeated_set_client_byte_reaches_bounded_page_count() {
    let mut db = Chisel::open_in_memory_with_options(Options::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"v").unwrap();
    db.commit().unwrap();

    let pages_before = db.stats().unwrap().total_pages;

    const N: u64 = 2000;
    for i in 0..N {
        db.begin().unwrap();
        db.set_client_byte(h, (i % 256) as u8).unwrap();
        db.commit().unwrap();
    }

    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;

    assert!(
        growth < 50,
        "file grew by {growth} pages over {N} set_client_byte calls \
         (pages_before={pages_before}, pages_after={pages_after}); \
         expected a bounded steady state, not a per-op leak"
    );

    assert_eq!(db.client_byte(h).unwrap(), ((N - 1) % 256) as u8);
}

// Tagged allocate + delete churn exercises the membership-index COW spine in
// addition to the handle table. Allocating then deleting a tagged chunk in a
// loop keeps a near-constant live set; the membership index inserts then
// removes the handle each cycle, COWing its own root->leaf spine. (The
// handle-table leaf fills with tombstones as handles are retired, so a
// modest amount of growth from tombstone accumulation is expected and the
// bound is looser than the pure-update cases.)
#[test]
fn tagged_allocate_delete_churn_reaches_bounded_page_count() {
    let mut db = Chisel::open_in_memory_with_options(Options::default()).unwrap();

    // Establish one long-lived tagged member so the tag's inner tree never
    // fully drains (draining abandons the inner tree, a separate concern).
    db.begin().unwrap();
    let keeper = db
        .allocate_tagged(b"keeper", chisel::Tag::new(7).unwrap())
        .unwrap();
    db.commit().unwrap();

    let pages_before = db.stats().unwrap().total_pages;

    const N: u64 = 1000;
    for _ in 0..N {
        db.begin().unwrap();
        let h = db
            .allocate_tagged(b"ephemeral", chisel::Tag::new(7).unwrap())
            .unwrap();
        db.commit().unwrap();

        db.begin().unwrap();
        db.delete(h).unwrap();
        db.commit().unwrap();
    }

    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;

    // Pre-fix: membership-index COW pages leaked on every allocate_tagged and
    // delete (on top of the handle-table leak), so growth was several × N.
    // Post-fix only handle-table tombstone accumulation remains, which is far
    // sub-linear. Bound generously but well below a per-op leak.
    assert!(
        growth < 200,
        "file grew by {growth} pages over {N} tagged alloc/delete cycles \
         (pages_before={pages_before}, pages_after={pages_after}); \
         expected sub-linear growth, not a per-op COW leak"
    );

    // The long-lived member must still be tagged and enumerable.
    assert_eq!(db.tag(keeper).unwrap().unwrap(), 7);
    assert_eq!(
        db.handles_with_tag(chisel::Tag::new(7).unwrap()).unwrap(),
        vec![keeper]
    );
}

// Reclamation reuses pages freed by *prior committed* transactions. This test
// drives a file-backed database through heavy churn (so the reuse path runs
// against real on-disk pages), then closes and reopens it and verifies every
// live value, tag, and client byte survived. It guards the core safety
// property: a reused page id never clobbers data the current durable state
// still depends on.
#[test]
fn heavy_churn_with_reclamation_survives_reopen_file_backed() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("churn.db");

    // model: handle -> (value, tag, client_byte)
    let mut model: HashMap<chisel::Handle, (Vec<u8>, u32, u8)> = HashMap::new();

    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();

        // Seed a working set with a mix of tagged/untagged and inline/overflow.
        db.begin().unwrap();
        for i in 0u64..40 {
            let tag = if i % 3 == 0 { (i % 5) as u32 + 1 } else { 0 };
            let val = vec![i as u8; 100 + (i as usize % 7) * 50];
            let h = if tag == 0 {
                db.allocate(&val).unwrap()
            } else {
                db.allocate_tagged(&val, chisel::Tag::new(tag).unwrap())
                    .unwrap()
            };
            model.insert(h, (val, tag, 0));
        }
        db.commit().unwrap();

        // Many churn rounds: update values (relocating storage), flip client
        // bytes, delete some handles and allocate replacements. Each round is
        // its own transaction so freed pages from round R become reusable in
        // round R+1 — exactly the reuse path under test.
        for round in 0u64..60 {
            db.begin().unwrap();
            let handles: Vec<chisel::Handle> = model.keys().copied().collect();
            for (idx, h) in handles.iter().enumerate() {
                match (round as usize + idx) % 4 {
                    0 => {
                        // same-size update
                        let new = vec![(round ^ h.get()) as u8; model[h].0.len()];
                        db.update(*h, &new).unwrap();
                        model.get_mut(h).unwrap().0 = new;
                    }
                    1 => {
                        // grow into an overflow value, then it can shrink later
                        let new = vec![(round as u8).wrapping_add(idx as u8); 9000];
                        db.update(*h, &new).unwrap();
                        model.get_mut(h).unwrap().0 = new;
                    }
                    2 => {
                        let b = (round as u8).wrapping_add(7);
                        db.set_client_byte(*h, b).unwrap();
                        model.get_mut(h).unwrap().2 = b;
                    }
                    _ => {
                        // delete + allocate a replacement (keeps the set size ~stable)
                        let (_, tag, _) = model.remove(h).unwrap();
                        if tag == 0 {
                            db.delete(*h).unwrap();
                        } else {
                            db.delete_tagged(*h, chisel::Tag::new(tag).unwrap())
                                .unwrap();
                        }
                        let val = vec![round as u8; 120];
                        let nh = if tag == 0 {
                            db.allocate(&val).unwrap()
                        } else {
                            db.allocate_tagged(&val, chisel::Tag::new(tag).unwrap())
                                .unwrap()
                        };
                        model.insert(nh, (val, tag, 0));
                    }
                }
            }
            db.commit().unwrap();
        }

        db.close().unwrap();
    }

    // Reopen from disk and verify the durable state matches the model exactly.
    let db = Chisel::open(&path, Options::default()).unwrap();
    let live: std::collections::HashSet<chisel::Handle> =
        db.handles().unwrap().into_iter().collect();
    let expected: std::collections::HashSet<chisel::Handle> = model.keys().copied().collect();
    assert_eq!(live, expected, "live handle set must survive reopen");

    for (h, (val, tag, cb)) in &model {
        assert_eq!(
            &db.read(*h).unwrap(),
            val,
            "value for handle {h} after reopen"
        );
        assert_eq!(
            db.tag(*h).unwrap().map_or(0, |t| t.get()),
            *tag,
            "tag for handle {h} after reopen"
        );
        assert_eq!(
            db.client_byte(*h).unwrap(),
            *cb,
            "client byte for handle {h} after reopen"
        );
    }

    // Reverse index must agree with the forward tags after all the churn.
    let mut expected_by_tag: HashMap<u32, Vec<chisel::Handle>> = HashMap::new();
    for (h, (_, tag, _)) in &model {
        if *tag != 0 {
            expected_by_tag.entry(*tag).or_default().push(*h);
        }
    }
    for (tag, mut handles) in expected_by_tag {
        let mut got = db.handles_with_tag(chisel::Tag::new(tag).unwrap()).unwrap();
        got.sort_unstable();
        handles.sort_unstable();
        assert_eq!(got, handles, "handles_with_tag({tag}) after reopen");
    }
}
