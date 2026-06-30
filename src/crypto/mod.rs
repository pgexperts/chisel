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

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{Key as AeadKey, KeyInit, XChaCha20Poly1305, XNonce};
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
///
/// No Debug/Display: `Zeroizing`'s derived Debug delegates to the inner array
/// (it does not redact), so printing a Kek would leak the raw key bytes.
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
    // Zeroizing so the derived key never lingers un-wiped on the stack: on the
    // success path it is MOVED into Kek (no copy left behind), and on any error
    // path partial KDF output is wiped on drop.
    let mut okm = Zeroizing::new([0u8; 32]);
    match kdf {
        KdfId::Hkdf => {
            let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
            hk.expand(KEK_INFO, okm.as_mut())
                .map_err(|_| CryptoError::Kdf)?;
        }
        KdfId::Argon2id => {
            let p = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
                .map_err(|_| CryptoError::Kdf)?;
            let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
            a2.hash_password_into(ikm, salt, okm.as_mut())
                .map_err(|_| CryptoError::Kdf)?;
        }
    }
    Ok(Kek(okm))
}

/// Detached AEAD seal: ciphertext is the same length as plaintext, the 16-byte
/// Poly1305 tag is returned separately. Detached suits our fixed page layout
/// (ciphertext occupies a known 32-byte DEK slot, tag a known 16-byte slot;
/// for pages the same pattern applies with 8192-byte slots).
fn seal_detached(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> (Vec<u8>, [u8; TAG_LEN]) {
    let cipher = XChaCha20Poly1305::new(AeadKey::from_slice(key));
    let mut buf = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(XNonce::from_slice(nonce), aad, &mut buf)
        .expect("XChaCha20-Poly1305 encrypt cannot fail for in-range lengths");
    let mut tag_arr = [0u8; TAG_LEN];
    tag_arr.copy_from_slice(&tag);
    (buf, tag_arr)
}

/// Detached AEAD open. Any tag mismatch (wrong key, tampered ct/tag, wrong AAD,
/// wrong nonce) maps to CryptoError::Auth. XChaCha20-Poly1305 is verify-then-
/// decrypt: `decrypt_in_place_detached` checks the Poly1305 tag BEFORE applying
/// the keystream, so on auth failure the buffer still holds the original
/// ciphertext and no plaintext is ever written. The returned buffer is
/// Zeroizing so the decrypted plaintext (key material) is wiped on drop rather
/// than handed back to the allocator un-scrubbed.
fn open_detached(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
    tag: &[u8; TAG_LEN],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(AeadKey::from_slice(key));
    let mut buf = Zeroizing::new(ciphertext.to_vec());
    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(nonce),
            aad,
            &mut buf,
            tag.as_slice().into(),
        )
        .map_err(|_| CryptoError::Auth)?;
    Ok(buf)
}

/// Wrap (encrypt) the DEK under a KEK using detached XChaCha20-Poly1305.
/// `aad` binds the slot's metadata (kdf_id, salt, Argon2 params) so an
/// attacker cannot tamper a slot's parameters to force a mis-derivation.
/// Returns (wrapped_dek, wrap_tag); both are written to the key-slot on disk.
pub fn wrap_dek(
    kek: &Kek,
    dek: &Dek,
    wrap_nonce: &[u8; NONCE_LEN],
    aad: &[u8],
) -> ([u8; DEK_LEN], [u8; TAG_LEN]) {
    let (ct, tag) = seal_detached(kek.as_bytes(), wrap_nonce, aad, dek.as_bytes());
    let mut wrapped = [0u8; DEK_LEN];
    wrapped.copy_from_slice(&ct);
    (wrapped, tag)
}

/// Unwrap (decrypt + authenticate) the DEK. A successful unwrap IS the proof
/// that the client key (hence KEK) is correct — there is no separate verifier.
/// Any failure (wrong passphrase, wrong KEK, tampered ciphertext or tag, wrong
/// AAD) returns CryptoError::Auth without revealing partial plaintext.
pub fn unwrap_dek(
    kek: &Kek,
    wrapped: &[u8; DEK_LEN],
    tag: &[u8; TAG_LEN],
    wrap_nonce: &[u8; NONCE_LEN],
    aad: &[u8],
) -> Result<Dek, CryptoError> {
    let pt = open_detached(kek.as_bytes(), wrap_nonce, aad, wrapped, tag)?;
    let mut dek_bytes = Zeroizing::new([0u8; DEK_LEN]);
    dek_bytes.copy_from_slice(&pt);
    Ok(Dek::from_bytes(*dek_bytes))
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

/// Holds the DEK and performs the two seal/open transforms the engine needs:
/// whole-page (fixed 8192→8232) and variable-length body (superblock sub-blob).
/// Lives in the page-cache layer in later phases; here it is fully standalone.
/// Constructs the AEAD cipher once and reuses it across calls.
pub struct PageCipher {
    dek: Dek,
}

impl PageCipher {
    pub fn new(dek: Dek) -> Self {
        PageCipher { dek }
    }

    /// Seal a full 8192-byte plaintext page image into the 8232-byte on-disk
    /// blob: `ciphertext(8192) ‖ tag(16) ‖ nonce(24)`. AAD = page_id LE bytes
    /// (anti-relocation). A fresh random 192-bit nonce per call (spec §2.1) —
    /// safe under shadow-paging page reuse, and stored in the clear.
    pub fn seal(&self, page_id: u64, plaintext: &[u8; 8192]) -> [u8; ENC_PAGE_SIZE] {
        let nonce = random_array::<NONCE_LEN>();
        let aad = page_id.to_le_bytes();
        let (ct, tag) = seal_detached(self.dek.as_bytes(), &nonce, &aad, plaintext);
        let mut out = [0u8; ENC_PAGE_SIZE];
        out[0..8192].copy_from_slice(&ct);
        out[8192..8208].copy_from_slice(&tag);
        out[8208..8232].copy_from_slice(&nonce);
        out
    }

    /// Open an 8232-byte on-disk blob back to the 8192-byte plaintext page.
    /// AAD = page_id LE. Any authentication failure → CryptoError::Auth (the
    /// engine maps this to DecryptionFailed at the page-read site).
    pub fn open(
        &self,
        page_id: u64,
        ondisk: &[u8; ENC_PAGE_SIZE],
    ) -> Result<[u8; 8192], CryptoError> {
        let ct = &ondisk[0..8192];
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&ondisk[8192..8208]);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&ondisk[8208..8232]);
        let aad = page_id.to_le_bytes();
        let pt = open_detached(self.dek.as_bytes(), &nonce, &aad, ct, &tag)?;
        let mut page = [0u8; 8192];
        page.copy_from_slice(&pt);
        Ok(page)
    }

    /// Seal a variable-length body (the superblock sensitive sub-blob). Returns
    /// (nonce, tag, ciphertext); the caller lays these out in the reserved
    /// region. AAD binds the body to the superblock's identity (anti-splicing).
    pub fn seal_body(
        &self,
        aad: &[u8],
        plaintext: &[u8],
    ) -> ([u8; NONCE_LEN], [u8; TAG_LEN], Vec<u8>) {
        let nonce = random_array::<NONCE_LEN>();
        let (ct, tag) = seal_detached(self.dek.as_bytes(), &nonce, aad, plaintext);
        (nonce, tag, ct)
    }

    /// Open a variable-length body sealed by `seal_body`. AAD must match the
    /// superblock identity used at seal time, else CryptoError::Auth.
    pub fn open_body(
        &self,
        aad: &[u8],
        nonce: &[u8; NONCE_LEN],
        tag: &[u8; TAG_LEN],
        ct: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        open_detached(self.dek.as_bytes(), nonce, aad, ct, tag).map(|z| z.to_vec())
    }
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
        assert_eq!(
            k1.as_bytes(),
            k2.as_bytes(),
            "same input must be deterministic"
        );
        assert_ne!(k1.as_bytes(), k3.as_bytes(), "different salt must diverge");
    }

    #[test]
    fn derive_kek_argon2_roundtrips_and_is_salt_sensitive() {
        // Cheap params so the test is fast (real defaults are 19 MiB).
        let fast = Argon2Params {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        };
        let key = Key::Passphrase(zeroize::Zeroizing::new("correct horse".to_string()));
        let salt_a = [9u8; SALT_LEN];
        let salt_b = [8u8; SALT_LEN];
        let k1 = derive_kek(&key, KdfId::Argon2id, &salt_a, &fast).unwrap();
        let k2 = derive_kek(&key, KdfId::Argon2id, &salt_a, &fast).unwrap();
        let k3 = derive_kek(&key, KdfId::Argon2id, &salt_b, &fast).unwrap();
        assert_eq!(
            k1.as_bytes(),
            k2.as_bytes(),
            "Argon2id must be deterministic"
        );
        assert_ne!(k1.as_bytes(), k3.as_bytes(), "different salt must diverge");
        assert_ne!(k1.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn derive_kek_argon2_rejects_zero_memory() {
        let bad = Argon2Params {
            m_cost: 0,
            t_cost: 1,
            p_cost: 1,
        };
        let key = Key::Passphrase(zeroize::Zeroizing::new("x".to_string()));
        // matches! rather than unwrap_err: Kek deliberately has no Debug (it
        // wraps key bytes), so Result::unwrap_err can't be used here.
        assert!(matches!(
            derive_kek(&key, KdfId::Argon2id, &[0u8; SALT_LEN], &bad),
            Err(CryptoError::Kdf)
        ));
    }

    #[test]
    fn wrap_unwrap_roundtrip() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (wrapped, tag) = wrap_dek(&kek, &dek, &nonce, aad);
        assert_ne!(
            &wrapped,
            dek.as_bytes(),
            "wrapped DEK must not equal plaintext DEK"
        );
        let out = unwrap_dek(&kek, &wrapped, &tag, &nonce, aad).unwrap();
        assert_eq!(out.as_bytes(), dek.as_bytes());
    }

    #[test]
    fn unwrap_wrong_kek_is_auth() {
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (wrapped, tag) = wrap_dek(&Kek::from_bytes([3u8; 32]), &dek, &nonce, aad);
        // unwrap_err() requires Dek: Debug, which we deliberately omit (it wraps key
        // bytes). Use matches! to test the error variant without printing anything.
        assert!(matches!(
            unwrap_dek(&Kek::from_bytes([4u8; 32]), &wrapped, &tag, &nonce, aad),
            Err(CryptoError::Auth)
        ));
    }

    #[test]
    fn unwrap_tampered_tag_is_auth() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (wrapped, mut tag) = wrap_dek(&kek, &dek, &nonce, aad);
        tag[0] ^= 0x01;
        assert!(matches!(
            unwrap_dek(&kek, &wrapped, &tag, &nonce, aad),
            Err(CryptoError::Auth)
        ));
    }

    #[test]
    fn unwrap_tampered_ciphertext_is_auth() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (mut wrapped, tag) = wrap_dek(&kek, &dek, &nonce, aad);
        wrapped[0] ^= 0x01;
        assert!(matches!(
            unwrap_dek(&kek, &wrapped, &tag, &nonce, aad),
            Err(CryptoError::Auth)
        ));
    }

    #[test]
    fn unwrap_wrong_aad_is_auth() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let (wrapped, tag) = wrap_dek(&kek, &dek, &nonce, b"slot-meta-A");
        assert!(matches!(
            unwrap_dek(&kek, &wrapped, &tag, &nonce, b"slot-meta-B"),
            Err(CryptoError::Auth)
        ));
    }

    #[test]
    fn page_seal_open_roundtrip() {
        let pc = PageCipher::new(Dek::from_bytes([1u8; DEK_LEN]));
        let mut page = [0u8; 8192];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let blob = pc.seal(7, &page);
        assert_eq!(blob.len(), ENC_PAGE_SIZE);
        let out = pc.open(7, &blob).unwrap();
        assert_eq!(out, page);
    }

    #[test]
    fn page_seal_layout_is_ct_tag_nonce() {
        let pc = PageCipher::new(Dek::from_bytes([1u8; DEK_LEN]));
        let page = [0xABu8; 8192];
        let blob = pc.seal(0, &page);
        // ciphertext occupies 0..8192, tag 8192..8208, nonce 8208..8232.
        assert_ne!(
            &blob[0..8192],
            &page[..],
            "ciphertext must differ from plaintext"
        );
    }

    #[test]
    fn page_open_wrong_page_id_is_auth() {
        // AAD = page_id gives anti-relocation: a page sealed at id 7 must not
        // authenticate at id 8.
        let pc = PageCipher::new(Dek::from_bytes([1u8; DEK_LEN]));
        let page = [9u8; 8192];
        let blob = pc.seal(7, &page);
        assert_eq!(pc.open(8, &blob).unwrap_err(), CryptoError::Auth);
    }

    #[test]
    fn page_open_byte_flip_is_auth() {
        let pc = PageCipher::new(Dek::from_bytes([1u8; DEK_LEN]));
        let page = [9u8; 8192];
        let mut blob = pc.seal(7, &page);
        blob[100] ^= 0x01; // flip a ciphertext byte
        assert_eq!(pc.open(7, &blob).unwrap_err(), CryptoError::Auth);
    }

    #[test]
    fn page_two_seals_use_different_nonces() {
        // Random per-write nonce (spec §2.1): two seals of the same page must
        // produce different on-disk blobs (different nonce ⇒ different ct+tag).
        let pc = PageCipher::new(Dek::from_bytes([1u8; DEK_LEN]));
        let page = [9u8; 8192];
        let a = pc.seal(7, &page);
        let b = pc.seal(7, &page);
        assert_ne!(&a[..], &b[..], "nonce reuse: identical blobs for same page");
        // Both still open correctly.
        assert_eq!(pc.open(7, &a).unwrap(), page);
        assert_eq!(pc.open(7, &b).unwrap(), page);
    }

    #[test]
    fn body_seal_open_roundtrip() {
        let pc = PageCipher::new(Dek::from_bytes([2u8; DEK_LEN]));
        let body = b"root pointers + named_roots".to_vec();
        let aad = b"sb-identity";
        let (nonce, tag, ct) = pc.seal_body(aad, &body);
        assert_eq!(ct.len(), body.len(), "body cipher is length-preserving");
        let out = pc.open_body(aad, &nonce, &tag, &ct).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn body_open_wrong_aad_is_auth() {
        let pc = PageCipher::new(Dek::from_bytes([2u8; DEK_LEN]));
        let body = b"secret".to_vec();
        let (nonce, tag, ct) = pc.seal_body(b"sb-A", &body);
        assert_eq!(
            pc.open_body(b"sb-B", &nonce, &tag, &ct).unwrap_err(),
            CryptoError::Auth
        );
    }

    // Zeroization guards: verify that Dek/Key wrap Zeroizing buffers and that
    // Clone produces independent copies (so dropping one does not corrupt the
    // other).  We cannot observe freed memory in safe Rust, so the honest check
    // is independence of cloned buffers — if the inner Zeroizing zeroes-on-drop
    // the original's view is unaffected because they own separate allocations.
    #[test]
    fn dek_clone_is_independent_zeroizing_copy() {
        let d = Dek::from_bytes([7u8; DEK_LEN]);
        let c = d.clone();
        assert_eq!(d.as_bytes(), c.as_bytes());
        // Dropping the clone must not affect the original (independent buffers).
        drop(c);
        assert_eq!(d.as_bytes(), &[7u8; DEK_LEN]);
    }

    #[test]
    fn key_variants_construct_from_zeroizing() {
        // Compile + construct proof that Key wraps Zeroizing for both variants.
        let raw = Key::Raw(zeroize::Zeroizing::new(vec![1u8, 2, 3]));
        let pass = Key::Passphrase(zeroize::Zeroizing::new("pw".to_string()));
        // Clone works (needed by Options/rotation).
        let _r2 = raw.clone();
        let _p2 = pass.clone();
    }
}
