---
id: 0028
title: A corrupt sealed superblock body is fatal, not a credential error
date: 2026-08-16
status: Accepted
summary: Because a successful key-slot unwrap cryptographically proves the credential, any later body-decrypt failure is corruption and reports DecryptionFailed rather than the operational InvalidEncryptionKey.
---

# 0028. A corrupt sealed superblock body is fatal, not a credential error

## Context

Opening an encrypted database mapped a `decrypt_body` failure to
`ChiselError::InvalidEncryptionKey` — the same operational error a wrong
passphrase produces. An operator with a damaged file was told to check their
credential; a retry loop keyed on `is_fatal()` would retry forever. The comment
above the mapping already stated the correct diagnosis ("a tag failure here
means corruption, not a wrong key") and then did the opposite thing.

The reason this looks unfixable at first glance is that AEAD failure is uniform
in isolation: a wrong key and a tampered ciphertext both just fail the Poly1305
tag check, with no signal distinguishing them. Inventing a distinction that does
not exist would be worse than the bug.

The distinction is real, but it is not in the failure — it is in the SEQUENCING.
`CryptoHeader::unlock` is itself an AEAD open: it trial-derives a KEK per active
slot and returns `Ok` only when `unwrap_dek`'s tag verifies. Its success is
cryptographic proof that the supplied credential derived the KEK that wrapped
this DEK. Every wrong-credential open dies there and never reaches
`decrypt_body`. What remains past that point is a damaged or forged body, a body
sealed under a different DEK, or an AAD no longer matching the cleartext
bootstrap fields — none of which another passphrase can fix.

Every non-corruption route to a post-unlock failure was enumerated and excluded:
there is exactly one DEK per database (`add_key`/`rotate_key`/`remove_key` only
re-wrap the same one; `rekey` mints a new DEK and publishes atomically by
rename, so no crash leaves header and body under different DEKs), the AAD is
built from the winner's own validated buffer, and a false-positive unlock is a
2^-128 tag forgery.

## Decision

We will report a post-unlock `decrypt_body` failure as
`ChiselError::DecryptionFailed { page_id }`, naming the superblock slot.

`InvalidEncryptionKey` remains correct for its actual meaning: no credential
matched any key slot.

## Alternatives considered

- **`CorruptSuperblock`.** Rejected. That variant means "no readable superblock
  at all", and its recovery story is "reopen; selection may land on a different
  slot". Selection here is deterministic (`max_by_key` over `txn_counter`), so a
  reopen returns the same error forever. The repo had already made this exact
  call for `InvalidFreemapDepth`, whose doc says so in near-verbatim terms.

- **A new dedicated variant** for "sealed body failed authentication".
  Rejected as unnecessary: `DecryptionFailed`'s existing doc already described
  this failure word for word, and the Python binding's own documentation already
  claimed `DecryptionFailedError` covered the superblock — aspirational before
  this change, true after it.

- **Leaving it and correcting the comment.** Rejected: the comment was already
  right and the code already wrong, so a doc-only fix would have documented the
  defect more clearly rather than removing it.

## Consequences

An operator facing a damaged encrypted database now gets an error that says the
file is damaged, and `is_fatal()` correctly stops a credential-retry loop.

One case genuinely cannot be distinguished and is unchanged: if the KEY SLOT
itself is damaged, `unlock` skips it and a CORRECT passphrase is still reported
as `InvalidEncryptionKey`. That limit is stated at the fix and pinned by an
existing test. No comment claims the change addresses it.

This is a user-visible behaviour change in the Python binding, inherited through
a mapping that already existed: a corrupt encrypted superblock now raises
`DecryptionFailedError` (a `FatalError`) instead of `InvalidEncryptionKeyError`
(an `OperationalError`). No Python test asserted the old behaviour.

The classification poisons nothing, because the error returns from
`open_existing` before any `TransactionManager` is constructed.

The blanket `From<CryptoError> for ChiselError -> InvalidEncryptionKey`
conversion remains the mechanism that makes this class of mis-attribution easy
to reintroduce, and two comments in the tree already record it biting once.
Whether it should exist at all is worth its own look.
