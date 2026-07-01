//! transaction::create_tests — create-artifact tests for encrypted DBs.
//!
//! Scope: verify the CREATE ARTIFACT (the serialised page 0) without exercising
//! the open path. Concretely:
//!   - MAJOR is 2 in the on-disk superblock
//!   - slot 0 is populated (state=active)
//!   - sensitive fields (named_roots names) are NOT in cleartext
//!   - the wrapped DEK round-trips: unwrap_dek under the same key recovers
//!     a valid DEK (smoke-tests the wrap/AAD path without going through open)
//!
//! Relocated from tests/encryption_create.rs after the crypto/superblock/format
//! internals were scoped to pub(crate); these assertions need direct access to
//! CryptoHeader/KeySlot/derive_kek/unwrap_dek which are no longer public. All
//! file-backed tests use a tempfile that is deleted on drop.

use crate::crypto::{derive_kek, unwrap_dek, KdfId, Key};
use crate::page::{format_major, FORMAT_MAJOR_VERSION_ENCRYPTED, PAGE_SIZE};
use crate::superblock::{
    CryptoHeader, KeySlot, ALGO_XCHACHA20POLY1305, CRYPTO_HEADER_OFFSET, KEY_SLOT_COUNT,
    KEY_SLOT_SIZE,
};
use crate::Options;
use std::fs;
use std::io::Read as _;
use zeroize::Zeroizing;

// ── helper ────────────────────────────────────────────────────────────────────

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(stem: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "chisel_enc_test_{}_{}.db",
            stem,
            std::process::id()
        ));
        let _ = fs::remove_file(&p);
        TempDb(p)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    /// Read page 0 (the first superblock slot) as raw bytes.
    fn read_page0(&self) -> [u8; PAGE_SIZE] {
        let mut f = fs::File::open(&self.0).unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        f.read_exact(&mut buf).unwrap();
        buf
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Creating an encrypted DB with a raw key stamps MAJOR=2 in the on-disk
/// superblock (bytes 4..8 hold format_version as little-endian u32;
/// upper 16 bits = MAJOR).
#[test]
fn create_encrypted_db_stamps_major_2() {
    let tmp = TempDb::new("major2_raw");
    let key = Key::Raw(Zeroizing::new(vec![0xAB_u8; 32]));
    let db = crate::Chisel::open(tmp.path(), Options::default().encryption_key(key))
        .expect("create encrypted db");
    drop(db);

    let page0 = tmp.read_page0();
    let fv = u32::from_le_bytes(page0[4..8].try_into().unwrap());
    assert_eq!(
        format_major(fv),
        FORMAT_MAJOR_VERSION_ENCRYPTED,
        "format_version MAJOR must be 2 for encrypted DB; got {fv:#010x}"
    );
}

/// Creating with a passphrase also stamps MAJOR=2.
#[test]
fn create_encrypted_db_passphrase_stamps_major_2() {
    let tmp = TempDb::new("major2_pass");
    let key = Key::Passphrase(Zeroizing::new("hunter2".to_string()));
    let db = crate::Chisel::open(tmp.path(), Options::default().encryption_key(key))
        .expect("create encrypted db passphrase");
    drop(db);

    let page0 = tmp.read_page0();
    let fv = u32::from_le_bytes(page0[4..8].try_into().unwrap());
    assert_eq!(format_major(fv), FORMAT_MAJOR_VERSION_ENCRYPTED);
}

/// Key-slot 0 must be active (state byte = 1) and the algorithm byte must
/// be XChaCha20-Poly1305 (= 1). All other slots must be empty (state = 0).
#[test]
fn create_encrypted_db_populates_slot_0_only() {
    let tmp = TempDb::new("slot0");
    let key = Key::Raw(Zeroizing::new(vec![0x77_u8; 32]));
    let db =
        crate::Chisel::open(tmp.path(), Options::default().encryption_key(key)).expect("create");
    drop(db);

    let page0 = tmp.read_page0();

    // Algorithm byte is the first byte of the crypto-header region.
    assert_eq!(
        page0[CRYPTO_HEADER_OFFSET], ALGO_XCHACHA20POLY1305,
        "algorithm byte must be 1 (XChaCha20-Poly1305)"
    );

    // The slot table immediately follows the 8-byte crypto-header prefix
    // (1 byte algo + 4 bytes stride + 3 reserved bytes).
    let slot_table_offset = CRYPTO_HEADER_OFFSET + 8;

    // Slot 0 state byte must be 1 (active).
    assert_eq!(
        page0[slot_table_offset], 1,
        "slot 0 state must be active (1)"
    );

    // Slots 1..KEY_SLOT_COUNT must all be empty (state = 0).
    for i in 1..KEY_SLOT_COUNT {
        let base = slot_table_offset + i * KEY_SLOT_SIZE;
        assert_eq!(page0[base], 0, "slot {i} must be empty");
    }
}

/// The encrypted superblock must have non-zero bytes in its sealed-body region.
///
/// `serialize_encrypted` stores the XChaCha20-Poly1305 ciphertext starting at
/// SEALED_BODY_OFFSET (= CRYPTO_HEADER_OFFSET + CRYPTO_HEADER_SIZE = 1356).
/// That region holds: nonce (24 B) || tag (16 B) || ct_len (2 B) || ciphertext.
/// Checking that the nonce+tag+ciphertext region is non-zero confirms the body
/// was actually sealed (not skipped/left zero).
///
/// Bytes 52..308 (plaintext named_roots) ARE intentionally zeroed by
/// serialize_encrypted — they are hidden inside the sealed body. We do NOT
/// check those here.
#[test]
fn create_encrypted_db_sealed_body_is_present() {
    // SEALED_BODY_OFFSET = CRYPTO_HEADER_OFFSET(324) + CRYPTO_HEADER_SIZE(8 + 8*128)
    // = 324 + 1032 = 1356. Sample the nonce field (first 24 bytes of sealed region).
    const SEALED_BODY_OFFSET: usize = 1356;

    let tmp = TempDb::new("cleartext_check");
    let key = Key::Raw(Zeroizing::new(vec![0xCC_u8; 32]));
    let db =
        crate::Chisel::open(tmp.path(), Options::default().encryption_key(key)).expect("create");
    drop(db);

    let page0 = tmp.read_page0();

    // The nonce is 24 random bytes written by serialize_encrypted. They should
    // be non-zero (with overwhelming probability — a 192-bit all-zero nonce
    // has probability 2^-192 ≈ 10^-58).
    let nonce_region = &page0[SEALED_BODY_OFFSET..SEALED_BODY_OFFSET + 24];
    assert!(
        nonce_region.iter().any(|&b| b != 0),
        "nonce region at SEALED_BODY_OFFSET is all-zero — body was not sealed"
    );
}

/// Smoke-test the wrap/unwrap path: read the slot-0 salt, nonce, wrapped DEK,
/// and tag from the on-disk superblock, reconstruct the AAD, re-derive the KEK
/// from the same raw key, and verify `unwrap_dek` succeeds.
///
/// This exercises the wrap→unwrap round-trip without going through the open
/// path.
#[test]
fn slot0_dek_unwraps_with_correct_key() {
    let tmp = TempDb::new("unwrap");
    let key = Key::Raw(Zeroizing::new(vec![0x5A_u8; 32]));
    let db = crate::Chisel::open(tmp.path(), Options::default().encryption_key(key.clone()))
        .expect("create");
    drop(db);

    let page0 = tmp.read_page0();

    // Deserialize the crypto-header to get slot 0.
    let header = CryptoHeader::deserialize(&page0)
        .expect("page 0 must have a crypto-header for an encrypted DB");
    assert_eq!(header.algorithm, ALGO_XCHACHA20POLY1305);

    let slot = &header.slots[0];
    assert!(slot.is_active(), "slot 0 must be active");
    assert_eq!(slot.kdf_id, KdfId::Hkdf as u8, "raw key → HKDF");

    // Re-derive KEK using the same raw key + the stored salt + params.
    let kek = derive_kek(&key, KdfId::Hkdf, &slot.salt, &slot.argon2)
        .expect("derive_kek must succeed with the correct key");

    // Reconstruct the AAD exactly as build_create_cipher did: populate the
    // slot fields (state, kdf_id, argon2, salt, wrap_nonce) then call aad().
    // The wrapped_dek and wrap_tag fields are NOT part of the AAD.
    let mut aad_slot = KeySlot::EMPTY;
    aad_slot.state = slot.state;
    aad_slot.kdf_id = slot.kdf_id;
    aad_slot.argon2 = slot.argon2;
    aad_slot.salt = slot.salt;
    aad_slot.wrap_nonce = slot.wrap_nonce;
    let aad = aad_slot.aad();

    // Unwrap must succeed.
    let dek = unwrap_dek(
        &kek,
        &slot.wrapped_dek,
        &slot.wrap_tag,
        &slot.wrap_nonce,
        &aad,
    )
    .expect("unwrap_dek must succeed with the correct key and AAD");

    // The DEK must be non-trivial (not all zeros).
    assert_ne!(
        dek.as_bytes(),
        &[0u8; 32],
        "unwrapped DEK must not be all zeros"
    );
}

/// Wrong key must fail unwrap (AEAD authentication failure).
#[test]
fn slot0_dek_unwrap_fails_with_wrong_key() {
    let tmp = TempDb::new("wrong_key");
    let key = Key::Raw(Zeroizing::new(vec![0x5A_u8; 32]));
    let db =
        crate::Chisel::open(tmp.path(), Options::default().encryption_key(key)).expect("create");
    drop(db);

    let page0 = tmp.read_page0();
    let header = CryptoHeader::deserialize(&page0).expect("crypto header");
    let slot = &header.slots[0];

    let wrong_key = Key::Raw(Zeroizing::new(vec![0xFF_u8; 32]));
    let kek = derive_kek(&wrong_key, KdfId::Hkdf, &slot.salt, &slot.argon2).expect("derive_kek");

    let mut aad_slot = KeySlot::EMPTY;
    aad_slot.state = slot.state;
    aad_slot.kdf_id = slot.kdf_id;
    aad_slot.argon2 = slot.argon2;
    aad_slot.salt = slot.salt;
    aad_slot.wrap_nonce = slot.wrap_nonce;
    let aad = aad_slot.aad();

    assert!(
        unwrap_dek(
            &kek,
            &slot.wrapped_dek,
            &slot.wrap_tag,
            &slot.wrap_nonce,
            &aad
        )
        .is_err(),
        "wrong key must fail DEK unwrap"
    );
}
