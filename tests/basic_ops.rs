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

// Handle 0 is reserved as the "no handle" sentinel; the allocator must never
// mint it. On a fresh store the first handle is 1, and ids increase from there.
// Dual-backed so both the on-disk superblock (file) and the in-memory roots
// (memory) seed the counter past the reserved value.
fn test_handle_zero_is_reserved_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let h1 = db.allocate(b"first").unwrap();
    let h2 = db.allocate(b"second").unwrap();
    db.commit().unwrap();
    assert_ne!(
        h1, 0,
        "handle 0 is the reserved no-handle sentinel and must never be minted"
    );
    assert_eq!(h1, 1, "the first handle on a fresh store must be 1");
    assert_eq!(h2, 2, "handles increase monotonically from 1");
    db.close().unwrap();
}

dual_backing_test!(
    test_handle_zero_is_reserved,
    test_handle_zero_is_reserved_body
);

// next_handle must persist across a close/reopen: it rides the superblock, not
// a handle-table scan, so a regression that reset it (or read it from a stale
// slot) would re-mint a still-live or retired handle — silent corruption. This
// guards that a post-reopen allocation strictly exceeds every pre-reopen handle
// and is never 0. File-backed only (reopen is meaningless for the in-memory
// backing, which loses all state on drop).
#[test]
fn next_handle_persists_across_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let max_before;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        let mut hs = Vec::new();
        for i in 0u8..10 {
            hs.push(db.allocate(&[i]).unwrap());
        }
        db.commit().unwrap();
        // Retire a couple — retired handles must not be reused either.
        db.begin().unwrap();
        db.delete(hs[3]).unwrap();
        db.delete(hs[7]).unwrap();
        db.commit().unwrap();
        max_before = *hs.iter().max().unwrap();
        db.close().unwrap();
    }
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"after-reopen").unwrap();
    db.commit().unwrap();
    assert_ne!(h, 0, "handle 0 is the reserved no-handle sentinel");
    assert!(
        h > max_before,
        "next_handle must persist across reopen: got {h}, but the max handle \
         before reopen was {max_before} — re-minting a live or retired handle \
         is silent corruption"
    );
}
