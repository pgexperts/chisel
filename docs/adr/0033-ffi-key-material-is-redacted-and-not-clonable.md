---
id: 0033
title: FFI key material is redacted in every rendering and is not clonable
date: 2026-10-15
status: Accepted
summary: ChiselKey and Options in chisel-ffi hand-write a redacting Debug and drop Clone, and the Swift ChiselKey conforms to CustomStringConvertible and CustomDebugStringConvertible with redacted output, so no formatting path prints a secret and no copy escapes zeroization.
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
- The Swift `ChiselKey` conforms to both `CustomStringConvertible` and
  `CustomDebugStringConvertible`, returning `ChiselKey.raw(<redacted>)` /
  `ChiselKey.passphrase(<redacted>)`. Both conformances are needed:
  `description` covers interpolation and `print`; `debugDescription` covers
  `String(reflecting:)`, `dump` and the debugger. `Options` redacts through its
  embedded key via Swift reflection. A test asserts this.

## Alternatives considered

- **Keep the derives and rely on call-site discipline.** Rejected: that is the
  trap the issue describes. The first careless `{:?}` leaks.
- **Wrap the payload in a `Secret`-style newtype across the FFI.** Rejected:
  UniFFI records and enums must carry plain lowerable types. A newtype would
  need a `custom_type!` conversion and still reach Swift as plain `Data`/`String`.
- **Redact in Rust only.** Rejected: the Swift values are generated
  independently and render through Swift's own protocols, so Rust-side
  redaction does not cover them.

## Consequences

- No formatting path in Rust or Swift prints key material, and logging
  `Options` is safe.
- A caller who needs two `Options` values must construct both. Nothing in the
  binding or its tests needed `Clone`.
- Swift's generated `ChiselKey` still exposes its associated values to pattern
  matching. Redaction covers rendering, not access.
- Any new field on `Options` that is secret must be handled in its hand-written
  `Debug`, and the Swift redaction test should be extended alongside it.
