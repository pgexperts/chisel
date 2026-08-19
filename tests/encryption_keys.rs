//! Integration tests for Chisel::add_key, Chisel::rotate_key, and Chisel::remove_key.
//!
//! Each test uses the public API only (Key / Chisel / ChiselError / Options).
//! The underlying DEK is never re-generated, so add_key / rotate_key are pure
//! superblock operations: no page is touched, data survives every credential change.

use chisel::Key;
use chisel::{Chisel, ChiselError, Options};
use tempfile::TempDir;
use zeroize::Zeroizing;

fn raw(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

// ── add_key ──────────────────────────────────────────────────────────────────

/// After add_key, the original key and the new key both open the database and
/// decrypt the same data (the DEK is shared between slots).
#[test]
fn add_key_lets_either_credential_open() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret").unwrap();
        db.commit().unwrap();
        db.add_key(&raw(1), &raw(2)).unwrap();
        db.close().unwrap();
        h
    };

    // Original key still decrypts.
    let db1 = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    assert_eq!(db1.read(h).unwrap(), b"secret");
    db1.close().unwrap();

    // New key also decrypts the same data (same DEK, different slot).
    let db2 = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert_eq!(db2.read(h).unwrap(), b"secret");
    db2.close().unwrap();
}

/// Wrong `existing` key returns InvalidEncryptionKey; the database is unmodified.
#[test]
fn add_key_wrong_existing_is_invalid_encryption_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    let err = db.add_key(&raw(9), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// Filling all 8 key slots returns NoFreeKeySlot on the ninth attempt.
#[test]
fn add_key_full_table_is_no_free_key_slot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Slot 0 is occupied by raw(1) at open; add keys 2..=8 to fill the other 7.
    for k in 2u8..=8 {
        db.add_key(&raw(1), &raw(k)).unwrap();
    }
    let err = db.add_key(&raw(1), &raw(99)).unwrap_err();
    assert!(
        matches!(err, ChiselError::NoFreeKeySlot),
        "expected NoFreeKeySlot, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// add_key on a plaintext database returns EncryptionNotSupported.
#[test]
fn add_key_plaintext_db_returns_encryption_not_supported() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let err = db.add_key(&raw(1), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::EncryptionNotSupported),
        "expected EncryptionNotSupported, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

// ── rotate_key ───────────────────────────────────────────────────────────────

/// After rotate_key(old, new): old no longer opens, new does, and data is intact.
#[test]
fn rotate_key_revokes_old_and_admits_new() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"data").unwrap();
        db.commit().unwrap();
        db.rotate_key(&raw(1), &raw(2)).unwrap();
        db.close().unwrap();
        h
    };

    // Old key is now refused.
    let err = Chisel::open(&path, Options::default().encryption_key(raw(1)))
        .err()
        .expect("old key must be rejected after rotate");
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );

    // New key opens and data is readable.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert!(!db.is_poisoned());
    assert_eq!(db.read(h).unwrap(), b"data");
    db.close().unwrap();
}

/// rotate_key on a plaintext database returns EncryptionNotSupported.
#[test]
fn rotate_key_plaintext_db_returns_encryption_not_supported() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let err = db.rotate_key(&raw(1), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::EncryptionNotSupported),
        "expected EncryptionNotSupported, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// rotate_key with a wrong `old` key returns InvalidEncryptionKey.
#[test]
fn rotate_key_wrong_old_is_invalid_encryption_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    let err = db.rotate_key(&raw(9), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// rotate_key when the slot table is full (no room to stage new) returns
/// NoFreeKeySlot.  The old slot is NOT pre-cleared to make room, since that
/// would create a zero-key window on crash.
#[test]
fn rotate_key_full_table_is_no_free_key_slot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Fill all 8 slots — slot 0 is raw(1), add 7 more.
    for k in 2u8..=8 {
        db.add_key(&raw(1), &raw(k)).unwrap();
    }
    // Full table: rotate must refuse rather than clear old first.
    let err = db.rotate_key(&raw(1), &raw(99)).unwrap_err();
    assert!(
        matches!(err, ChiselError::NoFreeKeySlot),
        "expected NoFreeKeySlot, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

// ── remove_key ───────────────────────────────────────────────────────────────

/// After remove_key the revoked credential is rejected at open; all other
/// credentials continue to decrypt the same data (DEK is shared across slots).
#[test]
fn remove_key_leaves_others_working() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"v").unwrap();
        db.commit().unwrap();
        db.add_key(&raw(1), &raw(2)).unwrap();
        db.remove_key(&raw(1)).unwrap(); // drop the first credential
        db.close().unwrap();
        h
    };
    // raw(1) is gone — open must fail.
    let err = Chisel::open(&path, Options::default().encryption_key(raw(1)))
        .err()
        .expect("old key must be rejected after remove");
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    // raw(2) still opens and reads the original data.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert_eq!(db.read(h).unwrap(), b"v");
    db.close().unwrap();
}

/// remove_key with the only active credential returns LastKeySlot and leaves
/// the database intact (the rejected op must not mutate anything).
#[test]
fn remove_last_key_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Only one active slot — removing it would permanently brick the database.
    let err = db.remove_key(&raw(1)).unwrap_err();
    assert!(
        matches!(err, ChiselError::LastKeySlot),
        "expected LastKeySlot, got {err:?}"
    );
    assert!(!db.is_poisoned());
    // Reopen with the same key to confirm nothing was mutated.
    drop(db);
    let db2 = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    assert!(!db2.is_poisoned());
    db2.close().unwrap();
}

/// remove_key with a key that unlocks no slot returns InvalidEncryptionKey.
#[test]
fn remove_unknown_key_is_invalid_encryption_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    db.add_key(&raw(1), &raw(2)).unwrap();
    let err = db.remove_key(&raw(9)).unwrap_err();
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

/// remove_key on a plaintext database returns EncryptionNotSupported.
#[test]
fn remove_key_plaintext_db_returns_encryption_not_supported() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default()).unwrap();
    let err = db.remove_key(&raw(1)).unwrap_err();
    assert!(
        matches!(err, ChiselError::EncryptionNotSupported),
        "expected EncryptionNotSupported, got {err:?}"
    );
    assert!(!db.is_poisoned());
}

// ── read-only handles (issue #179) ───────────────────────────────────────────

/// On a read-only handle every key operation must refuse OPERATIONALLY: the
/// handle stays usable and reads keep working.
///
/// This is a regression test for issue #179, and the defect it pins is subtle
/// enough to be worth stating: the error VARIANT was always correct. What was
/// wrong was the side effect. `rewrite_crypto_header` poisons on any `Err` from
/// its inner half, and that inner half opened with a `cache.flush()` whose
/// trailing fsync is unconditional — so `PageIo::fsync`'s read-only guard
/// turned an operational `ReadOnlyMode` into a permanently dead manager, and
/// every later call returned `Poisoned`. The `is_poisoned` and `read`
/// assertions below are therefore the load-bearing ones; the `matches!` lines
/// passed even before the fix.
///
/// `remove_key` is deliberately absent: this database has a single active slot,
/// so `remove_key` returns `LastKeySlot` before it ever reaches the choke point
/// under test, and would prove nothing.
#[test]
fn key_ops_on_a_read_only_handle_refuse_without_poisoning() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
        h
    };

    let mut db = Chisel::open(
        &path,
        Options::default().encryption_key(raw(1)).read_only(true),
    )
    .expect("an encrypted database must open read-only");

    let err = db.add_key(&raw(1), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::ReadOnlyMode),
        "expected ReadOnlyMode, got {err:?}"
    );
    assert!(
        !db.is_poisoned(),
        "ReadOnlyMode is an operational error; add_key must not poison the handle"
    );
    // The contract operational errors carry: the handle is still usable.
    assert_eq!(db.read(h).unwrap(), b"secret");

    let err = db.rotate_key(&raw(1), &raw(2)).unwrap_err();
    assert!(
        matches!(err, ChiselError::ReadOnlyMode),
        "expected ReadOnlyMode, got {err:?}"
    );
    assert!(
        !db.is_poisoned(),
        "ReadOnlyMode is an operational error; rotate_key must not poison the handle"
    );
    assert_eq!(db.read(h).unwrap(), b"secret");
}

// ── revocation durability against a torn slot (TESTS-CI-5, issue #111) ───────
//
// The key-slot table is CLEARTEXT at bytes 332..1356 of every superblock image,
// and the per-DB DEK never changes across a credential rotation. So a sibling
// slot still carrying the PRE-rotation table is not merely stale metadata — it
// is a live, unwrappable copy of the current DEK, usable by anyone holding the
// revoked credential. `rotate_key` scrubs every sibling for exactly this reason.
//
// Nothing pinned that. `tests/encryption_open.rs` already proves the engine will
// happily recover from a torn slot via its sibling, so the failure mode is one
// zeroed 8 KB region away: rotate, tear the winning slot, and see whether the
// old key gets back in.

/// On-disk stride of one encrypted page unit (8192 image + 16 tag + 24 nonce).
/// `ENC_PAGE_SIZE` is not public API, so it is restated here — the assertion at
/// the end of the test fails loudly if the real stride ever diverges.
const ENC_STRIDE: u64 = 8232;

/// Zero the 8192-byte superblock image inside slot `slot`'s unit, leaving the
/// trailing pad bytes and the file length untouched. Zeroed bytes fail
/// `verify_checksum`, so `Superblock::select` treats the slot as torn and falls
/// back to a sibling — the same mechanism
/// `encryption_open.rs::torn_slot_0_encrypted_db_recovers_via_sibling` uses.
fn tear_slot(path: &std::path::Path, slot: u64) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(slot * ENC_STRIDE)).unwrap();
    f.write_all(&[0u8; 8192]).unwrap();
    f.sync_all().unwrap();
}

/// A revoked key must STAY revoked when the slot that recorded the revocation
/// is torn away and recovery falls back to a sibling.
///
/// Slot arithmetic matters here and is easy to get backwards. `create_new`
/// seeds slot `i` at counter `N-1-i`, so an N=2 database starts with
/// `txn_counter = 1`. One commit takes it to 2 (slot `2 % 2 = 0`); `rotate_key`
/// takes it to 3 (slot `3 % 2 = 1`). **The rotation lands in slot 1**, so slot 1
/// is the one to tear — tearing slot 0 would destroy the STALE slot and the test
/// would pass for the wrong reason, proving nothing.
#[test]
fn rotated_key_stays_revoked_after_the_winning_slot_is_torn() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    let h = {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret").unwrap();
        db.commit().unwrap();
        db.rotate_key(&raw(1), &raw(2)).unwrap();
        db.close().unwrap();
        h
    };

    let len_before = std::fs::metadata(&path).unwrap().len();
    tear_slot(&path, 1);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        len_before,
        "tearing a slot must only change bytes, never the file length"
    );
    assert_eq!(
        len_before % ENC_STRIDE,
        0,
        "file length must be a whole number of encrypted page units; if this \
         fails, ENC_STRIDE here has diverged from the engine's real stride and \
         the tear hit the wrong offset"
    );

    // THE ASSERTION THAT MATTERS. Recovery now falls back to slot 0, whose
    // counter is lower and whose crypto header predates the rotation. Without
    // the sibling scrub, slot 0 still lists the OLD credential as active and
    // still wraps the same unchanged DEK, so raw(1) opens the database with
    // full read access — a revocation undone by a single torn write, or by an
    // attacker with write access to 8 KB of the file.
    let err = Chisel::open(&path, Options::default().encryption_key(raw(1)))
        .err()
        .expect("the revoked key must still be refused after the winning slot is torn");
    assert!(
        matches!(err, ChiselError::InvalidEncryptionKey),
        "expected InvalidEncryptionKey for the revoked key, got {err:?}"
    );

    // The other half: revocation must not have been achieved by making the
    // database unopenable. The new key still works and the data is intact.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2)))
        .expect("the current key must still open the database after the tear");
    assert!(!db.is_poisoned());
    assert_eq!(db.read(h).unwrap(), b"secret");
    db.close().unwrap();
}

/// The same guarantee for `remove_key`, which revokes without adding anything.
#[test]
fn removed_key_stays_revoked_after_the_winning_slot_is_torn() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");

    {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.begin().unwrap();
        db.allocate(b"secret").unwrap();
        db.commit().unwrap();
        db.add_key(&raw(1), &raw(2)).unwrap();
        db.remove_key(&raw(1)).unwrap();
        db.close().unwrap();
    }

    // Two header rewrites happened here (add, then remove), so rather than
    // recompute which slot won, assert the property that must hold for EVERY
    // slot: no slot anywhere in the file may still admit the removed key.
    // Tearing each in turn on a fresh copy says exactly that, and is robust to
    // the slot arithmetic changing.
    for slot in 0..2u64 {
        let dir2 = TempDir::new().unwrap();
        let path2 = dir2.path().join("db");
        std::fs::copy(&path, &path2).unwrap();
        tear_slot(&path2, slot);

        match Chisel::open(&path2, Options::default().encryption_key(raw(1))) {
            Err(ChiselError::InvalidEncryptionKey) => {}
            Err(other) => panic!("slot {slot} torn: unexpected error {other:?}"),
            Ok(_) => panic!(
                "slot {slot} torn: the REMOVED key opened the database — a stale \
                 key-slot table survived in a sibling"
            ),
        }

        // The other half, and it is not redundant with the `Err(other)` arm
        // above: a defect that made EVERY key fail with InvalidEncryptionKey —
        // a scrub that writes a garbage slot table, say — would satisfy the
        // match and still have bricked the database. Revocation must not be
        // achieved by denying everyone.
        let db =
            Chisel::open(&path2, Options::default().encryption_key(raw(2))).unwrap_or_else(|e| {
                panic!("slot {slot} torn: the surviving key must still open: {e:?}")
            });
        assert!(!db.is_poisoned());
        db.close().unwrap();
    }
}
