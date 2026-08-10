---
id: 0019
title: Freeze the superblock identity AAD layout for format MAJOR 2
date: 2026-08-10
status: Accepted
summary: The 24-byte sb_identity_aad layout is frozen for MAJOR 2; adding a field requires a MAJOR bump plus an offline rewrap, and a fixture test pins the bytes.
---

# 0019. Freeze the superblock identity AAD layout for format MAJOR 2

## Context

`Superblock::sb_identity_aad` returns a fixed 24-byte array (magic, format_version,
txn_counter, superblock_count) used as the AAD for the DEK-sealed superblock body.
Its last four bytes were commented "reserved (zero) for future AAD fields".

That reads as an extension point and is a format trap. Every superblock ever written
sealed its body against that exact byte string, so populating those bytes changes the
AAD for all of them: `open_body` returns `CryptoError::Auth` on every existing
encrypted database, which surfaces to the user as `InvalidEncryptionKey`. An upgraded
binary would report every CORRECT passphrase as wrong rather than reporting a format
break — the two are indistinguishable at the API.

Nothing versioned the AAD: no scheme byte, no branch on `format_version`, and no test
pinned its bytes. The encrypted-MINOR write-gate cannot catch it either, since the
binary believes it understands the file. An adversarial check confirmed the exposure
empirically: populating the reserved bytes fails only a dedicated fixture — all other
lib tests, including every encrypted round-trip, still pass, because seal and open are
self-consistent within one binary.

## Decision

The 24-byte layout is FROZEN for format MAJOR 2. The "reserved for future AAD fields"
comment is replaced by a statement of the freeze and of the migration path: adding an
AAD field requires bumping `FORMAT_MAJOR_VERSION_ENCRYPTED` (2 -> 3) plus an offline
pass that re-seals every superblock body under the new AAD. Two fixture tests pin the
exact bytes for the encrypted and plaintext series.

## Alternatives considered

- **Version the AAD now** (add a scheme byte) — rejected: adding a byte to the AAD IS
  the format break being guarded against. An unversioned AAD cannot be retroactively
  versioned without invalidating every sealed body.
- **Migrate the format immediately** — rejected as unjustified: there is no field
  waiting to be added, so this would spend a MAJOR bump and a rewrap pass on nothing.
- **Leave the comment and rely on review** — rejected: the comment actively invited
  the edit, and no test would have caught it.

## Consequences

The fixture is the sole and sufficient enforcement, which is unusual but correct here:
the defect is invisible to behavioural tests by construction. A failing fixture is a
deliberate signal, not a flaky assertion — its message says so.

A future AAD field is now expensive by design. That is the honest cost of the original
choice to seal against an unversioned identity string.
