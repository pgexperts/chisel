---
id: 0033
title: FFI key material is redacted in every rendering and is not clonable
date: 2026-10-15
status: Accepted
summary: ChiselKey and Options in chisel-ffi hand-write a redacting Debug and drop Clone, and the Swift ChiselKey redacts its description, debugDescription and Mirror, so no rendering or reflection path prints a secret and no Rust-side copy escapes zeroization.
---

# 0033. FFI key material is redacted in every rendering and is not clonable

## Context

`chisel-ffi`'s `ChiselKey` (`Raw { bytes }` / `Passphrase { phrase }`) and the
`Options` record that embeds it both derived `Debug` and `Clone` (SWIFT-6,
#155). No call site formatted them, but the derives were a standing trap. Any
future `tracing::debug!(?options)`, assertion failure, or `unwrap()` on a
`Result<_, Options>` would print the passphrase verbatim. On iOS, those
destinations (os_log, crash reporters) are routinely uploaded off-device. A
derived `Clone` leaks in the other direction: the copy holds a plain
`Vec<u8>`/`String` that is dropped without wiping. Both contradict the rest of
the key path, which wraps material in `Zeroizing`. The engine already
hand-writes a redacting `Debug` for `crypto::Key`.

The Swift side had the same exposure through string interpolation, `print`,
`String(reflecting:)`, `dump`, and the debugger.

## Decision

- `ChiselKey` and `Options` (`chisel-ffi/src/types.rs`) hand-write `Debug`.
  It prints the variant name, which says which credential kind was supplied,
  and redacts the payload. The byte length is omitted too, because key length
  is a fingerprint.
- `Options` writes its own `Debug` rather than relying on `ChiselKey`'s, so
  redaction is a property of the type and a future field cannot silently
  reintroduce the leak.
- `Clone` is removed from both. Every consumer takes them by value and
  `into_engine(self)` consumes them.
- The Swift `ChiselKey` conforms to `CustomStringConvertible`,
  `CustomDebugStringConvertible` and `CustomReflectable`. The first two return
  `ChiselKey.raw(<redacted>)` / `ChiselKey.passphrase(<redacted>)`, covering
  interpolation, `print` and `String(reflecting:)`. The third returns a Mirror
  with no children. That is required because `dump` prints the custom
  description as a header and then walks the Mirror, and an enum's associated
  value is one of its children. Without it, `dump(key)` and `dump(options)`
  printed the passphrase directly beneath `<redacted>`. That gap shipped with
  the stack and was found and closed while recording this ADR.
- `Options` has no Swift conformance of its own; it redacts through its
  embedded key. Tests cover interpolation, `String(describing:)`,
  `String(reflecting:)`, `dump` and a bare Mirror walk, both on the key and
  through `Options`.

## Alternatives considered

- **Keep the derives and rely on call-site discipline.** Rejected: that is the
  trap the issue describes. The first careless `{:?}` leaks.
- **Wrap the payload in a `Secret`-style newtype across the FFI.** Not
  recorded as considered in the spec, plan or review. If revisited, note that
  UniFFI lowers a `custom_type!` to its underlying type (as it does for `Tag`,
  `chisel-ffi/src/types.rs`), so the payload would still reach Swift as plain
  `Data`/`String`. That is an observation, not the recorded reason.
- **Redact in Rust only.** Rejected: the Swift values are generated
  independently and render through Swift's own protocols, so Rust-side
  redaction does not cover them.

## Consequences

- No `Debug`, description, or Mirror-based rendering in Rust or Swift prints
  key material, and logging `Options` is safe.
- A caller who needs two `Options` values must construct both. Nothing in the
  binding or its tests needed `Clone`.
- Swift's generated `ChiselKey` still exposes its associated values to pattern
  matching. Redaction covers rendering, not access.
- Any new field on `Options` that is secret must be handled in its hand-written
  `Debug`, and the Swift redaction test should be extended alongside it.
