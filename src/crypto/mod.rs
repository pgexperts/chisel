// src/crypto/mod.rs — Crypto core (layer 1, no engine coupling).
//
// Standalone at-rest encryption primitives for Chisel: the XChaCha20-Poly1305
// PageCipher (whole-page + variable-length body seal/open), the envelope KDF
// (HKDF-SHA256 for raw keys, Argon2id for passphrases), DEK wrap/unwrap, and
// the zeroizing key types. Nothing here touches page_io, the cache, or the
// superblock — those layers consume this module in later phases. See
// docs/specs/2026-06-29-on-disk-encryption-design.md §3.
//
// All randomness is OS-sourced (getrandom). Rolling our own crypto is
// forbidden; only the vetted RustCrypto primitives are used.

// ponytail: staged build — later tasks (PageCipher, KDF, wrap/unwrap) consume
// these types; suppress dead_code until the callers land rather than scattering
// #[allow] on every item.
#![allow(dead_code)]

use zeroize::Zeroizing;

/// On-disk stride of one encrypted page: 8192 ciphertext + 16 tag + 24 nonce.
/// The logical page stays 8192 (spec §4.1); only the I/O unit grows.
pub const ENC_PAGE_SIZE: usize = 8232;
/// XChaCha20 nonce length (192 bits). The extended nonce is what makes random
/// per-write nonces safe under shadow-paging page reuse (spec §2.1).
pub const NONCE_LEN: usize = 24;
/// Poly1305 authentication tag length.
pub const TAG_LEN: usize = 16;
/// Data Encryption Key length (256-bit).
pub const DEK_LEN: usize = 32;
/// Per-key-slot KDF salt length.
pub const SALT_LEN: usize = 16;

/// Client-supplied encryption credential. `Raw` is high-entropy key bytes
/// (derived via HKDF); `Passphrase` is a human secret (derived via Argon2id).
/// Both are zeroized on drop. `Clone` is needed because `Options` is consumed
/// by `open` while rotation APIs may also hold a key.
#[derive(Clone)]
pub enum Key {
    Raw(Zeroizing<Vec<u8>>),
    Passphrase(Zeroizing<String>),
}

/// The Data Encryption Key: seals every page and the superblock body. Generated
/// once at create time, held for the open session only, wiped on drop. Never
/// written to disk except KEK-wrapped in a key-slot.
pub struct Dek(Zeroizing<[u8; DEK_LEN]>);

impl Dek {
    /// Construct from raw bytes (used by unwrap_dek). Kept crate-internal-ish via
    /// module visibility; later phases hold a Dek but do not fabricate one.
    pub fn from_bytes(bytes: [u8; DEK_LEN]) -> Self {
        Dek(Zeroizing::new(bytes))
    }
    /// Borrow the raw key bytes. Callers must not copy these into a non-zeroizing
    /// buffer that outlives the operation.
    pub fn as_bytes(&self) -> &[u8; DEK_LEN] {
        &self.0
    }
}

impl Clone for Dek {
    fn clone(&self) -> Self {
        Dek(Zeroizing::new(*self.0))
    }
}

/// The Key Encryption Key: derived per-open from the client key + a slot's
/// salt/params. Only ever wraps/unwraps the DEK; transient, wiped on drop.
#[derive(Debug)]
pub struct Kek(Zeroizing<[u8; 32]>);

impl Kek {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Kek(Zeroizing::new(bytes))
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// KDF selector recorded per key-slot. The integer discriminants are part of
/// the on-disk format (written into the slot's `kdf_id` byte) — do not renumber.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum KdfId {
    Hkdf = 1,
    Argon2id = 2,
}

/// Argon2id cost parameters. Stored per-slot so a slot can be re-derived
/// regardless of the binary's current defaults.
#[derive(Clone, Copy, Debug)]
pub struct Argon2Params {
    pub m_cost: u32, // KiB of memory
    pub t_cost: u32, // iterations
    pub p_cost: u32, // lanes
}

impl Default for Argon2Params {
    /// OWASP-recommended Argon2id baseline (19 MiB, 2 iterations, 1 lane).
    fn default() -> Self {
        Argon2Params {
            m_cost: 19456,
            t_cost: 2,
            p_cost: 1,
        }
    }
}

/// Failures internal to the crypto layer. The engine layer maps these onto
/// ChiselError (Auth → InvalidEncryptionKey/DecryptionFailed depending on site;
/// Kdf/BadKeyLength → operational key errors). PartialEq for ergonomic tests.
#[derive(Debug, PartialEq)]
pub enum CryptoError {
    /// AEAD tag verification failed (wrong key, tampered ciphertext, wrong AAD).
    Auth,
    /// A key-derivation primitive rejected its parameters.
    Kdf,
    /// A raw key was not the length the KDF requires.
    BadKeyLength,
}

use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use sha2::Sha256;

/// HKDF info string binding derived KEKs to this construction/version. Changing
/// it is a format break (existing slots would stop unwrapping); versioned so a
/// future KDF revision can coexist.
const KEK_INFO: &[u8] = b"chisel-kek-v1";

/// Derive a 256-bit KEK from the client key and a slot's salt/params.
///
/// Dispatch is on `kdf`, NOT on the `Key` variant: the slot records which KDF
/// produced it, and that is the authority. A `Raw` key is the IKM for HKDF; a
/// `Passphrase` is the password for Argon2id. (A mismatched pairing — e.g. a
/// passphrase with KdfId::Hkdf — still derives a deterministic KEK; it simply
/// won't match the slot that was written with the other KDF, surfacing as an
/// unwrap Auth failure one layer up. The slot's kdf_id is the single source of
/// truth, so we never guess from the variant.)
pub fn derive_kek(
    key: &Key,
    kdf: KdfId,
    salt: &[u8; SALT_LEN],
    params: &Argon2Params,
) -> Result<Kek, CryptoError> {
    let ikm: &[u8] = match key {
        Key::Raw(bytes) => bytes.as_slice(),
        Key::Passphrase(s) => s.as_bytes(),
    };
    let mut okm = [0u8; 32];
    match kdf {
        KdfId::Hkdf => {
            let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
            hk.expand(KEK_INFO, &mut okm).map_err(|_| CryptoError::Kdf)?;
        }
        KdfId::Argon2id => {
            let p = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
                .map_err(|_| CryptoError::Kdf)?;
            let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
            a2.hash_password_into(ikm, salt, &mut okm)
                .map_err(|_| CryptoError::Kdf)?;
        }
    }
    Ok(Kek::from_bytes(okm))
}

/// Fill an N-byte array from the OS CSPRNG. Panics if the OS RNG is unavailable,
/// which on a supported platform indicates a broken system — there is no safe
/// fallback for key material, so failing loud is correct.
pub fn random_array<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("OS RNG unavailable");
    b
}

/// Generate a fresh random DEK from the OS CSPRNG.
pub fn random_dek() -> Dek {
    Dek::from_bytes(random_array::<DEK_LEN>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_spec() {
        // On-disk encrypted stride = 8192 ciphertext + 16 tag + 24 nonce.
        assert_eq!(ENC_PAGE_SIZE, 8232);
        assert_eq!(NONCE_LEN, 24);
        assert_eq!(TAG_LEN, 16);
        assert_eq!(DEK_LEN, 32);
        assert_eq!(SALT_LEN, 16);
        assert_eq!(ENC_PAGE_SIZE, 8192 + TAG_LEN + NONCE_LEN);
    }

    #[test]
    fn argon2_params_default_is_owasp() {
        let p = Argon2Params::default();
        assert_eq!(p.m_cost, 19456); // 19 MiB
        assert_eq!(p.t_cost, 2);
        assert_eq!(p.p_cost, 1);
    }

    #[test]
    fn kdf_id_discriminants_are_wire_stable() {
        // These ints are written into key-slots on disk; pin them.
        assert_eq!(KdfId::Hkdf as u8, 1);
        assert_eq!(KdfId::Argon2id as u8, 2);
        assert_ne!(KdfId::Hkdf, KdfId::Argon2id);
    }

    #[test]
    fn random_array_is_os_filled_and_distinct() {
        let a: [u8; 32] = random_array();
        let b: [u8; 32] = random_array();
        // Astronomically unlikely to collide; all-zero would mean RNG silent-failed.
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }

    #[test]
    fn random_dek_differs_each_call() {
        let d1 = random_dek();
        let d2 = random_dek();
        assert_ne!(d1.as_bytes(), d2.as_bytes());
    }

    #[test]
    fn crypto_error_is_comparable() {
        assert_eq!(CryptoError::Auth, CryptoError::Auth);
        assert_ne!(CryptoError::Auth, CryptoError::Kdf);
    }

    #[test]
    fn derive_kek_hkdf_matches_reference_construction() {
        // RFC 5869 Test Case 1 inputs (IKM/salt), our pinned info string.
        let ikm = [0x0bu8; 22];
        let salt: [u8; SALT_LEN] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let key = Key::Raw(zeroize::Zeroizing::new(ikm.to_vec()));
        let kek = derive_kek(&key, KdfId::Hkdf, &salt, &Argon2Params::default()).unwrap();

        // Independent reference: run hkdf directly with our exact salt+info.
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut expect = [0u8; 32];
        hk.expand(b"chisel-kek-v1", &mut expect).unwrap();
        assert_eq!(kek.as_bytes(), &expect);
    }

    #[test]
    fn derive_kek_hkdf_is_deterministic_and_salt_sensitive() {
        let key = Key::Raw(zeroize::Zeroizing::new(vec![7u8; 32]));
        let salt_a = [1u8; SALT_LEN];
        let salt_b = [2u8; SALT_LEN];
        let p = Argon2Params::default();
        let k1 = derive_kek(&key, KdfId::Hkdf, &salt_a, &p).unwrap();
        let k2 = derive_kek(&key, KdfId::Hkdf, &salt_a, &p).unwrap();
        let k3 = derive_kek(&key, KdfId::Hkdf, &salt_b, &p).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes(), "same input must be deterministic");
        assert_ne!(k1.as_bytes(), k3.as_bytes(), "different salt must diverge");
    }

    #[test]
    fn derive_kek_argon2_roundtrips_and_is_salt_sensitive() {
        // Cheap params so the test is fast (real defaults are 19 MiB).
        let fast = Argon2Params { m_cost: 256, t_cost: 1, p_cost: 1 };
        let key = Key::Passphrase(zeroize::Zeroizing::new("correct horse".to_string()));
        let salt_a = [9u8; SALT_LEN];
        let salt_b = [8u8; SALT_LEN];
        let k1 = derive_kek(&key, KdfId::Argon2id, &salt_a, &fast).unwrap();
        let k2 = derive_kek(&key, KdfId::Argon2id, &salt_a, &fast).unwrap();
        let k3 = derive_kek(&key, KdfId::Argon2id, &salt_b, &fast).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes(), "Argon2id must be deterministic");
        assert_ne!(k1.as_bytes(), k3.as_bytes(), "different salt must diverge");
        assert_ne!(k1.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn derive_kek_argon2_rejects_zero_memory() {
        let bad = Argon2Params { m_cost: 0, t_cost: 1, p_cost: 1 };
        let key = Key::Passphrase(zeroize::Zeroizing::new("x".to_string()));
        let err = derive_kek(&key, KdfId::Argon2id, &[0u8; SALT_LEN], &bad).unwrap_err();
        assert_eq!(err, CryptoError::Kdf);
    }
}
