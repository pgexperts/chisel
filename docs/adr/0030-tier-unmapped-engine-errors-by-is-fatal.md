---
id: 0030
title: Tier unmapped engine errors by is_fatal()
date: 2026-08-23
status: Accepted
summary: The Swift FFI's catch-all error case carries the engine's own is_fatal() verdict rather than assuming a severity tier for every unmapped variant.
---

# 0030. Tier unmapped engine errors by is_fatal()

## Context

`chisel::ChiselError` is `#[non_exhaustive]`, so no downstream binding can
match it exhaustively — the compiler will not even warn when the engine adds a
variant, and both bindings need a fallback arm to stay buildable across an
engine bump. The Swift FFI's fallback routed every unmapped variant to
`Poisoned`, on a fail-toward-fatal argument: better to over-report severity
than under-report it. In practice this accumulated five variants behind that
one case, at least one of which is operational rather than fatal. A caller
catching an operational condition (say, a validation error newly added to the
engine) would see it surface as `Poisoned` — fatal, unrecoverable — and either
abort work it could safely retry or alarm on something that was never a
durability problem.

## Decision

The fallback case carries the engine's own `is_fatal()` verdict instead of
assuming a tier: `Other { message, fatal }`. The mapping guard that keeps this
honest derives the engine's variant set from `src/error.rs` rather than from a
hand-maintained list, so an engine change that adds or removes a variant is
caught structurally rather than by someone remembering to update a parallel
enum by hand.

## Alternatives considered

- **Keep fail-toward-fatal** — routing every unmapped variant to `Poisoned`.
  Rejected: it mistiers every future operational variant that lands in the
  fallback, and it asserts an engine state (poisoned, unrecoverable) that is
  not actually true for those variants.
- **Add a `#[doc(hidden)]` exhaustive-match helper to the engine** — a
  crate-internal function the binding could call to force a compile error on a
  missed variant. Rejected: it puts a test-only concern in the engine's public
  API surface, and `#[doc(hidden)]` carries no semver guarantee — a later
  engine release could remove or change it without notice, which is a worse
  failure mode than the one it was meant to prevent.

## Consequences

- Swift's `category` for the fallback case is data read off `is_fatal()`
  rather than a compile-time constant, so callers switching on category must
  treat the fallback's severity as a runtime value like any other field.
- The mapping guard is a text scan coupled to `src/error.rs`'s formatting; if
  that file's structure changes in a way the scan does not anticipate, the
  guard fails loudly (rather than silently drifting), which is the intended
  trade — a noisy CI failure over a quiet mistier.
