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

use chisel::Chisel;
use common::{open_chisel, Backing};
use tempfile::NamedTempFile;

fn tag_survives_in_session_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h = db.allocate_tagged(b"relation row", 77).unwrap();
    db.commit().unwrap();
    assert_eq!(db.tag(h).unwrap(), 77);
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
        h = db.allocate_tagged(b"survive", 9).unwrap();
        db.allocate_tagged(b"survive2", 9).unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.tag(h).unwrap(), 9);
        assert_eq!(db.handles_with_tag(9).unwrap().len(), 2);
        db.close().unwrap();
    }
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
            db.allocate_tagged(format!("row {i}").as_bytes(), 1)
                .unwrap(),
        );
    }
    // A second relation under a different tag must survive the tag-1 drop intact.
    let keep = db.allocate_tagged(b"other relation", 2).unwrap();
    db.commit().unwrap();
    assert_eq!(db.handles_with_tag(1).unwrap().len(), 200);

    // Drop the whole tag-1 relation in bounded batches; accumulate the reported
    // handles to confirm every member is accounted for exactly once.
    db.begin().unwrap();
    let mut dropped = Vec::new();
    loop {
        let p = db.delete_with_tag(1, 64).unwrap();
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
        db.handles_with_tag(1).unwrap(),
        Vec::<u64>::new(),
        "no orphan index entries remain for the dropped tag"
    );
    for h in &hs {
        assert!(db.read(*h).is_err(), "dropped chunk {h} must be unreadable");
    }
    // The untouched relation is fully intact.
    assert_eq!(db.tag(keep).unwrap(), 2);
    assert_eq!(db.handles_with_tag(2).unwrap(), vec![keep]);
    db.close().unwrap();
}

#[test]
fn old_database_opens_with_all_untagged() {
    // A database created before tags (simulated with plain allocate) opens with
    // tag 0 everywhere and an empty membership index -- backward compatibility.
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
        assert_eq!(db.tag(h).unwrap(), 0);
        assert_eq!(db.handles_with_tag(0).unwrap(), Vec::<u64>::new());
        db.close().unwrap();
    }
}
