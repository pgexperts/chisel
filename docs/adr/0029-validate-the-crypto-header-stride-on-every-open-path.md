---
id: 0029
title: Validate the crypto-header stride on every open path, accepting a narrowing
date: 2026-08-16
status: Accepted
summary: A forged stride tolerated on the torn-page-0 path was republished into every slot by later commits until the file became unopenable, so the winning slot is now validated whichever path found it.
---

# 0029. Validate the crypto-header stride on every open path, accepting a narrowing

## Context

A forged `stride` in an encrypted database's crypto header was a hard error on
one open path and silently tolerated on the other. With page 0 intact, the
anchor check refuses any value but `ENC_PAGE_SIZE`. With page 0 torn, the
fallback hardcodes the constant, tests the surviving sibling's advertised stride
with `.is_some()`, and discards the value.

The damage is not at the I/O layer. The stride actually applied is the constant
on both paths, so there is no misaligned read, no divide-by-zero, no oversized
allocation. The damage is that the forged value is LAUNDERED INTO DURABLE STATE:
`open_existing` captures the winner's header verbatim into `crypto_header`, and
every commit republishes it. Commit round-robins slots, so after N commits every
slot carries the forged stride with a valid checksum — and the next open, page 0
now intact, hits the anchor check with no clean sibling left. A transient tamper
the engine chose to tolerate becomes a permanently unopenable database, and the
engine did the propagating.

Forging this requires write access to the file, since XXH3 is publicly
computable rather than keyed. Random media corruption cannot produce it: the
checksum fails and the slot is skipped. So the exposure is integrity and
availability, never confidentiality or memory safety.

## Decision

We will validate the winning slot's crypto-header stride after selection,
whichever path found it, and reject a mismatch as `CorruptSuperblock`.

The anchor check stays. It runs BEFORE `set_stride`, whose division a forged `0`
would fault on, so it is a pre-check the post-selection test cannot replace.

## Alternatives considered

- **Normalize `sb.encryption.stride` to `ENC_PAGE_SIZE`** on read. This would
  stop the self-bricking and keep more files openable. Rejected: it silently
  rewrites a field the file claims, and the anchor path had already established
  "reject" as this project's answer to exactly this input.

- **Leave the fallback path alone and document the asymmetry.** Rejected: the
  asymmetry is not the harm. The harm is the republication, which no comment
  prevents.

- **Validate at write time instead of read time.** Rejected as insufficient
  alone: the engine has only ever written the constant, so a wrong value can
  only arrive from outside the engine, which a write-side check never sees.

## Consequences

This NARROWS what opens, and the narrowing is real rather than theoretical. A
file with page 0 intact but a forged-stride sibling outranking it on
`txn_counter` opens today and will not after this change. That file is tampered
by construction, and its sealed body is the one about to be trusted, so refusing
is correct — but it is not a free change and the comment at the check says so.

A forged-stride file opened without a key now reports `CorruptSuperblock` rather
than `NoEncryptionKey`, because the check sits before the key-presence match,
mirroring the algorithm check above it.

Nothing is poisoned: the error returns before any `TransactionManager` exists.

The laundering shape is GENERAL, not stride-specific. `open_existing` captures
the winner's `CryptoHeader` verbatim and every commit republishes it, so any
future field added to that struct without a validation gate at the same place
inherits the identical self-bricking behaviour. Today `algorithm` and `stride`
are both gated; the next field must be too.
