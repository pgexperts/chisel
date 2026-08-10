---
id: 0020
title: Validate Argon2 cost parameters at the API boundary with a dedicated error
date: 2026-08-10
status: Accepted
summary: Options::argon2_params is range-checked in open/open_in_memory/rekey before any file I/O, raising a new operational InvalidArgon2Params instead of the credential-shaped InvalidEncryptionKey.
---

# 0020. Validate Argon2 cost parameters at the API boundary with a dedicated error

## Context

`Argon2Params` is a public struct with three unvalidated `u32` fields. Out-of-range
values reached `Params::new` deep inside the create path, failed as `CryptoError::Kdf`,
and were collapsed by the blanket `impl From<CryptoError> for ChiselError` into
`InvalidEncryptionKey`.

That diagnosis is not merely unhelpful — it is impossible. `InvalidEncryptionKey` means
"a key was supplied but no key-slot's wrapped DEK could be unwrapped", and on a database
being CREATED there is no key slot to mismatch. The actual cause, a rejected cost
parameter, appeared nowhere in the error. The wrong diagnosis had also been codified into
the public rustdoc.

## Decision

Cost parameters supplied through `Options` are range-checked at the API boundary, before
the file is opened or created, in all three entry points that accept them (`open`,
`open_in_memory_with_options`, `rekey`). Failures raise a new OPERATIONAL variant
`ChiselError::InvalidArgon2Params { m_cost, t_cost, p_cost }`.

Lower bounds are stated explicitly rather than delegated to `Params::new`, including
argon2's cross-field `m_cost >= p_cost * 8`. `derive_kek` keeps its own inline cap check:
that one guards untrusted bytes read from a hostile key slot, not a caller's Options, and
must stay on the read path regardless of what callers do.

## Alternatives considered

- **A checked constructor `Argon2Params::new(..) -> Result<..>`** — rejected: the fields
  are public and the type is `Copy`, so a constructor cannot be enforcing without
  privatising them, which is a public-API break; and `KeySlot::read_from` must be able to
  build the type from untrusted disk bytes WITHOUT validation, which is exactly why
  `derive_kek`'s guard exists.
- **Reuse `InvalidEncryptionKey`** — rejected: that is the status quo, and it reports a
  cause that cannot be true on the create path.
- **Validate only on create** — rejected: it diverges from the established
  `superblock_count` precedent, which is also create-only in effect but validated always
  so a malformed `Options` is caught up front.

## Consequences

`ChiselError` is `#[non_exhaustive]`, so the added variant is not a downstream match
break. It and the three new `MIN_ARGON2_*` constants are additive public API: the next
crates.io publish should be a minor version bump.

One behaviour change: an explicitly supplied out-of-range value now fails `open` even on
paths where it was previously ignored (reopen, raw keys, plaintext). This mirrors
`superblock_count` and is documented in the rustdoc and README.

The Python binding gains `InvalidArgon2ParamsError`, registered but currently unreachable
— `chisel.open()` exposes `encryption_key` but not `argon2_params`. It is added now
because `to_py_err`'s catchall routes unmapped variants by `is_fatal()` alone, so a
missing arm is a silent downgrade rather than a compile error.
