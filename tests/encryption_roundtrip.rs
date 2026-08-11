// tests/encryption_roundtrip.rs — end-to-end public-API encryption contract.
//
// Documents the three-case guarantee for encrypted databases: create + write
// with a key → reopen with the SAME key reads the value back; reopen with a
// WRONG key → InvalidEncryptionKey; reopen with NO key → NoEncryptionKey.
// Also covers the in-memory encrypted path: a DB opened with
// open_in_memory_with_options + an encryption key must write and read within
// the same session (there is no reopen for in-memory DBs, so wrong-key is
// not applicable).
//
// Uses a raw 32-byte key to avoid paying the Argon2id cost. Passphrase
// derivation is exercised in the crypto unit tests. Uses only the public API
// (chisel::{Chisel, ChiselError, Key, Options}); no crate-internal paths.

use chisel::{Chisel, ChiselError, Key, Options};

// PUBLIC-API-9 (issue #119): built via `Key::raw`, the constructor a downstream
// crate can call without adding `zeroize` to its own manifest. This file cannot
// PROVE that property — Cargo hands integration tests the crate's own
// dependencies, so `zeroize::Zeroizing` would resolve here either way — but the
// public-API contract test is the right place to exercise the path we tell
// downstream users to take.
fn raw_key(b: u8) -> Key {
    Key::raw(vec![b; 32])
}

#[test]
fn encrypted_roundtrip_and_wrong_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("enc.db");

    // Create encrypted, write a value, capture the raw handle id, close.
    let raw_handle = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw_key(0xAB)))
            .expect("create encrypted");
        db.begin().expect("begin");
        let h = db.allocate(b"secret-payload").expect("allocate");
        db.commit().expect("commit");
        h.get()
    };

    // Reopen with the SAME key: value reads back.
    {
        let db = Chisel::open(
            &path,
            Options::default()
                .create_if_missing(false)
                .encryption_key(raw_key(0xAB)),
        )
        .expect("reopen with correct key");
        let v = db
            .read(chisel::Handle::from(raw_handle))
            .expect("read after reopen");
        assert_eq!(&v, b"secret-payload");
    }

    // Reopen with the WRONG key: must return InvalidEncryptionKey.
    {
        let result = Chisel::open(
            &path,
            Options::default()
                .create_if_missing(false)
                .encryption_key(raw_key(0x00)),
        );
        assert!(result.is_err(), "wrong key must fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, ChiselError::InvalidEncryptionKey),
            "expected InvalidEncryptionKey, got {err:?}"
        );
    }

    // Reopen with NO key: must return NoEncryptionKey.
    {
        let result = Chisel::open(&path, Options::default().create_if_missing(false));
        assert!(result.is_err(), "missing key must fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, ChiselError::NoEncryptionKey),
            "expected NoEncryptionKey, got {err:?}"
        );
    }
}

#[test]
fn in_memory_encrypted_roundtrip() {
    // An in-memory encrypted DB must write and read back within the same session.
    // There is no reopen for in-memory DBs, so the test covers the allocate →
    // commit → read path under encryption without touching disk.
    let mut db =
        Chisel::open_in_memory_with_options(Options::default().encryption_key(raw_key(0x7F)))
            .expect("open in-memory encrypted");

    db.begin().expect("begin");
    let h1 = db.allocate(b"in-memory-value-alpha").expect("allocate h1");
    let h2 = db.allocate(b"in-memory-value-beta").expect("allocate h2");
    db.commit().expect("commit");

    assert_eq!(db.read(h1).expect("read h1"), b"in-memory-value-alpha");
    assert_eq!(db.read(h2).expect("read h2"), b"in-memory-value-beta");

    // A second transaction: update h1, verify h2 is unchanged.
    db.begin().expect("begin 2");
    db.update(h1, b"updated-alpha").expect("update h1");
    db.commit().expect("commit 2");

    assert_eq!(db.read(h1).expect("read h1 after update"), b"updated-alpha");
    assert_eq!(
        db.read(h2).expect("read h2 unchanged"),
        b"in-memory-value-beta"
    );
}

#[test]
fn in_memory_plaintext_roundtrip_baseline() {
    // Sanity baseline: plain in-memory DB (no key) works the same way.
    // Ensures any regression in the in-memory path is distinguishable from
    // an encryption-specific failure.
    let mut db =
        Chisel::open_in_memory_with_options(Options::default()).expect("open in-memory plaintext");
    db.begin().expect("begin");
    let h = db.allocate(b"plain").expect("allocate");
    db.commit().expect("commit");
    assert_eq!(db.read(h).expect("read"), b"plain");
}

#[test]
fn file_size_bytes_matches_stat_for_both_strides() {
    // CRYPTO-5: `file_size_bytes()` / `stats().file_size_bytes` used to
    // multiply the page count by the hardcoded PAGE_SIZE. Page counts are in
    // stride-units, and an encrypted DB's stride is ENC_PAGE_SIZE (8232), so
    // every encrypted database under-reported its own size by 0.49%.
    //
    // Asserting against stat(2) rather than a hardcoded expected size is what
    // makes this test fail on the bug: PAGE_SIZE arithmetic can only match
    // stat for the plaintext case, so the encrypted arm is the real check and
    // the plaintext arm guards against over-correcting in the other direction.
    for (name, key) in [("plain.db", None), ("enc.db", Some(raw_key(0x5C)))] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);

        let mut options = Options::default();
        if let Some(k) = key {
            options = options.encryption_key(k);
        }
        let mut db = Chisel::open(&path, options).expect("open");

        // Force the file past the superblock region so a stride error is
        // several pages' worth of bytes, not a rounding coincidence.
        db.begin().expect("begin");
        for i in 0..64u32 {
            db.allocate(&vec![i as u8; 4096]).expect("allocate");
        }
        db.commit().expect("commit");

        let on_disk = std::fs::metadata(&path).expect("stat").len();
        assert_eq!(
            db.file_size_bytes().expect("file_size_bytes"),
            on_disk,
            "{name}: file_size_bytes() disagrees with stat(2)"
        );
        assert_eq!(
            db.stats().expect("stats").file_size_bytes,
            on_disk,
            "{name}: stats().file_size_bytes disagrees with stat(2)"
        );
    }
}
