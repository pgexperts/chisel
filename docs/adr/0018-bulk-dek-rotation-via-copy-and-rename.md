---
id: 0018
title: Bulk DEK rotation as an offline copy-then-rename operation on a path
date: 2026-08-04
status: Accepted
summary: rekey() re-encrypts the whole database under a fresh DEK by building a replacement file and renaming it into place; it takes a path rather than a handle, and collapses the key-slot table to the single supplied credential.
---

# 0018. Bulk DEK rotation as an offline copy-then-rename operation on a path

## Context

ADR [0015](0015-on-disk-encryption-xchacha20-poly1305-envelope-keys.md) chose an
envelope scheme: one per-database DEK seals every page, and up to eight key
slots each wrap that DEK under a KEK derived from a client credential. That
makes *credential* rotation O(1) — `add_key`/`rotate_key`/`remove_key` touch
only the superblock, and no page is re-encrypted. It was recorded at the time
that full DEK rotation was deferred.

Deferred is not the same as unnecessary. The two operations answer different
questions, and only one of them was answerable:

- a credential leaked → deny that credential a way in → credential rotation
- **the DEK itself leaked** (process memory dump, core file, attached debugger)
  → the data must be re-sealed under a key the attacker does not have

Credential rotation cannot help with the second case at all, because the DEK it
re-wraps is the very thing that leaked. The fix for CRYPTO-1 sharpened the point
by documenting it in `rotate_key`: revocation denies entry, it does not
re-key. Without a bulk rotation there was no operation in the crate that did.

Two things made the design non-obvious.

**Crash safety cannot lean on shadow paging.** Shadow paging protects writes
that go to *new* pages; a DEK rotation rewrites every page where it already is.
A crash halfway through leaves some pages sealed under the new DEK and some
under the old, with the surviving superblock naming one of them — an
unrecoverable mix, and precisely the kind of half-state the engine otherwise
never produces.

**The operation invalidates its own handle.** Any strategy that replaces the
file by rename leaves a previously-opened file descriptor pointing at the
original, now-unlinked inode. Reads and writes through it would silently target
a deleted file.

## Decision

We will implement `Chisel::rekey(path, key, argon2_params)` as an **offline
operation on a path**, which builds the rotated database in a scratch file
beside the original and publishes it with an atomic `rename`.

Concretely: open normally (validating the key and taking the exclusive flock),
generate a fresh DEK, write every page into `<db>.rekey-tmp` — superblock slots
rebuilt from the winning superblock under a new crypto header, every other page
opened under the old DEK and re-sealed under the new one *at the same page id* —
`fsync`, `rename`, `fsync` the directory, then drop the handle.

The rotated database carries **exactly one key slot**: the credential supplied.

## Alternatives considered

- **In-place two-pass rotation.** Rejected: not crash-safe for the reason
  above, and making it so would need a journal — a second durability mechanism
  in an engine whose entire premise (ADR
  [0001](0001-shadow-paging-not-wal.md)) is that it does not have one.

- **`rekey(&mut self)` on a live handle.** Rejected: it would hand back a
  handle whose file descriptor names a deleted inode. Making that safe means
  swapping the descriptor and re-acquiring the flock underneath a live cache —
  real complexity to preserve an ergonomic nicety on an operation that rewrites
  the entire file and is not something anyone runs in a loop.

- **`rekey(self)` consuming the handle.** Closer, and it does dispose of the
  stale-descriptor hazard, but `Chisel` does not retain its path, so the caller
  would have to pass it back in and could pass the wrong one. A path-taking
  associated function has one source of truth and reads as what it is.

- **Re-wrapping the new DEK into all currently-active slots.** Not
  implementable, not merely declined: each slot's KEK is derived from *its own*
  credential, and only one was supplied. There is no way to produce a valid wrap
  for a credential you do not hold. It is also the safer default — if you are
  rotating because the DEK leaked, silently preserving every credential that
  could reach it is not what you want. `add_key` restores the others.

- **Keeping the older superblock slots' historical roots.** Declined. Their
  sealed bodies are under the old DEK, so preserving them means re-sealing
  states that are already unreachable. Every slot in the new file carries the
  winning superblock's roots at staggered counters, exactly as `create_new`
  seeds a fresh bank.

## Consequences

- Rotation costs O(total_pages) I/O and, transiently, a second copy of the
  database on disk. It is maintenance, not routine operation.
- The scratch file is created `O_EXCL | O_NOFOLLOW` mode 0600, matching the
  hardening the database and spillway received, so a planted file at the
  predictable scratch path cannot be adopted or followed.
- The directory `fsync` after the rename is best-effort: the file contents are
  already durable, so a failure costs rename durability across a crash rather
  than correctness.
- `rekey` must live outside `transaction/`, since it reaches across an open
  handle and the filesystem at once. It is its own module.
- Because it is a free function on a path, the PyO3 binding exposes it as a
  module-level `chisel.rekey(path, key)` mirroring `chisel.open()` rather than
  as a method — the same reasoning carried across the boundary.
- Rotation does not defend against per-page temporal replay
  ([#142](https://github.com/pgexperts/chisel/issues/142)); it does, however,
  invalidate every previously-sealed page image, so it is the operation that
  ends an attacker's ability to splice stale pages sealed under the old DEK.
