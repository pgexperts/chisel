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

// HANDLES-INDEX-2 (issue #112). A single transaction's page footprint must be
// bounded by the data it writes, not by how many mutations it makes.
//
// Every handle-table mutation COWs the whole root-to-leaf spine. The superseded
// pages queue in `txn_freed_pages` and do not reach the committed freemap until
// commit, so before the within-transaction recycle pool NOTHING a transaction
// freed was reusable by that same transaction: N allocations against a depth-1
// table extended the file by ~N*2 pages, dirtied all of them, and only marked
// them free once the commit that made them garbage had already flushed them.
//
// METRIC. This asserts on committed `total_pages` — the file high-water mark,
// which is what the issue's own title names — and NOT on
// `counters().pages_allocated`. That distinction is worth recording, because
// the obvious counter is the wrong one HERE: the recycle pool does not remove
// the per-level allocations, it satisfies them from a bounded pool of pages
// this transaction already superseded. `pages_allocated` counts a pool draw
// exactly like a file extension (both go through `PageCache::reissue_page`),
// so it stays at roughly 2 per op by design and cannot discriminate. What
// changes, and what this measures, is how many DISTINCT pages the transaction
// needs.
#[test]
fn many_allocates_in_one_transaction_do_not_grow_the_file_per_level() {
    let mut db = Chisel::open_in_memory_with_options(Options::default()).unwrap();

    // Seed past ENTRIES_PER_LEAF (510) so the handle table sits at depth >= 1
    // and every further insert has a real spine to COW. At depth 0 a mutation
    // COWs one page and the defect barely shows.
    db.begin().unwrap();
    for _ in 0..600 {
        db.allocate(b"seed").unwrap();
    }
    db.commit().unwrap();

    let pages_before = db.stats().unwrap().total_pages;

    const N: u64 = 2000;
    db.begin().unwrap();
    for i in 0..N {
        db.allocate(&i.to_le_bytes()).unwrap();
    }
    db.commit().unwrap();

    let growth = db.stats().unwrap().total_pages - pages_before;
    let per_op = growth as f64 / N as f64;

    // MEASURED, not reasoned. With the pool: growth = 10 pages, 0.005 per op —
    // the data pages the 2000 values pack into plus the handle-table leaves
    // they genuinely need. With the pool's feed disabled (a one-line revert of
    // `retire_superseded`'s gate): growth = 3318 pages, 1.659 per op. The gap
    // is 332x; the bound sits 20x above the observed figure and 16x below the
    // regression, so ordinary drift in value size or leaf fanout cannot reach
    // it but a return to per-mutation growth cannot miss it.
    //
    // 1.659 rather than the ~2.0 a depth-1 spine implies, because the seed
    // transaction's own supersedes reach the committed freemap at its commit
    // and absorb part of this transaction's churn. That absorption is exactly
    // the masking that makes a file-size metric weaker on a long-lived
    // database — here the seeding is small and fixed, so it costs a constant
    // factor, not the signal.
    assert!(
        per_op < 0.10,
        "one transaction allocated {per_op} pages per mutation ({growth} pages for {N} \
         allocates); the handle-table spine is being re-extended per mutation instead of \
         recycled within the transaction"
    );
}
