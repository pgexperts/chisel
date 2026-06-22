// recovery_tests.rs — Crash-recovery integration tests that need direct
// access to internal types (Superblock, PageType, page format constants)
// for corruption injection. Lives in src/ rather than tests/ because the
// I35 pub→pub(crate) reshape locks these internals down; integration
// tests no longer have access. The tests still exercise end-to-end
// recovery scenarios — they're "integration tests with internals
// access" not "unit tests of a single function".
#![cfg(test)]

use crate::page::{self, PageType, PAGE_SIZE};
use crate::superblock::Superblock;
use crate::{Chisel, ChiselError, Options};
use std::fs;
use std::io::{Read as _, Seek, SeekFrom, Write};
use tempfile::{NamedTempFile, TempDir};

/// Scan the file for the first page whose type tag matches `want` and
/// return its page id. Panics if no such page exists; test helper only.
///
/// This is used by the targeted corruption tests below so they can pick
/// exactly one Overflow or Data page to scribble on, instead of the
/// blunt "zero everything" approach.
fn find_page_of_type(path: &std::path::Path, want: PageType) -> u64 {
    let mut f = fs::File::open(path).unwrap();
    let len = f.metadata().unwrap().len();
    let n_pages = len / PAGE_SIZE as u64;
    // Slots 0..N are superblocks — they do not carry the common type tag,
    // so we skip them unconditionally. Chisel's DEFAULT_SUPERBLOCK_COUNT
    // is 2; the caller is responsible for not using this helper on
    // non-default superblock layouts.
    for p in 2..n_pages {
        f.seek(SeekFrom::Start(p * PAGE_SIZE as u64)).unwrap();
        let mut first = [0u8; 1];
        f.read_exact(&mut first).unwrap();
        if first[0] == want as u8 {
            return p;
        }
    }
    panic!("no page of type {want:?} found in {path:?}");
}

/// Read a page, mutate its bytes via `mutate`, restamp the checksum so
/// the page looks checksum-valid to the cache layer, and write it back.
/// Used to produce "structurally broken but checksum-valid" pages — the
/// class of corruption that layered validators have to catch.
fn rewrite_page_with_valid_checksum(
    path: &std::path::Path,
    page_id: u64,
    mutate: impl FnOnce(&mut [u8; PAGE_SIZE]),
) {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut buf = [0u8; PAGE_SIZE];
    f.seek(SeekFrom::Start(page_id * PAGE_SIZE as u64)).unwrap();
    f.read_exact(&mut buf).unwrap();
    mutate(&mut buf);
    page::stamp_checksum(&mut buf);
    f.seek(SeekFrom::Start(page_id * PAGE_SIZE as u64)).unwrap();
    f.write_all(&buf).unwrap();
    f.sync_all().unwrap();
}

/// Read the superblock slots (default layout: 2) and return the winning one,
/// so a corruption test can target the LIVE root page rather than a stale COW
/// copy that `find_page_of_type` might return.
fn active_superblock(path: &std::path::Path) -> Superblock {
    let mut f = fs::File::open(path).unwrap();
    let mut bufs = [[0u8; PAGE_SIZE]; crate::superblock::DEFAULT_SUPERBLOCK_COUNT as usize];
    for (i, b) in bufs.iter_mut().enumerate() {
        f.seek(SeekFrom::Start(i as u64 * PAGE_SIZE as u64))
            .unwrap();
        f.read_exact(b).unwrap();
    }
    Superblock::select(&bufs).expect("a valid winning superblock")
}

// A corrupt (checksum-valid) handle-table interior root whose child-0 cycles
// must NOT hang Chisel::open — open_existing calls HandleTable::recover_depth,
// whose depth cap turns the cycle into a typed fatal error. Regression for the
// deepdive review's radix corrupt-page finding (open-time hang vector).
#[test]
fn test_recovery_cyclic_handle_table_spine_is_rejected_not_hung() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        // > ENTRIES_PER_LEAF (510) handles forces the handle table to depth >= 1,
        // so its root is an interior page with a child-0 spine.
        for i in 0..600u64 {
            db.allocate(&i.to_le_bytes()).unwrap();
        }
        db.commit().unwrap();
    }
    let root = active_superblock(&path).root_handle_table_page;
    // Point the live root's child-0 at itself: a cyclic interior spine.
    rewrite_page_with_valid_checksum(&path, root, |buf| {
        buf[page::DATA_PAGE_HEADER_SIZE..page::DATA_PAGE_HEADER_SIZE + 8]
            .copy_from_slice(&root.to_le_bytes());
    });
    // Pre-fix: open hangs forever in recover_depth. Post-fix: it returns a fatal
    // typed error promptly.
    match Chisel::open(&path, Default::default()) {
        Err(e) => assert!(e.is_fatal(), "expected a fatal error, got {e:?}"),
        Ok(_) => panic!("Chisel::open accepted a cyclic handle-table spine"),
    }
}

// A corrupt (checksum-valid) outer-leaf packed value claiming an out-of-range
// inner-tree depth must surface a typed CorruptPage from the read path, not a
// panic / misleading downstream error.
#[test]
fn test_recovery_corrupt_membership_inner_depth_is_corrupt_page() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate_tagged(b"row", crate::Tag::new(7).unwrap())
            .unwrap();
        db.commit().unwrap();
    }
    // The outer (tag) tree is depth 0: its root IS the outer leaf, packing tag
    // 7's inner (depth:6 | root:58) at slot (7 % 1021 = 7).
    let outer = active_superblock(&path).root_membership_index_page;
    rewrite_page_with_valid_checksum(&path, outer, |buf| {
        let off = page::DATA_PAGE_HEADER_SIZE + 7 * 8;
        let packed = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        let corrupt = (packed & ((1u64 << 58) - 1)) | (30u64 << 58); // inner depth = 30
        buf[off..off + 8].copy_from_slice(&corrupt.to_le_bytes());
    });
    let db = Chisel::open(&path, Default::default()).unwrap();
    match db.handles_with_tag(crate::Tag::new(7).unwrap()) {
        Err(ChiselError::CorruptPage { .. }) => {}
        other => panic!("expected CorruptPage, got {other:?}"),
    }
}

#[test]
fn test_recovery_after_clean_close() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"durable data").unwrap();
        db.commit().unwrap();
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"durable data");
}

#[test]
fn test_recovery_uncommitted_data_lost() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let committed_handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        committed_handle = db.allocate(b"committed").unwrap();
        db.commit().unwrap();
        db.begin().unwrap();
        db.allocate(b"uncommitted").unwrap();
        // Drop without commit — simulates crash.
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(committed_handle).unwrap(), b"committed");
    assert!(db
        .read(crate::Handle::from(committed_handle.get() + 1))
        .is_err());
}

#[test]
fn test_recovery_corrupt_superblock_b() {
    // Besides proving slot-A fallback, this test pins a layout fact that
    // sibling tests depend on: a single first user commit lands in slot 0
    // (page 0), leaving slot 1 (page 1) as the still-empty fallback. That
    // "first commit -> page 0" invariant is what lets the tagged-index and
    // torn-second-superblock tests below corrupt page 1 to target the NEWER
    // commit. If commit-slot rotation ever changes parity, this test breaks
    // first and flags the assumption for the others.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"safe").unwrap();
        db.commit().unwrap();
    }
    // Corrupt superblock B (page 1) by zeroing it.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"safe");
}

#[test]
fn test_recovery_tagged_index_recovers_from_prior_superblock() {
    // The membership-index root (root_membership_index_page) rides the superblock
    // and swaps atomically with the handle-table / freemap roots on every commit.
    // Prove that atomicity for the index: commit a PRIOR state (tag 9 -> {h1}),
    // then a NEWER state (tag 9 -> {h1, h2}) which lands in the other superblock
    // slot. Corrupt the ACTIVE slot (page 1, the newer commit's slot — see
    // test_recovery_corrupt_superblock_b, which establishes that the first commit
    // lands in page 0). Recovery must reject the torn active slot, fall back to the
    // prior slot, and rebuild the membership index DEPTH (RadixU64::recover_depth)
    // from ITS root — so reads of that root see only the prior relation (h1),
    // never a half-torn mix.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let h1;
    let h2;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h1 = db
            .allocate_tagged(b"row1", crate::Tag::new(9).unwrap())
            .unwrap();
        db.commit().unwrap(); // prior state -> superblock slot A (page 0)
        db.begin().unwrap();
        h2 = db
            .allocate_tagged(b"row2", crate::Tag::new(9).unwrap())
            .unwrap();
        db.commit().unwrap(); // newer state -> superblock slot B (page 1), now active
    }
    // Corrupt the active superblock slot (page 1) by zeroing it.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    // Recovery falls back to the prior slot; its index holds only h1, and the
    // membership read path (recover_depth + handles_for_tag) returns it cleanly.
    // Teeth: if recovery had instead kept the corrupted active slot, this would
    // be [h1, h2] (the counterfactual: corrupting page 0 makes it fail that way).
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(
        db.handles_with_tag(crate::Tag::new(9).unwrap()).unwrap(),
        vec![h1]
    );
    assert_eq!(db.tag(h1).unwrap().unwrap(), 9);
    // h2 existed only in the corrupted newer state -> gone after recovery.
    assert!(db.read(h2).is_err());
}

#[test]
fn test_recovery_crash_between_data_fsync_and_superblock_fsync() {
    // commit_inner fsyncs ALL of a transaction's data pages (step 1) BEFORE it
    // writes and fsyncs the new superblock (steps 3-4). A crash in that window
    // leaves the new transaction's data pages fully durable on disk while the
    // superblock still points at the PRIOR committed state. The shadow-paging
    // contract: recovery returns cleanly to that prior state — the in-flight
    // transaction is lost ATOMICALLY (never half-applied), and the
    // written-but-unreferenced data pages are harmless orphans that the file's
    // larger-than-total_pages length is explicitly designed to tolerate
    // (open_existing rewinds next_page_id so they are overwritten, not leaked).
    //
    // We reproduce that exact on-disk state without a fault-injecting IO layer:
    // snapshot the superblock region after committing state A, run commit B
    // (which durably writes B's data pages AND advances the superblock to B),
    // then roll the superblock region back to A's bytes — leaving B's data
    // pages behind exactly as a crash before the superblock fsync would. This is
    // the inverse of test_recovery_superblock_pointing_past_eof_is_rejected (a
    // superblock ahead of the data is rejected; data ahead of the superblock
    // recovers cleanly).
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let ha;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        ha = db.allocate(b"state-A-value").unwrap();
        db.commit().unwrap();
    }

    // Snapshot the superblock region (the first `superblock_count` pages — read
    // the count from the winning slot rather than hardcoding the default).
    let sb_pages = active_superblock(&path).superblock_count as usize;
    let sb_region = {
        let mut f = fs::File::open(&path).unwrap();
        let mut buf = vec![0u8; sb_pages * PAGE_SIZE];
        f.read_exact(&mut buf).unwrap();
        buf
    };

    let hb;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        hb = db.allocate(b"state-B-value-must-be-lost").unwrap();
        db.commit().unwrap();
    }
    // Sanity: B genuinely extended the file past A's watermark (so the recovered
    // open really does face a longer-than-total_pages file, the orphan case).
    let len_after_b = fs::metadata(&path).unwrap().len();
    assert!(
        len_after_b > (sb_pages as u64 + 1) * PAGE_SIZE as u64,
        "B should have written data pages past the superblock region"
    );

    // Roll the superblock region back to A, keeping B's data pages on disk —
    // the precise state a crash between the data fsync and the superblock fsync
    // leaves behind.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&sb_region).unwrap();
        f.sync_all().unwrap();
    }

    // Recovery must land on state A: ha intact, B's transaction gone, and a
    // single live handle (B's handle never entered the recovered table).
    {
        let db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.read(ha).unwrap(), b"state-A-value");
        assert!(
            db.read(hb).is_err(),
            "state-B transaction must be lost atomically, not half-applied"
        );
        assert_eq!(
            db.handles().unwrap().len(),
            1,
            "recovered table must hold only state A's handle"
        );
    }

    // The database is fully usable after recovery — a fresh commit succeeds and
    // survives a reopen, proving B's orphaned pages did not corrupt the freemap
    // or the allocator. next_handle rewound to A, so the new handle reuses B's
    // id (and overwrites B's now-orphaned data page).
    let hc;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        hc = db.allocate(b"state-C-after-recovery").unwrap();
        db.commit().unwrap();
        assert_eq!(
            hc, hb,
            "next_handle must have rewound to state A on recovery"
        );
    }
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(ha).unwrap(), b"state-A-value");
    assert_eq!(db.read(hc).unwrap(), b"state-C-after-recovery");
}

#[test]
fn test_recovery_torn_first_commit() {
    // Regression test for ISSUES.md I2: historically `create_new` left
    // superblock slot 1 as an all-zero buffer. The very first user commit
    // writes slot 0 (txn_counter=2, even parity), overwriting the only
    // valid superblock on disk — so a torn write during that first commit
    // destroyed every recoverable state. The fix initializes both slots
    // with valid staggered-counter superblocks so slot 1 is always a
    // legitimate fallback.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    // Fresh database — no commits yet. Both slots should be valid.
    {
        let _db = Chisel::open(&path, Default::default()).unwrap();
    }

    // Simulate a torn write to slot 0 before any successful commit.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }

    // Must open successfully via the slot-1 fallback. Database is empty.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.handles().unwrap().len(), 0);
}

#[test]
fn test_recovery_both_superblocks_corrupt() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"doomed").unwrap();
        db.commit().unwrap();
    }
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }
    let result = Chisel::open(&path, Default::default());
    assert!(result.is_err());
}

#[test]
fn test_recovery_corrupted_overflow_page_surfaces_fatal_error() {
    // Targeted: find an Overflow-typed page on disk and zero it so its
    // checksum fails. The read path must walk the overflow chain and
    // hit the broken page via `cache.get()`, which validates on load
    // and raises `ChecksumMismatch` — a fatal error that poisons the
    // manager.
    //
    // This differs from the blunt "zero everything" version by leaving
    // the handle table intact, so the failure mode is unambiguously the
    // overflow walker tripping over a bad checksum rather than some
    // earlier integrity check catching the damage first.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        // 20 KB forces a multi-page overflow chain (~3 pages at 8 KiB
        // minus per-page overhead), so at least one page carries the
        // Overflow type tag.
        handle = db.allocate(&vec![0xAB; 20_000]).unwrap();
        db.commit().unwrap();
    }

    let overflow_page = find_page_of_type(&path, PageType::Overflow);
    // Zero the page. Checksum goes bad along with the rest, which is
    // exactly the bit-rot scenario we want to catch.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(overflow_page * PAGE_SIZE as u64))
            .unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }

    let db = Chisel::open(&path, Default::default())
        .expect("open should still succeed — only an overflow page is damaged");
    let err = db
        .read(handle)
        .expect_err("read of a broken overflow chain must fail");
    match err {
        ChiselError::ChecksumMismatch { page_id } => {
            assert_eq!(page_id, overflow_page, "wrong page id in error");
        }
        other => panic!("expected ChecksumMismatch on the overflow page, got {other:?}"),
    }
}

#[test]
fn test_read_of_structurally_broken_data_page_returns_corrupt_page() {
    // Regression test for the transaction.rs read path upgrade: when
    // the handle-table entry says a slot is Live but the target data
    // page is structurally nonsense (wrong page-type tag, inconsistent
    // slot directory, etc.), the read must surface `CorruptPage`
    // rather than masquerading as `InvalidHandle`.
    //
    // Why this matters: `InvalidHandle` is Operational — callers will
    // treat it as "oh, my handle is stale, move on". `CorruptPage` is
    // Fatal and poisons the manager so the caller is forced to stop
    // and reopen. Misclassifying corruption as a bad handle silently
    // hides real damage behind a caller-blamed error.
    //
    // Construction: stamp the data page's type byte to 0xFF (not a
    // valid PageType) and refresh the checksum so the cache layer
    // lets the bytes through. `DataPage::validate_header` then
    // rejects the page, `DataPage::read` returns None, and the
    // transaction layer must convert that None into CorruptPage.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"alive but doomed").unwrap();
        db.commit().unwrap();
    }

    let data_page = find_page_of_type(&path, PageType::Data);
    rewrite_page_with_valid_checksum(&path, data_page, |buf| {
        buf[0] = 0xFF; // not any PageType discriminant
    });

    let db = Chisel::open(&path, Default::default())
        .expect("open scans the handle table only, not data pages");
    let err = db.read(handle).expect_err("read must fail");
    match err {
        ChiselError::CorruptPage { page_id } => {
            assert_eq!(page_id, data_page, "CorruptPage error had wrong page id");
        }
        ChiselError::InvalidHandle(_) => panic!(
            "structurally-broken data page was misclassified as InvalidHandle; \
             the transaction.rs Live-slot upgrade is missing"
        ),
        other => panic!("expected CorruptPage, got {other:?}"),
    }
}

#[test]
fn test_recovery_truncated_file_is_rejected() {
    // A file shorter than the superblock region cannot possibly hold a
    // valid database. Open must refuse — the exact error shape depends
    // on which read fails first (InvalidPageId from the bounds check in
    // page_io, or CorruptSuperblock if the bytes parse as zeros), so we
    // accept any error but verify it is NOT success.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"x").unwrap();
        db.commit().unwrap();
    }
    // Chop the file down to a single page — slot 1 no longer exists.
    {
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(PAGE_SIZE as u64).unwrap();
    }
    // `Chisel` is not `Debug`, so unwrap_err/expect_err won't compile — match.
    let err = match Chisel::open(&path, Default::default()) {
        Ok(_) => panic!("truncated file was accepted by Chisel::open"),
        Err(e) => e,
    };
    // Pin the failure mode, not just "some error": a truncated file must be
    // rejected with a *fatal*, typed integrity error — not an operational
    // error and not a panic-y generic IoError. The exact variant depends on
    // which read fails first.
    assert!(
        matches!(
            err,
            ChiselError::InvalidPageId { .. }
                | ChiselError::CorruptSuperblock { .. }
                | ChiselError::FileSizeMismatch { .. }
        ),
        "expected a typed integrity error, got {err:?}"
    );
    assert!(
        err.is_fatal(),
        "truncation rejection must be a fatal error, got {err:?}"
    );
}

#[test]
fn test_recovery_superblock_pointing_past_eof_is_rejected() {
    // A valid-looking superblock whose `total_pages` exceeds the
    // physical file length is a crash symptom: the superblock fsync
    // committed the new metadata but the data-page writes that were
    // supposed to extend the file never landed. Open must surface
    // `FileSizeMismatch` rather than trusting the stale metadata and
    // reading past EOF.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"seed").unwrap();
        db.commit().unwrap();
    }

    // Write a valid superblock into slot 0 that claims the file is
    // much longer than it actually is. total_pages is usize-y here;
    // the real file length is just a handful of pages.
    let sb = Superblock {
        magic: page::MAGIC,
        format_version: page::FORMAT_VERSION,
        txn_counter: u64::MAX / 2, // ensure this slot wins select()
        root_handle_table_page: page::PAGE_ID_NONE,
        root_freemap_page: page::PAGE_ID_NONE,
        total_pages: 1_000_000,
        next_handle: 0,
        page_size: PAGE_SIZE as u32,
        named_roots: [crate::superblock::NamedRoot::EMPTY; crate::superblock::NAMED_ROOT_COUNT],
        superblock_count: crate::superblock::DEFAULT_SUPERBLOCK_COUNT,
        root_membership_index_page: page::PAGE_ID_NONE,
    };
    let buf = sb.serialize();
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&buf).unwrap();
        // Also stomp slot 1 so `select()` has no alternative but the
        // one we just wrote.
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }

    match Chisel::open(&path, Default::default()) {
        Err(ChiselError::FileSizeMismatch { expected, actual }) => {
            assert_eq!(expected, 1_000_000 * PAGE_SIZE as u64);
            assert!(actual < expected);
        }
        Err(other) => panic!("expected FileSizeMismatch, got {other:?}"),
        Ok(_) => panic!("expected FileSizeMismatch, open succeeded"),
    }
}

#[test]
fn test_recovery_torn_second_superblock_falls_back_to_previous() {
    // Models the precise crash window the commit protocol is designed
    // to survive: data pages for commit N+1 are fully fsync'd (step 1
    // of the protocol), then the superblock write or its fsync is
    // interrupted partway through so the inactive slot is left with a
    // garbage/zeroed payload.
    //
    // The old superblock is still intact and points at the post-commit-N
    // roots. Recovery must pick it via Superblock::select()'s "highest
    // valid txn_counter wins" rule and expose the post-commit-N state —
    // the data pages written for commit N+1 are orphaned but harmless.
    //
    // With DEFAULT_SUPERBLOCK_COUNT = 2, slot 0 is written by the first
    // user commit (txn_counter bumps from 1 → 2, even parity) and slot 1
    // by the second (3, odd parity). We corrupt slot 1 AFTER a second
    // commit to simulate the torn write.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let h1;
    let h2;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        h1 = db.allocate(b"from commit 1").unwrap();
        db.commit().unwrap();

        db.begin().unwrap();
        h2 = db.allocate(b"from commit 2").unwrap();
        db.commit().unwrap();
    }

    // Corrupt slot 1 (the most-recent commit) by zeroing it. Slot 0
    // still carries the post-commit-#1 superblock.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }

    // Recovery should pick slot 0 and expose the state from commit #1.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(h1).unwrap(), b"from commit 1");
    // h2 was only referenced by the now-lost slot-1 superblock; it must
    // not resolve against the post-commit-#1 handle table.
    assert!(
        db.read(h2).is_err(),
        "handle from the torn commit should not be visible after recovery"
    );
}

#[test]
fn test_reject_unsupported_format_version() {
    // Regression test for ISSUES.md I15: a superblock with a valid checksum
    // but a MAJOR-incompatible format_version must be refused with a
    // distinct error, not silently accepted. Post-I29 the gate is
    // major-only (`format_major(sb.format_version) != FORMAT_MAJOR_VERSION`),
    // so we forge a future MAJOR rather than a flat "+99" — the old +99
    // would now be a same-major MINOR bump and (correctly) succeed.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    // Create a fresh database (initializes both superblock slots at the
    // current MAJOR.MINOR).
    {
        let _db = Chisel::open(&path, Default::default()).unwrap();
    }

    // Overwrite BOTH slots with valid superblocks at a future MAJOR.
    // We must overwrite both — if we leave one at the current major,
    // select() would pick whichever had the higher counter. Using
    // identical-shape superblocks with controlled counters here
    // guarantees the future-major slot wins.
    let future_major = page::pack_format_version(page::FORMAT_MAJOR_VERSION.wrapping_add(1), 0);
    let mut sb = Superblock {
        magic: page::MAGIC,
        format_version: future_major,
        txn_counter: 5,
        root_handle_table_page: page::PAGE_ID_NONE,
        root_freemap_page: page::PAGE_ID_NONE,
        total_pages: 2,
        next_handle: 0,
        page_size: PAGE_SIZE as u32,
        named_roots: [crate::superblock::NamedRoot::EMPTY; crate::superblock::NAMED_ROOT_COUNT],
        superblock_count: crate::superblock::DEFAULT_SUPERBLOCK_COUNT,
        root_membership_index_page: page::PAGE_ID_NONE,
    };
    let buf_a = sb.serialize();
    sb.txn_counter = 4;
    let buf_b = sb.serialize();
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&buf_a).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&buf_b).unwrap();
        f.sync_all().unwrap();
    }

    match Chisel::open(&path, Default::default()) {
        Err(ChiselError::UnsupportedFormatVersion { found, expected }) => {
            assert_eq!(found, future_major);
            assert_eq!(expected, page::FORMAT_VERSION);
        }
        Err(other) => panic!("expected UnsupportedFormatVersion, got error {:?}", other),
        Ok(_) => panic!("expected UnsupportedFormatVersion, but open succeeded"),
    }
}

#[test]
fn test_read_takes_shared_reference() {
    // Regression test for ISSUES.md F3. `Chisel::read`, `handles`, and
    // `stats` must take `&self` so that downstream callers with a
    // shared reference (e.g. a `StorageEngine` trait whose read methods
    // are `&self`) don't need a `RefCell<Chisel>` wrapper. The test
    // compiles iff the signatures are correct.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle = {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"via shared ref").unwrap();
        db.commit().unwrap();
        h
    };

    let db = Chisel::open(&path, Default::default()).unwrap();

    // Hand out a shared reference and prove we can do all read-side work
    // through it without any interior wrapper.
    fn read_only_probe(db: &Chisel, h: crate::Handle) -> Vec<u8> {
        assert!(!db.is_poisoned());
        let _ = db.handles().unwrap();
        let _ = db.stats().unwrap();
        db.read(h).unwrap()
    }

    assert_eq!(read_only_probe(&db, handle), b"via shared ref");
    // Reading twice through the same shared ref also works.
    assert_eq!(read_only_probe(&db, handle), b"via shared ref");
}

#[test]
fn test_poison_recovery_by_reopen() {
    // End-to-end test for the ISSUES.md I1 poison recovery idiom. After
    // a poisoned handle is dropped and the database is reopened, data
    // committed before the poison must still be readable.
    //
    // This test cannot directly inject a fatal I/O error through the
    // public API, so it exercises the recovery path: commit some data,
    // then verify that a fresh open after drop sees that data. The
    // in-process poison semantics are covered by the unit tests in
    // transaction.rs.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle = {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        assert!(!db.is_poisoned());
        db.begin().unwrap();
        let h = db.allocate(b"survives recovery").unwrap();
        db.commit().unwrap();
        h
        // Drop here — simulates the user's response to a poisoned handle.
    };

    // Reopen: the shadow-paging recovery path picks the last durable
    // superblock, which references the committed value.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert!(!db.is_poisoned());
    assert_eq!(db.read(handle).unwrap(), b"survives recovery");
}

// --- F2: named roots ---

#[test]
fn test_named_roots_survive_commit_and_reopen() {
    // The core F2 use case: bind a name to a handle, commit, reopen the
    // database, and verify the name still resolves to the same handle.
    // This is what lets the client replace the "handle 0 is always meta"
    // convention with an explicit name → handle mapping.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let meta_handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        meta_handle = db.allocate(b"meta btree root state").unwrap();
        db.set_root_name("meta", meta_handle).unwrap();
        db.commit().unwrap();
    }

    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(meta_handle));
    assert_eq!(db.read(meta_handle).unwrap(), b"meta btree root state");
    // Unbound names must return Ok(None), not an error.
    assert_eq!(db.get_root_name("does_not_exist").unwrap(), None);
}

#[test]
fn test_named_roots_revert_on_rollback() {
    // A named-root mutation must revert when the enclosing transaction
    // is rolled back — the whole point of F2 (from the client's
    // perspective) is that named roots are transactional.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h1 = db.allocate(b"initial").unwrap();
    db.set_root_name("meta", h1).unwrap();
    db.commit().unwrap();

    // In a new transaction, change the binding and then roll back.
    db.begin().unwrap();
    let h2 = db.allocate(b"replacement").unwrap();
    db.set_root_name("meta", h2).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h2));
    db.rollback().unwrap();

    // After rollback, the committed binding must be restored.
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h1));
}

#[test]
fn test_named_roots_revert_on_rollback_to_savepoint() {
    // Savepoints must also snapshot and restore named roots — because
    // Savepoint captures a Roots clone, this is automatic if named_roots
    // is part of Roots. This test guards against someone refactoring
    // Roots and accidentally leaving named_roots out.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h1 = db.allocate(b"a").unwrap();
    db.set_root_name("meta", h1).unwrap();
    db.savepoint("sp").unwrap();

    let h2 = db.allocate(b"b").unwrap();
    db.set_root_name("meta", h2).unwrap();
    db.set_root_name("index", h2).unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h2));
    assert_eq!(db.get_root_name("index").unwrap(), Some(h2));

    db.rollback_to("sp").unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), Some(h1));
    assert_eq!(db.get_root_name("index").unwrap(), None);
    db.commit().unwrap();
}

#[test]
fn test_named_roots_validation_errors() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();

    // Empty name.
    assert!(matches!(
        db.set_root_name("", crate::Handle::from(0)),
        Err(ChiselError::InvalidRootName)
    ));
    // Too long (> 24 bytes).
    let too_long = "a".repeat(25);
    assert!(matches!(
        db.set_root_name(&too_long, crate::Handle::from(0)),
        Err(ChiselError::InvalidRootName)
    ));
    // Contains NUL.
    assert!(matches!(
        db.set_root_name("bad\0name", crate::Handle::from(0)),
        Err(ChiselError::InvalidRootName)
    ));
    // Max length (exactly 24 bytes) is valid.
    let exactly_24 = "a".repeat(24);
    assert!(db
        .set_root_name(&exactly_24, crate::Handle::from(42))
        .is_ok());
    assert_eq!(
        db.get_root_name(&exactly_24).unwrap(),
        Some(crate::Handle::from(42))
    );
    db.commit().unwrap();
}

#[test]
fn test_named_roots_table_full() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();

    // Fill all 8 slots. The literal 8 is coupled to superblock::NAMED_ROOT_COUNT
    // (the on-disk named-root array is fixed-size, [NamedRoot; 8]); if that
    // constant ever changes this bound must move with it, or the "table full"
    // assertion below stops testing the boundary.
    for i in 0..8 {
        let name = format!("r{i}");
        db.set_root_name(&name, crate::Handle::from(i as u64))
            .unwrap();
    }
    // Overwriting an existing name must still work even when full.
    db.set_root_name("r3", crate::Handle::from(999)).unwrap();
    assert_eq!(
        db.get_root_name("r3").unwrap(),
        Some(crate::Handle::from(999))
    );

    // Adding a NEW name must fail.
    assert!(matches!(
        db.set_root_name("overflow", crate::Handle::from(1234)),
        Err(ChiselError::RootNameTableFull)
    ));

    // Clearing a slot must reopen capacity.
    db.clear_root_name("r0").unwrap();
    assert!(db
        .set_root_name("new_one", crate::Handle::from(1234))
        .is_ok());
    assert_eq!(
        db.get_root_name("new_one").unwrap(),
        Some(crate::Handle::from(1234))
    );
    db.commit().unwrap();
}

#[test]
fn test_named_roots_clear_is_idempotent() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    db.set_root_name("meta", crate::Handle::from(7)).unwrap();
    db.clear_root_name("meta").unwrap();
    assert_eq!(db.get_root_name("meta").unwrap(), None);
    // Clearing a non-existent name is a no-op, not an error.
    db.clear_root_name("meta").unwrap();
    db.clear_root_name("never_set").unwrap();
    db.commit().unwrap();
}

// --- R2 / I9 / I10 / I11 / F1 / I12: the freemap bundle ---

#[test]
fn test_delete_then_allocate_reuses_pages() {
    // R2: after deletion, the freed data pages must become available to
    // subsequent allocations. The observable signature is that a
    // delete-then-reallocate cycle grows the file only by the
    // handle-table COW churn (~1 page per mutation), NOT by the full
    // data-page cost.
    //
    // Without R2, every allocate extends the file by 1 data page + 1
    // ht-cow page = ~2N pages for N allocations. With R2, the data
    // pages come from the freemap and we only pay ~N pages for ht
    // churn plus 1 for the freemap-page rewrite. So for N = 20:
    // without-R2 ≈ 40 new pages, with-R2 ≈ 21 new pages. Asserting
    // growth < 30 comfortably discriminates without being flaky to
    // minor commit-path accounting.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();

    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..20 {
        handles.push(db.allocate(format!("value-{i}").as_bytes()).unwrap());
    }
    db.commit().unwrap();

    db.begin().unwrap();
    for &h in &handles {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();
    let pages_after_delete = db.stats().unwrap().total_pages;

    db.begin().unwrap();
    for i in 0..20 {
        db.allocate(format!("replacement-{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after_reuse = db.stats().unwrap().total_pages;
    let growth = pages_after_reuse - pages_after_delete;
    assert!(
        growth < 30,
        "delete+reallocate grew file by {growth} pages; without R2 this would be ~40+, with R2 data pages are reused (only ht+freemap churn remains)"
    );
}

#[test]
fn test_update_does_not_leak_old_inline_page() {
    // I9: updating an inline handle many times must not grow the file
    // linearly. Pre-I9 each update leaked the old data page AND
    // allocated a new one, so the file grew by 1 data page + ht-cow
    // per update. Post-I9 the old data page goes to txn_freed_pages
    // and the next commit's allocations reuse it, so per-update growth
    // is dominated by ht-cow churn (~1 page) and the freemap page
    // rewrite.
    //
    // Each update is its own transaction here. With 50 updates and no
    // reuse, growth would be ~100 pages. With reuse, it's ~50-75 pages
    // (ht cow + freemap rewrite per commit). Asserting < 90 comfortably
    // discriminates.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handle = db.allocate(b"initial").unwrap();
    db.commit().unwrap();
    let pages_after_initial = db.stats().unwrap().total_pages;

    for i in 0..50 {
        db.begin().unwrap();
        db.update(handle, format!("round-{i}").as_bytes()).unwrap();
        db.commit().unwrap();
    }
    let pages_after_updates = db.stats().unwrap().total_pages;
    let growth = pages_after_updates - pages_after_initial;
    assert!(
        growth < 90,
        "50 updates grew the file by {growth} pages; without I9 this would be ~100+, with I9 old data pages are reused"
    );
    assert_eq!(db.read(handle).unwrap(), b"round-49");
}

#[test]
fn test_rollback_after_delete_does_not_free_pages() {
    // The freemap semantics under rollback: if a transaction deletes a
    // handle and then rolls back, the deletion is undone AND the old
    // data page must NOT have been added to the committed freemap.
    // Otherwise a subsequent transaction could reuse the page while
    // committed_roots still references it, overwriting live data.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handle = db.allocate(b"keep me").unwrap();
    db.commit().unwrap();

    // Delete + rollback: the delete's freeing should not become durable.
    db.begin().unwrap();
    db.delete(handle).unwrap();
    db.rollback().unwrap();

    // Handle must still be readable — the rollback restored it.
    assert_eq!(db.read(handle).unwrap(), b"keep me");

    // Allocate again: the new page must NOT collide with `handle`'s
    // data page. Reading `handle` after the allocation still gives the
    // original content.
    db.begin().unwrap();
    db.allocate(b"new value").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(handle).unwrap(), b"keep me");
}

#[test]
fn test_delete_many_atomicity() {
    // F1 / I12: delete_many runs inside a single transaction, so
    // rolling back discards the whole batch.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handles: Vec<crate::Handle> = (0..10)
        .map(|i| db.allocate(format!("h{i}").as_bytes()).unwrap())
        .collect();
    db.commit().unwrap();

    // Delete them all, then rollback — every handle must survive.
    db.begin().unwrap();
    db.delete_many(&handles).unwrap();
    db.rollback().unwrap();
    for (i, &h) in handles.iter().enumerate() {
        assert_eq!(db.read(h).unwrap(), format!("h{i}").as_bytes());
    }

    // Delete them all and commit — every handle must be gone, and the
    // freemap should now contain their pages.
    db.begin().unwrap();
    db.delete_many(&handles).unwrap();
    db.commit().unwrap();
    for &h in &handles {
        assert!(matches!(db.read(h), Err(ChiselError::InvalidHandle(_))));
    }

    // A subsequent bulk allocation should reuse the freed data pages.
    // Growth is bounded by ht-cow (~10 pages) + freemap page (~1), so
    // well under the ~20 pages that would be needed without reuse.
    let pages_before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..10 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;
    assert!(
        growth < 15,
        "bulk delete_many + re-allocate grew file by {growth} pages; without reuse this would be ~20+"
    );
}

#[test]
fn test_r1_small_values_pack_into_data_pages() {
    // R1: many small values in a single transaction must share data
    // pages instead of each getting its own. The observable signature
    // is dramatically lower file-page growth per value than the
    // pre-R1 "one value per page" model.
    //
    // For 100 small values (~16 bytes each, well under 8KB), pre-R1
    // would create 100 data pages + 100 ht-cow pages + 1 freemap
    // page ≈ 201 new pages. Post-R1, 100 values pack into ~1 data
    // page (or maybe 2 near a boundary) + 100 ht-cow + 1 freemap
    // ≈ 102 new pages. Asserting growth < 150 comfortably
    // discriminates.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    let pages_before = db.stats().unwrap().total_pages;
    db.begin().unwrap();
    for i in 0..100 {
        db.allocate(format!("v{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after = db.stats().unwrap().total_pages;
    let growth = pages_after - pages_before;
    assert!(
        growth < 150,
        "100 small values grew the file by {growth} pages; without R1 this would be ~200+, with R1 most values share data pages"
    );
}

#[test]
fn test_r1_packed_values_are_readable_after_reopen() {
    // Sanity: after R1 packs many values into the same data page and
    // commits, the values must all still be readable by handle (and
    // after a reopen, since the live-slot count map is rebuilt from
    // the handle-table scan).
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handles: Vec<crate::Handle>;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handles = (0..50)
            .map(|i| db.allocate(format!("packed-{i}").as_bytes()).unwrap())
            .collect();
        db.commit().unwrap();
    }

    let db = Chisel::open(&path, Default::default()).unwrap();
    for (i, &h) in handles.iter().enumerate() {
        assert_eq!(db.read(h).unwrap(), format!("packed-{i}").as_bytes());
    }
}

#[test]
fn test_r1_deleting_last_slot_frees_the_page() {
    // Under R1, a data page holding multiple slots is freed to the
    // freemap only when the LAST live slot is deleted. This test
    // allocates several values (which pack into one data page),
    // deletes all but one — the page must remain allocated — and
    // then deletes the last one — the page must now be free.
    //
    // The observable signature: after deleting all values and doing a
    // subsequent allocate, the file should NOT grow significantly
    // because the previously-packed page is reusable.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let handles: Vec<crate::Handle> = (0..5)
        .map(|i| db.allocate(format!("s{i}").as_bytes()).unwrap())
        .collect();
    db.commit().unwrap();
    let pages_after_alloc = db.stats().unwrap().total_pages;

    // Delete all five in one transaction — the last delete should
    // free the shared data page back to the freemap.
    db.begin().unwrap();
    for &h in &handles {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();

    // Allocate 5 more: they should reuse the freed packed page.
    db.begin().unwrap();
    for i in 0..5 {
        db.allocate(format!("r{i}").as_bytes()).unwrap();
    }
    db.commit().unwrap();
    let pages_after_reuse = db.stats().unwrap().total_pages;
    let growth = pages_after_reuse - pages_after_alloc;
    assert!(
        growth < 15,
        "re-allocate after bulk delete grew file by {growth} pages; packed page should be reusable"
    );
}

#[test]
fn test_r1_rollback_reverts_slot_counts() {
    // If a transaction inserts several packed values and rolls back,
    // the live-slot counts must revert so the next transaction sees
    // a clean slate.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h1 = db.allocate(b"survive-1").unwrap();
    let h2 = db.allocate(b"survive-2").unwrap();
    db.commit().unwrap();

    // Pack more values, then rollback.
    db.begin().unwrap();
    let _ = db.allocate(b"rolled-back-1").unwrap();
    let _ = db.allocate(b"rolled-back-2").unwrap();
    db.rollback().unwrap();

    // The pre-rollback handles must still be readable, AND
    // allocating new values must not behave as if the rolled-back
    // inserts left phantom slot counts.
    assert_eq!(db.read(h1).unwrap(), b"survive-1");
    assert_eq!(db.read(h2).unwrap(), b"survive-2");

    db.begin().unwrap();
    let h3 = db.allocate(b"after-rollback").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h3).unwrap(), b"after-rollback");
}

#[test]
fn test_freemap_persists_across_reopen() {
    // The freemap must round-trip through the superblock's
    // root_freemap_page. After a reopen, freed pages from the previous
    // session must still be reusable.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle_to_delete;
    let pages_after_delete;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        // Allocate many so the freed page is definitely inside the
        // active range, not tail garbage.
        for i in 0..5 {
            db.allocate(format!("keep-{i}").as_bytes()).unwrap();
        }
        handle_to_delete = db.allocate(b"delete me").unwrap();
        db.commit().unwrap();

        db.begin().unwrap();
        db.delete(handle_to_delete).unwrap();
        db.commit().unwrap();
        pages_after_delete = db.stats().unwrap().total_pages;
    }

    // Reopen and allocate: the freed page should be reused via the
    // freemap loaded from disk. Growth is bounded by 1 ht-cow + 1
    // freemap page ≈ 2, not 3 (1 data + 1 ht + 1 freemap) that we'd
    // pay without reuse.
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    db.allocate(b"reuse after reopen").unwrap();
    db.commit().unwrap();
    let pages_after_reopen_alloc = db.stats().unwrap().total_pages;
    let growth = pages_after_reopen_alloc - pages_after_delete;
    assert!(
        growth < 3,
        "after reopen, an allocation should reuse the freemap-tracked page (growth {growth})"
    );
}

// --- R4: configurable superblock count ---

#[test]
fn test_r4_create_with_custom_superblock_count() {
    // Creating a database with N=4 superblocks should work end-to-end
    // and the file should be large enough to hold all four slots.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    {
        let mut db = Chisel::open(&path, Options::default().superblock_count(4)).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"r4 payload").unwrap();
        db.commit().unwrap();
        assert_eq!(db.read(h).unwrap(), b"r4 payload");
    }

    // Reopen: N must be discovered from disk, not supplied again.
    // We pass the default (N=2) to show it's ignored for existing DBs.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.handles().unwrap().len(), 1);
}

#[test]
fn test_r4_invalid_superblock_count_rejected() {
    // N=1 is disqualified (no redundancy).
    let file = NamedTempFile::new().unwrap();
    let result = Chisel::open(file.path(), Options::default().superblock_count(1));
    assert!(matches!(
        result,
        Err(ChiselError::InvalidSuperblockCount { value: 1 })
    ));

    // N=17 exceeds MAX_SUPERBLOCKS (16).
    let file2 = NamedTempFile::new().unwrap();
    let result = Chisel::open(file2.path(), Options::default().superblock_count(17));
    assert!(matches!(
        result,
        Err(ChiselError::InvalidSuperblockCount { value: 17 })
    ));
}

#[test]
fn test_r4_n3_survives_two_consecutive_torn_writes() {
    // The motivating use case: N=3 should survive TWO consecutive
    // torn superblock writes (a torn commit followed by a torn
    // retry). We simulate this by creating a DB with N=3, doing
    // several commits so counters spread across all three slots,
    // then manually zeroing TWO of the slots and verifying the
    // third still recovers a valid database.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let handle;
    {
        let mut db = Chisel::open(&path, Options::default().superblock_count(3)).unwrap();
        // Do enough commits that each of the 3 slots has been
        // written at least once, so all three carry real data.
        db.begin().unwrap();
        handle = db.allocate(b"survives two tears").unwrap();
        db.commit().unwrap(); // txn=3 → slot 0
        db.begin().unwrap();
        db.allocate(b"txn 2").unwrap();
        db.commit().unwrap(); // txn=4 → slot 1
        db.begin().unwrap();
        db.allocate(b"txn 3").unwrap();
        db.commit().unwrap(); // txn=5 → slot 2
    }

    // Zero out slots 0 and 1 (two consecutive torn writes). Slot 2
    // — which holds the most recent commit — remains intact.
    {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
        f.sync_all().unwrap();
    }

    // Reopen: under N=2 this would be unrecoverable. Under N=3 we
    // fall back to slot 2 and the database is still openable with
    // the most recent committed state.
    let db = Chisel::open(&path, Default::default()).unwrap();
    assert_eq!(db.read(handle).unwrap(), b"survives two tears");
    assert_eq!(db.handles().unwrap().len(), 3);
}

#[test]
fn test_r4_commits_rotate_through_all_slots() {
    // With N=3, committing 6 times should write slots in the
    // sequence 0, 1, 2, 0, 1, 2. We verify indirectly by checking
    // that the file doesn't grow an extra page on every commit —
    // all commits reuse the fixed bank of N slots.
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    let mut db = Chisel::open(&path, Options::default().superblock_count(3)).unwrap();

    // First commit: adds handle-table + data + freemap pages.
    db.begin().unwrap();
    db.allocate(b"first").unwrap();
    db.commit().unwrap();
    let baseline = db.stats().unwrap().total_pages;

    // Do 5 more commits that each update the same handle. Under R1
    // packing, these all share the cursor page within each txn, so
    // the file should stay close to baseline.
    db.begin().unwrap();
    let h = db.allocate(b"packed").unwrap();
    db.commit().unwrap();
    for i in 0..4 {
        db.begin().unwrap();
        db.update(h, format!("update {i}").as_bytes()).unwrap();
        db.commit().unwrap();
    }
    let after = db.stats().unwrap().total_pages;

    // The superblock slot bank is fixed at N=3 pages; subsequent
    // commits rotate through those without allocating new slots.
    // Any growth is attributable to ht-cow + freemap, NOT to
    // additional superblock slots.
    assert!(
        after - baseline < 20,
        "5 commits under N=3 shouldn't grow the file significantly (baseline={baseline}, after={after})"
    );
}

#[test]
fn test_r4_default_is_two_slots() {
    // Default Options creates a 2-slot database, matching the
    // pre-R4 layout. Verified by the total_pages count after
    // creation: pages 0 and 1 are superblocks and nothing else
    // exists until the first user commit adds data.
    let file = NamedTempFile::new().unwrap();
    let db = Chisel::open(file.path(), Default::default()).unwrap();
    assert_eq!(db.stats().unwrap().total_pages, 2);
}

#[test]
fn test_file_not_found_without_create() {
    let path = std::path::PathBuf::from("/tmp/chisel_nonexistent_test.db");
    let _ = fs::remove_file(&path);
    let result = Chisel::open(&path, Options::default().create_if_missing(false));
    assert!(result.is_err());
}

// I115: corrupt the 4-byte magic (superblock offset 0..4) in EVERY superblock
// slot, re-stamping the checksum so the magic check (not the checksum check) is
// the failing gate. Superblock::deserialize then returns None for every slot,
// Superblock::select returns None, and open surfaces CorruptSuperblock. This
// PINS CorruptSuperblock as the sole expected variant AND proves InvalidMagic is
// unreachable (a bad magic never produces it) — the basis for removing the dead
// variant in the next task.
#[test]
fn corrupt_magic_surfaces_as_corrupt_superblock_not_invalid_magic() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.chisel");
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        db.allocate(b"x").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    for slot in 0..crate::superblock::DEFAULT_SUPERBLOCK_COUNT as u64 {
        rewrite_page_with_valid_checksum(&path, slot, |buf| {
            buf[0] ^= 0xFF; // flip a magic byte; magic lives at bytes 0..4
        });
    }
    match Chisel::open(&path, Default::default()) {
        Err(ChiselError::CorruptSuperblock { defects }) => {
            // I106: the real superblock slots (0 and 1) must be reported as
            // BadMagic — we corrupted the magic while keeping the checksum valid.
            for slot in 0..crate::superblock::DEFAULT_SUPERBLOCK_COUNT {
                assert!(
                    defects.iter().any(|d| d.slot == slot
                        && d.defect == crate::superblock::SuperblockDefect::BadMagic),
                    "slot {slot} should be reported BadMagic, got {defects:?}"
                );
            }
        }
        Err(e) => panic!("bad magic must surface as CorruptSuperblock, got {e:?}"),
        Ok(_) => panic!("Chisel::open accepted a fully-corrupted-magic file"),
    };
}
