---
id: 0016
title: Swift binding via UniFFI
date: 2026-07-19
status: Accepted
summary: The iOS/macOS Swift binding is a UniFFI-generated FFI over an Arc<Mutex<Chisel>> wrapper crate, with the engine crate left unchanged.
---

# 0016. Swift binding via UniFFI

## Context

Chisel needs to be embeddable in iOS and macOS applications as a reusable,
published Swift package. Swift can only call C, not Rust directly — so unlike
the existing Python binding (`python/`, PyO3 linked against the engine in one
address space), a Swift binding needs a C ABI in between. The engine is a
single-writer design (`chisel::Chisel` is `Send + !Sync`, every mutator takes
`&mut self`; see ADR-2), which collides head-on with Swift 6 strict concurrency
(`Sendable`) and ARC. The binding's hardest question is therefore not the CRUD
surface but how to cross into Swift's async world without violating the
one-client-at-a-time invariant — and to do it without modifying the engine.

## Decision

Ship a reusable SPM package. Bridge Rust → Swift with **UniFFI** (proc-macros,
library mode). A new `chisel-ffi` crate — the fourth workspace member, kept out
of `default-members` like `python/` — wraps the engine in
`Mutex<Option<chisel::Chisel>>` and exports a flat, synchronous API via
`#[uniffi::export]`; UniFFI generates both the C-ABI scaffolding and idiomatic
Swift (a real error enum, `Data` buffers, records). Two hand-written Swift
surfaces sit on top: a synchronous, thread-confined `ChiselDatabase` and an
`async` `actor ChiselStore` backed by a dedicated serial `DispatchQueue`.
Mutations are closure-only (`transaction { txn in … }`); reads, config, and key
management are top-level. The full surface is exposed, including on-disk
encryption (ADR-15) with a Keychain-aware `ChiselKey`. The engine crate is not
touched: the single-client invariant relocates from the borrow checker to the
`Mutex` at the FFI boundary.

## Alternatives considered

- **Hand-rolled C ABI + cbindgen** — maximal control and no codegen dependency,
  but a rich API becomes pages of `unsafe` boilerplate (length out-params for
  every `Vec<u8>`, error out-params, manual opaque-pointer lifecycles), each a
  place for UB. Rejected: UniFFI generates the error tree, buffer handling, and
  the SPM/xcframework packaging for free, and is the proven path for shipping a
  Rust engine to iOS (matrix-rust-sdk).
- **swift-bridge** — Swift-specific codegen, arguably more idiomatic in places,
  but a smaller, less battle-tested ecosystem, no Kotlin reuse, and thinner
  publish-an-xcframework documentation. Rejected on ecosystem maturity.
- **Python-style surface (raw `begin`/`commit` + top-level mutators)** — the
  whole-branch review found top-level mutators would be dead-on-arrival: the
  engine requires an active transaction for mutations and has no auto-begin, so
  `db.allocate()` at the top level always throws `NoActiveTransaction`. Rejected
  in favor of closure-only mutations, which make "one transaction, always
  resolved" structural.

## Consequences

- `Mutex<Chisel>` is `Send + Sync` because `Chisel` is `Send + !Sync` (it uses
  `RefCell`, no `Rc`); a compile-time `assert_send::<chisel::Chisel>()` in
  `chisel-ffi` guards the assumption, with a documented dedicated-thread-actor
  fallback if the engine ever becomes `!Send`. The `Option` lets `close(self)`
  (by-value in the engine) `.take()` and consume behind UniFFI's `&self`/`Arc`.
- The async `transaction` body is deliberately **synchronous** (`@Sendable`,
  non-`async`) so no `await` can suspend mid-transaction and interleave a second
  logical transaction on the `!Sync` engine — a type-level guarantee, backed by
  a dedicated serial queue so blocking fsync I/O never touches Swift's
  cooperative pool.
- `ChiselError.category` (operational vs fatal) follows the **Python binding's**
  caller-facing tiering — notably `Poisoned` is fatal — which deliberately
  diverges from the engine's manager-internal `is_fatal()` (ADR-6). Every error
  variant carries a `message: String` (the engine's `Display`) so Swift callers
  get diagnostics, not bare case names.
- Distribution is SPM with a prebuilt `.xcframework` across five Apple targets;
  a macOS CI job builds and tests it. The Thread-Sanitizer concurrency check is
  best-effort in CI because some hosts' SIP policy blocks sanitizer injection
  into the xctest helper.
- Relates to ADR-2 (single-writer), ADR-6 (poison model), ADR-15 (encryption).
  Supersedes nothing.
