---
id: 0006
title: Poison model on fatal errors
date: unknown
status: Accepted
---

# 0006. Poison model on fatal errors

**Context:** A failed `fsync()` on Linux post-2018 (the "fsyncgate" period) cannot be safely retried — the kernel may have already discarded the dirty pages, and a subsequent successful `fsync()` does NOT mean the earlier data is durable. Similarly, a checksum mismatch on page load means the on-disk state is corrupt and the in-memory reconstruction may be inconsistent with what's actually written. Continuing to use a `TransactionManager` after either condition risks reading torn or wrong state.

**Decision:** Any fatal error (commit-path `IoError`, `ChecksumMismatch`, `CorruptSuperblock`, etc. — see `ChiselError::is_fatal()` for the full list) sets a poison flag on `TransactionManager`. Every subsequent call returns `ChiselError::Poisoned`, including reads. The only legal recovery is to drop the `Chisel` handle and call `Chisel::open` again; the shadow-paging recovery path (`Superblock::select`) returns the database to its last-durable state.

**Alternatives considered:**

- *Retry the failed fsync.* Rejected per Linux fsyncgate semantics.
- *Restrict poisoning to writes; allow reads to continue.* Rejected because reads share the page cache with writes, and a corrupt page may have already been served to a previous read; we can't know which reads are tainted.
- *Auto-reopen on poison.* Rejected because reopen is a substantial state transition (file descriptors, locks, cache contents) that the caller must orchestrate. Forcing the caller to do it makes the recovery boundary explicit.

**Consequences:**

- *Positive:* Mirrors `std::sync::Mutex` poisoning — Rust developers will recognize the pattern.
- *Positive:* The recovery idiom (drop + reopen) exercises the same code path as crash recovery, which has the side benefit of testing the recovery path on every real-world poison event.
- *Positive:* No retry logic in the commit protocol. The protocol assumes happy-path fsync semantics; failure means stop.
- *Negative:* A poisoned `TransactionManager` is a permanent dead state. The caller MUST handle this — long-running services need a "drop and reopen on `Poisoned`" wrapper.

See `ISSUES.md` I1 for the full design and the project-memory note `project_chisel_i1_poison_decision`. **Update (2026-06-30):** the encryption feature extends the model consistently — `DecryptionFailed { page_id }` (a data-page AEAD auth failure after a valid open = tamper/corruption) is fatal/poisoning; a wrong or missing key at open (`InvalidEncryptionKey` / `NoEncryptionKey` / `EncryptionNotSupported`) and the key-management refusals (`NoFreeKeySlot` / `LastKeySlot`) are operational/retryable. The metadata-only `rewrite_crypto_header` rotation commit poisons on fsync failure exactly like `commit`. See ADR-15.

---
