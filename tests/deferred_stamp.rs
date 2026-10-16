// deferred_stamp.rs — end-to-end durability tests for the deferred data-page
// checksum stamp (#138).
//
// Role: the packing cursor no longer restamps a data page after every value;
// the page cache stamps it once, when its bytes leave the cache (the commit
// flush, or an eviction to the spillway whose blob the drain later copies
// verbatim to the main file). A path that skips that stamp commits a page whose
// checksum is stale, and the failure is invisible until a COLD read — which
// then raises a fatal `ChecksumMismatch` and poisons the handle. So every test
// here commits through a tiny, spilling cache, reopens the file (empty cache),
// and reads every value back.
//
// The workload is shaped to hit both enforcement points: small values pack
// many-per-page, so each data page is the cursor for dozens of inserts; a
// 4-page cache forces each finished cursor page (stale, flagged) out to the
// spillway; interleaved reads of older values evict the LIVE cursor too, so it
// is spilled, rehydrated, and packed into again; and the final cursor is still
// resident at commit, so it goes through flush Phase 1a.

use chisel::{Chisel, Handle, Key, Options, Zeroizing};
use std::path::Path;

const CACHE_PAGES: u64 = 4;
const VALUE_LEN: usize = 100;

fn opts() -> Options {
    let cache = CACHE_PAGES * 8192;
    Options::default()
        .cache_max_bytes(cache)
        .spillway_max_bytes(4096 * cache)
}

fn raw_key() -> Key {
    Key::Raw(Zeroizing::new(vec![0x5A; 32]))
}

fn value(seed: usize) -> Vec<u8> {
    (0..VALUE_LEN)
        .map(|i| (seed.wrapping_mul(31) ^ i) as u8)
        .collect()
}

fn spillway_len(path: &Path) -> u64 {
    let mut p = path.as_os_str().to_owned();
    p.push(".spillway");
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// One transaction of `count` packed inserts starting at `seed_base`, with a
/// burst of reads of older values every 50 inserts (to evict the live cursor
/// page) and a few updates (which pack the replacement into the cursor too).
/// Returns the expected handle → value map entries it produced.
fn packed_transaction(
    db: &mut Chisel,
    path: &Path,
    seed_base: usize,
    count: usize,
    expected: &mut Vec<(Handle, Vec<u8>)>,
) {
    db.begin().unwrap();
    let first_new = expected.len();
    for i in 0..count {
        let v = value(seed_base + i);
        let h = db.allocate(&v).unwrap();
        expected.push((h, v));
        if i % 50 == 49 {
            // Touch values spread across older pages: with a 4-page cache this
            // pushes every resident page — the live cursor included — to the
            // spillway, so the next insert rehydrates the cursor.
            let span = expected.len() - first_new;
            for j in (first_new..expected.len()).step_by(span.div_ceil(8)) {
                let (h, want) = &expected[j];
                assert_eq!(&db.read(*h).unwrap(), want, "in-txn read of #{j}");
            }
        }
    }
    // Updates relocate their value into the current cursor page.
    for j in (first_new..expected.len()).step_by(97) {
        let v = value(1_000_000 + seed_base + j);
        db.update(expected[j].0, &v).unwrap();
        expected[j].1 = v;
    }
    assert!(
        spillway_len(path) > 0,
        "precondition: the workload must spill mid-transaction, or the spill \
         path is not under test"
    );
    db.commit().unwrap();
}

fn assert_cold_reads(path: &Path, options: Options, expected: &[(Handle, Vec<u8>)]) {
    let db = Chisel::open(path, options).unwrap();
    for (i, (h, want)) in expected.iter().enumerate() {
        match db.read(*h) {
            Ok(got) => assert_eq!(&got, want, "value #{i} (handle {h:?}) changed"),
            Err(e) => panic!("cold read of value #{i} (handle {h:?}) failed: {e:?}"),
        }
    }
}

fn run(options: Options) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("deferred.chisel");
    let mut expected = Vec::new();
    {
        let mut db = Chisel::open(&path, options.clone()).unwrap();
        packed_transaction(&mut db, &path, 0, 1500, &mut expected);
        db.close().unwrap();
    }
    assert_cold_reads(&path, options.clone(), &expected);

    // A second packed transaction over a reopened database, so the cursor
    // pages are freemap-reused ids as well as file extensions.
    {
        let mut db = Chisel::open(&path, options.clone()).unwrap();
        let victims: Vec<Handle> = expected.iter().step_by(3).map(|(h, _)| *h).collect();
        db.begin().unwrap();
        for h in &victims {
            db.delete(*h).unwrap();
        }
        db.commit().unwrap();
        expected.retain(|(h, _)| !victims.contains(h));
        packed_transaction(&mut db, &path, 50_000, 1000, &mut expected);
        db.close().unwrap();
    }
    assert_cold_reads(&path, options, &expected);
}

#[test]
fn packed_values_survive_spill_drain_and_cold_reopen() {
    run(opts());
}

#[test]
fn packed_values_survive_spill_drain_and_cold_reopen_encrypted() {
    run(opts().encryption_key(raw_key()));
}
