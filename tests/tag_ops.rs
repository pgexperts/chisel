//! Integration tests for chunk tags: in-session readback (both backings),
//! durability of tags + the membership index across close/reopen, the F1/I12
//! relation-drop contract (`delete_with_tag` removes every handle of a tag with
//! no orphans, leaving other tags intact and the dropped chunks unreadable), and
//! backward compatibility with pre-tag databases (everything reads as tag 0).
//!
//! Page-level reclamation (freed pages returning to the freemap for reuse) is
//! deliberately NOT asserted here: file-size deltas are dominated by COW
//! write-amplification of the radix structures, not by data-page frees, so they
//! are a poor signal. That property is covered by focused freemap unit tests
//! (`src/freemap.rs`, `persist_freemap_*` in `src/transaction.rs`). The F1/I12
//! concern these tests guard is orphan-handle integrity, which is deterministic.
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
    assert!(db.read(a).is_err(), "chunk gone after delete_tagged");
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
        assert!(db.read(*h).is_err(), "dropped chunk {h} must be unreadable");
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
        // old `handles_with_tag(0)` empty-index check is removed — "untagged"
        // is fully captured by `tag(h) == None`.
        assert_eq!(db.tag(h).unwrap(), None);
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
