// basic_ops.rs — End-to-end public-API smoke tests.
//
// As of 2026-05-22 (I35 reshape) every internal-touching test that used
// to live here moved into the corresponding src/*.rs module's #[cfg(test)]
// mod (page, superblock, page_io, page_cache, freemap, data_page,
// handle_table). What remains is the public-API surface: round-trip of
// allocate/read/update/delete, durability across reopen, and the stats()
// shape. Dual-backed where it makes sense via dual_backing_test!.

mod common;
use common::{open_chisel, Backing};

use chisel::Chisel;
use tempfile::NamedTempFile;

// --- Chisel public API tests ---

fn test_chisel_public_api_roundtrip_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"value one").unwrap();
    let h2 = db.allocate(b"value two").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"value one");
    assert_eq!(db.read(h2).unwrap(), b"value two");
    db.begin().unwrap();
    db.update(h1, b"updated").unwrap();
    db.delete(h2).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"updated");
    assert!(db.read(h2).is_err());
    db.close().unwrap();
}

dual_backing_test!(
    test_chisel_public_api_roundtrip,
    test_chisel_public_api_roundtrip_body
);

#[test]
fn test_chisel_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"survive reopen").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.read(handle).unwrap(), b"survive reopen");
        db.close().unwrap();
    }
}

fn test_chisel_stats_body(b: &Backing) {
    let mut db = open_chisel(b);
    let stats = db.stats().unwrap();
    assert_eq!(stats.handle_count, 0);
    db.begin().unwrap();
    db.allocate(b"one").unwrap();
    db.allocate(b"two").unwrap();
    db.commit().unwrap();
    let stats = db.stats().unwrap();
    assert_eq!(stats.handle_count, 2);
}

dual_backing_test!(test_chisel_stats, test_chisel_stats_body);
