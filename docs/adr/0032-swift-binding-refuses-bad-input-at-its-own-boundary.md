---
id: 0032
title: The Swift binding refuses bad input at its own boundary, with a typed error
date: 2026-10-15
status: Accepted
summary: Zero tags, non-file URLs and escaped transactions are rejected by explicit checks in chisel-ffi or the Swift wrapper that throw a ChiselError case, instead of relying on UniFFI lifts, URL.path, or caller convention.
---

# 0032. The Swift binding refuses bad input at its own boundary, with a typed error

## Context

The binding's documented contract (swift/README.md) is that every throwing call
raises `ChiselError`, so a caller can `catch let e as ChiselError` and switch on
its category. The 2026-07-29 review (#104) found three inputs that broke that
contract:

- **Zero tag (SWIFT-1).** `Tag` was a UniFFI `custom_type!` over `u32` whose
  lift rejected zero through the engine's `TryFrom`. A lift failure reaches
  Swift as an untyped UniFFI internal error, not a `ChiselError`, so
  `allocateTagged(_, tag: 0)` escaped every documented catch.
- **Non-file URL (SWIFT-5).** The entry points took a `URL` and used `url.path`.
  On a non-file URL that silently drops the scheme and host, so a URL naming a
  remote resource became a local path that the engine would create and flock.
- **Escaped transaction (SWIFT-4).** `Transaction` was documented as
  non-escaping, but nothing enforced it. The FFI dispatches to whatever
  transaction the engine has open, so a write through an escaped copy silently
  joined, and committed with, the *next* transaction. The FFI had reserved
  `.AlreadyFinished` for this guard, but nothing ever raised it.

The Python binding had already settled the transaction and savepoint
questions (ADR 0025), and validates tags explicitly in `python/src/db.rs`.

## Decision

Each of these is checked explicitly at the binding's boundary and rejected with
a `ChiselError` case:

- `chisel-ffi` tagged methods take a raw `u32` and convert through `tag_from`,
  which returns a new `ChiselError::InvalidTag` on zero. No exported method
  takes `chisel::Tag`, so the lift never sees caller input.
- Every Swift entry point that accepts a URL routes through `chiselFilePath`
  (`swift/Sources/Chisel/FileURL.swift`), which requires `url.isFileURL` and
  throws `.DatabaseFileNotFound` otherwise. It reuses that existing case, with a
  message naming the problem, rather than adding a new one.
- `Transaction` carries a one-shot `finished` flag behind a shared reference
  (`TransactionState`). It is set by `defer` on every exit of `transaction { }`,
  commit failure included. Every forwarder checks it and throws
  `.AlreadyFinished` once it is set. `Transaction` stays a struct: copies share
  the flag, and `ChiselStore`'s `@Sendable` closure signature is unchanged.
- `savepoint { }` releases the savepoint after rolling back to it on the throw
  path, so a retried scope does not hit `DuplicateSavepoint`. This mirrors the
  Python savepoint scope in ADR 0025.

## Alternatives considered

- **Keep zero-tag rejection in the `custom_type!` lift.** Rejected: the lift
  cannot produce a typed error, which was the defect.
- **A Swift `Tag` struct with a failable initializer.** The 2026-07-18 binding
  design planned this, and the 2026-07-29 review offered it as one fix. The
  port spec chose the FFI check without recording why the struct lost.
  > Rationale not recovered from project sources. An evident difference: a
  > Swift-side type protects only Swift callers, while the FFI check protects
  > any UniFFI consumer.
- **Match Python's `ValueError` for zero tags.** Not possible in Swift's error
  model. The stronger "always `ChiselError`" contract was kept, accepting exact
  parity loss here.
- **A new error case for non-file URLs.** Rejected: it widens the FFI error
  surface for a condition `DatabaseFileNotFound` already describes.
- **Make `Transaction` a `final class`** (the port spec's first plan). Rejected
  in implementation: a class is not `Sendable`, so `ChiselStore.transaction`'s
  `@Sendable` body becomes a Swift 6 strict-concurrency error. The
  shared-reference flag gives the same escaped-copy behaviour without that cost.
- **Guard `Savepoint` too.** Not done: it has no forwarders for a guard to
  protect, and an escaped `Savepoint` can do nothing. Its doc comment says it
  needs the `Transaction` treatment if it ever gains an operation.

## Consequences

- A Swift caller's documented `catch let e as ChiselError` now really is total
  for these inputs, and swift/README.md's contract holds.
- Python and Swift differ on the zero-tag error type (`ValueError` vs
  `.InvalidTag`). `swift/PARITY.md` should say so wherever it lists the tagged
  methods.
- `TransactionState` is unsynchronized. That is sound only because the sync
  `ChiselDatabase` is thread-confined by contract and `ChiselStore` funnels
  every call through one serial queue. Loosening that confinement would need a
  synchronized flag.
- Any new FFI method that takes a tag must go through `tag_from`, and any new
  Swift entry point that takes a URL must go through `chiselFilePath`.
