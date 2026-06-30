// tests/encryption_open.rs — integration tests for the open_existing-with-key path.
//
// Exercises: correct-key round-trip, wrong-key operational error, missing-key
// error, spurious-key-on-plaintext error, and plaintext-DB regression.

use chisel::{Chisel, Key, Options};
use std::io::{Seek, SeekFrom, Write};
use zeroize::Zeroizing;

fn raw_key(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

/// Create an encrypted DB, insert a value, close, reopen with the same key,
/// verify the value is still readable.
#[test]
fn round_trip_open_with_correct_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    let handle;
    {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0x11)),
        )
        .unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"hello world").unwrap();
        db.commit().unwrap();
    }
    // Reopen with the same key: data must come back.
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .encryption_key(raw_key(0x11))
                .create_if_missing(false),
        )
        .unwrap();
        assert_eq!(db.read(handle).unwrap(), b"hello world");
    }
}

/// Wrong key must fail cleanly — not panic or poison the manager — and a
/// subsequent correct-key open must succeed (retryable error).
#[test]
fn wrong_key_is_operational_error_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0x11)),
        )
        .unwrap();
        db.begin().unwrap();
        db.commit().unwrap();
    }
    // Wrong key: must return an error.
    let err = Chisel::open(
        &path,
        Options::default()
            .encryption_key(raw_key(0x22))
            .create_if_missing(false),
    );
    assert!(err.is_err(), "wrong key must fail to open");

    // Correct key after a failed attempt: must succeed (wrong key is NOT fatal/poison).
    let ok = Chisel::open(
        &path,
        Options::default()
            .encryption_key(raw_key(0x11))
            .create_if_missing(false),
    );
    assert!(ok.is_ok(), "correct key must succeed after a wrong-key attempt");
}

/// Opening an encrypted DB without supplying a key must error.
#[test]
fn missing_key_on_encrypted_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0x11)),
        )
        .unwrap();
        db.begin().unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(&path, Options::default().create_if_missing(false));
    assert!(err.is_err(), "opening an encrypted DB without a key must fail");
}

/// Supplying a key to a plaintext DB must error.
#[test]
fn key_supplied_for_plaintext_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.chisel");
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(
        &path,
        Options::default()
            .encryption_key(raw_key(0x11))
            .create_if_missing(false),
    );
    assert!(err.is_err(), "supplying a key to a plaintext DB must fail");
}

/// Plaintext DB created and reopened without a key must still work (regression
/// guard: the version gate must not accidentally break MAJOR=1 DBs).
#[test]
fn plaintext_db_round_trips_without_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.chisel");
    let handle;
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"plain text").unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(&path, Options::default().create_if_missing(false)).unwrap();
        assert_eq!(db.read(handle).unwrap(), b"plain text");
    }
}

/// Passphrase-keyed DB round-trips: create with a passphrase, reopen with the
/// same passphrase, data is intact.
#[test]
fn passphrase_key_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("passphrase.chisel");
    let pass = || Key::Passphrase(Zeroizing::new("correct horse battery staple".to_string()));
    let handle;
    {
        let mut db =
            Chisel::open(&path, Options::default().encryption_key(pass())).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"secret").unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .encryption_key(pass())
                .create_if_missing(false),
        )
        .unwrap();
        assert_eq!(db.read(handle).unwrap(), b"secret");
    }
}

/// Regression: open an encrypted DB immediately after creation, WITHOUT any
/// user commit. Before the fix, `slot_idx = txn_counter % N = (N-1) % N = N-1`
/// pointed at the loser slot (counter 0) while the winner (counter N-1) is at
/// slot 0, causing an AAD mismatch and a spurious InvalidEncryptionKey.
#[test]
fn open_encrypted_db_with_no_commits_uses_correct_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.chisel");
    // Create with a named root set at create time (so there's something to verify
    // round-trips even without a user commit).
    {
        let _db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0x42)),
        )
        .unwrap();
        // Drop immediately — no begin/commit. This is the exact scenario the
        // create-seed inversion bug breaks: the winner slot is at page 0
        // (counter N-1) but txn_counter % N = N-1 != 0 for N=2.
    }
    // Reopen with the correct key: must not return InvalidEncryptionKey.
    let result = Chisel::open(
        &path,
        Options::default()
            .encryption_key(raw_key(0x42))
            .create_if_missing(false),
    );
    assert!(
        result.is_ok(),
        "correct key must open a never-committed encrypted DB; got: {:?}",
        result.err()
    );
}

/// Named root written under an encrypted DB must round-trip through open.
#[test]
fn named_root_round_trips_through_encrypted_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("named.chisel");
    let handle;
    {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0xAB)),
        )
        .unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"payload").unwrap();
        db.set_root_name("myroot", handle).unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .encryption_key(raw_key(0xAB))
                .create_if_missing(false),
        )
        .unwrap();
        let h = db.get_root_name("myroot").unwrap();
        assert!(h.is_some(), "named root must survive close+reopen");
        assert_eq!(db.read(h.unwrap()).unwrap(), b"payload");
    }
}

/// Uniform-stride regression (N>2, never committed): with the encryption spec's
/// uniform 8232 stride, a 4-superblock encrypted DB's file must be exactly
/// 4 * 8232 bytes from birth so page_count == total_pages == 4 on reopen. Under
/// the old PAGE_SIZE-stride-then-switch layout the file was 4 * 8192, which is
/// not a multiple of 8232, so the open-time page_count (file_len/8232 = 3 < 4)
/// would spuriously raise FileSizeMismatch. N=4 (not the default N=2) is chosen
/// because integer-division masked the N=2 case at exactly one boundary.
#[test]
fn never_committed_encrypted_db_with_extra_superblocks_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n4.chisel");
    {
        let _db = Chisel::open(
            &path,
            Options::default()
                .encryption_key(raw_key(0x55))
                .superblock_count(4),
        )
        .unwrap();
        // Drop with no begin/commit — exercise the create-only file layout.
    }
    let reopened = Chisel::open(
        &path,
        Options::default()
            .encryption_key(raw_key(0x55))
            .create_if_missing(false),
    );
    assert!(
        reopened.is_ok(),
        "never-committed N=4 encrypted DB must reopen; got: {:?}",
        reopened.err()
    );
}

/// Uniform-stride regression (multi-page, committed): allocate a value large
/// enough to force overflow data pages, commit, reopen, and read it back. This
/// drives the cold-load seal/open path across several data pages AND confirms
/// the committed file (superblock slots + data pages, all at 8232 stride) stays
/// a clean multiple of the stride so reopen's total_pages check passes.
#[test]
fn multi_page_encrypted_value_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.chisel");
    // 32 KiB payload spans multiple 8 KiB pages via the overflow chain.
    let payload: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();
    let handle;
    {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0x77)),
        )
        .unwrap();
        db.begin().unwrap();
        handle = db.allocate(&payload).unwrap();
        db.commit().unwrap();
    }
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .encryption_key(raw_key(0x77))
                .create_if_missing(false),
        )
        .unwrap();
        assert_eq!(
            db.read(handle).unwrap(),
            payload,
            "multi-page encrypted value must survive close+reopen byte-for-byte"
        );
    }
}

/// R4 crash-recovery for encrypted DBs: a torn write to slot 0 must still open
/// via a valid sibling slot. This exercises the uniform-stride bootstrap's
/// torn-slot-0 fallback: page 0 fails to deserialize, so open_existing cannot
/// learn the stride from it and must speculatively retry at the encrypted
/// stride to read sibling slots (which sit at their true 8232-byte offsets) and
/// recover the correct DEK + state. Without that fallback the correct key would
/// spuriously fail to open a recoverable file.
#[test]
fn torn_slot_0_encrypted_db_recovers_via_sibling() {
    // ENC_PAGE_SIZE is not part of the public API; encode the on-disk slot
    // stride locally. A mismatch would make the seek miss slot 0 and the test
    // would (correctly) fail loudly rather than silently pass.
    const ENC_STRIDE: u64 = 8232;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.chisel");
    let h1;
    {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0x99)),
        )
        .unwrap();
        // Two commits so BOTH slots (N=2) hold valid post-commit superblocks:
        // commit 1 → slot 0, commit 2 → slot 1. After corrupting slot 0,
        // recovery must fall back to slot 1.
        db.begin().unwrap();
        h1 = db.allocate(b"survives the tear").unwrap();
        db.commit().unwrap();
        db.begin().unwrap();
        let _h2 = db.allocate(b"second commit").unwrap();
        db.commit().unwrap();
    }
    // Simulate a torn write to slot 0: zero its first PAGE_SIZE bytes so the
    // page-0 image fails to deserialize (anchor cannot learn the stride).
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; 8192]).unwrap();
        f.sync_all().unwrap();
    }
    // The file must still be a clean multiple of the encrypted stride (the
    // corruption only overwrote bytes, did not change the length).
    let len = std::fs::metadata(&path).unwrap().len();
    assert_eq!(
        len % ENC_STRIDE,
        0,
        "encrypted file length {len} must stay a multiple of {ENC_STRIDE}"
    );
    // Recovery must open via the intact sibling slot 1 and expose commit-2
    // state, which still resolves the commit-1 handle h1.
    let db = Chisel::open(
        &path,
        Options::default()
            .encryption_key(raw_key(0x99))
            .create_if_missing(false),
    )
    .expect("correct key must recover a torn-slot-0 encrypted DB via its sibling");
    assert_eq!(db.read(h1).unwrap(), b"survives the tear");
}
