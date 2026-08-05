---
id: 0015
title: On-disk encryption (XChaCha20-Poly1305, envelope keys)
date: 2026-06-30
status: Accepted
---

# 0015. On-disk encryption (XChaCha20-Poly1305, envelope keys)

**Context:** Chisel originally listed "no encryption at rest" as an explicit non-goal (see Out of scope, now superseded), deferring confidentiality to filesystem-level encryption. That is adequate against whole-device theft but not for a client that needs per-database confidentiality *it* controls — a supplied key, not the OS's — with cryptographic tamper-detection. The requirement: the client program supplies a key when opening a database; every byte Chisel then writes is encrypted and integrity-protected, and rotating that credential must be cheap.

**Decision:** Opt-in **authenticated** at-rest encryption. A client supplies a `Key` (raw bytes or a passphrase) via `Options::encryption_key`; without one, the database is plaintext exactly as before. Core choices:

- **Cipher: XChaCha20-Poly1305**, a fresh **random 192-bit nonce per page write**, `AAD = page_id` (anti-relocation). AEAD (authenticated) over a length-preserving mode so tampering is *cryptographically* detected, not merely obscured.
- **Envelope keys.** A random per-database **DEK** seals every page and the sensitive superblock fields; the DEK is stored **wrapped** under a **KEK** derived from the client key (HKDF-SHA256 for raw keys, Argon2id for passphrases) in an **8-slot key-slot table** in the superblock's plaintext reserved region (offset 324). Credential rotation re-wraps the DEK — **O(1), no data re-encryption**.
- **Page format: an 8232-byte on-disk stride** (`ENC_PAGE_SIZE` = 8192 ciphertext + 16 tag + 24 nonce); the *logical* page stays 8192, so the freemap/data-page/handle-table geometry is untouched — encryption is a transform at the **page-I/O seam** (`PageCache` owns a `PageCipher`; `page_io` is stride-aware but crypto-agnostic). Encrypted DBs use this uniform stride from birth, including superblock slots (the 8192 image zero-padded into an 8232 unit); open bootstraps by reading page 0 at offset 0, learning the stride from its plaintext crypto-header, then reading the remaining slots at that stride.
- **Format gate: MAJOR bump 1 → 2** (the first real exercise of ADR-7's MAJOR tier). An encryption-unaware binary refuses a MAJOR=2 file; plaintext databases stay MAJOR=1 and byte-identical.
- **Spillway and superblock are in scope.** The spillway holds *sealed* blobs (seal-once on evict, verbatim copy on drain — never plaintext); the sensitive superblock body (roots, counters, and the user-chosen `named_roots`) is DEK-sealed, leaving only the bootstrap header + key-slot table in cleartext.

**Alternatives considered:**

- *AES-256-XTS (length-preserving).* Zero per-page overhead, no format change, the FDE industry standard — but **confidentiality only**, no cryptographic tamper-detection (would lean on the non-cryptographic XXH3). Rejected: the client wanted authenticated encryption, and since no production databases exist the one-time format change is free.
- *Deterministic nonce from `(page_id, counter)`.* Tempting (no stored nonce) but **unsafe under shadow paging**: a crashed transaction discards its writes and returns page_ids to the freemap while the durable counter doesn't advance, so the same page_id can get different plaintext at the same counter → keystream reuse. XChaCha's 192-bit **random** nonce sidesteps the whole class (no persisted counter, crash-safe) — this drove the cipher choice.
- *AES-256-GCM.* Fine with AES-NI, but its 96-bit nonce forces the deterministic-nonce hazard above or a stored counter; ChaCha is also constant-time in portable software (no hardware dependence) — the better default for an embedded library that runs wherever the client runs.
- *Encrypt only data pages, leave the superblock plaintext.* Rejected: `named_roots` holds user-chosen names (real user data); a plaintext superblock body leaks them.
- *Full DEK rotation (re-encrypt every page under a new DEK).* Deferred (ISSUES.md I142) — a heavy whole-file operation for the "the DEK itself is compromised" case, distinct from the implemented credential (KEK) rotation.
- *Keeping "encryption is out of scope."* Superseded — the requirement is per-database, client-controlled confidentiality that filesystem encryption cannot provide.

**Consequences:**

- *Positive:* Plaintext databases are provably byte-and-behavior identical (every divergence is a `Some/None` cipher branch); zero regression for existing/unencrypted files.
- *Positive:* Credential rotation is O(1) — `add_key` / `rotate_key` / `remove_key` (Rust + Python) re-wrap the stable DEK without touching data; `rotate_key` stages the new key before revoking the old (no zero-key window), and `remove_key` refuses the last active slot (brick prevention).
- *Positive:* Reuses the existing A/B superblock + poison model — a metadata-only `rewrite_crypto_header` commit persists a rotated slot table atomically (write inactive slot → fsync → promote), so a crash mid-rotation leaves the old slot table intact.
- *Threat-model boundary (documented non-guarantees, spec §9):* confidentiality + per-page/superblock tamper-detection + anti-relocation; **no** rollback/replay protection (an attacker substituting a wholly older, validly-signed image is undetectable without an external trust anchor); the DEK is plaintext in process memory during an open session (mitigated by zeroize-on-drop, not by encryption).
- *Negative:* Encrypted files are ~0.5% larger (the 40-byte per-page trailer) and MAJOR=2, so encryption-unaware binaries can't read them (by design).
- *Reversibility:* Hard — a new on-disk format (encrypted stride, sealed superblock, key-slot table) plus a page-I/O seal seam and a key-management API cluster. Additive to plaintext DBs (it does not affect existing readers), but removal would delete `src/crypto/`, `src/superblock/crypto_header.rs`, `src/transaction/keys.rs`, and the public key API.

Spec: `docs/specs/2026-06-29-on-disk-encryption-design.md`. Plan: `docs/plans/2026-06-29-on-disk-encryption.md`. Implemented 2026-06-30 across 6 phases (crypto core → superblock/key-flow → page-I/O + cache + spillway → public API + Python → key rotation → docs/version). See ARCHITECTURE.md "On-disk encryption" and ISSUES.md I142 (deferred bulk DEK rotation). Public API is deliberately narrow: only `Key`, `Argon2Params`, and the encryption error variants are public; the crypto/superblock internals are `pub(crate)`.

---

