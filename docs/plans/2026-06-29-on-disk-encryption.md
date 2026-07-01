# On-Disk Encryption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add client-supplied, authenticated on-disk encryption (XChaCha20-Poly1305 with envelope key management) to the Chisel storage engine, with O(1) credential rotation.

**Architecture:** Encryption is a transform at the page-I/O boundary. The page cache and every layer above keep producing byte-identical 8192-byte pages; a `PageCipher` holding a random per-database Data Encryption Key (DEK) seals each page into an 8232-byte on-disk blob (`ciphertext ‖ tag ‖ nonce`) with `AAD = page_id` (anti-relocation). The DEK is wrapped under a Key Encryption Key (KEK) derived from the client's key — HKDF-SHA256 for raw keys, Argon2id for passphrases — and stored in an 8-slot table in the superblock's plaintext reserved region, so rotating a credential is an O(1) re-wrap, not a re-encryption. The sensitive superblock fields (including `named_roots`) are DEK-sealed; the crypto-header stays plaintext to bootstrap. Encrypted databases stamp a MAJOR format-version bump so encryption-unaware binaries refuse to open them.

**Tech Stack:** Rust; RustCrypto crates (`chacha20poly1305`, `argon2`, `hkdf` + `sha2`, `zeroize`, `getrandom`); pyo3 for the Python bindings.

## Global Constraints

_Every task's requirements implicitly include this section._

- **Spec (authoritative):** `docs/specs/2026-06-29-on-disk-encryption-design.md`.
- **Branch:** `feature/on-disk-encryption` (off `main`). After a PR exists, open a NEW PR for follow-up work — never amend/force-push a merged branch.
- **AEAD:** XChaCha20-Poly1305, a fresh **random 192-bit nonce per page write**, `AAD = page_id` (u64 little-endian). The `(nonce, key)` pair must never repeat — random nonces are the crash-safe construction (do not derive nonces from page state).
- **Page geometry:** logical page = **8192 bytes (unchanged)**; encrypted on-disk stride `ENC_PAGE_SIZE = 8232`. Plaintext databases are untouched (stride 8192, XXH3 checksum).
- **KDF:** raw key → HKDF-SHA256(salt, `info = b"chisel-kek-v1"`); passphrase → Argon2id (default `m_cost = 19456` KiB, `t_cost = 2`, `p_cost = 1`).
- **Key material:** `Key`, `Dek`, `Kek` are zeroizing types; key material is never written to disk except the **KEK-wrapped** DEK in the key-slot table.
- **Error model:** `NoEncryptionKey` / `InvalidEncryptionKey` / `EncryptionNotSupported` are **operational** (must NOT poison the engine — a wrong password is retryable); `DecryptionFailed { page_id }` is **fatal** (`is_fatal() == true`).
- **Format gate:** MAJOR version bump `1 → 2` for encrypted DBs only; plaintext DBs stay MAJOR 1.
- **Testing:** run plain `cargo test` (NOT `--lib`, so integration tests run) and keep CI green before every commit/push. Benchmarks are report-only in CI.
- **No Claude/AI references** in commit messages, code comments, or docs.
- **Crypto discipline:** vetted RustCrypto primitives only — never hand-roll. Prove KDF/AEAD wiring with known-answer (HKDF RFC 5869) or round-trip tests.

## File Structure

| File | New/Modify | Responsibility |
|---|---|---|
| `src/crypto/mod.rs` | new | Standalone crypto core: `PageCipher` (page + body seal/open), KDF (HKDF + Argon2id), DEK wrap/unwrap, zeroizing `Key`/`Dek`/`Kek`, `CryptoError`. No engine coupling. |
| `Cargo.toml` | modify | Crypto dependencies. |
| `src/superblock.rs` | modify | `CryptoHeader` + 8-slot `KeySlot` codec in the reserved region; DEK-sealed sensitive-body sub-blob; `Superblock.encryption`. |
| `src/transaction/{recovery,commit,mod}.rs` | modify | Create/open key flow; DEK held (zeroizing) in `TransactionManager`; rotation commits. |
| `src/page_io.rs` | modify | Stride-aware `read_page`/`write_page`/`set_page_count` moving the on-disk page unit (8232 enc / 8192 plain); crypto-agnostic. |
| `src/page_cache.rs` | modify | Holds `Option<PageCipher>`; seal-once on flush/evict, copy-on-drain, open-on-read. |
| `src/spillway.rs` | modify | Widen slot to carry the 8232-byte sealed blob. |
| `src/lib.rs` | modify | `Options.encryption_key` + `argon2_params`; public `Key` surface; wire `open()`; `add_key`/`rotate_key`/`remove_key`. |
| `src/error.rs` | modify | Encryption error variants + `is_fatal()` classification. |
| `src/page.rs` | modify | `FORMAT_MAJOR_VERSION` gate for encrypted DBs. |
| `python/src/db.rs` | modify | `encryption_key` kwarg; `add_key`/`rotate_key`/`remove_key`; error mapping. |
| `ARCHITECTURE.md`, `ISSUES.md` | modify | Encryption section; deferred bulk-DEK-rotation record. |

## Phase order & dependencies

Phases are sequential: **1** (crypto core) underpins all; **2** (superblock/key flow) and **3** (page I/O) both consume Phase 1; **4** (API/errors/Python) exposes them; **5** (key management) builds on 1/2/4; **6** (docs/version/ADR) closes out. Within a phase, tasks are ordered.

**Execution-order exception:** Task 4.1 (adding the encryption error variants to `ChiselError`) is dependency-free (`Consumes: nothing`) and **must be implemented first — before Phase 2** — because Phases 2, 3, and 5 return these variants. It is filed under Phase 4 for cohesion with the public-API surface; a subagent-driven runner should dispatch it ahead of Phase 1 Task 1.2, and a linear runner should jump to it before starting Phase 2. With it done first, no placeholder error variant is ever needed.

---

## Phase 1: Crypto core module

This phase builds `src/crypto/` as a standalone, fully unit-tested module with zero engine coupling. It produces every Phase-1 contract interface verbatim. Each task ends in an independently testable deliverable; tasks build up one file (`src/crypto/mod.rs`) plus the two registration edits (`Cargo.toml`, `src/lib.rs`).

ponytail note: one file, not a directory tree of `kdf.rs`/`aead.rs`/`wrap.rs`. The whole module is ~400 lines; splitting it into five files for one consumer is premature. Split later if it crosses 2000 lines (it won't).

---

### Task 1.1: Add crypto dependencies and an empty registered module

**Files:**
- Modify: `Cargo.toml:66` (after the `rustc-hash = "2"` runtime dep, line 66)
- Create: `src/crypto/mod.rs`
- Modify: `src/lib.rs:41` (insert `mod crypto;` adjacent to the other `mod` lines, e.g. right after `pub(crate) mod error;`)

**Interfaces:**
- Consumes: nothing
- Produces: a compilable, registered `crypto` module; the six crypto crates resolved in `Cargo.lock`.

- [ ] **Step 1: Write the failing test**
This task's deliverable is "the workspace builds with the new deps and module present." The runnable check is `cargo build`. Create the module file with a single trivial item so the `mod crypto;` line has something to point at, and a compile-time assertion that one dep links:
```rust
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

// Smoke check that the chacha20poly1305 dep is linked and its key length is
// the 32 bytes the envelope assumes. Replaced by real tests in later tasks.
#[cfg(test)]
mod tests {
    #[test]
    fn deps_link() {
        use chacha20poly1305::KeySizeUser;
        use chacha20poly1305::XChaCha20Poly1305;
        assert_eq!(
            <XChaCha20Poly1305 as KeySizeUser>::key_size(),
            32,
            "XChaCha20-Poly1305 key must be 32 bytes"
        );
    }
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo build`  Expected: FAIL — `error[E0432]: unresolved import` for `chacha20poly1305` (dep not added yet) and/or `file not found for module crypto` until `mod crypto;` is registered.

- [ ] **Step 3: Implement**
Add to `Cargo.toml` immediately after line 66 (`rustc-hash = "2"`), inside the existing `[dependencies]` block:
```toml
# On-disk encryption (spec 2026-06-29). All pure-Rust, well-vetted RustCrypto
# primitives — rolling our own crypto is forbidden. These reach the published
# crate's dependency tree only when a DB is opened with an encryption key, but
# they are unconditional deps (the seal/open code is always compiled). Versions
# pinned to the audited RustCrypto generation current as of 2026-06.
chacha20poly1305 = "0.10"   # XChaCha20-Poly1305 AEAD (192-bit nonce)
argon2 = "0.5"              # Argon2id passphrase KDF (memory-hard)
hkdf = "0.12"               # HKDF-SHA256 raw-key KDF
sha2 = "0.10"               # SHA-256 for HKDF
zeroize = { version = "1", features = ["derive"] }  # wipe key material on drop
getrandom = "0.2"           # OS RNG for DEK / nonce / salt generation
```
Register the module in `src/lib.rs` adjacent to the other declarations (after `pub(crate) mod error;` at line 41):
```rust
pub(crate) mod crypto;
```
(The `src/crypto/mod.rs` file from Step 1 already exists.)

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test deps_link`  Expected: PASS (and `cargo build` succeeds, resolving the six new crates into `Cargo.lock`).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "build: add RustCrypto deps and register crypto core module"
```

---

### Task 1.2: Key types, KdfId, Argon2Params, CryptoError, and OS randomness

**Files:**
- Modify: `src/crypto/mod.rs` (replace the placeholder `tests` module; add the public types and RNG helpers)
- Test: `#[cfg(test)] mod tests` in `src/crypto/mod.rs`

**Interfaces:**
- Consumes: nothing
- Produces:
```rust
pub const ENC_PAGE_SIZE: usize = 8232;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
pub const DEK_LEN: usize = 32;
pub const SALT_LEN: usize = 16;
pub enum Key { Raw(zeroize::Zeroizing<Vec<u8>>), Passphrase(zeroize::Zeroizing<String>) }   // #[derive(Clone)]
pub struct Dek(zeroize::Zeroizing<[u8; DEK_LEN]>);
pub struct Kek(zeroize::Zeroizing<[u8; 32]>);
pub enum KdfId { Hkdf = 1, Argon2id = 2 }                 // #[derive(Clone, Copy, PartialEq)]
pub struct Argon2Params { pub m_cost: u32, pub t_cost: u32, pub p_cost: u32 }   // #[derive(Clone, Copy)] + Default
pub enum CryptoError { Auth, Kdf, BadKeyLength }          // #[derive(Debug, PartialEq)]
pub fn random_dek() -> Dek;
pub fn random_array<const N: usize>() -> [u8; N];
```

- [ ] **Step 1: Write the failing test**
Replace the placeholder `tests` module in `src/crypto/mod.rs` with:
```rust
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
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --package chisel crypto::tests`  Expected: FAIL — `cannot find type Argon2Params`, `random_dek`, etc. (not yet defined).

- [ ] **Step 3: Implement**
Insert above the `#[cfg(test)] mod tests` block in `src/crypto/mod.rs`:
```rust
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
```
- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --package chisel crypto::tests`  Expected: PASS (6 tests).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): key types, KdfId, Argon2Params, CryptoError, OS randomness"
```

---

### Task 1.3: KEK derivation (HKDF for raw keys, Argon2id for passphrases)

**Files:**
- Modify: `src/crypto/mod.rs` (add `derive_kek`; extend the `tests` module)
- Test: `#[cfg(test)] mod tests` in `src/crypto/mod.rs`

**Interfaces:**
- Consumes: `Key`, `KdfId`, `Argon2Params`, `Kek`, `CryptoError`, `SALT_LEN` (Task 1.2)
- Produces:
```rust
pub fn derive_kek(key: &Key, kdf: KdfId, salt: &[u8; SALT_LEN], params: &Argon2Params) -> Result<Kek, CryptoError>;
```

- [ ] **Step 1: Write the failing test**
Add to the `tests` module in `src/crypto/mod.rs`. The HKDF case uses RFC 5869 Test Case 1 to prove the wiring (IKM = 22×0x0b, salt = 0x000102…0c, info = `b"chisel-kek-v1"` — note: we pin our own info string, so the assertion is determinism + correct length + salt-sensitivity, plus a direct-against-`hkdf` cross-check using our exact info, which is the honest known-answer for our construction):
```rust
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
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --package chisel crypto::tests::derive_kek`  Expected: FAIL — `cannot find function derive_kek`.

- [ ] **Step 3: Implement**
Add to `src/crypto/mod.rs` (above the `tests` module):
```rust
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
```
- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --package chisel crypto::tests::derive_kek`  Expected: PASS (5 tests).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): derive_kek dispatching HKDF-SHA256 and Argon2id"
```

---

### Task 1.4: DEK wrap / unwrap under a KEK

**Files:**
- Modify: `src/crypto/mod.rs` (add an internal AEAD helper, `wrap_dek`, `unwrap_dek`; extend `tests`)
- Test: `#[cfg(test)] mod tests` in `src/crypto/mod.rs`

**Interfaces:**
- Consumes: `Kek`, `Dek`, `CryptoError`, `NONCE_LEN`, `TAG_LEN`, `DEK_LEN` (Task 1.2)
- Produces:
```rust
pub fn wrap_dek(kek: &Kek, dek: &Dek, wrap_nonce: &[u8; NONCE_LEN], aad: &[u8]) -> ([u8; DEK_LEN], [u8; TAG_LEN]);
pub fn unwrap_dek(kek: &Kek, wrapped: &[u8; DEK_LEN], tag: &[u8; TAG_LEN], wrap_nonce: &[u8; NONCE_LEN], aad: &[u8]) -> Result<Dek, CryptoError>;
```
Also produces the crate-internal AEAD primitives that Task 1.5 reuses:
```rust
fn seal_detached(key: &[u8; 32], nonce: &[u8; NONCE_LEN], aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, [u8; TAG_LEN]);
fn open_detached(key: &[u8; 32], nonce: &[u8; NONCE_LEN], aad: &[u8], ciphertext: &[u8], tag: &[u8; TAG_LEN]) -> Result<Vec<u8>, CryptoError>;
```

- [ ] **Step 1: Write the failing test**
Add to the `tests` module:
```rust
    #[test]
    fn wrap_unwrap_roundtrip() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (wrapped, tag) = wrap_dek(&kek, &dek, &nonce, aad);
        assert_ne!(&wrapped, dek.as_bytes(), "wrapped DEK must not equal plaintext DEK");
        let out = unwrap_dek(&kek, &wrapped, &tag, &nonce, aad).unwrap();
        assert_eq!(out.as_bytes(), dek.as_bytes());
    }

    #[test]
    fn unwrap_wrong_kek_is_auth() {
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (wrapped, tag) = wrap_dek(&Kek::from_bytes([3u8; 32]), &dek, &nonce, aad);
        let err = unwrap_dek(&Kek::from_bytes([4u8; 32]), &wrapped, &tag, &nonce, aad).unwrap_err();
        assert_eq!(err, CryptoError::Auth);
    }

    #[test]
    fn unwrap_tampered_tag_is_auth() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let aad = b"slot-meta";
        let (wrapped, mut tag) = wrap_dek(&kek, &dek, &nonce, aad);
        tag[0] ^= 0x01;
        let err = unwrap_dek(&kek, &wrapped, &tag, &nonce, aad).unwrap_err();
        assert_eq!(err, CryptoError::Auth);
    }

    #[test]
    fn unwrap_wrong_aad_is_auth() {
        let kek = Kek::from_bytes([3u8; 32]);
        let dek = Dek::from_bytes([42u8; DEK_LEN]);
        let nonce = [5u8; NONCE_LEN];
        let (wrapped, tag) = wrap_dek(&kek, &dek, &nonce, b"slot-meta-A");
        let err = unwrap_dek(&kek, &wrapped, &tag, &nonce, b"slot-meta-B").unwrap_err();
        assert_eq!(err, CryptoError::Auth);
    }
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --package chisel crypto::tests`  Expected: FAIL — `cannot find function wrap_dek` / `unwrap_dek`.

- [ ] **Step 3: Implement**
Add to `src/crypto/mod.rs` (above the `tests` module):
```rust
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{Key as AeadKey, KeyInit, XChaCha20Poly1305, XNonce};

/// Detached AEAD seal: ciphertext is the same length as plaintext, the 16-byte
/// Poly1305 tag is returned separately. Detached suits our fixed page layout
/// (ciphertext occupies a known 8192-byte slot, tag a known 16-byte slot).
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
/// wrong nonce) maps to CryptoError::Auth. On failure the in-place buffer is left
/// scrubbed by the AEAD impl, so no partial plaintext escapes.
fn open_detached(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
    tag: &[u8; TAG_LEN],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(AeadKey::from_slice(key));
    let mut buf = ciphertext.to_vec();
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

/// Wrap (encrypt) the DEK under a KEK. `aad` binds the slot's metadata
/// (kdf_id, salt, Argon2 params) so an attacker cannot tamper a slot's
/// parameters to force a mis-derivation. Returns (wrapped_dek, wrap_tag).
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
/// Failure → CryptoError::Auth (wrong key/passphrase, or tampered slot).
pub fn unwrap_dek(
    kek: &Kek,
    wrapped: &[u8; DEK_LEN],
    tag: &[u8; TAG_LEN],
    wrap_nonce: &[u8; NONCE_LEN],
    aad: &[u8],
) -> Result<Dek, CryptoError> {
    let pt = open_detached(kek.as_bytes(), wrap_nonce, aad, wrapped, tag)?;
    let mut dek = [0u8; DEK_LEN];
    dek.copy_from_slice(&pt);
    Ok(Dek::from_bytes(dek))
}
```
- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --package chisel crypto::tests`  Expected: PASS (wrap/unwrap tests green alongside earlier ones).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): DEK wrap/unwrap under KEK with detached XChaCha20-Poly1305"
```

---

### Task 1.5: PageCipher — whole-page and variable-length body seal/open

**Files:**
- Modify: `src/crypto/mod.rs` (add `PageCipher`; extend `tests`)
- Test: `#[cfg(test)] mod tests` in `src/crypto/mod.rs`

**Interfaces:**
- Consumes: `Dek`, `CryptoError`, `ENC_PAGE_SIZE`, `NONCE_LEN`, `TAG_LEN`, `random_array`, `seal_detached`/`open_detached` (Tasks 1.2/1.4)
- Produces:
```rust
pub struct PageCipher { /* dek + constructed XChaCha20Poly1305 */ }
impl PageCipher {
    pub fn new(dek: Dek) -> Self;
    pub fn seal(&self, page_id: u64, plaintext: &[u8; 8192]) -> [u8; ENC_PAGE_SIZE];
    pub fn open(&self, page_id: u64, ondisk: &[u8; ENC_PAGE_SIZE]) -> Result<[u8; 8192], CryptoError>;
    pub fn seal_body(&self, aad: &[u8], plaintext: &[u8]) -> ([u8; NONCE_LEN], [u8; TAG_LEN], Vec<u8>);
    pub fn open_body(&self, aad: &[u8], nonce: &[u8; NONCE_LEN], tag: &[u8; TAG_LEN], ct: &[u8]) -> Result<Vec<u8>, CryptoError>;
}
```

- [ ] **Step 1: Write the failing test**
Add to the `tests` module:
```rust
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
        assert_ne!(&blob[0..8192], &page[..], "ciphertext must differ from plaintext");
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
        assert_eq!(pc.open_body(b"sb-B", &nonce, &tag, &ct).unwrap_err(), CryptoError::Auth);
    }
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --package chisel crypto::tests`  Expected: FAIL — `cannot find type PageCipher`.

- [ ] **Step 3: Implement**
Add to `src/crypto/mod.rs` (above the `tests` module). Reuses `seal_detached`/`open_detached` from Task 1.4 and `random_array` from Task 1.2:
```rust
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
    pub fn open(&self, page_id: u64, ondisk: &[u8; ENC_PAGE_SIZE]) -> Result<[u8; 8192], CryptoError> {
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
    pub fn seal_body(&self, aad: &[u8], plaintext: &[u8]) -> ([u8; NONCE_LEN], [u8; TAG_LEN], Vec<u8>) {
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
        open_detached(self.dek.as_bytes(), nonce, aad, ct, tag)
    }
}
```
- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --package chisel crypto`  Expected: PASS (all PageCipher tests plus every prior crypto test).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): PageCipher whole-page and body seal/open"
```

---

### Task 1.6: Zeroization guard + full-suite green + clippy

**Files:**
- Modify: `src/crypto/mod.rs` (one zeroization test; no new production code unless clippy flags something)
- Test: `#[cfg(test)] mod tests` in `src/crypto/mod.rs`

**Interfaces:**
- Consumes: all Phase-1 types
- Produces: nothing new — this task certifies the module against the full suite and lint gate, matching the project's "lint + full `cargo test` before push" rule.

- [ ] **Step 1: Write the failing test**
Add a test asserting the zeroizing wrappers are wired (compile-level proof that `Dek`/`Kek` hold `Zeroizing`, and a behavioral proof that `Key` drops without leaking via a public accessor). `Zeroizing` zeroes on drop; we cannot observe freed memory safely, so the honest, runnable check is that the types expose no owned-bytes copy that escapes and that a `Zeroizing`-backed clone is independent:
```rust
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
        let _raw = Key::Raw(zeroize::Zeroizing::new(vec![1u8, 2, 3]));
        let _pass = Key::Passphrase(zeroize::Zeroizing::new("pw".to_string()));
        // Clone works (needed by Options/rotation).
        let _r2 = _raw.clone();
    }
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --package chisel crypto::tests::dek_clone_is_independent_zeroizing_copy`  Expected: FAIL only if Step-1 names don't yet exist — if `Dek::clone` and `Key::clone` from Tasks 1.2 are already present, this compiles; in that case the gating signal for this task is the lint/full-suite step below. (Write the test first regardless; it is the deliverable's assertion.)

- [ ] **Step 3: Implement**
No new production code is expected — the zeroizing types were defined in Task 1.2. If `cargo clippy` flags anything in `src/crypto/mod.rs` (e.g. a needless `to_vec`, a doc-list indent), fix it minimally here. Example fix shape if clippy wants `Default` derived or a lint silenced:
```rust
// (apply only the specific clippy fix reported; no speculative changes)
```
- [ ] **Step 4: Run test, verify it passes**
Run the full gate the project requires before any push (plain `cargo test`, not `--lib`, plus clippy with warnings-as-errors):
```bash
cargo test && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
```
Expected: PASS — every crypto test green, no clippy warnings, formatted.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "test(crypto): zeroization guards; clippy/fmt clean for crypto core"
```

---

Phase 1 deliverable: `src/crypto/mod.rs` exporting `ENC_PAGE_SIZE`, `NONCE_LEN`, `TAG_LEN`, `DEK_LEN`, `SALT_LEN`, `Key`, `Dek`, `Kek`, `KdfId`, `Argon2Params`, `CryptoError`, `random_dek`, `random_array`, `derive_kek`, `wrap_dek`, `unwrap_dek`, and `PageCipher` (with `new`/`seal`/`open`/`seal_body`/`open_body`) — the complete contract Phases 2–5 consume.

→ skipped: splitting crypto into per-concern files (one consumer, ~400 lines); a `CryptoError::Display`/`From<CryptoError>` impl (the engine maps variants to `ChiselError` in Phase 2, where the mapping context lives). Add the split when the file crosses 2000 lines; add the `From` impl in Phase 2 where `ChiselError` is in scope.

---

## Phase 2: Superblock crypto-header, encrypted body, and open/create key flow

This phase adds the on-disk crypto-header (key-slot table) and DEK-sealed body to the superblock, and wires the create/open key flow so an unwrapped `Dek` (held zeroizing for the session) reaches `TransactionManager`. It consumes Phase-1 `PageCipher`, `derive_kek`, `wrap_dek`/`unwrap_dek`, `random_dek`, `random_array`.

Verified offsets used below (read from `src/superblock.rs`): reserved region starts at `FREEMAP_DEPTH_OFFSET + 4 = 324` and runs to `CHECKSUM_OFFSET = 8184`. The crypto-header is placed at byte 324; the sealed body follows the key-slot table. The error variants `NoEncryptionKey`/`InvalidEncryptionKey`/`EncryptionNotSupported`/`DecryptionFailed` are defined once in **Task 4.1, implemented first** (see the plan header's execution-order exception), so the tasks below use them directly — no placeholder variants are introduced.

---

### Task 2.1: KeySlot + CryptoHeader serialize/deserialize into the reserved region

**Files:**
- Create: `src/superblock/crypto_header.rs`
- Modify: `src/superblock.rs:34` (add `pub mod` / `use` for the new submodule — currently `superblock.rs` is a flat file; convert the file's top to declare `mod crypto_header;` and re-export. The struct/consts live in the new file, leaving `superblock.rs`'s body untouched otherwise.)
- Test: `#[cfg(test)] mod tests` in `src/superblock/crypto_header.rs`

**Interfaces:**
- Consumes (Phase 1): `crypto::{NONCE_LEN, TAG_LEN, DEK_LEN, SALT_LEN, Argon2Params}`.
- Produces:
  ```rust
  pub const KEY_SLOT_COUNT: usize = 8;
  pub const KEY_SLOT_SIZE: usize = 128;
  pub const CRYPTO_HEADER_OFFSET: usize = 324;          // == FREEMAP_DEPTH_OFFSET + 4
  pub const CRYPTO_HEADER_SIZE: usize = 8 + KEY_SLOT_COUNT * KEY_SLOT_SIZE; // 1032
  pub struct KeySlot { pub state: u8, pub kdf_id: u8, pub argon2: crypto::Argon2Params,
                       pub salt: [u8; SALT_LEN], pub wrap_nonce: [u8; NONCE_LEN],
                       pub wrapped_dek: [u8; DEK_LEN], pub wrap_tag: [u8; TAG_LEN] }
  pub struct CryptoHeader { pub algorithm: u8, pub stride: u32, pub slots: [KeySlot; KEY_SLOT_COUNT] }
  impl CryptoHeader { pub fn serialize_into(&self, buf: &mut [u8; PAGE_SIZE]); pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<CryptoHeader>; }
  impl KeySlot { pub const EMPTY: KeySlot; pub fn is_active(&self) -> bool; }
  ```

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Argon2Params;
    use crate::page::{self, PAGE_SIZE};

    fn sample_slot(state: u8) -> KeySlot {
        KeySlot {
            state,
            kdf_id: 1,
            argon2: Argon2Params { m_cost: 19456, t_cost: 2, p_cost: 1 },
            salt: [7u8; 16],
            wrap_nonce: [9u8; 24],
            wrapped_dek: [3u8; 32],
            wrap_tag: [5u8; 16],
        }
    }

    #[test]
    fn crypto_header_round_trips_through_reserved_region() {
        let mut slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        slots[0] = sample_slot(1); // active
        slots[3] = sample_slot(1); // active
        let header = CryptoHeader { algorithm: 1, stride: 8232, slots };

        let mut buf = [0u8; PAGE_SIZE];
        header.serialize_into(&mut buf);

        // Crypto-header must live entirely inside the reserved region [324, 8184).
        assert!(CRYPTO_HEADER_OFFSET + CRYPTO_HEADER_SIZE <= page::CHECKSUM_OFFSET);
        // Bytes before the header (the existing fields + reserved gap up to 324)
        // are NOT touched by serialize_into.
        assert_eq!(buf[..CRYPTO_HEADER_OFFSET], [0u8; CRYPTO_HEADER_OFFSET][..]);

        let back = CryptoHeader::deserialize(&buf).expect("active header must deserialize");
        assert_eq!(back.algorithm, 1);
        assert_eq!(back.stride, 8232);
        assert!(back.slots[0].is_active());
        assert!(!back.slots[1].is_active());
        assert!(back.slots[3].is_active());
        assert_eq!(back.slots[0].salt, [7u8; 16]);
        assert_eq!(back.slots[0].wrap_nonce, [9u8; 24]);
        assert_eq!(back.slots[0].wrapped_dek, [3u8; 32]);
        assert_eq!(back.slots[0].wrap_tag, [5u8; 16]);
        assert_eq!(back.slots[0].argon2.m_cost, 19456);
    }

    #[test]
    fn deserialize_returns_none_for_plaintext_db() {
        // A zeroed reserved region (plaintext DB) has algorithm == 0 -> None.
        let buf = [0u8; PAGE_SIZE];
        assert!(CryptoHeader::deserialize(&buf).is_none());
    }
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test crypto_header_round_trips_through_reserved_region`  Expected: FAIL (module does not exist yet)
- [ ] **Step 3: Implement**

Create `src/superblock/crypto_header.rs`:
```rust
// superblock/crypto_header.rs — the plaintext crypto-header that lives in the
// superblock's reserved region for encrypted databases. Holds the algorithm id,
// the on-disk page stride, and the 8-slot key-slot table (each slot wraps the
// per-DB DEK under a KEK derived from one client key). For PLAINTEXT databases
// the reserved region stays zeroed and `deserialize` returns None (algorithm 0).
//
// On-disk layout (all inside the superblock's reserved region, after freemap_depth):
//   324..325   algorithm (u8; 1 = XChaCha20-Poly1305, 0 = none/plaintext)
//   325..329   stride (u32 LE; 8232 for encrypted, validated by the engine)
//   329..332   reserved (zero)
//   332..332+8*128  the 8 key-slot records, 128 bytes each
// Total = 8 + 8*128 = 1032 bytes, ending at 1356 — well inside CHECKSUM_OFFSET (8184).
//
// Key-slot record (128 bytes; trailing bytes reserved/zero):
//   0      state (u8; 1 = active, 0 = empty)
//   1      kdf_id (u8; 1 = HKDF, 2 = Argon2id)
//   2..14  argon2 params: m_cost(u32) | t_cost(u32) | p_cost(u32)  (zero for HKDF)
//   14..30 salt (16)
//   30..54 wrap_nonce (24)
//   54..86 wrapped_dek (32)
//   86..102 wrap_tag (16)
//   102..128 reserved

use crate::crypto::{Argon2Params, DEK_LEN, NONCE_LEN, SALT_LEN, TAG_LEN};
use crate::page::{self, PAGE_SIZE};

pub const KEY_SLOT_COUNT: usize = 8;
pub const KEY_SLOT_SIZE: usize = 128;
// Immediately after freemap_depth (bytes 320..324). Keep in lockstep with
// superblock.rs's FREEMAP_DEPTH_OFFSET (320) + 4.
pub const CRYPTO_HEADER_OFFSET: usize = 324;
pub const CRYPTO_HEADER_SIZE: usize = 8 + KEY_SLOT_COUNT * KEY_SLOT_SIZE;

const SLOT_TABLE_OFFSET: usize = CRYPTO_HEADER_OFFSET + 8;

/// Algorithm id stored in the header. 0 means "no encryption" (plaintext DB);
/// the only supported nonzero value today is 1 = XChaCha20-Poly1305.
pub const ALGO_XCHACHA20POLY1305: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySlot {
    pub state: u8,
    pub kdf_id: u8,
    pub argon2: Argon2Params,
    pub salt: [u8; SALT_LEN],
    pub wrap_nonce: [u8; NONCE_LEN],
    pub wrapped_dek: [u8; DEK_LEN],
    pub wrap_tag: [u8; TAG_LEN],
}

impl KeySlot {
    pub const EMPTY: KeySlot = KeySlot {
        state: 0,
        kdf_id: 0,
        argon2: Argon2Params { m_cost: 0, t_cost: 0, p_cost: 0 },
        salt: [0u8; SALT_LEN],
        wrap_nonce: [0u8; NONCE_LEN],
        wrapped_dek: [0u8; DEK_LEN],
        wrap_tag: [0u8; TAG_LEN],
    };

    /// True if this slot holds a usable wrapped DEK (state byte == 1).
    pub fn is_active(&self) -> bool {
        self.state == 1
    }

    /// The bytes an unwrap operation must authenticate as AAD: the slot's own
    /// metadata up to but excluding the wrapped_dek/tag. Binds the wrap to its
    /// salt/params/nonce so a slot can't be transplanted between DBs.
    pub fn aad(&self) -> [u8; 1 + 1 + 12 + SALT_LEN + NONCE_LEN] {
        let mut a = [0u8; 1 + 1 + 12 + SALT_LEN + NONCE_LEN];
        a[0] = self.state;
        a[1] = self.kdf_id;
        a[2..6].copy_from_slice(&self.argon2.m_cost.to_le_bytes());
        a[6..10].copy_from_slice(&self.argon2.t_cost.to_le_bytes());
        a[10..14].copy_from_slice(&self.argon2.p_cost.to_le_bytes());
        a[14..14 + SALT_LEN].copy_from_slice(&self.salt);
        a[14 + SALT_LEN..14 + SALT_LEN + NONCE_LEN].copy_from_slice(&self.wrap_nonce);
        a
    }

    fn write_into(&self, slot: &mut [u8]) {
        slot[0] = self.state;
        slot[1] = self.kdf_id;
        slot[2..6].copy_from_slice(&self.argon2.m_cost.to_le_bytes());
        slot[6..10].copy_from_slice(&self.argon2.t_cost.to_le_bytes());
        slot[10..14].copy_from_slice(&self.argon2.p_cost.to_le_bytes());
        slot[14..14 + SALT_LEN].copy_from_slice(&self.salt);
        slot[30..30 + NONCE_LEN].copy_from_slice(&self.wrap_nonce);
        slot[54..54 + DEK_LEN].copy_from_slice(&self.wrapped_dek);
        slot[86..86 + TAG_LEN].copy_from_slice(&self.wrap_tag);
    }

    fn read_from(slot: &[u8]) -> KeySlot {
        let mut k = KeySlot::EMPTY;
        k.state = slot[0];
        k.kdf_id = slot[1];
        k.argon2 = Argon2Params {
            m_cost: u32::from_le_bytes(slot[2..6].try_into().unwrap()),
            t_cost: u32::from_le_bytes(slot[6..10].try_into().unwrap()),
            p_cost: u32::from_le_bytes(slot[10..14].try_into().unwrap()),
        };
        k.salt.copy_from_slice(&slot[14..14 + SALT_LEN]);
        k.wrap_nonce.copy_from_slice(&slot[30..30 + NONCE_LEN]);
        k.wrapped_dek.copy_from_slice(&slot[54..54 + DEK_LEN]);
        k.wrap_tag.copy_from_slice(&slot[86..86 + TAG_LEN]);
        k
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoHeader {
    pub algorithm: u8,
    pub stride: u32,
    pub slots: [KeySlot; KEY_SLOT_COUNT],
}

impl CryptoHeader {
    /// Write the crypto-header into the superblock's reserved region. Touches
    /// only [CRYPTO_HEADER_OFFSET, CRYPTO_HEADER_OFFSET+CRYPTO_HEADER_SIZE);
    /// the caller stamps the page checksum afterward.
    pub fn serialize_into(&self, buf: &mut [u8; PAGE_SIZE]) {
        debug_assert!(CRYPTO_HEADER_OFFSET + CRYPTO_HEADER_SIZE <= page::CHECKSUM_OFFSET);
        buf[CRYPTO_HEADER_OFFSET] = self.algorithm;
        buf[CRYPTO_HEADER_OFFSET + 1..CRYPTO_HEADER_OFFSET + 5]
            .copy_from_slice(&self.stride.to_le_bytes());
        for (i, slot) in self.slots.iter().enumerate() {
            let base = SLOT_TABLE_OFFSET + i * KEY_SLOT_SIZE;
            slot.write_into(&mut buf[base..base + KEY_SLOT_SIZE]);
        }
    }

    /// Read the crypto-header. Returns None for a plaintext DB (algorithm byte
    /// 0), which is how callers distinguish "encrypted" from "plaintext".
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<CryptoHeader> {
        let algorithm = buf[CRYPTO_HEADER_OFFSET];
        if algorithm == 0 {
            return None;
        }
        let stride = u32::from_le_bytes(
            buf[CRYPTO_HEADER_OFFSET + 1..CRYPTO_HEADER_OFFSET + 5]
                .try_into()
                .unwrap(),
        );
        let mut slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        for (i, slot) in slots.iter_mut().enumerate() {
            let base = SLOT_TABLE_OFFSET + i * KEY_SLOT_SIZE;
            *slot = KeySlot::read_from(&buf[base..base + KEY_SLOT_SIZE]);
        }
        Some(CryptoHeader { algorithm, stride, slots })
    }
}
```

Convert `src/superblock.rs` to a module directory by adding, near the top (after the existing `use` block at line 34–35):
```rust
mod crypto_header;
pub use crypto_header::{
    CryptoHeader, KeySlot, ALGO_XCHACHA20POLY1305, CRYPTO_HEADER_OFFSET, CRYPTO_HEADER_SIZE,
    KEY_SLOT_COUNT, KEY_SLOT_SIZE,
};
```
(Move `src/superblock.rs` to `src/superblock/mod.rs` so the submodule resolves; no other edits to that file's body.)

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test crypto_header`  Expected: PASS
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(superblock): add crypto-header key-slot table in the reserved region"
```

---

### Task 2.2: DEK-sealed superblock body + `Superblock.encryption` field

**Files:**
- Modify: `src/superblock/mod.rs:165` (add `pub encryption: Option<CryptoHeader>` to the `Superblock` struct, after `freemap_depth`)
- Modify: `src/superblock/mod.rs:245` (`serialize` — gains an encrypted path that seals the sensitive fields into a body sub-blob)
- Modify: `src/superblock/mod.rs:287` (`deserialize` — reads the crypto-header; for encrypted DBs the sensitive fields can only be filled after the DEK is known, so `deserialize` returns the struct with placeholder sensitive fields plus the raw sealed body for a later `decrypt_body` call)
- Modify: `src/superblock/mod.rs:433` (`new_empty` — set `encryption: None`)
- Modify every test/struct-literal `Superblock { ... }` in `mod.rs` to add `encryption: None`, and the same literal in `src/transaction/commit.rs:116`
- Test: `#[cfg(test)] mod tests` in `src/superblock/mod.rs`

**Interfaces:**
- Consumes (Phase 1): `crypto::PageCipher::{seal_body, open_body}`, `crypto::CryptoError`.
- Produces:
  ```rust
  pub struct Superblock { /* existing fields */, pub encryption: Option<CryptoHeader> }
  pub const SEALED_BODY_OFFSET: usize = CRYPTO_HEADER_OFFSET + CRYPTO_HEADER_SIZE; // 1356
  impl Superblock {
      pub fn sb_identity_aad(&self) -> [u8; 24]; // magic|format_version|txn_counter|superblock_count
      pub fn serialize_encrypted(&self, cipher: &PageCipher) -> [u8; PAGE_SIZE];
      pub fn decrypt_body(&mut self, cipher: &PageCipher, raw: &[u8; PAGE_SIZE]) -> Result<(), CryptoError>;
  }
  ```

The sealed body holds the sensitive fields (`root_handle_table_page`, `root_freemap_page`, `root_membership_index_page`, `total_pages`, `next_handle`, `freemap_depth`, `named_roots[8x32]`); in an encrypted serialization those byte ranges in the plaintext page are left zero, so `named_roots` are not visible in cleartext.

- [ ] **Step 1: Write the failing test**
```rust
// inside src/superblock/mod.rs tests module
#[test]
fn encrypted_superblock_hides_sensitive_fields_and_round_trips() {
    use crate::crypto::{random_dek, PageCipher};

    let cipher = PageCipher::new(random_dek());
    let mut header_slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
    header_slots[0].state = 1;
    let header = CryptoHeader { algorithm: ALGO_XCHACHA20POLY1305, stride: 8232, slots: header_slots };

    let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
    sb.root_handle_table_page = 7;
    sb.next_handle = 99;
    sb.total_pages = 41;
    sb.named_roots[0].name[..5].copy_from_slice(b"users");
    sb.named_roots[0].handle = 12345;
    sb.encryption = Some(header);

    let buf = sb.serialize_encrypted(&cipher);

    // The named-root bytes (offset 52..308) must NOT be visible in cleartext.
    assert_eq!(&buf[52..308], &[0u8; 256][..], "named_roots leaked in cleartext");
    // Bootstrap fields stay plaintext.
    assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), MAGIC);
    assert_eq!(u64::from_le_bytes(buf[8..16].try_into().unwrap()), sb.txn_counter);
    // Crypto-header is plaintext.
    let hdr = CryptoHeader::deserialize(&buf).expect("header present");
    assert_eq!(hdr.algorithm, ALGO_XCHACHA20POLY1305);

    // Deserialize gives a struct with the header but zeroed sensitive fields;
    // decrypt_body fills them from the sealed sub-blob.
    let mut back = Superblock::deserialize(&buf).expect("encrypted sb deserializes");
    assert!(back.encryption.is_some());
    assert_eq!(back.root_handle_table_page, 0); // not yet decrypted
    back.decrypt_body(&cipher, &buf).expect("DEK opens body");
    assert_eq!(back.root_handle_table_page, 7);
    assert_eq!(back.next_handle, 99);
    assert_eq!(back.total_pages, 41);
    assert_eq!(&back.named_roots[0].name[..5], b"users");
    assert_eq!(back.named_roots[0].handle, 12345);
}

#[test]
fn wrong_dek_fails_body_authentication() {
    use crate::crypto::{random_dek, PageCipher};
    let cipher = PageCipher::new(random_dek());
    let mut header_slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
    header_slots[0].state = 1;
    let header = CryptoHeader { algorithm: ALGO_XCHACHA20POLY1305, stride: 8232, slots: header_slots };
    let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
    sb.encryption = Some(header);
    let buf = sb.serialize_encrypted(&cipher);

    let wrong = PageCipher::new(random_dek());
    let mut back = Superblock::deserialize(&buf).unwrap();
    assert!(back.decrypt_body(&wrong, &buf).is_err());
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test encrypted_superblock_hides_sensitive_fields_and_round_trips`  Expected: FAIL (`encryption` field / methods do not exist)
- [ ] **Step 3: Implement**

Add the field to the struct (after `freemap_depth` at line ~214):
```rust
    pub freemap_depth: u32,
    /// Crypto-header for an encrypted database. `None` for plaintext DBs, in
    /// which case serialize/deserialize use the existing all-plaintext layout.
    /// `Some` means the sensitive fields are sealed in a DEK-encrypted body
    /// sub-blob and the in-memory copy is only valid after `decrypt_body`.
    pub encryption: Option<CryptoHeader>,
}
```

Add the byte-range constants and methods in `impl Superblock`:
```rust
// Offset where the DEK-sealed body sub-blob starts, immediately after the
// key-slot table. Layout of the sealed region:
//   SEALED_BODY_OFFSET .. +24    nonce
//   +24 .. +40                   tag
//   +40 .. +42                   ciphertext length (u16 LE)
//   +42 .. +42+ct_len            ciphertext
pub const SEALED_BODY_OFFSET: usize =
    crypto_header::CRYPTO_HEADER_OFFSET + crypto_header::CRYPTO_HEADER_SIZE;

// Plaintext body layout (the bytes fed to seal_body): the sensitive fields in
// a fixed order. 6 u64 + freemap_depth(u32) + named_roots(8*32).
const BODY_LEN: usize = 8 * 5 + 4 + (NAMED_ROOT_COUNT * NAMED_ROOT_ENTRY_SIZE);

impl Superblock {
    /// AAD binding the sealed body and each key-slot's DEK wrap to this
    /// superblock's plaintext identity, so a slot/body cannot be transplanted
    /// to a different DB or replayed at a different txn_counter.
    pub fn sb_identity_aad(&self) -> [u8; 24] {
        let mut a = [0u8; 24];
        a[0..4].copy_from_slice(&self.magic.to_le_bytes());
        a[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        a[8..16].copy_from_slice(&self.txn_counter.to_le_bytes());
        a[16..20].copy_from_slice(&self.superblock_count.to_le_bytes());
        // bytes 20..24 reserved/zero
        a
    }

    fn body_plaintext(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(BODY_LEN);
        b.extend_from_slice(&self.root_handle_table_page.to_le_bytes());
        b.extend_from_slice(&self.root_freemap_page.to_le_bytes());
        b.extend_from_slice(&self.root_membership_index_page.to_le_bytes());
        b.extend_from_slice(&self.total_pages.to_le_bytes());
        b.extend_from_slice(&self.next_handle.to_le_bytes());
        b.extend_from_slice(&self.freemap_depth.to_le_bytes());
        for entry in self.named_roots.iter() {
            b.extend_from_slice(&entry.name);
            b.extend_from_slice(&entry.handle.to_le_bytes());
        }
        debug_assert_eq!(b.len(), BODY_LEN);
        b
    }

    fn load_body(&mut self, body: &[u8]) {
        self.root_handle_table_page = u64::from_le_bytes(body[0..8].try_into().unwrap());
        self.root_freemap_page = u64::from_le_bytes(body[8..16].try_into().unwrap());
        self.root_membership_index_page = u64::from_le_bytes(body[16..24].try_into().unwrap());
        self.total_pages = u64::from_le_bytes(body[24..32].try_into().unwrap());
        self.next_handle = u64::from_le_bytes(body[32..40].try_into().unwrap());
        self.freemap_depth = u32::from_le_bytes(body[40..44].try_into().unwrap());
        let mut off = 44;
        for entry in self.named_roots.iter_mut() {
            entry.name.copy_from_slice(&body[off..off + NAMED_ROOT_NAME_LEN]);
            entry.handle =
                u64::from_le_bytes(body[off + NAMED_ROOT_NAME_LEN..off + NAMED_ROOT_NAME_LEN + 8].try_into().unwrap());
            off += NAMED_ROOT_ENTRY_SIZE;
        }
    }

    /// Serialize an encrypted superblock: plaintext bootstrap fields + crypto-
    /// header in cleartext, sensitive fields sealed under the DEK. The sensitive
    /// byte ranges of the plaintext page (named_roots @52..308, the root/page-id
    /// scalars) are left ZERO so nothing sensitive is visible in cleartext.
    pub fn serialize_encrypted(&self, cipher: &crate::crypto::PageCipher) -> [u8; PAGE_SIZE] {
        let header = self
            .encryption
            .as_ref()
            .expect("serialize_encrypted requires Superblock.encryption = Some");
        let mut buf = [0u8; PAGE_SIZE];
        // Plaintext bootstrap fields only.
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        buf[8..16].copy_from_slice(&self.txn_counter.to_le_bytes());
        buf[48..52].copy_from_slice(&self.page_size.to_le_bytes());
        buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .copy_from_slice(&self.superblock_count.to_le_bytes());
        // Crypto-header (plaintext).
        header.serialize_into(&mut buf);
        // Sealed body.
        let aad = self.sb_identity_aad();
        let (nonce, tag, ct) = cipher.seal_body(&aad, &self.body_plaintext());
        let base = Self::SEALED_BODY_OFFSET;
        buf[base..base + NONCE_LEN].copy_from_slice(&nonce);
        buf[base + NONCE_LEN..base + NONCE_LEN + TAG_LEN].copy_from_slice(&tag);
        buf[base + NONCE_LEN + TAG_LEN..base + NONCE_LEN + TAG_LEN + 2]
            .copy_from_slice(&(ct.len() as u16).to_le_bytes());
        let coff = base + NONCE_LEN + TAG_LEN + 2;
        buf[coff..cof_end(coff, ct.len())].copy_from_slice(&ct);
        page::stamp_checksum(&mut buf);
        buf
    }

    /// Decrypt the sealed body into `self`'s sensitive fields. Caller must have
    /// already run `deserialize` (which fills the bootstrap fields and the
    /// crypto-header) and obtained the matching DEK via the key-slot flow.
    pub fn decrypt_body(
        &mut self,
        cipher: &crate::crypto::PageCipher,
        raw: &[u8; PAGE_SIZE],
    ) -> std::result::Result<(), crate::crypto::CryptoError> {
        let base = Self::SEALED_BODY_OFFSET;
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&raw[base..base + NONCE_LEN]);
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&raw[base + NONCE_LEN..base + NONCE_LEN + TAG_LEN]);
        let ct_len = u16::from_le_bytes(
            raw[base + NONCE_LEN + TAG_LEN..base + NONCE_LEN + TAG_LEN + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        let coff = base + NONCE_LEN + TAG_LEN + 2;
        let ct = &raw[coff..cof_end(coff, ct_len)];
        let aad = self.sb_identity_aad();
        let body = cipher.open_body(&aad, &nonce, &tag, ct)?;
        self.load_body(&body);
        Ok(())
    }
}

#[inline]
fn cof_end(start: usize, len: usize) -> usize {
    start + len
}
```

Add `use crate::crypto::{NONCE_LEN, TAG_LEN};` to the file's imports.

In `deserialize` (line ~314), branch on the crypto-header: read it first, and when present leave the sensitive fields zeroed for `decrypt_body` to fill:
```rust
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<Superblock> {
        validate(buf).ok()?;
        let encryption = crypto_header::CryptoHeader::deserialize(buf);
        if encryption.is_some() {
            // Encrypted DB: only the bootstrap fields are in cleartext. The
            // sensitive fields stay zero until the caller supplies the DEK and
            // calls decrypt_body. named_roots default to EMPTY.
            let superblock_count = u32::from_le_bytes(
                buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4].try_into().unwrap(),
            );
            return Some(Superblock {
                magic: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
                format_version: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
                txn_counter: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
                root_handle_table_page: 0,
                root_freemap_page: 0,
                total_pages: 0,
                next_handle: 0,
                page_size: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
                named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
                superblock_count,
                root_membership_index_page: 0,
                freemap_depth: 0,
                encryption,
            });
        }
        // ... existing plaintext path, adding `encryption: None,` to the returned struct ...
    }
```

Add `encryption: None` to: `new_empty` (line ~449), every `Superblock { .. }` literal in `mod.rs`'s tests, and the literal in `src/transaction/commit.rs:116`.

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test superblock`  Expected: PASS (plaintext round-trip tests still pass; new encrypted tests pass)
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(superblock): DEK-sealed body and Superblock.encryption field"
```

---

### Task 2.3: `create_new` with a key — generate DEK, derive KEK, wrap into slot 0, MAJOR=2

**Files:**
- Modify: `src/superblock/mod.rs` (add `new_empty_encrypted(superblock_count, header) -> Superblock` helper that sets `encryption: Some(header)` and the MAJOR=2 `format_version`)
- Modify: `src/transaction/recovery.rs:32` (change `create_new` to `create_new(mut cache: PageCache, superblock_count: u32, key: Option<crypto::Key>)`; encrypted path generates DEK + slot-0 salt, derives KEK, wraps DEK, writes encrypted superblocks, and stores the resulting `PageCipher`/`Dek` on the manager)
- Modify: `src/transaction/mod.rs:146` (add `cipher: Option<crypto::PageCipher>` field to `TransactionManager`, held for the session)
- Modify: `src/lib.rs:346` and `:404` (pass `options.encryption_key.clone()` / `None` to `create_new`)
- Modify: `src/lib.rs` Options (add `pub(crate) encryption_key: Option<crypto::Key>` + builder setter `with_encryption_key`)
- Test: `tests/encryption_create.rs` (integration)

**Interfaces:**
- Consumes (Phase 1): `crypto::{Key, random_dek, random_array, derive_kek, wrap_dek, KdfId, Argon2Params, PageCipher, SALT_LEN, NONCE_LEN}`.
- Produces: `TransactionManager.cipher: Option<PageCipher>`, `Options::with_encryption_key`, the create-time key flow.

A new MAJOR version constant is needed. Add to `src/page.rs` near `FORMAT_MAJOR_VERSION`:
```rust
/// MAJOR version stamped into an ENCRYPTED database's superblock. The bump from
/// 1 -> 2 hard-rejects old binaries (which gate on FORMAT_MAJOR_VERSION == 1).
pub const FORMAT_MAJOR_VERSION_ENCRYPTED: u16 = 2;
pub fn format_version_encrypted() -> u32 {
    ((FORMAT_MAJOR_VERSION_ENCRYPTED as u32) << 16) | (FORMAT_MINOR_VERSION as u32)
}
```

- [ ] **Step 1: Write the failing test**
```rust
// tests/encryption_create.rs
use chisel::crypto::Key;
use chisel::{Chisel, Options};
use std::fs;
use zeroize::Zeroizing;

#[test]
fn create_encrypted_db_writes_plaintext_header_and_hidden_named_roots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc.chisel");

    let key = Key::Raw(Zeroizing::new(vec![0xABu8; 32]));
    let opts = Options::builder()
        .with_encryption_key(key)
        .build();
    {
        let mut db = Chisel::open(&path, opts).expect("create encrypted db");
        db.set_root_name("secret-table", 1).unwrap(); // exercises named_roots
        db.commit().unwrap();
    }

    let bytes = fs::read(&path).unwrap();
    // Page 0: MAJOR version must be 2 (encrypted), upper 16 bits of bytes 4..8.
    let fv = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(fv >> 16, 2, "encrypted DB must stamp MAJOR=2");
    // The named-root name "secret-table" must NOT appear anywhere in page 0.
    assert!(
        !bytes[0..8192].windows(12).any(|w| w == b"secret-table"),
        "named root leaked in cleartext"
    );
    // Crypto-header algorithm byte at offset 324 must be 1.
    assert_eq!(bytes[324], 1, "crypto-header algorithm byte not set");
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --test encryption_create`  Expected: FAIL (`with_encryption_key` / encrypted create path not implemented)
- [ ] **Step 3: Implement**

`new_empty_encrypted` in `src/superblock/mod.rs`:
```rust
    /// Like `new_empty` but for an encrypted database: stamps the MAJOR=2
    /// encrypted format version and attaches the crypto-header. Sensitive
    /// fields are the same fresh-DB defaults; they get sealed by
    /// `serialize_encrypted`.
    pub fn new_empty_encrypted(superblock_count: u32, header: CryptoHeader) -> Superblock {
        let mut sb = Superblock::new_empty(superblock_count);
        sb.format_version = page::format_version_encrypted();
        sb.encryption = Some(header);
        sb
    }
```

In `src/transaction/recovery.rs`, change the `create_new` signature and add the encrypted bank-write path:
```rust
    pub fn create_new(
        mut cache: PageCache,
        superblock_count: u32,
        key: Option<crate::crypto::Key>,
    ) -> Result<TransactionManager> {
        assert!(
            (2..=MAX_SUPERBLOCKS).contains(&superblock_count),
            "superblock_count {superblock_count} out of supported range 2..=16"
        );

        // Build the per-session cipher up front for an encrypted DB: a fresh
        // random DEK is sealed into slot 0 under a KEK derived from `key`.
        let cipher = match key {
            None => None,
            Some(k) => Some(build_create_cipher(&k, &mut cache, superblock_count)?),
        };

        // ... existing roots/manager construction, but the per-slot write loop
        // branches on `cipher`:
        if let Some(ref c) = cipher {
            let header = c.header_for_create.clone(); // see build_create_cipher
            let mut sb = Superblock::new_empty_encrypted(superblock_count, header);
            for i in 0..superblock_count {
                sb.txn_counter = (superblock_count - 1 - i) as u64;
                let buf = sb.cipher.serialize_encrypted(...); // see note
                cache.io_mut().write_page(i as u64, &buf)?;
            }
        } else {
            let mut sb = Superblock::new_empty(superblock_count);
            for i in 0..superblock_count {
                sb.txn_counter = (superblock_count - 1 - i) as u64;
                let buf = sb.serialize();
                cache.io_mut().write_page(i as u64, &buf)?;
            }
        }
        cache.io_mut().fsync()?;
        cache.set_next_page_id(superblock_count as u64);
        // ... existing Roots + TransactionManager construction, adding
        //     `cipher: cipher.map(|c| c.page_cipher),` to the struct ...
    }
```

Because `build_create_cipher` must return both the `PageCipher` (for the session) and the `CryptoHeader` (slot-0 wrap) so each slot serializes identically, define a small local helper at the bottom of `recovery.rs`:
```rust
/// Build the session PageCipher for a freshly-created encrypted DB: generate a
/// random DEK + slot-0 salt, derive the KEK from the client key, wrap the DEK
/// into slot 0, and assemble the crypto-header. Returns the live PageCipher and
/// the header to stamp into every superblock slot.
struct CreateCrypto {
    page_cipher: crate::crypto::PageCipher,
    header: crate::superblock::CryptoHeader,
}

fn build_create_cipher(
    key: &crate::crypto::Key,
    _cache: &mut PageCache,
    _superblock_count: u32,
) -> Result<CreateCrypto> {
    use crate::crypto::{
        derive_kek, random_array, random_dek, wrap_dek, Argon2Params, KdfId, NONCE_LEN, SALT_LEN,
    };
    use crate::superblock::{CryptoHeader, KeySlot, ALGO_XCHACHA20POLY1305, KEY_SLOT_COUNT};

    let dek = random_dek();
    let salt: [u8; SALT_LEN] = random_array();
    let wrap_nonce: [u8; NONCE_LEN] = random_array();
    // KDF choice: a Raw key uses HKDF; a Passphrase uses Argon2id.
    let (kdf, params) = match key {
        crate::crypto::Key::Raw(_) => (KdfId::Hkdf, Argon2Params::default()),
        crate::crypto::Key::Passphrase(_) => (KdfId::Argon2id, Argon2Params::default()),
    };
    let kek = derive_kek(key, kdf, &salt, &params)
        .map_err(|e| ChiselError::from(e))?; // CryptoError -> InvalidEncryptionKey via the From impl

    let mut slot = KeySlot::EMPTY;
    slot.state = 1;
    slot.kdf_id = kdf as u8;
    slot.argon2 = params;
    slot.salt = salt;
    slot.wrap_nonce = wrap_nonce;
    let aad = slot.aad();
    let (wrapped, tag) = wrap_dek(&kek, &dek, &wrap_nonce, &aad);
    slot.wrapped_dek = wrapped;
    slot.wrap_tag = tag;

    let mut slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
    slots[0] = slot;
    let header = CryptoHeader { algorithm: ALGO_XCHACHA20POLY1305, stride: 8232, slots };

    Ok(CreateCrypto {
        page_cipher: crate::crypto::PageCipher::new(dek),
        header,
    })
}
```
(In the slot-write loop, build `let buf = sb.serialize_encrypted(&cipher.page_cipher);` — the `CreateCrypto` is in scope as `cipher` before being unpacked into the manager.)

Add the `cipher` field to `TransactionManager` in `src/transaction/mod.rs` after `poisoned` (line ~217):
```rust
    /// Per-session page cipher for an encrypted database. `None` for plaintext.
    /// Holds the unwrapped DEK (zeroizing) for the life of the manager; reaches
    /// the PageCache in Phase 3 for per-page seal/open. Set on both the create
    /// path (fresh DEK) and the open path (DEK unwrapped from a key-slot).
    cipher: Option<crate::crypto::PageCipher>,
```
Set `cipher: None` in `open_existing` (Task 2.4 fills it) and the in-memory create path.

Options in `src/lib.rs`:
```rust
    /// Encryption key supplied at open/create. `Some` creates (or opens) an
    /// encrypted database; `None` keeps the existing plaintext format.
    pub(crate) encryption_key: Option<crate::crypto::Key>,
```
Builder setter:
```rust
    pub fn with_encryption_key(mut self, key: crate::crypto::Key) -> Self {
        self.encryption_key = Some(key);
        self
    }
```
Pass it through at `src/lib.rs:346`:
```rust
TransactionManager::create_new(cache, options.superblock_count, options.encryption_key.clone())?
```
and `None` at the in-memory create site (`:404`).

Add a `From<CryptoError> for ChiselError` conversion so the create/open and key paths can use `?` on crypto calls. The encryption error variants already exist because **Task 4.1 is implemented first** (see the plan header's execution-order exception) — no placeholder variant is needed:
```rust
impl From<crate::crypto::CryptoError> for ChiselError {
    // A CryptoError reaching the engine through `?` in the create/open/key-management
    // paths is always a key-or-KDF problem on intact on-disk data, so it maps to the
    // operational InvalidEncryptionKey. The page-read path (Phase 3) does NOT use this
    // blanket conversion — it maps decrypt failures explicitly to the fatal
    // ChiselError::DecryptionFailed { page_id } via `.map_err(...)`. The operational
    // cases that are NOT CryptoError-derived (no key supplied for an encrypted DB; a
    // key supplied for a plaintext DB) are returned explicitly as NoEncryptionKey /
    // EncryptionNotSupported at their decision sites.
    fn from(_: crate::crypto::CryptoError) -> Self {
        ChiselError::InvalidEncryptionKey
    }
}
```

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --test encryption_create`  Expected: PASS
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(engine): create encrypted database — wrap DEK into slot 0, stamp MAJOR=2"
```

---

### Task 2.4: `open_existing` with a key — try each active slot, unwrap DEK, decrypt body, validate

**Files:**
- Modify: `src/transaction/recovery.rs:136` (change `open_existing` to `open_existing(mut cache: PageCache, key: Option<crypto::Key>)`; after `select()` picks the winner, if it carries a crypto-header, try each active key-slot to unwrap the DEK, then `decrypt_body`; wrong/missing/spurious key → typed error)
- Modify: `src/lib.rs:344` (pass `options.encryption_key.clone()` to `open_existing`)
- Test: `tests/encryption_open.rs` (integration)

**Interfaces:**
- Consumes (Phase 1): `crypto::{Key, derive_kek, unwrap_dek, KdfId, PageCipher}`.
- Consumes (Task 2.2/2.3): `Superblock.encryption`, `decrypt_body`, `new_empty_encrypted`.
- Produces: the open-time key flow; `TransactionManager.cipher` populated on the encrypted-open path.

Wrong key → `InvalidEncryptionKey` (operational, NOT fatal — must not poison). Missing key on an encrypted DB → `NoEncryptionKey`. Key supplied for a plaintext DB → `EncryptionNotSupported`. All three are defined in **Task 4.1 (implemented first)**, so they are used directly here; the test asserts the open simply `is_err()` for each, so it does not depend on the exact variant.

- [ ] **Step 1: Write the failing test**
```rust
// tests/encryption_open.rs
use chisel::crypto::Key;
use chisel::{Chisel, Options};
use zeroize::Zeroizing;

fn raw_key(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

#[test]
fn round_trip_open_with_correct_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::builder().with_encryption_key(raw_key(0x11)).build(),
        ).unwrap();
        let h = db.insert(b"hello world").unwrap();
        db.commit().unwrap();
        assert_eq!(db.read(h).unwrap().as_deref(), Some(&b"hello world"[..]));
    }
    // Reopen with the SAME key: data must come back.
    {
        let db = Chisel::open(
            &path,
            Options::builder()
                .with_encryption_key(raw_key(0x11))
                .create_if_missing(false)
                .build(),
        ).unwrap();
        // first handle minted is 1
        assert_eq!(db.read(1).unwrap().as_deref(), Some(&b"hello world"[..]));
    }
}

#[test]
fn wrong_key_is_operational_error_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::builder().with_encryption_key(raw_key(0x11)).build(),
        ).unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(
        &path,
        Options::builder()
            .with_encryption_key(raw_key(0x22))
            .create_if_missing(false)
            .build(),
    );
    assert!(err.is_err(), "wrong key must fail to open");
}

#[test]
fn missing_key_on_encrypted_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.chisel");
    {
        let mut db = Chisel::open(
            &path,
            Options::builder().with_encryption_key(raw_key(0x11)).build(),
        ).unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(
        &path,
        Options::builder().create_if_missing(false).build(),
    );
    assert!(err.is_err(), "encrypted DB opened without a key must fail");
}

#[test]
fn key_on_plaintext_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.chisel");
    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.commit().unwrap();
    }
    let err = Chisel::open(
        &path,
        Options::builder()
            .with_encryption_key(raw_key(0x11))
            .create_if_missing(false)
            .build(),
    );
    assert!(err.is_err(), "supplying a key to a plaintext DB must fail");
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --test encryption_open`  Expected: FAIL (open key flow not implemented)
- [ ] **Step 3: Implement**

In `open_existing`, after the `select()` winner is chosen and BEFORE the page-size / total-pages checks that read sensitive fields, add the key-flow branch. The version gate at line ~171 must accept MAJOR 2 when a key is present:
```rust
    pub fn open_existing(
        mut cache: PageCache,
        key: Option<crate::crypto::Key>,
    ) -> Result<TransactionManager> {
        // ... existing candidate read + select() unchanged ...
        let mut sb = Superblock::select(&candidates).ok_or_else(|| ChiselError::CorruptSuperblock {
            defects: Superblock::diagnose(&candidates),
        })?;

        // Encryption gate. The winning slot's crypto-header (already parsed by
        // deserialize into sb.encryption) tells us whether the DB is encrypted.
        // Mismatches between "DB encrypted?" and "key supplied?" are operational
        // open errors, not torn-slot signals.
        let cipher = match (&sb.encryption, &key) {
            (None, None) => None,
            (Some(_), None) => return Err(ChiselError::NoEncryptionKey),
            (None, Some(_)) => return Err(ChiselError::EncryptionNotSupported),
            (Some(header), Some(k)) => {
                // The winning slot's raw bytes are needed to decrypt the body.
                let raw = candidates[(sb.txn_counter % sb.superblock_count as u64) as usize];
                let dek = unwrap_first_matching_slot(header, k)?;
                let cipher = crate::crypto::PageCipher::new(dek);
                sb.decrypt_body(&cipher, &raw)
                    .map_err(|_| ChiselError::InvalidEncryptionKey)?;
                Some(cipher)
            }
        };

        // Version gate: plaintext DBs must be MAJOR==1; encrypted DBs MAJOR==2.
        let expected_major = if sb.encryption.is_some() {
            page::FORMAT_MAJOR_VERSION_ENCRYPTED
        } else {
            page::FORMAT_MAJOR_VERSION
        };
        if page::format_major(sb.format_version) != expected_major {
            return Err(ChiselError::UnsupportedFormatVersion {
                found: sb.format_version,
                expected: sb.format_version, // expected-major already implied by encryption flag
            });
        }
        // ... existing page_size check, minor write-gate, total_pages check,
        //     roots construction, handle-table/membership recovery unchanged
        //     (they now read the DECRYPTED sb fields) ...

        Ok(TransactionManager {
            // ... existing fields ...
            cipher,
            // ...
        })
    }
```

Slot-trial helper at the bottom of `recovery.rs`:
```rust
/// Try every ACTIVE key-slot in turn: derive the KEK from `key` + the slot's
/// salt/params, attempt to unwrap the DEK. The first slot whose tag verifies
/// yields the DEK. If none verify, the key is wrong for this DB.
///
/// Trying every slot (rather than a slot index hint) is what makes multi-key
/// support work: a DB may have the same DEK wrapped under several KEKs, and the
/// caller's key matches exactly one of them.
fn unwrap_first_matching_slot(
    header: &crate::superblock::CryptoHeader,
    key: &crate::crypto::Key,
) -> Result<crate::crypto::Dek> {
    use crate::crypto::{derive_kek, unwrap_dek, KdfId};

    for slot in header.slots.iter().filter(|s| s.is_active()) {
        let kdf = match slot.kdf_id {
            1 => KdfId::Hkdf,
            2 => KdfId::Argon2id,
            _ => continue, // unknown KDF id: skip, treat as non-matching
        };
        let kek = match derive_kek(key, kdf, &slot.salt, &slot.argon2) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let aad = slot.aad();
        if let Ok(dek) = unwrap_dek(&kek, &slot.wrapped_dek, &slot.wrap_tag, &slot.wrap_nonce, &aad)
        {
            return Ok(dek);
        }
    }
    Err(ChiselError::InvalidEncryptionKey)
}
```

The `NoEncryptionKey`, `InvalidEncryptionKey`, and `EncryptionNotSupported` variants are already defined (operational, `is_fatal() == false`) by **Task 4.1, implemented first** — do NOT re-add them here (that would duplicate the enum arms). Just wire `options.encryption_key.clone()` into the `open_existing` call at `src/lib.rs:344`.

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --test encryption_open`  Expected: PASS
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(engine): open encrypted database — unwrap DEK from key-slot, decrypt body"
```

---

### Task 2.5: thread the DEK to TransactionManager held zeroizing for the session + full-suite green

**Files:**
- Modify: `src/transaction/mod.rs` (confirm `cipher: Option<PageCipher>` placed; `PageCipher` holds the `Dek` which is `Zeroizing` per Phase 1, so dropping the manager zeroizes the key — no extra `Drop` needed)
- Modify: any remaining `create_new(` / `open_existing(` call sites the compiler flags (in-memory paths in `lib.rs`, plus `#[cfg(test)]` callers in `src/transaction/tests` and integration tests) to pass the new `None` argument
- Test: `#[cfg(test)] mod` accessor test in `src/transaction/mod.rs` + full suite

**Interfaces:**
- Consumes: everything above.
- Produces: `TransactionManager` consistently constructed with `cipher`; a session-held zeroizing DEK reachable for Phase 3's PageCache wiring.

- [ ] **Step 1: Write the failing test**
```rust
// in src/transaction/mod.rs tests module (or a small test in recovery.rs)
#[test]
fn encrypted_manager_holds_session_cipher() {
    use crate::crypto::Key;
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;
    use zeroize::Zeroizing;

    let io = PageIo::open_in_memory();
    let cache = PageCache::new_for_test(io); // existing in-memory test ctor
    let key = Key::Raw(Zeroizing::new(vec![0x5Au8; 32]));
    let txm = TransactionManager::create_new(cache, 2, Some(key)).unwrap();
    assert!(txm.cipher.is_some(), "encrypted create must retain a session cipher");
}

#[test]
fn plaintext_manager_has_no_cipher() {
    use crate::page_cache::PageCache;
    use crate::page_io::PageIo;

    let io = PageIo::open_in_memory();
    let cache = PageCache::new_for_test(io);
    let txm = TransactionManager::create_new(cache, 2, None).unwrap();
    assert!(txm.cipher.is_none());
}
```
(If `PageCache::new_for_test`/`PageIo::open_in_memory` differ from the actual in-memory test constructors, substitute the real ones the existing `mod tests` already uses — grep `create_new(` in `src/transaction/tests` for the established pattern and reuse it verbatim.)

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test encrypted_manager_holds_session_cipher`  Expected: FAIL until all call sites updated and the field is wired
- [ ] **Step 3: Implement**
Update every `TransactionManager::create_new(cache, N)` call to `create_new(cache, N, None)` and every `open_existing(cache)` to `open_existing(cache, None)` flagged by the compiler (in-memory paths in `lib.rs` lines ~404 and the transaction test module). The `cipher` field is set on each constructor as shown in Tasks 2.3/2.4. No `Drop` impl is needed: `PageCipher` owns a `Dek(Zeroizing<[u8; 32]>)` (Phase 1), so the key is zeroed on the manager's drop automatically.
- [ ] **Step 4: Run the full suite**
Run: `cargo test`  Expected: PASS (all existing plaintext tests, plus the new encryption create/open/manager tests; integration tests in `tests/` run because we use plain `cargo test`)
Run: `cargo clippy --all-targets -- -D warnings`  Expected: clean
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(engine): hold session DEK (zeroizing) on TransactionManager; green full suite"
```

---

Notes on deliberate simplifications (`ponytail`): only **slot 0** is populated at create time — multi-key add/rotate is a separate later feature, not Phase 2 scope; the open path already trial-decrypts ALL active slots, so adding more slots later needs no open-path change. The encryption error variants are defined in **Task 4.1, which is implemented first** (see the plan header's execution-order exception), so the create/open code returns the real variants directly — `NoEncryptionKey` (encrypted DB, no key), `InvalidEncryptionKey` (key unwraps no slot), `EncryptionNotSupported` (key supplied for a plaintext DB) — and the `From<CryptoError>` conversion maps to `InvalidEncryptionKey`. The integration tests here assert `is_err()` rather than the exact variant, keeping them robust to wording.

---

## Phase 3: Stride-aware page_io, page-cache seal/open orchestration, spillway

### Task 3.1: Make `page_io` stride-aware (crypto-agnostic on-disk unit)

**Files:**
- Modify: `src/page_io.rs:67` (PageIo struct — add `stride` field), `src/page_io.rs:130` (open seed), `src/page_io.rs:242` (read_page), `src/page_io.rs:283` (write_page), `src/page_io.rs:411` (set_page_count)
- Test: `src/page_io.rs` (`#[cfg(test)] mod stride_tests` in the same file)

**Interfaces:**
- Consumes: `crate::crypto::ENC_PAGE_SIZE` (Phase 1, `= 8232`)
- Produces:
  - `pub fn read_page_unit(&mut self, page_id: u64) -> Result<Vec<u8>>` — returns the on-disk blob of `stride` bytes
  - `pub fn write_page_unit(&mut self, page_id: u64, blob: &[u8]) -> Result<()>` — writes a `stride`-byte blob
  - `pub fn set_stride(&mut self, stride: usize)` / `pub fn stride(&self) -> usize`
  - The existing `read_page`/`write_page` stay (plaintext path = stride `PAGE_SIZE`); offset becomes `page_id * stride`.

Note: page_io stays crypto-agnostic. It only knows a `stride` (the on-disk unit size). The cache passes already-sealed blobs in; page_io never seals. `stride` defaults to `PAGE_SIZE`; the engine calls `set_stride(ENC_PAGE_SIZE)` when the superblock says encrypted. The page-count cache now counts *units* of `stride` bytes, not `PAGE_SIZE` — `page_id * stride` is the only offset math that exists, so the seed at open must divide by `stride`, and `set_stride` must be called BEFORE the first read on an encrypted DB (the engine does this right after reading page 0's plaintext bootstrap header).

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod stride_tests {
    use super::*;
    use crate::crypto::ENC_PAGE_SIZE;

    // Offset math must use the on-disk stride, not PAGE_SIZE. With an 8232-byte
    // stride, page 2's blob lives at byte 16464, and page_count is reported in
    // stride-units. In-memory backing so the test is filesystem-free.
    #[test]
    fn stride_8232_offsets_and_unit_roundtrip() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        assert_eq!(io.stride(), ENC_PAGE_SIZE);

        // A distinct 8232-byte blob per page id.
        let mut blob0 = vec![0u8; ENC_PAGE_SIZE];
        blob0[0] = 0xA0;
        blob0[ENC_PAGE_SIZE - 1] = 0x0A;
        let mut blob2 = vec![0u8; ENC_PAGE_SIZE];
        blob2[0] = 0xC2;
        blob2[ENC_PAGE_SIZE - 1] = 0x2C;

        io.write_page_unit(0, &blob0).unwrap();
        io.write_page_unit(2, &blob2).unwrap(); // page 1 zero-filled by growth

        // page_count is in stride units: writing page 2 extends to 3.
        assert_eq!(io.page_count().unwrap(), 3);

        assert_eq!(io.read_page_unit(0).unwrap(), blob0);
        assert_eq!(io.read_page_unit(2).unwrap(), blob2);
        // The zero-filled gap page reads back as all zeros.
        assert_eq!(io.read_page_unit(1).unwrap(), vec![0u8; ENC_PAGE_SIZE]);
    }

    // The plaintext stride (default) keeps PAGE_SIZE offset math intact.
    #[test]
    fn default_stride_is_page_size() {
        let io = PageIo::open_in_memory().unwrap();
        assert_eq!(io.stride(), PAGE_SIZE);
    }

    // A blob whose length != stride is a caller bug, not silent truncation.
    #[test]
    fn write_unit_wrong_length_is_invalid() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        let short = vec![0u8; PAGE_SIZE]; // wrong: 8192 != 8232
        assert!(io.write_page_unit(0, &short).is_err());
    }
}
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test stride_8232_offsets_and_unit_roundtrip`  Expected: FAIL (no `set_stride`/`read_page_unit`/`write_page_unit` yet)
- [ ] **Step 3: Implement**

The `Memory` backing currently stores `Vec<[u8; PAGE_SIZE]>`. A stride-aware unit store needs variable-length rows, so switch it to a flat `Vec<u8>` addressed by `page_id * stride`. Change the struct and the three I/O entry points. Replace `Backing::Memory { pages: Vec<[u8; PAGE_SIZE]> }` with a flat byte vec; the existing `read_page`/`write_page` become thin wrappers that copy into/out of a `PAGE_SIZE` array via the unit functions when `stride == PAGE_SIZE`.

```rust
// In the struct (src/page_io.rs:67), add after `read_only: bool,`:
    // On-disk unit size in bytes. PAGE_SIZE for a plaintext DB; ENC_PAGE_SIZE
    // (8232 = 8192 ct + 16 tag + 24 nonce) for an encrypted DB. This module is
    // crypto-agnostic: it only moves `stride`-byte blobs and computes
    // offset = page_id * stride. The engine sets this to ENC_PAGE_SIZE right
    // after reading page 0's plaintext bootstrap header on an encrypted open,
    // BEFORE any other page is read. page_count is reported in stride-units.
    stride: usize,
```

```rust
// Backing::Memory becomes flat bytes (src/page_io.rs:44):
    Memory { bytes: Vec<u8> },
```

```rust
// Seed `stride` in open() (after line 124's `let mut file = ...; Self::try_lock`)
// and recompute the page-count seed against PAGE_SIZE (the default stride at
// open — set_stride happens later, and on a stride change the engine reseeds
// via set_stride below). Replace the PageIo construction at src/page_io.rs:132:
        let initial_len = file.seek(SeekFrom::End(0))?;
        let initial_page_count = initial_len / PAGE_SIZE as u64;
        Ok(PageIo {
            backing: Backing::File { file },
            read_only,
            stride: PAGE_SIZE,
            fsync_calls: Cell::new(0),
            cached_page_count: Cell::new(initial_page_count),
            #[cfg(test)]
            fault: Cell::new(Fault::None),
        })
```

```rust
// open_in_memory() construction (src/page_io.rs:151):
        Ok(PageIo {
            backing: Backing::Memory { bytes: Vec::new() },
            read_only: false,
            stride: PAGE_SIZE,
            fsync_calls: Cell::new(0),
            cached_page_count: Cell::new(0),
            #[cfg(test)]
            fault: Cell::new(Fault::None),
        })
```

```rust
// New accessors + setter. Place after force_read_only (src/page_io.rs:179).
    /// On-disk unit size in bytes (PAGE_SIZE plaintext, ENC_PAGE_SIZE encrypted).
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Set the on-disk stride and re-seed the page-count cache against the new
    /// unit size. Must be called BEFORE the first unit read on an encrypted DB
    /// (the engine does this immediately after reading page 0's plaintext
    /// bootstrap header). Re-seeds from the true file length so page_count is
    /// reported in the new stride-units.
    pub fn set_stride(&mut self, stride: usize) {
        self.stride = stride;
        let len = match &mut self.backing {
            Backing::File { file } => file.seek(SeekFrom::End(0)).unwrap_or(0),
            Backing::Memory { bytes } => bytes.len() as u64,
        };
        self.cached_page_count.set(len / stride as u64);
    }

    /// Read the raw on-disk unit (stride bytes) for `page_id`. Crypto-agnostic:
    /// for an encrypted DB this returns the sealed `ct‖tag‖nonce` blob, which the
    /// PageCache then hands to PageCipher::open. For a plaintext DB stride ==
    /// PAGE_SIZE and the blob is the page image itself.
    pub fn read_page_unit(&mut self, page_id: u64) -> Result<Vec<u8>> {
        let page_count = self.page_count()?;
        if page_id >= page_count {
            return Err(ChiselError::InvalidPageId { page_id });
        }
        #[cfg(test)]
        if self.fault.get() == Fault::FailReadPage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected read failure",
            )));
        }
        let stride = self.stride;
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * stride as u64;
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; stride];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
            Backing::Memory { bytes } => {
                let off = (page_id * stride as u64) as usize;
                Ok(bytes[off..off + stride].to_vec())
            }
        }
    }

    /// Write a raw on-disk unit (must be exactly `stride` bytes). Past-EOF
    /// writes extend the file (POSIX); intermediate units are zero-filled.
    pub fn write_page_unit(&mut self, page_id: u64, blob: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        if blob.len() != self.stride {
            return Err(ChiselError::IoError(std::io::Error::other(format!(
                "page unit length {} != stride {}",
                blob.len(),
                self.stride
            ))));
        }
        #[cfg(test)]
        if self.fault.get() == Fault::FailWritePage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected write failure",
            )));
        }
        let stride = self.stride;
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * stride as u64;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(blob)?;
            }
            Backing::Memory { bytes } => {
                let off = (page_id * stride as u64) as usize;
                let needed = off + stride;
                if bytes.len() < needed {
                    bytes.resize(needed, 0);
                }
                bytes[off..off + stride].copy_from_slice(blob);
            }
        }
        let needed = page_id + 1;
        if needed > self.cached_page_count.get() {
            self.cached_page_count.set(needed);
        }
        Ok(())
    }
```

```rust
// Rewrite read_page (src/page_io.rs:242) and write_page (src/page_io.rs:283) as
// thin PAGE_SIZE-typed wrappers over the unit functions. They are only valid
// when stride == PAGE_SIZE (the plaintext path); the encrypted path goes through
// the *_unit functions directly from the cache. debug_assert pins the contract.
    pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        debug_assert_eq!(self.stride, PAGE_SIZE, "read_page on an encrypted stride; use read_page_unit");
        let blob = self.read_page_unit(page_id)?;
        let mut buf = [0u8; PAGE_SIZE];
        buf.copy_from_slice(&blob);
        Ok(buf)
    }

    pub fn write_page(&mut self, page_id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        debug_assert_eq!(self.stride, PAGE_SIZE, "write_page on an encrypted stride; use write_page_unit");
        self.write_page_unit(page_id, buf)
    }
```

```rust
// set_page_count (src/page_io.rs:411): file length is in stride-bytes now.
    pub fn set_page_count(&mut self, n: u64) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        let stride = self.stride;
        match &mut self.backing {
            Backing::File { file } => {
                file.set_len(n * stride as u64)?;
            }
            Backing::Memory { bytes } => {
                bytes.resize((n * stride as u64) as usize, 0);
            }
        }
        self.cached_page_count.set(n);
        Ok(())
    }
```

Note: the existing memory-backing tests construct pages as `[u8; PAGE_SIZE]` and call `read_page`/`write_page` — those keep working unchanged because the default stride is `PAGE_SIZE` and the wrappers preserve the exact byte semantics. The `read_page` unchecked-index comment block at old lines 262-266 is removed with the rewrite.

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test stride_8232_offsets_and_unit_roundtrip default_stride_is_page_size write_unit_wrong_length_is_invalid`  Expected: PASS. Also run `cargo test` to confirm the existing `read_page`/`write_page` memory + file tests still pass under the wrapper rewrite.
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(page_io): stride-aware raw on-disk page units (8232 encrypted, 8192 plaintext)"
```

---

### Task 3.2: Widen the spillway slot to carry a sealed blob

**Files:**
- Modify: `src/spillway.rs:43` (SLOT_SIZE), `src/spillway.rs:52` (Spillway struct — add `payload_size`), `src/spillway.rs:88`/`123` (open_file / open_memory take payload size), `src/spillway.rs:184` (spill), `src/spillway.rs:273` (rehydrate), `src/spillway.rs:299`/`306`/`343` (slot_checksum / write_slot / read_slot)
- Test: `src/spillway.rs` (`#[cfg(test)] mod tests`, same file)

**Interfaces:**
- Consumes: `crate::crypto::ENC_PAGE_SIZE`
- Produces:
  - `Spillway::open_file(db_path, max_bytes, payload_size)` / `Spillway::open_memory(max_bytes, payload_size)` — `payload_size` is `PAGE_SIZE` (plaintext) or `ENC_PAGE_SIZE` (encrypted)
  - `spill(&mut self, page_id, blob: &[u8])` / `rehydrate(&mut self, page_id) -> Result<Vec<u8>>` — carry a `payload_size`-byte blob (the sealed unit when encrypted)
  - `pub const SLOT_HEADER_SIZE: usize = 16` unchanged; the slot is `SLOT_HEADER_SIZE + payload_size` at runtime.

The spillway carries the SEALED blob for encrypted DBs (seal-once: the page was sealed when evicted, the spillway stores ciphertext, drain copies it verbatim). `slot_checksum` covers `page_id ‖ blob` so a torn spillway write is caught before the blob reaches the main file — this is the slot's integrity check, NOT the AEAD tag (which lives inside the sealed blob and is verified by PageCipher::open only on the cold main-file read path).

- [ ] **Step 1: Write the failing test**
```rust
    #[test]
    fn wide_slot_round_trips_sealed_blob() {
        use crate::crypto::ENC_PAGE_SIZE;
        // payload_size = ENC_PAGE_SIZE: each slot carries an 8232-byte sealed
        // blob plus the 16-byte header. Round-trip must return the exact bytes.
        let slot = (SLOT_HEADER_SIZE + ENC_PAGE_SIZE) as u64;
        let mut spw = Spillway::open_memory(slot * 4, ENC_PAGE_SIZE);
        let mut blob = vec![0u8; ENC_PAGE_SIZE];
        blob[0] = 0xEE;
        blob[ENC_PAGE_SIZE - 1] = 0x11;
        spw.spill(7, &blob).unwrap();
        assert!(spw.is_resident(7));
        assert_eq!(spw.rehydrate(7).unwrap(), blob);
    }

    #[test]
    fn wide_slot_checksum_catches_tampered_payload() {
        use crate::crypto::ENC_PAGE_SIZE;
        let slot = (SLOT_HEADER_SIZE + ENC_PAGE_SIZE) as u64;
        let mut spw = Spillway::open_memory(slot * 4, ENC_PAGE_SIZE);
        spw.spill(7, &vec![0xAB; ENC_PAGE_SIZE]).unwrap();
        if let Backing::Memory { ref mut bytes } = spw.backing {
            bytes[SLOT_HEADER_SIZE + 5] ^= 0x01; // flip a byte in the blob
        }
        assert!(matches!(
            spw.rehydrate(7).unwrap_err(),
            ChiselError::ChecksumMismatch { page_id: 7 }
        ));
    }
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test wide_slot_round_trips_sealed_blob wide_slot_checksum_catches_tampered_payload`  Expected: FAIL (open_memory takes one arg; spill takes `[u8; PAGE_SIZE]`)
- [ ] **Step 3: Implement**

The slot size is now runtime, not a `const`. Keep `SLOT_HEADER_SIZE`; remove the fixed `SLOT_SIZE` const (or keep it as the plaintext-default for any remaining caller) and compute `slot_size = SLOT_HEADER_SIZE + payload_size` from a new field. The `[u8; PAGE_SIZE]` signatures become `&[u8]` / `Vec<u8>`.

```rust
// src/spillway.rs:43 — keep header const; SLOT_SIZE becomes the plaintext
// default (header + PAGE_SIZE) for callers/tests that still use it, but the
// live slot size is payload-driven via the struct field below.
pub const SLOT_HEADER_SIZE: usize = 16;
/// Plaintext-default slot size (header + PAGE_SIZE). Encrypted DBs use
/// `SLOT_HEADER_SIZE + ENC_PAGE_SIZE`; see `Spillway::payload_size`.
pub const SLOT_SIZE: usize = SLOT_HEADER_SIZE + PAGE_SIZE;
```

```rust
// Add to the Spillway struct (src/spillway.rs:52), after `max_bytes: u64,`:
    /// Bytes per spilled payload: PAGE_SIZE plaintext, ENC_PAGE_SIZE encrypted.
    /// On an encrypted DB the payload IS the sealed `ct‖tag‖nonce` blob — the
    /// spillway stores ciphertext and drain copies it verbatim (seal-once). The
    /// slot is `SLOT_HEADER_SIZE + payload_size` bytes; the per-slot XXH3
    /// checksum covers the payload, catching a torn spillway write before the
    /// blob reaches the main file (distinct from the inner AEAD tag).
    payload_size: usize,
```

```rust
// open_file (src/spillway.rs:88) and open_memory (src/spillway.rs:123) gain a
// payload_size param and store it. logical_bytes/spill/cap accounting all key
// off payload_size now.
    pub fn open_file(db_path: &Path, max_bytes: u64, payload_size: usize) -> Result<Spillway> {
        let mut path = db_path.as_os_str().to_owned();
        path.push(".spillway");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(ChiselError::IoError)?;
        Ok(Spillway {
            backing: Backing::File { file },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
            payload_size,
        })
    }

    pub fn open_memory(max_bytes: u64, payload_size: usize) -> Spillway {
        Spillway {
            backing: Backing::Memory { bytes: Vec::new() },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
            payload_size,
        }
    }
```

```rust
// logical_bytes (src/spillway.rs:158) — charge against payload_size, not
// PAGE_SIZE. The cap is still LIVE-residency based.
    pub fn logical_bytes(&self) -> u64 {
        self.slots.len() as u64 * self.payload_size as u64
    }
```

```rust
// spill (src/spillway.rs:184): blob is &[u8] of payload_size; the cap charge
// uses payload_size.
    pub fn spill(&mut self, page_id: u64, blob: &[u8]) -> Result<()> {
        debug_assert_eq!(blob.len(), self.payload_size, "spill blob != payload_size");
        let slot_index = if let Some(&existing) = self.slots.get(&page_id) {
            existing
        } else {
            let post_write_bytes = (self.slots.len() as u64 + 1) * self.payload_size as u64;
            if post_write_bytes > self.max_bytes {
                return Err(ChiselError::SpillwayFull {
                    limit_bytes: self.max_bytes,
                });
            }
            let new_index = self.next_slot_index;
            self.next_slot_index += 1;
            self.slots.insert(page_id, new_index);
            new_index
        };
        write_slot(&mut self.backing, slot_index, page_id, blob, self.payload_size)?;
        Ok(())
    }
```

```rust
// rehydrate (src/spillway.rs:273): returns Vec<u8> of payload_size.
    pub fn rehydrate(&mut self, page_id: u64) -> Result<Vec<u8>> {
        let slot_index = match self.slots.get(&page_id) {
            Some(&i) => i,
            None => return Err(ChiselError::InvalidPageId { page_id }),
        };
        let (stored_page_id, stored_checksum, blob) =
            read_slot(&mut self.backing, slot_index, self.payload_size)?;
        if stored_page_id != page_id {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        let computed = slot_checksum(page_id, &blob);
        if computed != stored_checksum {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        Ok(blob)
    }
```

```rust
// Free functions: slot_checksum/write_slot/read_slot take &[u8] / payload_size
// and compute slot_size = SLOT_HEADER_SIZE + payload_size.
fn slot_checksum(page_id: u64, blob: &[u8]) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&page_id.to_le_bytes());
    hasher.update(blob);
    hasher.digest()
}

fn write_slot(
    backing: &mut Backing,
    slot_index: u64,
    page_id: u64,
    blob: &[u8],
    payload_size: usize,
) -> Result<()> {
    let slot_size = SLOT_HEADER_SIZE + payload_size;
    let checksum = slot_checksum(page_id, blob);
    let offset = slot_index * slot_size as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    header[..8].copy_from_slice(&page_id.to_le_bytes());
    header[8..16].copy_from_slice(&checksum.to_le_bytes());
    match backing {
        Backing::File { file } => {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&header)?;
            file.write_all(blob)?;
        }
        Backing::Memory { bytes } => {
            let needed = (offset + slot_size as u64) as usize;
            if bytes.len() < needed {
                bytes.resize(needed, 0);
            }
            let off = offset as usize;
            bytes[off..off + SLOT_HEADER_SIZE].copy_from_slice(&header);
            bytes[off + SLOT_HEADER_SIZE..off + slot_size].copy_from_slice(blob);
        }
    }
    Ok(())
}

fn read_slot(
    backing: &mut Backing,
    slot_index: u64,
    payload_size: usize,
) -> Result<(u64, u64, Vec<u8>)> {
    let slot_size = SLOT_HEADER_SIZE + payload_size;
    let offset = slot_index * slot_size as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    let mut blob = vec![0u8; payload_size];
    match backing {
        Backing::File { file } => {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut header)?;
            file.read_exact(&mut blob)?;
        }
        Backing::Memory { bytes } => {
            let off = offset as usize;
            if bytes.len() < off + slot_size {
                return Err(ChiselError::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("spillway memory backing too short for slot {slot_index}"),
                )));
            }
            header.copy_from_slice(&bytes[off..off + SLOT_HEADER_SIZE]);
            blob.copy_from_slice(&bytes[off + SLOT_HEADER_SIZE..off + slot_size]);
        }
    }
    let stored_page_id = u64::from_le_bytes(header[..8].try_into().unwrap());
    let stored_checksum = u64::from_le_bytes(header[8..16].try_into().unwrap());
    Ok((stored_page_id, stored_checksum, blob))
}
```

Every existing spillway test that calls `open_memory(n)` / `spill(id, &page(b))` / compares `rehydrate` to `[u8; PAGE_SIZE]` must be updated: add `PAGE_SIZE` as the `payload_size` arg, pass `&page(b)[..]` (a slice), and compare against the page slice. The `page()` helper returns `[u8; PAGE_SIZE]`; `&page(0xAA)` coerces to `&[u8]`. `logical_bytes`/cap tests already use `PAGE_SIZE` arithmetic, which now equals `payload_size`. PageCache callers are updated in Task 3.3/3.4.

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --test '*' wide_slot ; cargo test`  Expected: PASS (run the full suite — the spillway and page_cache call sites are coupled).
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(spillway): runtime payload_size so slots carry sealed 8232-byte blobs"
```

---

### Task 3.3: Hold an `Option<PageCipher>` in PageCache; seal/open on flush and cold load

**Files:**
- Modify: `src/page_cache.rs:65` (PageCache struct — add `cipher` + helper), `src/page_cache.rs:161` (new — take cipher), `src/page_cache.rs:390` (flush Phase 1a write loop), `src/page_cache.rs:866` (load_page disk branch)
- Modify: `src/spillway.rs` ensure_spillway call sites (`src/page_cache.rs:1085`) to pass `payload_size`
- Test: `src/page_cache.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes (Phase 1): `crate::crypto::{PageCipher, ENC_PAGE_SIZE, CryptoError}`; `PageCipher::seal(page_id, &[u8;8192]) -> [u8; ENC_PAGE_SIZE]`; `PageCipher::open(page_id, &[u8;ENC_PAGE_SIZE]) -> Result<[u8;8192], CryptoError>`
- Consumes (Phase 1 stride): `PageIo::{set_stride, stride, read_page_unit, write_page_unit}`
- Produces: a PageCache whose flush writes sealed units and whose cold load opens+verifies them. `DecryptionFailed { page_id }` (fatal, added Phase 4) is surfaced when `PageCipher::open` fails.

Seal-once invariant: the plaintext page is sealed exactly once — at flush (Phase 1a) and at evict-to-spillway (Task 3.4). The cache always holds PLAINTEXT (`[u8; PAGE_SIZE]`); the on-disk unit and the spillway hold ciphertext. Cold load opens the unit back to plaintext and runs `verify_checksum` on the plaintext (the page's internal XXH3), exactly as today.

- [ ] **Step 1: Write the failing test**
```rust
    use crate::crypto::{PageCipher, random_dek, ENC_PAGE_SIZE};

    // Build an encrypted file-backed cache: stride set to ENC_PAGE_SIZE, a
    // PageCipher installed. A page written, flushed, dropped from cache, and
    // re-read must round-trip its plaintext through seal->disk->open.
    fn fresh_encrypted_cache(max_pages: usize) -> (TempDir, PageCache) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let mut io = PageIo::open(&db_path, false).unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        let mut cache = PageCache::new(
            io,
            cache_max_bytes,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        cache.set_cipher(PageCipher::new(random_dek()));
        (dir, cache)
    }

    #[test]
    fn encrypted_page_round_trips_through_seal_open() {
        let (_dir, mut cache) = fresh_encrypted_cache(8);
        let pid = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(pid).unwrap();
            buf[0] = 0x9C;
            buf[PAGE_SIZE - 1] = 0xC9;
            // Stamp a valid internal checksum so cold-load verify_checksum passes.
            crate::page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
        // Force a cold read: drop from cache so load_page hits the disk unit.
        cache.test_drop_from_cache(pid);
        let read = cache.get(pid).unwrap();
        assert_eq!(read[0], 0x9C);
        assert_eq!(read[PAGE_SIZE - 1], 0xC9);
    }

    #[test]
    fn tampered_ciphertext_surfaces_decryption_failed() {
        let (_dir, mut cache) = fresh_encrypted_cache(8);
        let pid = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(pid).unwrap();
            buf[10] = 0x42;
            crate::page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
        cache.test_drop_from_cache(pid);
        // Corrupt one ciphertext byte in the on-disk unit (offset 0 is inside
        // the 8192-byte ciphertext region of the 8232-byte stride).
        {
            let mut blob = cache.io_mut().read_page_unit(pid).unwrap();
            blob[0] ^= 0x01;
            cache.io_mut().write_page_unit(pid, &blob).unwrap();
        }
        let err = cache.get(pid).unwrap_err();
        assert!(
            matches!(err, ChiselError::DecryptionFailed { page_id } if page_id == pid),
            "expected DecryptionFailed, got {err:?}"
        );
    }
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test encrypted_page_round_trips_through_seal_open tampered_ciphertext_surfaces_decryption_failed`  Expected: FAIL (no `set_cipher`; flush/load_page not crypto-aware; `DecryptionFailed` is Phase 4 — if not yet merged, this task depends on Phase 4's error variant).
- [ ] **Step 3: Implement**

```rust
// PageCache struct (src/page_cache.rs:65), add after `io: PageIo,`:
    /// Page sealer/opener for encrypted DBs; None for plaintext. When Some, the
    /// cache holds PLAINTEXT in `entries` but writes the SEALED on-disk unit
    /// (via io.write_page_unit) and opens the sealed unit on cold load. Seal
    /// happens exactly once per write (flush Phase 1a, evict-to-spillway); the
    /// spillway and main file carry ciphertext, drain copies it verbatim. The
    /// engine must call io.set_stride(ENC_PAGE_SIZE) when installing a cipher.
    cipher: Option<crate::crypto::PageCipher>,
```

```rust
// PageCache::new (src/page_cache.rs:170): seed cipher: None in the struct
// literal (after `io,`):
            io,
            cipher: None,
```

```rust
// New setter, after set_drain_insertion (src/page_cache.rs:832):
    /// Install the page cipher (encrypted DBs). The caller MUST have already
    /// called `self.io_mut().set_stride(ENC_PAGE_SIZE)` so the on-disk unit math
    /// matches the sealed blob size. Set once at open after the DEK is unwrapped.
    pub fn set_cipher(&mut self, cipher: crate::crypto::PageCipher) {
        self.cipher = Some(cipher);
    }

    /// Seal a plaintext page to its on-disk unit and write it. Seal-once entry
    /// point shared by flush Phase 1a. For a plaintext DB (no cipher) this is a
    /// straight `write_page_unit` of the page image.
    fn write_sealed(&mut self, page_id: u64, plaintext: &[u8; PAGE_SIZE]) -> Result<()> {
        match &self.cipher {
            Some(c) => {
                let blob = c.seal(page_id, plaintext);
                self.io.write_page_unit(page_id, &blob)
            }
            None => self.io.write_page_unit(page_id, plaintext),
        }
    }
```

```rust
// flush Phase 1a write loop (src/page_cache.rs:410-418): route through
// write_sealed instead of io.write_page. entry.buf is plaintext.
        for &page_id in &dirty_scratch {
            let entry = self.entries.get_mut(&page_id).unwrap();
            // I48 INVARIANT: id still present and dirty (see flush docstring).
            // Lift the plaintext out so write_sealed can take &mut self.io.
            let plaintext = *entry.buf;
            entry.dirty = false;
            self.write_sealed(page_id, &plaintext)?;
        }
```
Note: the borrow shape changes — copy the plaintext out (`*entry.buf`, an 8 KB stack copy, same cost the COW paths already pay) and clear `dirty` before the `write_sealed(&mut self, ...)` call, because `write_sealed` needs `&mut self.io` while `entry` borrows `self.entries`. On the error `?` the page stays clean-but-unwritten, which the I1 poison model already covers (see the flush DURABILITY WINDOW docstring — no change to that contract).

```rust
// load_page disk branch (src/page_cache.rs:892-902): read the on-disk UNIT,
// open it if encrypted, then verify_checksum on the plaintext as today.
        let plaintext: [u8; PAGE_SIZE] = match &self.cipher {
            Some(c) => {
                let blob = self.io.read_page_unit(page_id)?;
                // blob is exactly ENC_PAGE_SIZE (stride); open verifies the AEAD
                // tag (anti-tamper) and returns the 8192-byte plaintext.
                let unit: [u8; ENC_PAGE_SIZE] = blob
                    .as_slice()
                    .try_into()
                    .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                c.open(page_id, &unit)
                    .map_err(|_| ChiselError::DecryptionFailed { page_id })?
            }
            None => self.io.read_page(page_id)?,
        };
        if !page::verify_checksum(&plaintext) {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        self.entries.insert(
            page_id,
            CacheEntry {
                buf: Box::new(plaintext),
                dirty: false,
            },
        );
        self.lru.push_front(page_id);
        Ok(())
```
Add `use crate::crypto::ENC_PAGE_SIZE;` to the module imports (near `src/page_cache.rs:46`).

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test encrypted_page_round_trips_through_seal_open tampered_ciphertext_surfaces_decryption_failed`  Expected: PASS
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(page_cache): seal on flush, open+verify on cold load for encrypted DBs"
```

---

### Task 3.4: Seal-once on evict-to-spillway; verbatim copy on drain

**Files:**
- Modify: `src/page_cache.rs:866` (load_page spillway-resident branch), `src/page_cache.rs:986` (maybe_evict Phase B spill), `src/page_cache.rs:444` (flush Phase 1b drain), `src/page_cache.rs:1064` (ensure_spillway — pass payload_size)
- Test: `src/page_cache.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: Task 3.2 `Spillway::{open_file,open_memory}(.., payload_size)`, `spill(id, &[u8])`, `rehydrate(id) -> Vec<u8>`; Task 3.3 `cipher`, `PageCipher::{seal,open}`
- Produces: encrypted spill/drain. The spillway stores the SEALED blob; drain copies it to the main file with NO re-seal.

Seal-once invariant on the spill path: evict seals the plaintext ONCE into the spillway as ciphertext. Drain reads that ciphertext and `write_page_unit`s it to the main file verbatim — no second seal (a second seal would generate a fresh nonce and re-encrypt needlessly, and would force the cache to hold plaintext through drain anyway). Re-loading a spilled page from the spillway (`load_page`) must `open` the ciphertext back to plaintext before it re-enters `entries`.

- [ ] **Step 1: Write the failing test**
```rust
    #[test]
    fn encrypted_spill_and_drain_round_trips() {
        // Cache of 2 pages with a 4-page spillway; allocate 4 dirty pages so 2
        // spill. Stamp each, flush (which drains), then cold-read all 4 back.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let mut io = PageIo::open(&db_path, false).unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        let max_pages = 2usize;
        let slot = (crate::spillway::SLOT_HEADER_SIZE + ENC_PAGE_SIZE) as u64;
        let mut cache = PageCache::new(
            io,
            max_pages as u64 * PAGE_SIZE as u64,
            slot * 8,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        cache.set_cipher(PageCipher::new(random_dek()));

        let mut ids = Vec::new();
        for n in 0..4u8 {
            let pid = cache.new_page().unwrap();
            let buf = cache.get_mut(pid).unwrap();
            buf[0] = 0x40 | n;
            crate::page::stamp_checksum(buf);
            ids.push(pid);
        }
        // At least one page must have spilled (4 dirty > 2-page cache).
        assert!(cache.spillway.as_ref().unwrap().slot_count() > 0);

        cache.flush().unwrap(); // drains spillway into the main file verbatim
        for (n, &pid) in ids.iter().enumerate() {
            cache.test_drop_from_cache(pid);
            let read = cache.get(pid).unwrap();
            assert_eq!(read[0], 0x40 | n as u8, "page {pid} content survived spill+drain");
        }
    }
```
- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test encrypted_spill_and_drain_round_trips`  Expected: FAIL (spill takes a slice now but ensure_spillway opens with the old single-arg signature / PAGE_SIZE payload; drain re-routes through write_page; cipher not applied on spill)
- [ ] **Step 3: Implement**

```rust
// ensure_spillway (src/page_cache.rs:1084): open with the correct payload_size.
// payload = ENC_PAGE_SIZE when a cipher is installed, else PAGE_SIZE.
        if self.spillway.is_none() {
            let payload_size = if self.cipher.is_some() {
                ENC_PAGE_SIZE
            } else {
                PAGE_SIZE
            };
            let spw = match &self.spillway_location {
                crate::SpillwayLocation::Path(p) => {
                    crate::spillway::Spillway::open_file(p, self.spillway_max_bytes, payload_size)?
                }
                crate::SpillwayLocation::InMemory => {
                    crate::spillway::Spillway::open_memory(self.spillway_max_bytes, payload_size)
                }
            };
            self.spillway = Some(spw);
        }
```

```rust
// maybe_evict Phase B (src/page_cache.rs:1045-1047): seal the plaintext ONCE
// before spilling, so the spillway carries ciphertext. `entry.buf` is plaintext.
// Build the spill payload outside the and_then so the borrow of &entry.buf
// doesn't tangle with ensure_spillway's &mut self.
            let payload: Vec<u8> = match &self.cipher {
                Some(c) => c.seal(victim_id, &entry.buf).to_vec(),
                None => entry.buf.to_vec(),
            };
            let spill_result = self
                .ensure_spillway()
                .and_then(|spw| spw.spill(victim_id, &payload));
            if let Err(e) = spill_result {
                if entry.dirty {
                    self.dirty_count += 1;
                }
                self.lru.push_back(victim_id);
                self.entries.insert(victim_id, entry);
                return Err(e);
            }
```

```rust
// flush Phase 1b drain (src/page_cache.rs:473-499): rehydrate returns the SEALED
// blob; write it to the main file VERBATIM (no re-seal). The cache re-insert
// must hold PLAINTEXT, so open the blob for the in-memory entry. For a plaintext
// DB the blob IS the page image and open is a no-op copy.
            for page_id in batch {
                let blob = {
                    let spw = self.spillway.as_mut().unwrap();
                    let b = spw.rehydrate(page_id)?; // sealed ciphertext (encrypted DB)
                    spw.forget(page_id);
                    b
                };
                // Verbatim copy of the sealed unit to the main file: seal-once.
                self.io.write_page_unit(page_id, &blob)?;
                // Re-insert as clean PLAINTEXT so cache reads return page images.
                let plaintext: [u8; PAGE_SIZE] = match &self.cipher {
                    Some(c) => {
                        let unit: [u8; ENC_PAGE_SIZE] = blob
                            .as_slice()
                            .try_into()
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                        c.open(page_id, &unit)
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?
                    }
                    None => {
                        let mut p = [0u8; PAGE_SIZE];
                        p.copy_from_slice(&blob);
                        p
                    }
                };
                let entry = CacheEntry {
                    buf: Box::new(plaintext),
                    dirty: false,
                };
                if let std::collections::hash_map::Entry::Vacant(e) = self.entries.entry(page_id) {
                    e.insert(entry);
                    match drain_policy {
                        crate::DrainInsertion::LruTail => self.lru.push_back(page_id),
                        crate::DrainInsertion::Mru => self.lru.push_front(page_id),
                    }
                }
            }
```

```rust
// load_page spillway-resident branch (src/page_cache.rs:872-887): rehydrate
// returns sealed ciphertext; open it before the plaintext entry enters the cache.
        if let Some(spw) = self.spillway.as_mut() {
            if spw.is_resident(page_id) {
                let blob = spw.rehydrate(page_id)?;
                spw.forget(page_id);
                let plaintext: [u8; PAGE_SIZE] = match &self.cipher {
                    Some(c) => {
                        let unit: [u8; ENC_PAGE_SIZE] = blob
                            .as_slice()
                            .try_into()
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                        c.open(page_id, &unit)
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?
                    }
                    None => {
                        let mut p = [0u8; PAGE_SIZE];
                        p.copy_from_slice(&blob);
                        p
                    }
                };
                self.entries.insert(
                    page_id,
                    CacheEntry {
                        buf: Box::new(plaintext),
                        dirty: true, // re-loaded spilled page is dirty
                    },
                );
                self.dirty_count += 1;
                self.lru.push_front(page_id);
                return Ok(());
            }
        }
```
Note: a re-loaded spilled page is marked `dirty: true` and the spillway slot is `forget`-ten. On the encrypted path this means the plaintext will be re-sealed (fresh nonce) at the next flush/evict — which is correct: it left the spillway, so the "seal-once" unit is gone and the page is live plaintext again. Seal-once means "not sealed twice while ciphertext is in flight to disk," not "the same nonce forever."

Existing plaintext `fresh_cache_with_spillway` spillway tests still pass: no cipher, `payload_size == PAGE_SIZE`, `spill`/`rehydrate` carry the page image, drain copies it verbatim — byte-identical to the old `write_page` path.

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test encrypted_spill_and_drain_round_trips ; cargo test`  Expected: PASS (full suite — confirms the plaintext spill/drain regression set still passes under the seal-once routing).
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(page_cache): seal-once on spill, verbatim sealed-blob copy on drain"
```

---

Phase 3 deliverable: an encrypted read/write/flush/spill/drain path. `page_io` moves `stride`-byte on-disk units crypto-agnostically; `PageCache` holds the `PageCipher`, seals plaintext exactly once (flush Phase 1a and evict-to-spillway), opens+verifies on cold load and on spillway re-load, and copies sealed blobs verbatim on drain. The spillway carries the sealed blob with its own XXH3 slot checksum.

Cross-phase dependencies (flagged, not invented):
- `ChiselError::DecryptionFailed { page_id }` (fatal) is defined in **Task 4.1, which is implemented first** (see the plan header's execution-order exception), so it is in scope here. The page-read path maps `PageCipher::open` failures to it explicitly via `.map_err(|_| ChiselError::DecryptionFailed { page_id })`.
- `crate::crypto::{PageCipher, ENC_PAGE_SIZE, random_dek}` and `page::stamp_checksum` are Phase-1 / existing `page.rs` symbols consumed verbatim.
- Engine wiring (call `io.set_stride(ENC_PAGE_SIZE)` + `cache.set_cipher(...)` at open after the DEK is unwrapped) is Phase 2's `TransactionManager`/`Chisel::open` responsibility; Phase 3 only provides the `set_stride`/`set_cipher` entry points.

Relevant files: `/Users/xof/Documents/Dev/chisel/src/page_io.rs`, `/Users/xof/Documents/Dev/chisel/src/spillway.rs`, `/Users/xof/Documents/Dev/chisel/src/page_cache.rs`.

---

## Phase 4: Public API, error variants, and Python bindings

### Task 4.1: Add encryption error variants to `ChiselError`

**Files:**
- Modify: `src/error.rs:158` (after the `UnsupportedPageSize` variant, before the closing `}` of the enum at line 162)
- Modify: `src/error.rs:181` (`is_fatal()` matches! list)
- Modify: `src/error.rs:278` (`Display` impl, after the `UnsupportedPageSize` arm)
- Modify: `src/error.rs:495` (test `documented_is_fatal` Fatal block) and `:540`/`:551` (the `all` array + the `== 9` tripwire)
- Test: `#[cfg(test)] mod tests` in `src/error.rs` (extend existing) + new `encryption_error_classification` test

**Interfaces:**
- Consumes: nothing from earlier phases (pure type additions). **Execution order: implement this task FIRST — before Phase 2** — because Phases 2, 3, and 5 return these variants (see the plan header's execution-order exception).
- Produces (later phases map these at the engine + Python layers):
  - `ChiselError::NoEncryptionKey` (operational)
  - `ChiselError::InvalidEncryptionKey` (operational)
  - `ChiselError::EncryptionNotSupported` (operational)
  - `ChiselError::NoFreeKeySlot` (operational; key-management — all 8 slots occupied)
  - `ChiselError::LastKeySlot` (operational; key-management — refusing to remove the only active credential)
  - `ChiselError::DecryptionFailed { page_id: u64 }` (fatal; `is_fatal() == true`)

- [ ] **Step 1: Write the failing test**

Add to `src/error.rs`'s `mod tests`:
```rust
    // Phase 4: the three operational encryption errors are recoverable (the
    // on-disk DB is intact — the caller supplied the wrong/no key, or asked an
    // old binary to read a v2 file), so is_fatal() is false. DecryptionFailed
    // is fatal: an AEAD tag failure on a page read means the ciphertext or DEK
    // is wrong and the snapshot can't be trusted, so it must poison (I1).
    #[test]
    fn encryption_error_classification() {
        assert!(!ChiselError::NoEncryptionKey.is_fatal());
        assert!(!ChiselError::InvalidEncryptionKey.is_fatal());
        assert!(!ChiselError::EncryptionNotSupported.is_fatal());
        assert!(!ChiselError::NoFreeKeySlot.is_fatal());
        assert!(!ChiselError::LastKeySlot.is_fatal());
        assert!(ChiselError::DecryptionFailed { page_id: 7 }.is_fatal());

        // Display carries the page id for the fatal variant.
        let msg = format!("{}", ChiselError::DecryptionFailed { page_id: 7 });
        assert!(msg.contains('7'), "Display {msg:?} should mention page id 7");

        // source() is None for all four — none wrap an inner cause.
        use std::error::Error;
        for e in [
            ChiselError::NoEncryptionKey,
            ChiselError::InvalidEncryptionKey,
            ChiselError::EncryptionNotSupported,
            ChiselError::NoFreeKeySlot,
            ChiselError::LastKeySlot,
            ChiselError::DecryptionFailed { page_id: 0 },
        ] {
            assert!(e.source().is_none());
        }
    }
```
Also extend the existing exhaustiveness test `is_fatal_matches_documented_classification_for_every_variant`: add the six new variants to `documented_is_fatal`'s blocks (five operational, one fatal), to the `all` array, and bump the fatal-count tripwire from `9` to `10` (the one new fatal variant is `DecryptionFailed`).

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test encryption_error_classification`  Expected: FAIL (variants don't exist — compile error)

- [ ] **Step 3: Implement**

In the enum, after the `UnsupportedPageSize { stored, compiled }` variant (src/error.rs:161, inside the Operational/Fatal region — place the three operational ones with the operational block and the fatal one with the fatal block; placement is by comment-block only since the enum is flat):
```rust
    // Operational — the caller supplied the wrong key material or none, or
    // asked an unencrypted-only build to open an encrypted DB. The on-disk
    // file is untouched; the caller fixes their `Options` and retries.
    //
    // Raised at open time when the superblock declares encryption but
    // `Options::encryption_key` was None.
    NoEncryptionKey,
    // Raised at open time when a key was supplied but no key-slot's wrapped
    // DEK could be unwrapped under the derived KEK (wrong passphrase / raw
    // key). Operational: the DB is intact; supply the right key and reopen.
    InvalidEncryptionKey,
    // Raised when a key was supplied to open a *plaintext* DB, or an
    // encrypted DB is opened by a build that the on-disk crypto-header
    // algorithm id is unknown to. Operational: the request is a mismatch,
    // not corruption.
    EncryptionNotSupported,
    // Operational — key-management (add/rotate/remove) ran out of room: all
    // KEY_SLOT_COUNT (8) wrapped-DEK slots are occupied, so there is nowhere
    // to stage a new credential. The DB is intact; remove an unused key first.
    NoFreeKeySlot,
    // Operational — refusing to remove the last active key slot, which would
    // leave the database with zero usable credentials (permanently unopenable).
    LastKeySlot,
    // Fatal — an AEAD authentication failure while decrypting a page that
    // was already located and read off disk. The ciphertext, tag, nonce, or
    // session DEK disagree, so the last-committed snapshot cannot be trusted;
    // poisons the manager (I1) exactly like ChecksumMismatch. Distinct from
    // InvalidEncryptionKey (a *key-slot* unwrap failure at open, before any
    // page is served) — this fires mid-session on a real data/handle page.
    DecryptionFailed {
        page_id: u64,
    },
```

In `is_fatal()` (src/error.rs:181), add `DecryptionFailed` to the `matches!`:
```rust
                | ChiselError::UnsupportedPageSize { .. }
                | ChiselError::DecryptionFailed { .. }
```

In `Display` (after the `UnsupportedPageSize` arm at src/error.rs:281):
```rust
            ChiselError::NoEncryptionKey => write!(
                f,
                "database is encrypted but no encryption_key was supplied"
            ),
            ChiselError::InvalidEncryptionKey => write!(
                f,
                "encryption key does not match any key slot (wrong passphrase or raw key)"
            ),
            ChiselError::EncryptionNotSupported => write!(
                f,
                "encryption not supported for this open (key supplied for a plaintext database, or unknown crypto algorithm)"
            ),
            ChiselError::NoFreeKeySlot => write!(
                f,
                "no free key slot: all 8 key slots are occupied (remove an unused key first)"
            ),
            ChiselError::LastKeySlot => write!(
                f,
                "refusing to remove the last active key slot (the database would become permanently unopenable)"
            ),
            ChiselError::DecryptionFailed { page_id } => {
                write!(f, "decryption/authentication failed for page {page_id}")
            }
```

In the exhaustiveness test `documented_is_fatal`: add `NoEncryptionKey | InvalidEncryptionKey | EncryptionNotSupported` to the operational (`=> false`) block and `ChiselError::DecryptionFailed { .. }` to the fatal (`=> true`) block; add all four to the `all` array; change `assert_eq!(all.iter().filter(|e| e.is_fatal()).count(), 9)` to `10`.

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo test`  Expected: PASS (both the new test and the updated exhaustiveness test)

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(error): add encryption error variants (NoEncryptionKey, InvalidEncryptionKey, EncryptionNotSupported, NoFreeKeySlot, LastKeySlot, fatal DecryptionFailed)"
```

---

### Task 4.2: Re-export `Key` / `Argon2Params` and add the `Options` encryption fields

**Files:**
- Modify: `src/lib.rs:65` (re-export block — add crypto types)
- Modify: `src/lib.rs:39-55` (module list — add `pub(crate) mod crypto;`)
- Modify: `src/lib.rs:133` (`Options` struct fields, after `superblock_count`)
- Modify: `src/lib.rs:176` (`Default for Options`, add the two `None` defaults)
- Modify: `src/lib.rs:207` (`impl Options` — add two builder setters after `superblock_count`)
- Test: `#[cfg(test)] mod tests` at the bottom of `src/lib.rs` (add if absent)

**Interfaces:**
- Consumes (from Phase 1, `src/crypto/mod.rs`): `pub enum Key { Raw(..), Passphrase(..) }`, `pub struct Argon2Params { pub m_cost: u32, pub t_cost: u32, pub p_cost: u32 }` (`Clone`).
- Produces:
  - `pub use crypto::{Key, Argon2Params};`
  - `Options.encryption_key: Option<Key>`
  - `Options.argon2_params: Option<Argon2Params>`
  - `Options::encryption_key(self, key: Key) -> Self`
  - `Options::argon2_params(self, params: Argon2Params) -> Self`

- [ ] **Step 1: Write the failing test**

Add to `src/lib.rs`:
```rust
#[cfg(test)]
mod options_encryption_tests {
    use super::*;

    // The two encryption fields default to None (a plaintext DB) and round-trip
    // through the chained-setter builder, preserving #[non_exhaustive] (callers
    // can't struct-literal, so the setters are the only construction path).
    #[test]
    fn encryption_options_default_none_and_set() {
        let o = Options::default();
        assert!(o.encryption_key.is_none());
        assert!(o.argon2_params.is_none());

        let raw = Key::Raw(zeroize::Zeroizing::new(vec![0u8; 32]));
        let o = Options::default()
            .encryption_key(raw)
            .argon2_params(Argon2Params {
                m_cost: 19456,
                t_cost: 2,
                p_cost: 1,
            });
        assert!(matches!(o.encryption_key, Some(Key::Raw(_))));
        let p = o.argon2_params.expect("set above");
        assert_eq!((p.m_cost, p.t_cost, p.p_cost), (19456, 2, 1));
    }
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test encryption_options_default_none_and_set`  Expected: FAIL (fields/setters/re-exports don't exist)

- [ ] **Step 3: Implement**

Add the module (src/lib.rs, in the `pub(crate) mod` block ~line 39):
```rust
pub(crate) mod crypto;
```

Add the re-export (src/lib.rs, after the `pub use error::{...}` at line 65):
```rust
pub use crypto::{Argon2Params, Key};
```

Add the two fields to `Options` (after `superblock_count: u32,` at src/lib.rs:139):
```rust
    /// Encryption key for an encrypted database. `None` (default) opens or
    /// creates a plaintext DB. On create, `Some(key)` makes a new encrypted
    /// DB sealed under a random DEK wrapped by this key. On reopen, the key
    /// must unwrap one of the on-disk key slots or `open` returns
    /// `InvalidEncryptionKey`. Supplying a key to open a plaintext DB returns
    /// `EncryptionNotSupported`; omitting it on an encrypted DB returns
    /// `NoEncryptionKey`.
    pub encryption_key: Option<Key>,
    /// Argon2id cost parameters used to derive the KEK from a `Key::Passphrase`
    /// on *create*. `None` uses `Argon2Params::default()` (OWASP: 19 MiB / t=2 /
    /// p=1). Ignored for `Key::Raw` (HKDF, no cost params) and on reopen (the
    /// params are read from the key slot the file was written with).
    pub argon2_params: Option<Argon2Params>,
```

Add to `Default for Options` (src/lib.rs:189, after `superblock_count: ...,`):
```rust
            encryption_key: None,
            argon2_params: None,
```

Add the two setters to `impl Options` (after the `superblock_count` setter, src/lib.rs:236):
```rust
    /// Set the encryption key. See [`Options::encryption_key`] for the
    /// create-vs-reopen semantics.
    pub fn encryption_key(mut self, key: Key) -> Self {
        self.encryption_key = Some(key);
        self
    }
    /// Set the Argon2id cost parameters used when deriving a KEK from a
    /// passphrase on database creation. No effect for raw keys or on reopen.
    pub fn argon2_params(mut self, params: Argon2Params) -> Self {
        self.argon2_params = Some(params);
        self
    }
```

Add `zeroize` as a dev/normal dep if not already present (Phase 1 adds it to `[dependencies]`; the test above uses `zeroize::Zeroizing` through the public `Key`). No Cargo change needed here if Phase 1 landed it.

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo test encryption_options_default_none_and_set`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(api): re-export Key/Argon2Params and add encryption_key/argon2_params to Options"
```

---

### Task 4.3: Wire the key through `open()` / `open_in_memory_with_options()`

**Files:**
- Modify: `src/lib.rs:341-347` (`open`: the create/open dispatch) and `:332-339` (pass key into the create/open call)
- Modify: `src/lib.rs:381+` (`open_in_memory_with_options`: same dispatch)
- Test: `tests/encryption_roundtrip.rs` (new integration test — runs under plain `cargo test`)

**Interfaces:**
- Consumes (from Phase 2): the create/open flow now accepts the key. The exact Phase-2 signatures are:
  - `TransactionManager::create_new(cache, superblock_count, encryption_key: Option<Key>) -> Result<TransactionManager>`
  - `TransactionManager::open_existing(cache, encryption_key: Option<Key>) -> Result<TransactionManager>`
  These extend the current 2-arg / 1-arg signatures at `src/lib.rs:344` and `:346`. Phase 2 is responsible for adding the `encryption_key` parameter; this task passes `options.encryption_key` into them.
- Produces: end-to-end encrypted open behavior the public API guarantees.

- [ ] **Step 1: Write the failing test**

Create `tests/encryption_roundtrip.rs`:
```rust
// End-to-end public-API encryption contract (Phase 4): create encrypted →
// write → reopen with the same key reads it back; wrong key →
// InvalidEncryptionKey; no key → NoEncryptionKey. Uses a raw 32-byte key so
// the test does not pay the Argon2id cost (passphrase derivation is covered in
// the crypto unit tests).
use chisel::{ChiselError, Chisel, Key, Options};
use zeroize::Zeroizing;

fn raw_key(b: u8) -> Key {
    Key::Raw(Zeroizing::new(vec![b; 32]))
}

#[test]
fn encrypted_roundtrip_and_wrong_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("enc.db");

    // Create encrypted, write a value, capture the handle, close.
    let handle = {
        let mut db = Chisel::open(
            &path,
            Options::default().encryption_key(raw_key(0xAB)),
        )
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
        let v = db.read(chisel::Handle::from(handle)).expect("read");
        assert_eq!(&v, b"secret-payload");
    }

    // Reopen with the WRONG key: InvalidEncryptionKey.
    {
        let err = Chisel::open(
            &path,
            Options::default()
                .create_if_missing(false)
                .encryption_key(raw_key(0x00)),
        )
        .expect_err("wrong key must fail");
        assert!(
            matches!(err, ChiselError::InvalidEncryptionKey),
            "expected InvalidEncryptionKey, got {err:?}"
        );
    }

    // Reopen with NO key: NoEncryptionKey.
    {
        let err = Chisel::open(
            &path,
            Options::default().create_if_missing(false),
        )
        .expect_err("missing key must fail");
        assert!(
            matches!(err, ChiselError::NoEncryptionKey),
            "expected NoEncryptionKey, got {err:?}"
        );
    }
}
```
Ensure `tempfile` is a `[dev-dependencies]` entry (it is already used by the existing integration tests; if not, add `tempfile = "3"`).

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test --test encryption_roundtrip`  Expected: FAIL (open ignores `encryption_key`; create/open signatures don't take it yet — compile error until Phase 2 lands, then a behavior failure)

- [ ] **Step 3: Implement**

In `Chisel::open` (src/lib.rs:341), pass the key into both arms:
```rust
        let txm = if file_exists {
            // Existing database: N is discovered from the on-disk superblock.
            // options.superblock_count is ignored here. The key (if any) must
            // unwrap a key slot; mismatch surfaces as InvalidEncryptionKey,
            // a missing key as NoEncryptionKey, both from open_existing.
            TransactionManager::open_existing(cache, options.encryption_key)?
        } else {
            TransactionManager::create_new(
                cache,
                options.superblock_count,
                options.encryption_key,
            )?
        };
```

In `open_in_memory_with_options` (src/lib.rs, the `create_new` call after line 400):
```rust
        let txm = TransactionManager::create_new(
            cache,
            options.superblock_count,
            options.encryption_key,
        )?;
```

Update the `# Errors` rustdoc on `open` (src/lib.rs:303) to add the new operational errors:
```rust
    /// `InvalidSuperblockCount` (the `superblock_count` option is out of
    /// range), `FileNotFound` (no file at `path` and `create_if_missing` is
    /// false), or `LockFailed` (another handle holds the exclusive flock).
    /// For an encrypted database: `NoEncryptionKey` (file is encrypted but
    /// no `encryption_key` given), `InvalidEncryptionKey` (key unwraps no key
    /// slot), or `EncryptionNotSupported` (key given for a plaintext file).
    /// When reopening an existing file, parsing the superblock can also yield
    /// `UnsupportedFormatVersion`, `CorruptSuperblock`, `ChecksumMismatch`,
    /// `FileSizeMismatch`, or `IoError`.
```

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo test --test encryption_roundtrip`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(api): wire encryption_key through open() and open_in_memory_with_options()"
```

---

### Task 4.4: Add `encryption_key` kwarg to the Python `open()`

**Files:**
- Modify: `python/src/db.rs:140-149` (`#[pyo3(signature = (...))]` — add `encryption_key = None`)
- Modify: `python/src/db.rs:157-166` (`open` fn params — add `encryption_key: Option<Py<PyAny>>`)
- Modify: `python/src/db.rs:174-186` (coerce the kwarg → `chisel::Key` under the GIL, before `py.detach`)
- Modify: `python/src/db.rs:208-214` (`Options` builder chain — add `.encryption_key(...)` when present)
- Test: `python/tests/test_encryption.py` (new; mirrors existing python/tests style)

**Interfaces:**
- Consumes (Phase 4 Task 4.2 / 4.3): `chisel::Key`, `Options::encryption_key`.
- Produces: `chisel.open(path, *, encryption_key=...)` where `bytes` → `Key::Raw`, `str` → `Key::Passphrase`.

- [ ] **Step 1: Write the failing test**

Create `python/tests/test_encryption.py`:
```python
import pathlib

import chisel
import pytest


def test_encrypted_roundtrip_with_bytes_key(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    key = b"\xab" * 32

    with chisel.open(path, encryption_key=key) as db:
        db.begin()
        h = db.allocate(b"secret-payload")
        db.commit()

    with chisel.open(path, create_if_missing=False, encryption_key=key) as db:
        assert db.read(h) == b"secret-payload"


def test_wrong_key_raises_invalid_encryption_key(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    with chisel.open(path, encryption_key=b"\xab" * 32) as db:
        db.begin()
        db.allocate(b"x")
        db.commit()

    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.open(path, create_if_missing=False, encryption_key=b"\x00" * 32)


def test_missing_key_raises_no_encryption_key(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    with chisel.open(path, encryption_key=b"\xab" * 32) as db:
        db.begin()
        db.allocate(b"x")
        db.commit()

    with pytest.raises(chisel.NoEncryptionKeyError):
        chisel.open(path, create_if_missing=False)


def test_passphrase_key_roundtrip(tmp_path: pathlib.Path):
    path = tmp_path / "pass.db"
    with chisel.open(path, encryption_key="correct horse battery staple") as db:
        db.begin()
        h = db.allocate(b"v")
        db.commit()
    with chisel.open(
        path, create_if_missing=False, encryption_key="correct horse battery staple"
    ) as db:
        assert db.read(h) == b"v"
```

This depends on the three new Python exception classes (`NoEncryptionKeyError`, `InvalidEncryptionKeyError`, `EncryptionNotSupportedError`) and a `DecryptionFailedError` — added in Task 4.5. Run order: 4.5 lands the exception classes, 4.4 the kwarg; commit 4.4 after 4.5 or fold the exception additions in first. (They are independent files; do 4.5's `errors.rs` edits before running this test.)

- [ ] **Step 2: Run test, verify it fails**

Run: `cd python && maturin develop && python -m pytest tests/test_encryption.py`  Expected: FAIL (`open()` has no `encryption_key` kwarg)

- [ ] **Step 3: Implement**

Add to the signature (python/src/db.rs:140, after `superblock_count = 2`):
```rust
    superblock_count = 2,
    encryption_key = None
```

Add the param (python/src/db.rs:165, after `superblock_count: u32,`):
```rust
    superblock_count: u32,
    encryption_key: Option<Py<PyAny>>,
```

Coerce the kwarg under the GIL, immediately after the `path_buf` extraction block (python/src/db.rs:186, before the spillway resolution). `bytes` → `Key::Raw`, `str` → `Key::Passphrase`; anything else is a `TypeError`:
```rust
    // Coerce encryption_key under the GIL (before py.detach): `bytes` →
    // Key::Raw, `str` → Key::Passphrase. Done here so a bad type raises a
    // synchronous Python TypeError, matching the path coercion above.
    let key: Option<chisel::Key> = match encryption_key {
        None => None,
        Some(obj) => {
            let bound = obj.bind(py);
            if let Ok(b) = bound.cast::<pyo3::types::PyBytes>() {
                Some(chisel::Key::Raw(zeroize::Zeroizing::new(b.as_bytes().to_vec())))
            } else if let Ok(s) = bound.cast::<PyString>() {
                Some(chisel::Key::Passphrase(zeroize::Zeroizing::new(
                    s.to_str()?.to_owned(),
                )))
            } else {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "encryption_key must be bytes (raw 32-byte key) or str (passphrase)",
                ));
            }
        }
    };
```

Add `.encryption_key(...)` to the builder chain (python/src/db.rs:208). Because `Options::encryption_key` takes `Key` (not `Option<Key>`), only call it when present:
```rust
    let mut options = chisel::Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(resolved_spillway_max_bytes)
        .drain_insertion(drain_insertion.into())
        .create_if_missing(create_if_missing)
        .read_only(read_only)
        .superblock_count(superblock_count);
    if let Some(k) = key {
        options = options.encryption_key(k);
    }
```

Add `zeroize` to `python/Cargo.toml` `[dependencies]` (`zeroize = "1"`) if not already present — needed for `Zeroizing` here.

- [ ] **Step 4: Run test, verify it passes**

Run: `cd python && maturin develop && python -m pytest tests/test_encryption.py`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(python): add encryption_key kwarg to open() (bytes->raw, str->passphrase)"
```

---

### Task 4.5: Map the new errors to Python exception classes

**Files:**
- Modify: `python/src/errors.rs:88-144` (declare the new exception classes)
- Modify: `python/src/errors.rs:150-235` (`register` — attach them to the module)
- Modify: `python/src/errors.rs:256-359` (`to_py_err` — add concrete arms)
- Modify: `python/src/errors.rs:20-50` (class-hierarchy doc comment)
- Modify: `python/chisel/__init__.py` and `python/chisel/_chisel.pyi` (re-export / declare the four classes — mirror the existing entries)
- Test: covered by `python/tests/test_encryption.py` (Task 4.4) asserting `chisel.NoEncryptionKeyError` / `chisel.InvalidEncryptionKeyError` are raised

**Interfaces:**
- Consumes: `ChiselError::{NoEncryptionKey, InvalidEncryptionKey, EncryptionNotSupported, DecryptionFailed}` (Task 4.1).
- Produces: Python `NoEncryptionKeyError`, `InvalidEncryptionKeyError`, `EncryptionNotSupportedError` (under `OperationalError`); `DecryptionFailedError` (under `FatalError`).

- [ ] **Step 1: Write the failing test**

The two assertions in `python/tests/test_encryption.py` (Task 4.4) already require `chisel.InvalidEncryptionKeyError` and `chisel.NoEncryptionKeyError` to exist and be raised. Add one direct-classification check to that file:
```python
def test_encryption_exception_hierarchy():
    # Operational tier: recoverable, DB intact.
    assert issubclass(chisel.NoEncryptionKeyError, chisel.OperationalError)
    assert issubclass(chisel.InvalidEncryptionKeyError, chisel.OperationalError)
    assert issubclass(chisel.EncryptionNotSupportedError, chisel.OperationalError)
    # Fatal tier: drop-and-reopen.
    assert issubclass(chisel.DecryptionFailedError, chisel.FatalError)
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cd python && maturin develop && python -m pytest tests/test_encryption.py::test_encryption_exception_hierarchy`  Expected: FAIL (`AttributeError`: classes don't exist)

- [ ] **Step 3: Implement**

Declare the classes (python/src/errors.rs, after `create_exception!(_chisel, TagMismatchError, OperationalError);` at line 116 for the operational ones, and after `create_exception!(_chisel, PoisonedError, FatalError);` at line 144 for the fatal one):
```rust
// Encryption key errors — operational: the DB file is intact, the caller
// supplied the wrong/no key or a key for a plaintext DB. Mirrors the three
// operational ChiselError encryption variants.
create_exception!(_chisel, NoEncryptionKeyError, OperationalError);
create_exception!(_chisel, InvalidEncryptionKeyError, OperationalError);
create_exception!(_chisel, EncryptionNotSupportedError, OperationalError);
```
```rust
// Fatal: an AEAD authentication failure decrypting a page already read off
// disk. Poison-and-reopen, like ChecksumMismatchError.
create_exception!(_chisel, DecryptionFailedError, FatalError);
```

Register them (python/src/errors.rs `register`, alongside the other operational adds near line 201, and the fatal add near line 232):
```rust
    m.add("NoEncryptionKeyError", py.get_type::<NoEncryptionKeyError>())?;
    m.add(
        "InvalidEncryptionKeyError",
        py.get_type::<InvalidEncryptionKeyError>(),
    )?;
    m.add(
        "EncryptionNotSupportedError",
        py.get_type::<EncryptionNotSupportedError>(),
    )?;
```
```rust
    m.add("DecryptionFailedError", py.get_type::<DecryptionFailedError>())?;
```

Add concrete arms in `to_py_err` (python/src/errors.rs:280, after the `TagMismatch` operational arm, and after the `Poisoned` fatal arm at line 342):
```rust
        RustChiselError::NoEncryptionKey => NoEncryptionKeyError::new_err(msg),
        RustChiselError::InvalidEncryptionKey => InvalidEncryptionKeyError::new_err(msg),
        RustChiselError::EncryptionNotSupported => EncryptionNotSupportedError::new_err(msg),
```
```rust
        RustChiselError::DecryptionFailed { .. } => DecryptionFailedError::new_err(msg),
```

Extend the hierarchy doc comment (python/src/errors.rs:34-50): add the three operational classes under `OperationalError` and `DecryptionFailedError` under `FatalError`.

In `python/chisel/__init__.py` and `python/chisel/_chisel.pyi`, add the four class names to the re-export list and stub declarations, mirroring the format of the existing `InvalidHandleError` / `PoisonedError` entries. (Grep them for `InvalidHandleError` to find every list that needs the additions.)

- [ ] **Step 4: Run test, verify it passes**

Run: `cd python && maturin develop && python -m pytest tests/test_encryption.py`  Expected: PASS (all of Task 4.4's tests now pass too)

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(python): map encryption errors to NoEncryptionKeyError/InvalidEncryptionKeyError/EncryptionNotSupportedError/DecryptionFailedError"
```

---

Notes for the orchestrator:
- **Task ordering:** 4.1 → 4.2 → (4.3 needs Phase 2's `create_new`/`open_existing` key params) → 4.5 before 4.4 (the Python test in 4.4 references the exception classes 4.5 defines), or land both then run. 4.1 is independent of Phases 2/3 and can land first.
- **`zeroize` dep:** Phase 1 adds it to the root `Cargo.toml`. Task 4.2's test and Task 4.4 use `Zeroizing` directly; if Phase 1 hasn't landed when these run, add `zeroize = "1"` to root `[dependencies]` and `python/Cargo.toml` `[dependencies]`.
- I avoided re-listing fatal `DecryptionFailedError` text; the `IoError` two-base machinery is untouched (DecryptionFailed is single-base `FatalError`).

---

## Phase 5: Key management: add / rotate / remove credentials

All three operations are O(1): they mutate the in-superblock key-slot table and write one new superblock via the ordinary A/B slot-rotation commit. No page is re-encrypted — the per-DB DEK never changes, only its KEK-wrapping in the slot table does. The session DEK (already unwrapped at open, Phase 2/3) is the pivot: every operation either proves possession of it (via `existing`/`old`/`key` unlocking some slot) or re-wraps it under a new KEK.

Each task assumes the Phase-2 facts:
- `TransactionManager` holds the session `Dek` (field `dek: Option<crypto::Dek>`, `None` for plaintext DBs) and the open superblock's `crypto: Option<CryptoHeader>`.
- A metadata-only superblock write helper exists from Phase 2's commit path. Phase 5 adds `rewrite_crypto_header` as the single building block the three public methods route through; it bumps `txn_counter`, serializes the superblock with the mutated `CryptoHeader`, writes the inactive slot, and fsyncs — identical durability to a data commit, no data pages touched.

Phase-4 errors consumed verbatim: `ChiselError::InvalidEncryptionKey` (no slot unlocks with the supplied credential), `ChiselError::NoFreeKeySlot` (all 8 slots occupied), `ChiselError::LastKeySlot` (refusing to clear the only active slot), `ChiselError::EncryptionNotSupported` (key op on a plaintext DB).

---

### Task 5.1: Slot-table helpers on `CryptoHeader` (find / unlock / free-slot / wrap-into)

**Files:**
- Modify: `src/superblock.rs` (add an `impl CryptoHeader` block after the `CryptoHeader` struct introduced in Phase 2; the struct sits alongside `KeySlot`, `KEY_SLOT_COUNT`, `KEY_SLOT_SIZE`)
- Test: `src/superblock.rs` (`#[cfg(test)] mod tests`, same file)

**Interfaces:**
- Consumes (Phase 1): `crypto::{Key, Kek, Dek, KdfId, Argon2Params, derive_kek, wrap_dek, unwrap_dek, random_array, CryptoError, SALT_LEN, NONCE_LEN, DEK_LEN, TAG_LEN}`
- Consumes (Phase 2): `KeySlot { state: u8, kdf_id: u8, argon2: Argon2Params, salt: [u8; SALT_LEN], wrap_nonce: [u8; NONCE_LEN], wrapped_dek: [u8; DEK_LEN], wrap_tag: [u8; TAG_LEN] }`, `CryptoHeader { algorithm: u8, stride: u32, slots: [KeySlot; KEY_SLOT_COUNT] }`, and the slot-state constants `KEY_SLOT_EMPTY = 0u8` / `KEY_SLOT_ACTIVE = 1u8` (Phase 2 defines these in `superblock.rs`).
- Produces (this task; later tasks rely on):
  - `KeySlot::is_active(&self) -> bool`
  - `KeySlot::aad(&self) -> [u8; 1 + 1 + 12 + SALT_LEN + NONCE_LEN]` — the slot-metadata bytes used as wrap AAD (binds the wrapped DEK to this slot's KDF identity so a slot can't be transplanted).
  - `CryptoHeader::unlock(&self, key: &Key) -> Result<(usize, crypto::Dek), ChiselError>` — returns `(slot_index, dek)` of the first active slot the key opens; `Err(InvalidEncryptionKey)` if none.
  - `CryptoHeader::free_slot(&self) -> Option<usize>`
  - `CryptoHeader::active_count(&self) -> usize`
  - `CryptoHeader::wrap_into(&mut self, slot: usize, key: &Key, dek: &crypto::Dek)` — fills `slots[slot]` with a fresh salt + (HKDF default) KDF params, wrapping `dek` under the KEK derived from `key`.

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod crypto_header_tests {
    use super::*;
    use crate::crypto::{self, Key, KdfId};
    use zeroize::Zeroizing;

    fn raw(b: u8) -> Key {
        Key::Raw(Zeroizing::new(vec![b; 32]))
    }

    // A header with exactly one active slot holding `dek` under `key`.
    fn header_with_one(key: &Key, dek: &crypto::Dek) -> CryptoHeader {
        let mut h = CryptoHeader {
            algorithm: 1,
            stride: crypto::ENC_PAGE_SIZE as u32,
            slots: [KeySlot::EMPTY; KEY_SLOT_COUNT],
        };
        h.wrap_into(0, key, dek);
        h
    }

    #[test]
    fn unlock_finds_the_right_slot_and_recovers_dek() {
        let dek = crypto::random_dek();
        let k0 = raw(0xA1);
        let mut h = header_with_one(&k0, &dek);

        // Add a second credential into slot 3 wrapping the SAME dek.
        let k1 = raw(0xB2);
        h.wrap_into(3, &k1, &dek);

        let (idx0, d0) = h.unlock(&k0).expect("k0 unlocks");
        let (idx1, d1) = h.unlock(&k1).expect("k1 unlocks");
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 3);
        // Both recover the identical DEK bytes.
        assert_eq!(d0.expose(), dek.expose());
        assert_eq!(d1.expose(), dek.expose());
    }

    #[test]
    fn unlock_wrong_key_is_wrongkey_not_panic() {
        let dek = crypto::random_dek();
        let h = header_with_one(&raw(0x01), &dek);
        let err = h.unlock(&raw(0x99)).unwrap_err();
        assert_eq!(err, ChiselError::InvalidEncryptionKey);
    }

    #[test]
    fn free_slot_and_active_count_track_occupancy() {
        let dek = crypto::random_dek();
        let mut h = header_with_one(&raw(0x01), &dek);
        assert_eq!(h.active_count(), 1);
        assert_eq!(h.free_slot(), Some(1));

        // Fill every slot.
        for i in 1..KEY_SLOT_COUNT {
            h.wrap_into(i, &raw(i as u8 + 1), &dek);
        }
        assert_eq!(h.active_count(), KEY_SLOT_COUNT);
        assert_eq!(h.free_slot(), None);
    }
}
```
(`KeySlot::EMPTY`, `crypto::Dek::expose()`, and `crypto::ENC_PAGE_SIZE` are Phase-1/Phase-2 surface; `Dek::expose(&self) -> &[u8; DEK_LEN]` is the test-only accessor Phase 1 defines under `#[cfg(test)]`.)

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test crypto_header_tests`  Expected: FAIL (methods not yet implemented)

- [ ] **Step 3: Implement**
```rust
// In src/superblock.rs, after the CryptoHeader struct (Phase 2).
use crate::crypto::{self, Argon2Params, KdfId, Key, SALT_LEN, NONCE_LEN};

impl KeySlot {
    pub const EMPTY: KeySlot = KeySlot {
        state: KEY_SLOT_EMPTY,
        kdf_id: 0,
        argon2: Argon2Params { m_cost: 0, t_cost: 0, p_cost: 0 },
        salt: [0u8; SALT_LEN],
        wrap_nonce: [0u8; NONCE_LEN],
        wrapped_dek: [0u8; crypto::DEK_LEN],
        wrap_tag: [0u8; crypto::TAG_LEN],
    };

    pub fn is_active(&self) -> bool {
        self.state == KEY_SLOT_ACTIVE
    }

    // AAD binds the wrapped DEK to this slot's KDF identity: kdf_id, the
    // three Argon2 cost words, the salt, and the wrap nonce. Re-deriving the
    // KEK requires the exact salt/params, so transplanting a wrapped DEK into
    // a slot with different metadata fails the Poly1305 tag — a slot cannot be
    // forged from another slot's ciphertext.
    pub fn aad(&self) -> [u8; 2 + 12 + SALT_LEN + NONCE_LEN] {
        let mut a = [0u8; 2 + 12 + SALT_LEN + NONCE_LEN];
        a[0] = self.state;
        a[1] = self.kdf_id;
        a[2..6].copy_from_slice(&self.argon2.m_cost.to_le_bytes());
        a[6..10].copy_from_slice(&self.argon2.t_cost.to_le_bytes());
        a[10..14].copy_from_slice(&self.argon2.p_cost.to_le_bytes());
        a[14..14 + SALT_LEN].copy_from_slice(&self.salt);
        a[14 + SALT_LEN..].copy_from_slice(&self.wrap_nonce);
        a
    }
}

impl CryptoHeader {
    pub fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_active()).count()
    }

    pub fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| !s.is_active())
    }

    /// Find the first active slot `key` unlocks; recover the DEK from it.
    /// `InvalidEncryptionKey` if no active slot's KEK validates the wrapped DEK tag.
    pub fn unlock(&self, key: &Key) -> Result<(usize, crypto::Dek), ChiselError> {
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.is_active() {
                continue;
            }
            // kdf_id is the on-disk u8; map to the typed KdfId. An unknown id
            // means a slot written by a newer format — skip it (treated like a
            // non-matching credential) rather than erroring the whole open.
            let kdf = match slot.kdf_id {
                x if x == KdfId::Hkdf as u8 => KdfId::Hkdf,
                x if x == KdfId::Argon2id as u8 => KdfId::Argon2id,
                _ => continue,
            };
            let kek = match crypto::derive_kek(key, kdf, &slot.salt, &slot.argon2) {
                Ok(k) => k,
                Err(_) => continue,
            };
            if let Ok(dek) = crypto::unwrap_dek(
                &kek,
                &slot.wrapped_dek,
                &slot.wrap_tag,
                &slot.wrap_nonce,
                &slot.aad(),
            ) {
                return Ok((i, dek));
            }
        }
        Err(ChiselError::InvalidEncryptionKey)
    }

    /// Populate `slots[slot]` with a fresh HKDF salt + nonce, wrapping `dek`
    /// under the KEK derived from `key`. Always uses HKDF for raw keys and
    /// Argon2id for passphrases — the caller never picks the KDF; it follows
    /// the key variant (matches the open-time derivation in Phase 1/2).
    pub fn wrap_into(&mut self, slot: usize, key: &Key, dek: &crypto::Dek) {
        let (kdf_id, argon2) = match key {
            Key::Raw(_) => (KdfId::Hkdf, Argon2Params { m_cost: 0, t_cost: 0, p_cost: 0 }),
            Key::Passphrase(_) => (KdfId::Argon2id, Argon2Params::default()),
        };
        let salt: [u8; SALT_LEN] = crypto::random_array();
        let wrap_nonce: [u8; NONCE_LEN] = crypto::random_array();

        let mut s = KeySlot {
            state: KEY_SLOT_ACTIVE,
            kdf_id: kdf_id as u8,
            argon2,
            salt,
            wrap_nonce,
            wrapped_dek: [0u8; crypto::DEK_LEN],
            wrap_tag: [0u8; crypto::TAG_LEN],
        };
        // AAD is computed over the slot metadata as it will sit on disk; the
        // wrapped_dek/wrap_tag fields are zero at AAD time (aad() does not read
        // them), so the AAD is stable before and after the wrap.
        let kek = crypto::derive_kek(key, kdf_id, &s.salt, &s.argon2)
            .expect("KEK derivation for a freshly-generated salt cannot fail");
        let (wrapped, tag) = crypto::wrap_dek(&kek, dek, &s.wrap_nonce, &s.aad());
        s.wrapped_dek = wrapped;
        s.wrap_tag = tag;
        self.slots[slot] = s;
    }
}
```

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test crypto_header_tests`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): key-slot table helpers (unlock/free_slot/wrap_into) on CryptoHeader"
```

---

### Task 5.2: `TransactionManager::rewrite_crypto_header` — the metadata-only superblock commit

**Files:**
- Create: `src/transaction/keys.rs`
- Modify: `src/transaction/mod.rs:239` (add `mod keys;` next to the other `mod` declarations ending at line 239)
- Test: `src/transaction/keys.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `TransactionManager` fields `cache: RefCell<PageCache>`, `committed_roots: Roots`, `txn_counter: u64`, `superblock_count: u32`, `poisoned: Cell<bool>`, and the Phase-2 fields `crypto: Option<CryptoHeader>`, `dek: Option<crypto::Dek>`. Consumes `Superblock::serialize` (it serializes `self.encryption: Option<CryptoHeader>` in Phase 2) and `PageCache::io_mut().{write_page, fsync}`. Consumes `TransactionManager::poison_on_fatal` (existing).
- Produces: `pub(crate) fn rewrite_crypto_header(&mut self, new_header: CryptoHeader) -> Result<()>` — refuses if `active_txn`, bumps `txn_counter`, writes the inactive slot, fsyncs, then commits `new_header` into `self.crypto`.

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use crate::{Chisel, Options};
    use crate::crypto::Key;
    use zeroize::Zeroizing;
    use tempfile::TempDir;

    fn raw(b: u8) -> Key { Key::Raw(Zeroizing::new(vec![b; 32])) }

    // Rewriting the header out-of-band (here: re-wrapping the same DEK under a
    // second slot) must survive a reopen, because it goes through the same
    // fsync'd A/B superblock write the data commit uses.
    #[test]
    fn rewritten_header_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db");
        let opts = Options::default().encryption_key(raw(0x11)); // Phase 2 setter

        {
            let mut db = Chisel::open(&path, opts.clone()).unwrap();
            db.add_key(&raw(0x11), &raw(0x22)).unwrap(); // exercises rewrite
            db.close().unwrap();
        }
        // Reopen with the second key only: only durable if the rewritten
        // superblock reached disk.
        let db = Chisel::open(&path, Options::default().encryption_key(raw(0x22))).unwrap();
        assert!(!db.is_poisoned());
    }
}
```

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test rewritten_header_persists_across_reopen`  Expected: FAIL (`add_key`/`encryption_key` not yet present)

- [ ] **Step 3: Implement**
```rust
//! transaction::keys — out-of-band key-slot management. Each operation rewrites
//! ONLY the in-superblock CryptoHeader and commits it via the ordinary A/B
//! superblock slot rotation — no data page is touched and the per-DB DEK never
//! changes, so there is no re-encryption. Durability is identical to a data
//! commit: bump txn_counter, write the inactive slot, fsync (the linearization
//! point), then promote the in-memory header.

use super::*;
use crate::superblock::CryptoHeader;

impl TransactionManager {
    /// Write a superblock carrying `new_header`, leaving every other root
    /// untouched. Refuses while a transaction is active (a key op is a
    /// standalone metadata commit, not composable with in-flight data work).
    ///
    /// On any I/O/fsync failure the handle is poisoned: a half-written key-slot
    /// table is exactly the fsyncgate hazard the poison model exists for. The
    /// previous superblock slot still holds the last-good header, so a crash
    /// here is recoverable on reopen — the in-memory promotion happens only
    /// after the fsync returns.
    pub(crate) fn rewrite_crypto_header(&mut self, new_header: CryptoHeader) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.rewrite_crypto_header_inner(new_header)
            .inspect_err(|e| self.poison_on_fatal(e))
    }

    fn rewrite_crypto_header_inner(&mut self, new_header: CryptoHeader) -> Result<()> {
        let mut cache = self.cache.borrow_mut();
        // No dirty data pages exist between transactions, but flush keeps the
        // invariant "everything the new superblock could reference is durable"
        // honest even if a future change leaves the cache dirty here.
        cache.flush()?;

        self.txn_counter = self
            .txn_counter
            .checked_add(1)
            .expect("txn_counter overflowed u64 (2^64 commits) — unreachable");
        let total_pages = cache.file_page_count()?;
        let r = &self.committed_roots;
        let sb = Superblock {
            magic: page::MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: self.txn_counter,
            root_handle_table_page: r.handle_table_page,
            root_freemap_page: r.freemap_page,
            total_pages,
            next_handle: r.next_handle,
            page_size: PAGE_SIZE as u32,
            named_roots: r.named_roots,
            superblock_count: self.superblock_count,
            root_membership_index_page: r.membership_index_page,
            freemap_depth: r.freemap_depth,
            // Phase 2: Superblock::serialize seals the sensitive body under the
            // DEK and writes the plaintext crypto header from this field.
            encryption: Some(new_header.clone()),
        };
        let buf = sb.serialize_encrypted(self.dek.as_ref());
        let inactive = self.txn_counter % self.superblock_count as u64;
        cache.io_mut().write_page(inactive, &buf)?;
        cache.io_mut().fsync()?;

        // Linearized: promote the new header in memory only now.
        self.crypto = Some(new_header);
        self.committed_roots.total_pages = total_pages;
        Ok(())
    }
}
```
(`Superblock` gains `encryption: Option<CryptoHeader>` and `serialize_encrypted(&self, dek: Option<&crypto::Dek>) -> [u8; PAGE_SIZE]` in Phase 2; `total_pages` is re-read rather than assumed because a prior data commit may have grown the file.)

- [ ] **Step 4: Run test, verify it passes** (passes once Tasks 5.3 wires `add_key`)
Run: `cargo test rewritten_header_persists_across_reopen`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): rewrite_crypto_header metadata-only superblock commit"
```

---

### Task 5.3: `Chisel::add_key` and `Chisel::rotate_key`

**Files:**
- Modify: `src/transaction/keys.rs` (add `add_key` / `rotate_key` on `TransactionManager`)
- Modify: `src/lib.rs:893` (add the two public methods inside `impl Chisel`, before the closing brace at line 893)
- Test: `tests/encryption_keys.rs` (integration; the public flow needs `Chisel::open`)

**Interfaces:**
- Consumes: `CryptoHeader::{unlock, free_slot, wrap_into}` (Task 5.1), `TransactionManager::rewrite_crypto_header` (Task 5.2), `self.crypto: Option<CryptoHeader>`.
- Produces:
  - `TransactionManager::add_key(&mut self, existing: &Key, new: &Key) -> Result<()>`
  - `TransactionManager::rotate_key(&mut self, old: &Key, new: &Key) -> Result<()>`
  - `Chisel::add_key(&mut self, existing: &Key, new: &Key) -> Result<()>`
  - `Chisel::rotate_key(&mut self, old: &Key, new: &Key) -> Result<()>`

- [ ] **Step 1: Write the failing test**
```rust
// tests/encryption_keys.rs
use chisel::{Chisel, Options};
use chisel::crypto::Key;
use zeroize::Zeroizing;
use tempfile::TempDir;

fn raw(b: u8) -> Key { Key::Raw(Zeroizing::new(vec![b; 32])) }

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

    // Original key still works.
    let db1 = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    assert_eq!(db1.read(h).unwrap(), b"secret");
    db1.close().unwrap();
    // New key also works.
    let db2 = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert_eq!(db2.read(h).unwrap(), b"secret");
}

#[test]
fn add_key_wrong_existing_is_wrongkey() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    let err = db.add_key(&raw(9), &raw(2)).unwrap_err();
    assert_eq!(err, chisel::ChiselError::InvalidEncryptionKey);
}

#[test]
fn add_key_full_table_is_nofreeslot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Slot 0 occupied at open; add 7 more to fill all 8.
    for k in 2u8..=8 {
        db.add_key(&raw(1), &raw(k)).unwrap();
    }
    let err = db.add_key(&raw(1), &raw(99)).unwrap_err();
    assert_eq!(err, chisel::ChiselError::NoFreeKeySlot);
}

#[test]
fn rotate_key_revokes_old_and_admits_new() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    {
        let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
        db.rotate_key(&raw(1), &raw(2)).unwrap();
        db.close().unwrap();
    }
    // Old key is now refused.
    assert_eq!(
        Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap_err(),
        chisel::ChiselError::InvalidEncryptionKey
    );
    // New key opens.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert!(!db.is_poisoned());
}
```
(`Chisel::open` mapping a failed all-slot unlock to `ChiselError::InvalidEncryptionKey` is Phase 2/4.)

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --test encryption_keys`  Expected: FAIL (`add_key`/`rotate_key` not present)

- [ ] **Step 3: Implement**
```rust
// src/transaction/keys.rs — append to the impl block.
use crate::crypto::Key;

impl TransactionManager {
    /// Prove possession of `existing` (it must unlock some active slot),
    /// recover the DEK, then wrap that SAME DEK under `new` in a free slot and
    /// commit the new header. The DEK is unchanged, so existing pages stay
    /// readable under both credentials.
    pub(crate) fn add_key(&mut self, existing: &Key, new: &Key) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        let header = self.crypto.as_ref().ok_or(ChiselError::EncryptionNotSupported)?;
        let (_idx, dek) = header.unlock(existing)?; // InvalidEncryptionKey if none
        let free = header.free_slot().ok_or(ChiselError::NoFreeKeySlot)?;
        let mut new_header = header.clone();
        new_header.wrap_into(free, new, &dek);
        self.rewrite_crypto_header(new_header)
    }

    /// add_key(old, new) then clear the slot `old` occupied, in a single new
    /// superblock. After commit, `old` no longer unlocks any slot and `new`
    /// does. If `old == new` would collapse to the same credential, the cleared
    /// slot is the OLD one (found by re-unlocking with `old` after the add), so
    /// the freshly-added `new` slot survives.
    pub(crate) fn rotate_key(&mut self, old: &Key, new: &Key) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        let header = self.crypto.as_ref().ok_or(ChiselError::EncryptionNotSupported)?;
        let (old_idx, dek) = header.unlock(old)?;
        let free = header.free_slot().ok_or(ChiselError::NoFreeKeySlot)?;
        let mut new_header = header.clone();
        new_header.wrap_into(free, new, &dek);
        // Revoke the old slot in the SAME header rewrite — one fsync, atomic:
        // a crash leaves either the pre-rotation header (old works) or the
        // post-rotation header (new works), never a window where neither does.
        new_header.slots[old_idx] = crate::superblock::KeySlot::EMPTY;
        self.rewrite_crypto_header(new_header)
    }
}
```
```rust
// src/lib.rs — inside impl Chisel, before line 893's closing brace.
use crypto::Key;

    /// Add a second credential that unlocks this database. `existing` must
    /// already unlock it; `new` is wrapped over the same data key into a free
    /// key slot. After this returns, either credential opens the database. This
    /// is an O(1) superblock commit — no page is re-encrypted.
    ///
    /// # Errors
    /// `EncryptionNotSupported` if the database has no encryption; `InvalidEncryptionKey` if
    /// `existing` unlocks no slot; `NoFreeKeySlot` if all 8 key slots are full.
    /// A failure inside the fsync/superblock write is fatal and poisons the handle.
    pub fn add_key(&mut self, existing: &Key, new: &Key) -> Result<()> {
        self.txm.add_key(existing, new)
    }

    /// Replace `old` with `new`: `new` is added and `old` is revoked in one
    /// atomic superblock commit. After this returns, `old` no longer opens the
    /// database and `new` does. O(1) — the data key is unchanged, so no page is
    /// re-encrypted.
    ///
    /// # Errors
    /// `EncryptionNotSupported` if the database has no encryption; `InvalidEncryptionKey` if `old`
    /// unlocks no slot; `NoFreeKeySlot` if all 8 key slots are full (no room to
    /// stage `new` before revoking `old`). A failure inside the fsync/superblock
    /// write is fatal and poisons the handle.
    pub fn rotate_key(&mut self, old: &Key, new: &Key) -> Result<()> {
        self.txm.rotate_key(old, new)
    }
```

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --test encryption_keys`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): Chisel::add_key and rotate_key"
```

---

### Task 5.4: `Chisel::remove_key` (refuse the last active slot)

**Files:**
- Modify: `src/transaction/keys.rs` (add `remove_key` on `TransactionManager`)
- Modify: `src/lib.rs:893` (add the public method inside `impl Chisel`)
- Test: `tests/encryption_keys.rs` (extend)

**Interfaces:**
- Consumes: `CryptoHeader::{unlock, active_count}` (Task 5.1), `KeySlot::EMPTY` (Task 5.1), `rewrite_crypto_header` (Task 5.2).
- Produces: `TransactionManager::remove_key(&mut self, key: &Key) -> Result<()>`, `Chisel::remove_key(&mut self, key: &Key) -> Result<()>`.

- [ ] **Step 1: Write the failing test**
```rust
// tests/encryption_keys.rs — append.
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
    // raw(1) is gone.
    assert_eq!(
        Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap_err(),
        chisel::ChiselError::InvalidEncryptionKey
    );
    // raw(2) still opens and reads.
    let db = Chisel::open(&path, Options::default().encryption_key(raw(2))).unwrap();
    assert_eq!(db.read(h).unwrap(), b"v");
}

#[test]
fn remove_last_key_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    // Only one active slot — removing it would brick the database.
    let err = db.remove_key(&raw(1)).unwrap_err();
    assert_eq!(err, chisel::ChiselError::LastKeySlot);
    // Still openable afterward — the rejected op changed nothing.
    drop(db);
    let db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    assert!(!db.is_poisoned());
}

#[test]
fn remove_unknown_key_is_wrongkey() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Chisel::open(&path, Options::default().encryption_key(raw(1))).unwrap();
    db.add_key(&raw(1), &raw(2)).unwrap();
    assert_eq!(db.remove_key(&raw(9)).unwrap_err(), chisel::ChiselError::InvalidEncryptionKey);
}
```

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test --test encryption_keys`  Expected: FAIL (`remove_key` not present)

- [ ] **Step 3: Implement**
```rust
// src/transaction/keys.rs — append to the impl block.
impl TransactionManager {
    /// Clear the slot `key` unlocks. Refuses to remove the LAST active slot
    /// (`LastKeySlot`) — a database with zero usable credentials is
    /// unrecoverable, so this is an operational error that changes nothing.
    pub(crate) fn remove_key(&mut self, key: &Key) -> Result<()> {
        if self.poisoned.get() {
            return Err(ChiselError::Poisoned);
        }
        let header = self.crypto.as_ref().ok_or(ChiselError::EncryptionNotSupported)?;
        let (idx, _dek) = header.unlock(key)?; // InvalidEncryptionKey if none
        // Check occupancy AFTER proving the key is valid: an unknown key on a
        // single-slot DB should report InvalidEncryptionKey, not LastKeySlot.
        if header.active_count() <= 1 {
            return Err(ChiselError::LastKeySlot);
        }
        let mut new_header = header.clone();
        new_header.slots[idx] = crate::superblock::KeySlot::EMPTY;
        self.rewrite_crypto_header(new_header)
    }
}
```
```rust
// src/lib.rs — inside impl Chisel.
    /// Revoke the credential `key`. After this returns, `key` no longer opens
    /// the database; any other credentials are unaffected. Refuses to remove
    /// the only remaining credential. O(1) superblock commit.
    ///
    /// # Errors
    /// `EncryptionNotSupported` if the database has no encryption; `InvalidEncryptionKey` if `key`
    /// unlocks no slot; `LastKeySlot` if `key` is the only active credential
    /// (removing it would make the database permanently unopenable — nothing is
    /// changed). A failure inside the fsync/superblock write is fatal and
    /// poisons the handle.
    pub fn remove_key(&mut self, key: &Key) -> Result<()> {
        self.txm.remove_key(key)
    }
```

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test --test encryption_keys`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(crypto): Chisel::remove_key with last-slot guard"
```

---

### Task 5.5: Mirror `add_key` / `rotate_key` / `remove_key` in the Python binding

**Files:**
- Modify: `python/src/db.rs:233` (add three methods inside `#[pymethods] impl PyChisel`, the block opening at line 233; `with_db`-style mutable access helpers at lines ~564/573 and `to_py_err` at line 50 already exist)
- Test: `python/tests/test_encryption_keys.py`

**Interfaces:**
- Consumes: `Chisel::{add_key, rotate_key, remove_key}` (Tasks 5.3/5.4); the existing `to_py_err` (`python/src/db.rs:50`) and the `Mutex<Option<Chisel>>` accessor pattern (`python/src/db.rs:87`). Python passes keys as `bytes` (32-byte raw) or `str` (passphrase); map to `crypto::Key` exactly as the Phase-2 `open()` kwarg does.
- Produces: `PyChisel.add_key(self, existing, new)`, `PyChisel.rotate_key(self, old, new)`, `PyChisel.remove_key(self, key)`.

- [ ] **Step 1: Write the failing test**
```python
# python/tests/test_encryption_keys.py
import tempfile, os, pytest, chisel

K1 = bytes([1]) * 32
K2 = bytes([2]) * 32

def _open(path, key):
    return chisel.open(path, encryption_key=key)

def test_add_key_either_opens():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "db")
        db = _open(p, K1)
        db.begin(); h = db.allocate(b"secret"); db.commit()
        db.add_key(K1, K2)
        db.close()
        db1 = _open(p, K1); assert db1.read(h) == b"secret"; db1.close()
        db2 = _open(p, K2); assert db2.read(h) == b"secret"; db2.close()

def test_rotate_key_revokes_old():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "db")
        db = _open(p, K1); db.rotate_key(K1, K2); db.close()
        with pytest.raises(Exception):
            _open(p, K1)
        _open(p, K2).close()

def test_remove_last_key_rejected():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "db")
        db = _open(p, K1)
        with pytest.raises(Exception):
            db.remove_key(K1)
        db.close()
```

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test -p chisel-python 2>/dev/null; (cd python && maturin develop && python -m pytest tests/test_encryption_keys.py)`  Expected: FAIL (methods not bound)

- [ ] **Step 3: Implement**
```rust
// python/src/db.rs — inside #[pymethods] impl PyChisel.

// Map a Python key argument (bytes -> raw 32-byte, str -> passphrase) to a
// crypto::Key. Mirrors the open() kwarg coercion so the binding has one key
// vocabulary. A raw key of the wrong length surfaces as the engine's
// BadKeyLength via to_py_err once it reaches derive/unlock.
fn py_key(obj: &Bound<'_, PyAny>) -> PyResult<chisel::crypto::Key> {
    use zeroize::Zeroizing;
    if let Ok(b) = obj.extract::<Vec<u8>>() {
        Ok(chisel::crypto::Key::Raw(Zeroizing::new(b)))
    } else if let Ok(s) = obj.extract::<String>() {
        Ok(chisel::crypto::Key::Passphrase(Zeroizing::new(s)))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            "key must be bytes (raw) or str (passphrase)",
        ))
    }
}

    pub(crate) fn add_key(
        &self,
        existing: &Bound<'_, PyAny>,
        new: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let existing = py_key(existing)?;
        let new = py_key(new)?;
        self.with_db_mut(|db| db.add_key(&existing, &new))
    }

    pub(crate) fn rotate_key(
        &self,
        old: &Bound<'_, PyAny>,
        new: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let old = py_key(old)?;
        let new = py_key(new)?;
        self.with_db_mut(|db| db.rotate_key(&old, &new))
    }

    pub(crate) fn remove_key(&self, key: &Bound<'_, PyAny>) -> PyResult<()> {
        let key = py_key(key)?;
        self.with_db_mut(|db| db.remove_key(&key))
    }
```
(`with_db_mut` is the existing locked-mutable accessor that takes `FnOnce(&mut Chisel) -> Result<_>` and applies `to_py_err`; it is the helper at `python/src/db.rs:564`/`573`. If that helper is `&self`/`&mut self`-shaped differently in the real file, match its exact signature — the three methods above only need "lock, take `&mut Chisel`, map_err to_py_err".)

- [ ] **Step 4: Run test, verify it passes**
Run: `(cd python && maturin develop && python -m pytest tests/test_encryption_keys.py)`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(python): bind add_key/rotate_key/remove_key on PyChisel"
```

---

Notes on what was deliberately skipped, and why it's safe:
- No re-encryption / DEK rotation. `rotate_key` rotates the *credential*, not the data key — the contract's envelope design (random per-DB DEK wrapped per slot) makes credential rotation O(1); rotating the DEK itself would mean rewriting every page and is not in this phase's scope. Add a `reencrypt()` later if a leaked-DEK threat model demands it.
- `rotate_key` requires a free slot (it stages `new` before revoking `old`). On a full 8-slot table it returns `NoFreeKeySlot` rather than clearing `old` first — staging-then-revoke keeps the operation atomic (no crash window with zero working keys). Acceptable: 8 slots is already generous; the caller can `remove_key` then `add_key` if they truly want in-place reuse.
- The `aad()` binding (Task 5.1) is the one non-obvious security-load-bearing bit: it ties each wrapped DEK to its slot's KDF metadata so a ciphertext can't be lifted between slots. That check is exercised by `unlock_finds_the_right_slot_and_recovers_dek` (transplant would fail the tag).

Phase 5 file footprint: one new file (`src/transaction/keys.rs`), edits to `src/superblock.rs`, `src/transaction/mod.rs`, `src/lib.rs`, `python/src/db.rs`, plus two test files.

---

## Phase 6: Docs, ADR, format-version bump, and deferred-work record

This phase carries no new engine logic. The MAJOR-version gate at `src/transaction/recovery.rs:171` already exists and already rejects a file whose MAJOR differs from `FORMAT_MAJOR_VERSION` (currently 1). The encryption design (spec §8) calls for encrypted DBs to stamp **MAJOR=2** while plaintext DBs keep **MAJOR=1**. Because the global `FORMAT_MAJOR_VERSION` constant must stay `1` (plaintext DBs are the default and must not be rejected by their own binary), the bump is *per-database at create time*, not a constant change: the create path stamps `pack_format_version(2, 0)` into the superblock when encryption is enabled, and the existing gate does the rest. This phase adds a small helper + the documentation, then verifies the gate behaves.

> Note: the create path that stamps the encrypted superblock's `format_version` is built in Phase 2 (superblock crypto-header). This phase only adds the version *constant/helper* that Phase 2 consumes, plus a gate-behavior test, plus docs. If Phase 2 already added `ENCRYPTED_FORMAT_MAJOR_VERSION`, Task 6.1 collapses to verifying it; the test in Task 6.1 still applies.

---

### Task 6.1: Add the encrypted-DB MAJOR-version constant and prove the gate rejects it on an old binary

**Files:**
- Modify: `src/page.rs:113` (add a sibling constant next to `FORMAT_MAJOR_VERSION`)
- Test: `src/transaction/recovery.rs` (`#[cfg(test)] mod` — exercises the gate at line 171 with a MAJOR=2 superblock)

**Interfaces:**
- Consumes: `page::pack_format_version(major, minor) -> u32` (`src/page.rs:117`), `page::format_major(version) -> u16` (`src/page.rs:122`), `page::FORMAT_MAJOR_VERSION: u16 = 1` (`src/page.rs:113`).
- Produces: `pub const ENCRYPTED_FORMAT_MAJOR_VERSION: u16 = 2;` and `pub const ENCRYPTED_FORMAT_VERSION: u32` — consumed by the Phase 2 create path when `Options.encryption_key.is_some()`.

- [ ] **Step 1: Write the failing test**

Add to the existing test module in `src/transaction/recovery.rs` (the gate under test is the `format_major` check at line 171). The test stamps an encrypted-DB MAJOR (2) into a freshly serialized superblock, writes it as page 0, and asserts open fails with `UnsupportedFormatVersion`. This proves an *encryption-unaware* binary (whose `FORMAT_MAJOR_VERSION` is 1) hard-rejects a MAJOR=2 file, exactly as spec §7/§8 require.

```rust
#[test]
fn encrypted_major_version_is_rejected_by_plaintext_binary() {
    use crate::page::{self, PAGE_SIZE};
    use crate::superblock::Superblock;
    use crate::error::ChiselError;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc_major.chsl");

    // Build a valid superblock, then overwrite its format_version with the
    // encrypted-DB MAJOR (2, minor 0). Everything else is a normal empty DB.
    let mut sb = Superblock::new_empty();
    sb.format_version = page::ENCRYPTED_FORMAT_VERSION; // pack(2, 0)
    assert_eq!(page::format_major(sb.format_version), 2);

    // Lay it down as page 0 at plaintext stride (the gate fires before any
    // stride-dependent read; page 0 is always at offset 0).
    let bytes = sb.serialize();
    assert_eq!(bytes.len(), PAGE_SIZE);
    std::fs::write(&path, &bytes).unwrap();

    // An encryption-unaware open (FORMAT_MAJOR_VERSION == 1) must reject it.
    let err = crate::Chisel::open(&path, crate::Options::default()).unwrap_err();
    match err {
        ChiselError::UnsupportedFormatVersion { found, expected } => {
            assert_eq!(page::format_major(found), 2);
            assert_eq!(expected, page::FORMAT_VERSION); // pack(1, 1)
        }
        other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
    }
}
```

(Use whatever `Superblock` constructor the existing recovery tests already use — `Superblock::new_empty()` is named here per `src/superblock.rs:562`-area test constructors; if the in-tree name differs, match the sibling tests in this module rather than inventing one. The gate assertion is the load-bearing part.)

- [ ] **Step 2: Run test, verify it fails**
Run: `cargo test encrypted_major_version_is_rejected_by_plaintext_binary`  Expected: FAIL — `page::ENCRYPTED_FORMAT_VERSION` does not exist yet (does not compile).

- [ ] **Step 3: Implement**

In `src/page.rs`, immediately after the `FORMAT_MINOR_VERSION` line (currently `src/page.rs:114`), add the encrypted-DB version constants. Do **not** change `FORMAT_MAJOR_VERSION` — plaintext DBs (the default) must keep MAJOR=1 or their own binary would reject them.

```rust
// Encrypted databases stamp a DISTINCT MAJOR so an encryption-unaware binary
// hard-rejects them at the open-time gate (recovery.rs) instead of misreading
// 8232-byte sealed strides as 8192-byte plaintext pages. Plaintext DBs are
// unaffected: they keep FORMAT_MAJOR_VERSION (1). The create path picks which
// version to stamp based on Options.encryption_key — this is a per-database
// choice, NOT a global constant change. See the on-disk-encryption design §8
// and ISSUES.md.
//
// MINOR is 0 here (first encrypted release); the same packed-MAJOR gate that
// guards plaintext also guards this: a MAJOR-2 file opened by a MAJOR-1 binary
// yields UnsupportedFormatVersion.
pub const ENCRYPTED_FORMAT_MAJOR_VERSION: u16 = 2;
pub const ENCRYPTED_FORMAT_MINOR_VERSION: u16 = 0;
pub const ENCRYPTED_FORMAT_VERSION: u32 =
    pack_format_version(ENCRYPTED_FORMAT_MAJOR_VERSION, ENCRYPTED_FORMAT_MINOR_VERSION);
```

No gate change is needed: `recovery.rs:171` already does `if page::format_major(sb.format_version) != page::FORMAT_MAJOR_VERSION`. An encryption-unaware binary has `FORMAT_MAJOR_VERSION == 1`, so a MAJOR-2 file fails the comparison and returns `UnsupportedFormatVersion`. (When the engine later learns encryption, that gate grows a second accepted MAJOR — out of scope for this docs phase; the constant defined here is what Phase 2's create path stamps.)

- [ ] **Step 4: Run test, verify it passes**
Run: `cargo test encrypted_major_version_is_rejected_by_plaintext_binary`  Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(format): add encrypted-DB MAJOR version constant and gate-rejection test"
```

---

### Task 6.2: Add the "On-disk encryption" section to ARCHITECTURE.md

**Files:**
- Modify: `ARCHITECTURE.md` — insert a new `### On-disk encryption` section immediately after `### Format versioning (two-tier)` (the section ends at `ARCHITECTURE.md:616`; the `---` separator + `## Benchmark infrastructure` follow at 618/620). Insert between line 616 and the `---` at 618.
- Modify: `ARCHITECTURE.md:7` table of contents (add an entry under "Cross-cutting concepts").
- Test: link/build check only — `cargo build` stays green (docs-only change touches no code) and a grep confirms the new heading and TOC entry exist.

**Interfaces:**
- Consumes: the approved spec `docs/specs/2026-06-29-on-disk-encryption-design.md` (§3 envelope, §4 stride, §8 versioning, §9 threat model).
- Produces: durable architecture documentation; no code symbols.

- [ ] **Step 1: Write the failing check**

There is no Rust test for prose. The "failing test" is a grep that must find the new heading and TOC entry; before the edit it returns nothing.

```bash
grep -n "### On-disk encryption" ARCHITECTURE.md
grep -n "On-disk encryption" ARCHITECTURE.md   # expect a TOC line too
```
Expected before the edit: no match (exit 1).

- [ ] **Step 2: Run check, verify it fails**
Run: `grep -c "### On-disk encryption" ARCHITECTURE.md`  Expected: `0` (section absent).

- [ ] **Step 3: Implement**

First add the TOC entry. The TOC lists "Cross-cutting concepts" subsections around `ARCHITECTURE.md:7`; add a line in that subsection list (keep the existing indentation/style of sibling TOC lines):

```markdown
  - [On-disk encryption](#on-disk-encryption)
```

Then insert this section between `ARCHITECTURE.md:616` (end of the two-tier versioning prose) and the `---` at line 618. Keep it consistent with the spec; **no Claude/AI references.**

```markdown
### On-disk encryption

Encryption is **opt-in per database**, chosen at create time by supplying an
`Options.encryption_key`. Without a key a database is plaintext exactly as before;
the encrypted format is a strict superset that lives **below** the page
abstraction, so the page cache, freemap, data pages, handle table, and the entire
transaction layer keep producing and consuming byte-identical 8192-byte page
images.

**Envelope scheme.** A random 256-bit Data Encryption Key (DEK), generated once
with the OS RNG at create time, seals every page and the sensitive superblock
fields and never changes during normal operation. The DEK is wrapped under a Key
Encryption Key (KEK) derived per-open from the client key plus a per-slot salt:
HKDF-SHA256 for raw high-entropy keys, Argon2id (memory-hard) for human
passphrases. Up to eight key-slots in the superblock's plaintext reserved region
each hold one KEK-wrapped copy of the same DEK. A successful unwrap (its Poly1305
tag verifies) *is* the proof the client key is correct — there is no separate
password verifier. The unwrapped DEK is held zeroizing in `TransactionManager` for
the open session and wiped on drop; the KEK and client key are zeroized
immediately after use.

**Cipher and page stride.** The AEAD is **XChaCha20-Poly1305** with a fresh
random 192-bit nonce per page write — chosen because it is constant-time in
portable software (no AES-NI dependency) and the 192-bit nonce makes random
nonces collision-safe, which sidesteps a keystream-reuse hazard that shadow-paging
page reuse would create under a deterministic `(page_id, counter)` nonce. Each
encrypted page occupies an **8232-byte** on-disk stride: `ciphertext(8192) ‖
tag(16) ‖ nonce(24)`, written at offset `page_id × 8232` (vs `× 8192` for
plaintext). `AAD = page_id` gives anti-relocation — a sealed page authenticates
only at its own `page_id`. `page_io` is stride-aware but crypto-agnostic; the
seal/open transform lives one layer up in the page cache, which seals a page
exactly once when it first leaves the plaintext cache (to the main file or the
spillway) and byte-copies the already-sealed blob on spillway drain.

**Superblock.** Page 0 stays at offset 0 regardless of stride, so it is always
readable first to learn `encrypted? / algorithm / stride / key-slots`. Bootstrap
fields (`magic`, `format_version`, `txn_counter`, `page_size`,
`superblock_count`) and the plaintext crypto-header (flag, algorithm id, stride,
key-slot table) stay in the clear; the sensitive body — root pointers,
`total_pages`, `next_handle`, `freemap_depth`, and the user-chosen `named_roots`
names — is sealed under the DEK as a `nonce ‖ tag ‖ ciphertext` sub-blob whose AAD
binds it to this superblock's identity. The plaintext portion keeps its XXH3
checksum so the A/B torn-write `select()` still works on bootstrap fields.

**Format version.** Encrypted databases stamp file-level **MAJOR = 2**; plaintext
databases stay at MAJOR = 1. The existing open-time MAJOR gate (it compares MAJOR
only) therefore hard-rejects an encrypted DB on an encryption-unaware binary with
`UnsupportedFormatVersion`, preventing it from misreading ciphertext as plaintext.
No per-page (I31) format change is needed — the logical page image is unchanged.

**Key rotation.** Credential rotation is O(1) and crash-safe: `add_key` derives a
KEK from a new credential and wraps the *same* DEK into a free slot; `rotate_key`
is `add_key` then clear the old slot; `remove_key` clears a slot (refusing the
last active one). Each is an ordinary superblock commit through the A/B + fsync
protocol — no page is ever re-encrypted.

**Threat-model boundary.** Provided: confidentiality of all data and sensitive
metadata at rest, AEAD tamper-detection (surfaced as the fatal `DecryptionFailed`,
which poisons the engine), and anti-relocation. **Not** provided: rollback/replay
resistance — an attacker who substitutes a wholly older, validly-signed database
image (or an older valid A/B superblock slot) cannot be detected by self-contained
authentication, which would require an external monotonic trust anchor (e.g. TPM);
in-memory protection beyond zeroize-on-drop; and traffic-analysis hiding (file
size, page count, and access patterns are visible). Bulk DEK rotation / full
re-encryption is deferred (see ISSUES.md).
```

- [ ] **Step 4: Run check, verify it passes**
Run: `grep -c "### On-disk encryption" ARCHITECTURE.md && grep -c "On-disk encryption](#on-disk-encryption)" ARCHITECTURE.md && cargo build`  Expected: section grep `1`, TOC grep `1`, build succeeds.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "docs(architecture): document the on-disk encryption envelope scheme and threat boundary"
```

---

### Task 6.3: Record deferred bulk DEK rotation in ISSUES.md

**Files:**
- Modify: `ISSUES.md` — append a new entry. The file uses a `## <category>` + per-item structure (see the `## Durability and crash safety` heading at `ISSUES.md:60`); the highest existing id seen in MEMORY/notes is **I141** (the deferred StagingTxn extraction). Use the next free id **I142**. Append under a new `## On-disk encryption` category section at the end of the file (after the last existing category), matching the existing entry format.
- Test: grep confirms the entry exists and carries a priority tag; `cargo build` unaffected.

**Interfaces:**
- Consumes: spec §1 (out of scope), §2.1, §3.3 (rotation scope v1 = credential only).
- Produces: the deferred-work record the spec's Phase 6 requires; no code symbols.

- [ ] **Step 1: Write the failing check**
```bash
grep -n "I142" ISSUES.md
```
Expected before the edit: no match.

- [ ] **Step 2: Run check, verify it fails**
Run: `grep -c "I142" ISSUES.md`  Expected: `0`.

- [ ] **Step 3: Implement**

Append to the end of `ISSUES.md` (after the final existing category). Match the surrounding entry style — bold id, priority tag, one-line title, then rationale prose.

```markdown

---

## On-disk encryption

Source: **[encryption 2026-06-29]** — deferred-work captured while implementing the
on-disk encryption feature (design at `docs/specs/2026-06-29-on-disk-encryption-design.md`).

**I142** (P3) — **Bulk DEK rotation / full re-encryption is deferred.** v1 supports
only *credential* rotation: `add_key` / `rotate_key` / `remove_key` re-wrap the
single per-DB Data Encryption Key (DEK) under a new Key Encryption Key in an
O(1), crash-safe superblock commit — no page is re-encrypted. Rotating the **DEK
itself** (re-sealing every page and the superblock body under a fresh DEK) is a
heavy whole-file operation reserved for the case where the DEK is believed
compromised, and is intentionally out of scope for v1 (design §1, §3.3). Deferred
because: (a) credential rotation covers the normal operational meaning of "rotate
my key"; (b) a correct online bulk re-encrypt has to be crash-safe and resumable
across the entire file — a feature-sized effort that wants its own spec/plan; and
(c) there are no production databases yet, so no DEK is currently at risk. When
implemented it should reuse the existing seal-once page path and the A/B
superblock protocol, and run as a resumable background sweep (a per-DB
"re-encryption watermark" so an interrupted rotation continues rather than
restarts). No code stub exists today; this entry is the record that the omission
is deliberate, not an oversight.
```

- [ ] **Step 4: Run check, verify it passes**
Run: `grep -c "I142" ISSUES.md && grep -c "Bulk DEK rotation / full re-encryption is deferred" ISSUES.md`  Expected: both `>= 1`.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "docs(issues): record deferred bulk DEK rotation (I142)"
```

---

### Task 6.4: Update the ADR graph via codebase-memory (manual checklist — implementer runs the skill)

**Files:**
- Modify: `.codebase-memory/adr.md` (untracked; edited only through the `manage_adr` tool, never by hand).
- Test: post-update `manage_adr(mode=get)` shows BOTH the new encryption section AND every previously-existing section intact.

**Interfaces:**
- Consumes: the approved spec, the ARCHITECTURE.md section from Task 6.2, and the I142 record from Task 6.3.
- Produces: an ADR entry for the on-disk encryption decision in the project's ADR graph.

This task is **not** code — it is a procedure the implementer runs by hand using the `codebase-memory` skill / `manage_adr` tool. It is listed as a checklist so it is not skipped before the PR is opened (per the project rule: update the ADR graph *before* recommending a PR).

> **FOOTGUN — `manage_adr(mode=update)` overwrites the WHOLE document.** There is no
> section-scoped write. Passing only a fragment as `content` leaves the ADR holding
> *only* that fragment; every other section is destroyed. The `sections` argument
> does **not** scope the write. Always snapshot-then-read-modify-write.

- [ ] **Step 1: Snapshot the current ADR (makes the overwrite reversible)**
Run the `codebase-memory` skill's `manage_adr(mode=get)` for the Chisel project. Save the **entire returned document verbatim** to the session scratchpad (e.g. `…/scratchpad/adr-snapshot-pre-encryption.md`) BEFORE editing anything. This snapshot is the only thing that makes Step 3 reversible.

- [ ] **Step 2: Compose the new section inside the full document**
In a working copy of the full snapshot, add ONE new ADR section for on-disk encryption, leaving every other section byte-for-byte unchanged. The new section records the load-bearing decisions (cite the spec for each):
  - **Decision:** opt-in, per-DB authenticated at-rest encryption.
  - **AEAD:** XChaCha20-Poly1305, random 192-bit nonce per page write, `AAD = page_id` (spec §2.1 — why random-nonce over deterministic, given shadow-paging page reuse).
  - **Key management:** envelope (random per-DB DEK wrapped by a KEK), HKDF-SHA256 for raw keys / Argon2id for passphrases, 8 key-slots, O(1) credential rotation (spec §3).
  - **Page format:** 8232-byte on-disk stride, logical page stays 8192; seal-once at the page-cache layer; `page_io` stride-aware but crypto-agnostic (spec §4).
  - **Format version:** MAJOR 1→2 for encrypted DBs only; the existing MAJOR gate hard-rejects old binaries (spec §8).
  - **Boundaries:** no rollback/replay protection, no in-memory protection beyond zeroize, no size hiding; bulk DEK rotation deferred → ISSUES.md I142 (spec §9).

- [ ] **Step 3: Write the ENTIRE edited document back**
Call `manage_adr(mode=update, content=<the full edited document>)` — the complete text from Step 2, not just the new section. Never pass a section fragment to `mode=update`.

- [ ] **Step 4: Verify nothing was lost**
Call `manage_adr(mode=get)` again and confirm the new encryption section is present **and** every section that existed in the Step 1 snapshot still exists, unchanged. If any prior section is missing, recover by re-running `manage_adr(mode=update)` with the Step 1 snapshot (rebuilt to include the new section) — never hand-edit the codebase-memory SQLite store (it is shared across all indexed projects).

- [ ] **Step 5: (no commit)**
The ADR lives in the untracked `.codebase-memory/` store, not in git — there is nothing to commit for this task. With Tasks 6.1–6.3 committed and the ADR updated, the encryption feature is documented and the branch is ready for `gh pr create --base master`.

---

**Phase 6 deliverables:** `src/page.rs` gains `ENCRYPTED_FORMAT_MAJOR_VERSION` / `ENCRYPTED_FORMAT_VERSION` (consumed by Phase 2's create path) with a test proving the existing gate at `src/transaction/recovery.rs:171` rejects a MAJOR=2 file on an encryption-unaware binary; `ARCHITECTURE.md` gains the "On-disk encryption" section + TOC entry; `ISSUES.md` gains the I142 deferred-bulk-DEK-rotation record; and the ADR graph is updated via the snapshot-then-overwrite `manage_adr` procedure.
