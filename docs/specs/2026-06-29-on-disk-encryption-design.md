# On-Disk Encryption — Design

Date: 2026-06-29
Status: Approved (design), pending implementation plan
Topic: Client-supplied at-rest encryption for the Chisel storage engine

---

## 1. Goal and scope

Add **authenticated on-disk encryption** to Chisel. The client program supplies an
encryption key (raw key bytes or a passphrase) when opening the database. With a key,
every byte Chisel writes to durable storage is encrypted and integrity-protected;
without a key, the database is plaintext exactly as today (encryption is opt-in per
database, chosen at create time).

In scope:

- One AEAD algorithm balancing speed and security: **XChaCha20-Poly1305**.
- **Credential (key) rotation** that is cheap (O(1), no bulk re-encryption).
- Confidentiality and cryptographic tamper-detection for: main-file pages
  (data / index / freemap / handle-table / overflow / membership), the **spillway**
  overflow file, and the **sensitive superblock fields** (including user-chosen
  `named_roots` names).

Out of scope (documented as known boundaries, see §9):

- Full **Data Encryption Key (DEK) rotation** / bulk re-encryption (deferred; a heavy
  whole-file operation reserved for "the DEK itself is compromised").
- **Rollback / replay** resistance against an attacker who can substitute a wholly
  older, validly-signed database image (needs an external trust anchor; impossible for
  a self-contained file).

---

## 2. Decisions (and why)

| Decision | Choice | Rationale |
|---|---|---|
| Integrity model | **Authenticated (AEAD)**, not length-preserving (XTS) | Cryptographic tamper-detection, not just confidentiality. Cheap to adopt now: no production databases exist, so the one-time format change is free. Upgrades integrity from the non-cryptographic XXH3 (forgeable) to an unforgeable Poly1305 tag. |
| Cipher | **XChaCha20-Poly1305** (extended 192-bit nonce) | Constant-time in portable software (no AES-NI dependency) — correct default for an embedded library that runs on unknown hardware. The 192-bit nonce lets us use **random** nonces safely (see §2.1), eliminating a crash-reuse hazard that the 96-bit variant would introduce. Crypto throughput is far below fsync latency, so AES-GCM's hardware edge is irrelevant here. |
| Key management | **Envelope**: random per-DB **DEK** wrapped by a **KEK** derived from the client key | Makes credential rotation O(1) (re-wrap the DEK) instead of O(database size). |
| Key derivation | **HKDF-SHA256** for raw keys; **Argon2id** for passphrases | Right tool per input entropy: HKDF is fast and correct for high-entropy keys; Argon2id is memory-hard to resist brute-forcing low-entropy passphrases. The chosen KDF is recorded per key-slot. |
| Page format | **Larger on-disk stride (8232 B)**; logical page stays **8192 B** | Smallest blast radius: the page cache, freemap, data pages, handle table, and the entire transaction layer keep producing/consuming byte-identical 8192-byte pages. Only the I/O stride and the seal/open transform change. Cost: 0.49% larger encrypted files. Alternative (shrink logical content to fit a 40-byte trailer inside 8192) was rejected — it would make every geometry constant encryption-dependent, an invasive and bug-prone refactor of the engine's core page math. |
| Rotation scope (v1) | **Credential rotation only** (key-slot re-wrap) | Covers the normal meaning of "rotate my key." Bulk DEK rotation deferred. |

### 2.1 The nonce hazard that drove the cipher choice

A naïve design derives each page's nonce deterministically from `(page_id, counter)`.
That is **unsafe** under Chisel's shadow-paging + freemap page reuse:

- Shadow paging discards a crashed transaction's writes and returns its `page_id`s to
  the freemap. The durable superblock's counters did **not** advance (the commit never
  finished).
- After the crash, Chisel legitimately writes **different plaintext to the same
  `page_id`** while any persisted counter still sits at its pre-crash value.
- Result: identical `(key, nonce)` over different plaintext — catastrophic keystream
  reuse for any Poly1305/GCM construction. AAD does not help; it is the **nonce + key**
  pair that must never repeat.

Using a deterministic nonce safely would require a persisted, crash-monotonic counter
with gap reservation — extra moving parts and a subtle invariant. Instead we use
**XChaCha20-Poly1305 with a fresh random 192-bit nonce per page write**. Collision
probability is negligible (< 2⁻³² well past 2⁸⁰ writes), there is nothing to persist,
and the crash-reuse class of bugs cannot occur. The nonce is stored alongside the
ciphertext (it is not secret).

---

## 3. Cryptographic architecture

### 3.1 Key hierarchy (envelope)

```
client key ──KDF(per-slot salt)──▶  KEK  ──unwraps──▶  DEK  ──seals──▶  pages + superblock body
  raw bytes  → HKDF-SHA256                 (256-bit,           (XChaCha20-Poly1305,
  passphrase → Argon2id                     random,             AAD = page_id)
                                            stable for DB life)
```

- **DEK** (Data Encryption Key): 256-bit, generated once with `OsRng` at create time.
  It seals every page and the sensitive superblock fields. It never changes during
  normal operation; it is held in memory (zeroizing) for the open session only.
- **KEK** (Key Encryption Key): 256-bit, derived per-open from the client key + the
  matching key-slot's salt/params. It only ever wraps/unwraps the DEK.
- A successful unwrap (the wrap's Poly1305 tag verifies) **is** the proof that the
  client key is correct — there is no separate password verifier to leak.

### 3.2 Key-slot table

Eight fixed slots live in the superblock's plaintext reserved region (§5). Each slot:

| Field | Size | Notes |
|---|---|---|
| `state` | 1 | 0 = empty, 1 = active |
| `kdf_id` | 1 | 1 = HKDF-SHA256, 2 = Argon2id |
| (reserved) | 2 | alignment / future |
| `argon2_m_cost` | 4 | KiB; 0 for HKDF |
| `argon2_t_cost` | 4 | iterations; 0 for HKDF |
| `argon2_p_cost` | 4 | lanes; 0 for HKDF |
| `salt` | 16 | per-slot, random |
| `wrap_nonce` | 24 | XChaCha nonce for the DEK wrap (random) |
| `wrapped_dek` | 32 | DEK ciphertext |
| `wrap_tag` | 16 | Poly1305 tag over the wrap |
| (padding) | — | pad slot to a fixed 128 bytes |

8 slots × 128 B = 1024 B, comfortably inside the ~7860 free reserved bytes.
The wrap's AAD binds the slot metadata (`kdf_id`, `salt`, Argon2 params) so an
attacker cannot tamper a slot's parameters to force mis-derivation.

### 3.3 Rotation operations (all are ordinary superblock commits)

- `add_key(existing_key, new_key)`: derive a KEK from `new_key`, wrap the **same** DEK
  into a free slot. Lets a new credential go live before the old one is retired.
- `rotate_key(old_key, new_key)`: `add_key` then clear the old slot.
- `remove_key(slot)`: clear a slot (refuse to remove the last active slot).

Each writes a new superblock via the existing A/B + fsync protocol, so rotation is
crash-safe and O(1) — no page is re-encrypted.

### 3.4 Dependencies (all pure-Rust, well-vetted)

`chacha20poly1305` (XChaCha20-Poly1305), `argon2`, `hkdf` + `sha2`, `zeroize`,
`rand_core`/`getrandom`. Rolling our own crypto is explicitly forbidden; only vetted
primitives are used.

---

## 4. Page format and the I/O seam

### 4.1 On-disk encrypted page

For an encrypted database, each page occupies a stride of **8232 bytes**:

```
 offset 0                         8192        8208           8232
 ┌──────────────────────────────────┬───────────┬──────────────┐
 │ ciphertext (8192)                 │ tag (16)  │ nonce (24)   │
 └──────────────────────────────────┴───────────┴──────────────┘
   = XChaCha20-Poly1305 seal of the           Poly1305    random per write
     full 8192-byte plaintext page image      tag         (not secret)
```

- The plaintext input is the **entire normal 8192-byte page image** — header, body,
  slots, freemap bitmaps, and the existing XXH3 checksum — produced byte-for-byte as
  today by the layers above. Encryption is a transform strictly below the page
  abstraction.
- `AAD = page_id` gives **anti-relocation**: a sealed page authenticates only at its
  own `page_id`; an attacker cannot move a valid ciphertext to a different slot.
- The on-disk offset is `page_id × 8232` (vs `page_id × 8192` for plaintext DBs).
  Verified that the offset is computed in exactly three places
  (`page_io::read_page` @256, `write_page` @296, `set_page_count` @418) plus the
  spillway's own offset math — a small, contained change.
- The inner XXH3 checksum is now redundant with the AEAD tag but is **kept** so the
  upper layers stay untouched; it serves as a cheap inner sanity check after `open`.

### 4.2 The seam and seal-once invariant

```
WRITE (flush):  cache plaintext[8192]  ──seal(page_id)──▶  on-disk blob[8232]  ──▶ main file @ page_id×8232
EVICT (spill):  cache plaintext[8192]  ──seal(page_id)──▶  on-disk blob[8232]  ──▶ spillway slot
DRAIN:          spillway blob[8232]     ───────copy───────────────────────────────▶ main file @ page_id×8232
READ:           main file blob[8232]   ──open(page_id)──▶  plaintext[8192]      ──▶ cache
```

**Seal-once:** a page is sealed exactly once when it first leaves the plaintext cache —
whether to the main file or the spillway. Draining the spillway to the main file is a
**byte copy** of the already-sealed blob, never a re-seal, because both use the same
`AAD = page_id`. This avoids a crypto round-trip on drain and keeps a single ciphertext
per write.

**Layering:** the `PageCipher` (holds the DEK) lives in the page-cache layer, which
already owns both `page_io` and the spillway and orchestrates flush / spill / drain.
`page_io` becomes **stride-aware but crypto-agnostic**: it reads/writes the on-disk
page unit (8232 encrypted, 8192 plaintext) at the correct offset; the seal/open
decision lives one layer up. Spillway slots widen to hold the 8232-byte sealed blob
(`SLOT_HEADER_SIZE 16 + 8232`); the spillway's `slot_checksum` is computed over the
sealed bytes and continues to protect spillway round-trips.

---

## 5. Superblock handling

The superblock (pages `0..superblock_count`) is **not** whole-page sealed — it must
bootstrap the key material. Page 0 sits at offset 0 regardless of stride, so it is
always readable first to learn `encrypted? / algorithm / stride / key-slots`, after
which the remaining slots are read at the correct stride.

Within the superblock image:

- **Plaintext (bootstrap, unchanged offsets):** `magic` (0..4), `format_version`
  (4..8), `txn_counter` (8..16, needed to select the active slot), `page_size`
  (48..52), `superblock_count` (308..312).
- **New plaintext crypto-header**, placed in the reserved region (from offset 324,
  ~7860 free bytes): an encryption-enabled flag, algorithm id, on-disk stride, and the
  8-slot key-slot table (§3.2).
- **Encrypted under the DEK** (a `nonce ‖ tag ‖ ciphertext` sub-blob also in the
  reserved region): the sensitive body — root pointers (`root_handle_table_page`,
  `root_freemap_page`, `root_membership_index_page`), `total_pages`, `next_handle`,
  `freemap_depth`, and **`named_roots`** (user-chosen UTF-8 names — real user data that
  must not leak). The body's AAD binds it to this superblock's identity
  (`magic`, `format_version`, `txn_counter`, `superblock_count`) to prevent splicing.

The plaintext portion keeps its existing XXH3 checksum so torn-write detection in
`Superblock::select()` still works on the bootstrap fields; the encrypted body is
additionally protected by its own AEAD tag.

**Open flow:** read page 0 plaintext → if encrypted, derive a KEK from the client key
and each active slot's salt/params, try to unwrap the DEK (first success wins; ≤ 8
attempts) → decrypt the body sub-blob → validate `total_pages` against file size →
proceed. To stay robust across a key-rotation that landed between the A/B slots, the
key-slot tables of all readable superblock slots are considered when unwrapping, while
roots come from the highest-`txn_counter` slot. (`stride`/`algorithm` are stable across
rotations, so reading them from either slot is safe.)

---

## 6. Public API and key lifetime

### 6.1 Rust

```rust
pub enum Key {
    Raw(Zeroizing<Vec<u8>>),          // high-entropy key  → HKDF-SHA256
    Passphrase(Zeroizing<String>),    // human passphrase  → Argon2id
}

// Options gains (it is already #[non_exhaustive], lib.rs:131):
pub struct Options {
    // ...existing...
    pub encryption_key: Option<Key>,
    pub argon2_params: Option<Argon2Params>,  // create-time default for passphrase slots
}

// New methods on Chisel:
fn add_key(&mut self, existing: &Key, new: &Key) -> Result<()>;
fn rotate_key(&mut self, old: &Key, new: &Key) -> Result<()>;
fn remove_key(&mut self, slot_or_key: ...) -> Result<()>;
```

The engine (`TransactionManager`) holds the unwrapped DEK in a zeroizing wrapper for
the session and wipes it on drop. The client key and derived KEK are zeroized
immediately after use. No key material is ever written to disk except the
KEK-**wrapped** DEK in the key-slot table.

### 6.2 Python (pyo3)

`open(..., encryption_key=...)`: `bytes` → `Key::Raw`, `str` → `Key::Passphrase`.
Plus `add_key` / `rotate_key` / `remove_key` methods mirroring the Rust API.

---

## 7. Errors

Slots into the existing operational-vs-fatal (I1 poison) model:

- **Operational (retryable — must NOT poison the engine):**
  - `NoEncryptionKey` — opening an encrypted DB without a key.
  - `InvalidEncryptionKey` — supplied key unwraps no slot (wrong key/passphrase).
  - `EncryptionNotSupported` / `UnexpectedKey` — key supplied for a plaintext DB, or
    mismatch.
- **Fatal (poison — corruption/tamper after a valid open):**
  - `DecryptionFailed { page_id }` — a page's AEAD tag fails verification after the DB
    was opened with a valid key. This means on-disk tampering or corruption.
- **Format gate:** an old, encryption-unaware Chisel binary cannot open an encrypted
  DB — the **MAJOR format-version bump** (1 → 2) yields `UnsupportedFormatVersion`,
  preventing it from misreading ciphertext as plaintext.

---

## 8. Format versioning

- File-level **MAJOR** bump `1 → 2` for encrypted databases (hard-rejects old binaries).
  Plaintext databases are unaffected and continue at the current version. Because no
  production databases exist, this bump is free; the exact post-1.0 numbering is settled
  at release.
- No per-page (I31) format change is required: the logical 8192-byte page image is
  unchanged; encryption lives entirely below it.

---

## 9. Threat model — what this does and does not protect

Provided (under the AEAD model):

- **Confidentiality** of all user data and sensitive metadata at rest.
- **Tamper-detection**: any modification of a page or the superblock body is detected
  (Poly1305/AEAD), surfaced as `DecryptionFailed` (fatal).
- **Anti-relocation**: `AAD = page_id` prevents moving a valid ciphertext to another
  slot.

Not provided (documented boundaries):

- **Rollback / replay resistance**: an attacker with file access who substitutes a
  wholly older, validly-signed database image (or an older valid A/B superblock slot)
  cannot be detected by self-contained authentication. Defeating this requires an
  external monotonic trust anchor (e.g., TPM), which is out of scope for a file-based
  embedded store.
- **In-memory protection**: the page cache and the DEK are plaintext in process memory
  during an open session (mitigated by zeroize-on-drop, not by encryption).
- **Traffic-analysis / size**: file size, page count, and access patterns are not
  hidden.

---

## 10. Testing

- **Unit:** KDF known-answer vectors (HKDF, Argon2id); AEAD seal/open round-trip; DEK
  wrap/unwrap; wrong-key rejection; byte-flip in ciphertext → `DecryptionFailed`;
  relocation (move a sealed page to a wrong `page_id`) → auth failure; key-slot codec
  round-trip; zeroization (where testable).
- **Integration:** create encrypted DB → write → close → reopen with key → read back;
  reopen with wrong key → `InvalidEncryptionKey`; reopen without key → `NoEncryptionKey`;
  open encrypted DB with old format → `UnsupportedFormatVersion`; forced spillway
  spill+drain under encryption; `add_key` / `rotate_key` (old fails, new works) /
  `remove_key`; crash-during-encrypted-commit recovery picks the prior valid superblock.
- **Property/fuzz:** random page contents seal/open round-trip; any corrupted ciphertext
  is always detected.
- **Benchmarks** (report-only in CI, per project policy): encrypted vs plaintext
  throughput, confirming encryption overhead is small relative to fsync-dominated
  commit latency; KDF cost (Argon2id) on open.
- Coverage aimed near 100%, consistent with the project standard.

---

## 11. Phasing (each layer completed before the next)

1. **Crypto core** — a standalone module (`src/crypto/` or `src/encryption/`):
   `PageCipher` (seal/open), KDF (HKDF + Argon2id), DEK wrap/unwrap, key-slot codec,
   zeroizing `Key`/DEK types. Fully unit-tested in isolation, no engine coupling.
2. **Superblock crypto-header + encrypted body** — extend serialize/deserialize; the
   create-new and open-existing key flows; the operational error variants.
3. **Page I/O encryption** — stride-aware `page_io`; `PageCipher` wired into the
   page-cache flush / spill / drain paths (seal-once invariant); spillway slot widening.
4. **Public API + errors + Python** — `Options.encryption_key`, the `Key` enum, error
   variants, pyo3 kwargs.
5. **Key-management API** — `add_key` / `rotate_key` / `remove_key` (Rust + Python).
6. **Docs + ADR + format-version bump** — ARCHITECTURE.md, ADR graph (codebase-memory),
   the MAJOR version change, and an ISSUES.md entry for deferred bulk DEK rotation.

---

## 12. Affected code (verified)

| Area | File(s) | Change |
|---|---|---|
| I/O stride + raw on-disk unit | `src/page_io.rs` (offsets @256/@296/@418) | stride = 8232 when encrypted; read/write the on-disk page unit |
| Seal/open orchestration | `src/page_cache.rs` (flush @~420, spill/drain) | hold `PageCipher`; seal-once on flush/evict; copy on drain |
| Spillway | `src/spillway.rs` (`SLOT_SIZE` @43) | widen slot to carry the 8232-byte sealed blob |
| Superblock | `src/superblock.rs` (`serialize` @245, reserved ≥324) | crypto-header + encrypted body sub-blob; key-slot codec |
| Open/create/rotate | `src/lib.rs` (`Options` @131, `open` @310), `src/transaction/` (recovery, commit, mod) | key flow, DEK in `TransactionManager`, rotation ops |
| Errors | `src/error.rs` | operational + fatal encryption variants |
| Format version | `src/page.rs` (`FORMAT_MAJOR_VERSION` @113) | MAJOR bump for encrypted DBs |
| Python | `python/src/db.rs` | `encryption_key` kwarg + rotation methods |
| New module | `src/crypto/` (new) | all primitives, KDF, wrap, key-slot codec |
| Deps | `Cargo.toml` | chacha20poly1305, argon2, hkdf, sha2, zeroize, rand_core/getrandom |
