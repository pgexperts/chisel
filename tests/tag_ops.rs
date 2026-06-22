//! Integration tests for chunk tags: in-session readback (both backings),
//! durability of tags + the membership index across close/reopen, the F1/I12
//! relation-drop contract (`delete_with_tag` removes every handle of a tag with
//! no orphans, leaving other tags intact and the dropped chunks unreadable), and
//! backward compatibility with pre-tag databases (everything reads as tag 0).
//!
//! Page-level reclamation of tag tree pages is asserted in
//! `recreating_a_dropped_tag_reuses_freed_pages` (I118): `create_root` now
//! routes through the freemap-aware allocator so dropped-tag pages are reused
//! on re-create rather than extending the file. General data-page reclamation
//! is covered by focused freemap unit tests (`src/freemap.rs`,
//! `persist_freemap_*` in `src/transaction.rs`). The F1/I12 concern these
//! tests guard is orphan-handle integrity, which is deterministic.
mod common;

use chisel::{Chisel, ChiselError, Handle, Tag};
use common::{open_chisel, Backing};
use tempfile::NamedTempFile;

fn tag_survives_in_session_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db
        .allocate_tagged(b"relation row", Tag::new(77).unwrap())
        .unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap().unwrap(), 77);
    // Also exercise the membership-index read path on BOTH backings (the
    // in-memory backing's index read is otherwise only covered transitively).
    assert_eq!(db.handles_with_tag(Tag::new(77).unwrap()).unwrap(), vec![h]);
    db.close().unwrap();
}
dual_backing_test!(tag_survives_in_session, tag_survives_in_session_body);

#[test]
fn tag_and_index_survive_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h = db
            .allocate_tagged(b"survive", Tag::new(9).unwrap())
            .unwrap();
        db.allocate_tagged(b"survive2", Tag::new(9).unwrap())
            .unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.tag(h).unwrap().unwrap(), 9);
        assert_eq!(db.handles_with_tag(Tag::new(9).unwrap()).unwrap().len(), 2);
        db.close().unwrap();
    }
}

// delete_tagged is the ownership-asserting single delete. Its mismatch arm has
// an in-crate unit test, but the public-API success path (correct tag -> chunk
// gone AND removed from the reverse index) and the public TagMismatch were
// untested — a mis-wired delegation would pass everything else. Cover both arms.
#[test]
fn delete_tagged_verifies_tag_then_self_maintains_index() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let a = db.allocate_tagged(b"a", Tag::new(42).unwrap()).unwrap();
    let b = db.allocate_tagged(b"b", Tag::new(42).unwrap()).unwrap();
    db.commit().unwrap();

    // Wrong tag: TagMismatch, and the chunk + reverse index are left untouched.
    db.begin().unwrap();
    let err = match db.delete_tagged(a, Tag::new(99).unwrap()) {
        Ok(()) => panic!("delete_tagged with a non-matching tag must fail"),
        Err(e) => e,
    };
    assert!(
        matches!(
            err,
            ChiselError::TagMismatch { handle, expected, actual }
                if handle == a && expected == 99 && actual == 42
        ),
        "expected TagMismatch {{handle: a, expected: 99, actual: 42}}, got {err:?}"
    );
    db.commit().unwrap();
    assert_eq!(
        db.read(a).unwrap(),
        b"a",
        "chunk untouched after TagMismatch"
    );
    assert_eq!(
        db.handles_with_tag(Tag::new(42).unwrap()).unwrap().len(),
        2,
        "reverse index untouched after TagMismatch"
    );

    // Correct tag: the chunk is deleted AND drops out of the reverse index.
    db.begin().unwrap();
    db.delete_tagged(a, Tag::new(42).unwrap()).unwrap();
    db.commit().unwrap();
    assert!(
        matches!(db.read(a), Err(ChiselError::InvalidHandle(_))),
        "chunk gone after delete_tagged: expected InvalidHandle"
    );
    assert_eq!(
        db.handles_with_tag(Tag::new(42).unwrap()).unwrap(),
        vec![b],
        "a removed from the reverse index; b remains"
    );
}

#[test]
fn dropping_a_relation_removes_all_handles_no_orphans() {
    // The F1/I12 relation-drop contract: delete_with_tag must drop EVERY handle of
    // the tag (no orphans in the handle table or the membership index) and make
    // each dropped chunk unreadable, while leaving other tags untouched. This is
    // the orphan-handle leak F1/I12 is about (the prior fix was delete_many); the
    // bounded loop is the relation-drop idiom.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let mut hs = Vec::new();
    for i in 0..200u64 {
        hs.push(
            db.allocate_tagged(format!("row {i}").as_bytes(), Tag::new(1).unwrap())
                .unwrap(),
        );
    }
    // A second relation under a different tag must survive the tag-1 drop intact.
    let keep = db
        .allocate_tagged(b"other relation", Tag::new(2).unwrap())
        .unwrap();
    db.commit().unwrap();
    assert_eq!(
        db.handles_with_tag(Tag::new(1).unwrap()).unwrap().len(),
        200
    );

    // Drop the whole tag-1 relation in bounded batches; accumulate the reported
    // handles to confirm every member is accounted for exactly once.
    db.begin().unwrap();
    let mut dropped = Vec::new();
    loop {
        let p = db.delete_with_tag(Tag::new(1).unwrap(), 64).unwrap();
        dropped.extend(p.deleted);
        if p.complete {
            break;
        }
    }
    db.commit().unwrap();

    dropped.sort_unstable();
    let mut expected = hs.clone();
    expected.sort_unstable();
    assert_eq!(
        dropped, expected,
        "every tagged handle reported dropped, once"
    );
    assert_eq!(
        db.handles_with_tag(Tag::new(1).unwrap()).unwrap(),
        Vec::<Handle>::new(),
        "no orphan index entries remain for the dropped tag"
    );
    for h in &hs {
        assert!(
            matches!(db.read(*h), Err(ChiselError::InvalidHandle(_))),
            "dropped chunk {h} must be InvalidHandle after delete_with_tag"
        );
    }
    // The untouched relation is fully intact.
    assert_eq!(db.tag(keep).unwrap().unwrap(), 2);
    assert_eq!(
        db.handles_with_tag(Tag::new(2).unwrap()).unwrap(),
        vec![keep]
    );
    db.close().unwrap();

    // Durability: the drop mutated the membership-index root via COW. Reopen
    // from the persisted superblock and re-assert — a drop that updated the
    // in-memory root but mis-persisted the COW'd index pages would resurrect the
    // dropped handles here, which the in-session checks above cannot catch (the
    // additive path is covered by tag_and_index_survive_reopen; this is the
    // drop path).
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(
        db.handles_with_tag(Tag::new(1).unwrap()).unwrap(),
        Vec::<Handle>::new(),
        "dropped tag-1 relation must stay empty across reopen (no resurrection)"
    );
    for h in &hs {
        assert!(
            db.read(*h).is_err(),
            "dropped chunk {h} must be unreadable after reopen"
        );
    }
    // The other relation survives the reopen with its value intact.
    assert_eq!(db.tag(keep).unwrap().unwrap(), 2);
    assert_eq!(
        db.handles_with_tag(Tag::new(2).unwrap()).unwrap(),
        vec![keep]
    );
    assert_eq!(db.read(keep).unwrap(), b"other relation");
    db.close().unwrap();
}

#[test]
fn old_database_opens_with_all_untagged() {
    // A database created before tags (simulated with plain allocate) opens with
    // tag 0 everywhere and an empty membership index -- backward compatibility.
    // This asserts the observable open-time contract; the literal pre-tag
    // zeroed-field normalization (root_membership_index_page == 0 -> PAGE_ID_NONE)
    // is unit-covered by the superblock deserialize test.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h = db.allocate(b"legacy").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        // I126: an untagged handle now reports `None`, not the old sentinel 0.
        // Tag 0 is no longer expressible (`Tag(0)` is unconstructable), so the
        // old `handles_with_tag(0)` empty-index check can't be written verbatim;
        // the "empty membership index" coverage is preserved by querying an
        // arbitrary real tag, which must have no members in a legacy database.
        assert_eq!(db.tag(h).unwrap(), None);
        assert_eq!(
            db.handles_with_tag(Tag::new(1).unwrap()).unwrap(),
            Vec::<Handle>::new()
        );
        db.close().unwrap();
    }
}

// I126: the Option<Tag> contract at the public boundary. An untagged handle
// reports None; a tagged one reports Some(Tag). The transposition guarantee for
// delete_tagged(Handle, Tag) is enforced by the type system at compile time and
// needs no runtime test.
#[test]
fn tag_of_untagged_handle_is_none() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"plain").unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap(), None);
}

#[test]
fn tag_of_tagged_handle_is_some() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate_tagged(b"row", Tag::new(42).unwrap()).unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap(), Some(Tag::new(42).unwrap()));
    assert_eq!(db.tag(h).unwrap().unwrap(), 42);
}

// I118: a tag's inner-tree root now allocates via the freemap-aware path,
// so dropping a tag fully (freeing its tree) and re-creating it reuses the
// freed page(s) rather than extending the file.
#[test]
fn recreating_a_dropped_tag_reuses_freed_pages() {
    let mut db = Chisel::open_in_memory().unwrap();
    // Establish a steady state: create tag 1, fully drop it, commit (so its
    // freed pages land in the freemap), so the NEXT create can reuse them.
    db.begin().unwrap();
    db.allocate_tagged(b"row", Tag::new(1).unwrap()).unwrap();
    db.commit().unwrap();
    db.begin().unwrap();
    loop {
        let p = db.delete_with_tag(Tag::new(1).unwrap(), 64).unwrap();
        if p.complete {
            break;
        }
    }
    db.commit().unwrap();
    let pages_after_drop = db.stats().unwrap().total_pages;
    // Re-create the same tag: with the fix, create_root reuses a freed page,
    // so total_pages must NOT grow.
    db.begin().unwrap();
    db.allocate_tagged(b"row again", Tag::new(1).unwrap())
        .unwrap();
    db.commit().unwrap();
    let pages_after_recreate = db.stats().unwrap().total_pages;
    assert!(
        pages_after_recreate <= pages_after_drop,
        "re-creating a dropped tag must reuse freed pages, not extend: \
         {pages_after_drop} -> {pages_after_recreate}"
    );
}

#[test]
fn dropping_a_deep_tag_reopens_empty() {
    // I118 deep-tree completion, durability: a tag with > SLOTS_PER_PAGE (1021)
    // handles has a multi-LEVEL inner tree, so dropping it makes `remove` free the
    // whole tree via `free_subtree` — including COMMITTED pages referenced by the
    // on-disk superblock until the drop commits. This file-backed test confirms
    // that whole-tree free PERSISTS: after a multi-batch drop + reopen, the tag is
    // empty with no resurrection of any of the multi-level tree's handles. (The
    // unit test pins free_subtree's page-freeing in-memory; this covers the
    // committed/reopen path the in-memory reuse test cannot.)
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let n = 1100u64; // > 1021 forces inner-tree depth >= 1
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        // Allocate in committed chunks so the cache stays bounded (no CacheFull).
        let mut i = 0u64;
        while i < n {
            db.begin().unwrap();
            let end = (i + 256).min(n);
            while i < end {
                db.allocate_tagged(format!("row {i}").as_bytes(), Tag::new(1).unwrap())
                    .unwrap();
                i += 1;
            }
            db.commit().unwrap();
        }
        assert_eq!(
            db.handles_with_tag(Tag::new(1).unwrap()).unwrap().len(),
            n as usize
        );

        // Drop the whole tag in bounded batches, each its own commit — this is the
        // multi-batch path where the final tree mixes this-txn and prior-committed
        // pages, all freed by free_subtree.
        loop {
            db.begin().unwrap();
            let p = db.delete_with_tag(Tag::new(1).unwrap(), 256).unwrap();
            db.commit().unwrap();
            if p.complete {
                break;
            }
        }
        assert_eq!(
            db.handles_with_tag(Tag::new(1).unwrap()).unwrap(),
            Vec::<Handle>::new()
        );
        db.close().unwrap();
    }
    // Reopen from the persisted superblock: the dropped deep tag must stay empty.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(
        db.handles_with_tag(Tag::new(1).unwrap()).unwrap(),
        Vec::<Handle>::new(),
        "a fully-dropped deep (multi-level) tag must stay empty across reopen"
    );
    db.close().unwrap();
}

// I125: a tombstoned handle is rejected as InvalidHandle by EVERY read/delete
// entry point, uniformly — the "is this handle live?" rule lives in one place
// (`lookup_live`), not in scattered per-method guards. client_byte /
// set_client_byte are pinned in tests/client_byte.rs; this pins the tag-aware
// pair the deepdive flagged as the likely-to-diverge "fourth caller": a naive
// `.tag` read off the looked-up entry would let tag() read a stale tag through a
// tombstone, and delete_tagged() would surface TagMismatch instead of
// InvalidHandle. Both must reject the dead handle before inspecting its tag.
#[test]
fn deleted_handle_is_invalid_across_all_entry_points() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate_tagged(b"row", Tag::new(7).unwrap()).unwrap();
    // Tombstone it within the same transaction (read-your-own-writes makes the
    // tombstone visible to every subsequent lookup below).
    db.delete(h).unwrap();

    // read() and tag() must reject the tombstone, not surface stale state.
    assert!(matches!(db.read(h), Err(ChiselError::InvalidHandle(_))));
    assert!(matches!(db.tag(h), Err(ChiselError::InvalidHandle(_))));
    // delete_tagged() must reject the dead handle with InvalidHandle, NOT
    // TagMismatch — it cannot have read a tag off a deleted entry.
    assert!(matches!(
        db.delete_tagged(h, Tag::new(7).unwrap()),
        Err(ChiselError::InvalidHandle(_))
    ));
    db.rollback().unwrap();
}
