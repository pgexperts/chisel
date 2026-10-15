# Swift binding: port onto the published lineage and make it a repo member

**Date:** 2026-08-23
**Status:** Implemented per `docs/plans/2026-08-23-swift-binding-port.md`.
**Supersedes nothing.** Extends ADR [0016](../adr/0016-swift-binding-via-uniffi.md) (Swift binding via UniFFI),
which is already Accepted on `main` and records the *shape* of the binding. This
spec covers only getting that shape onto the published tree and finishing it.

## Context

`chisel-ffi/` and `swift/` do not exist on `origin/main`. They live on the local
branch `design/swift-binding`, whose root commit (`46718e2`, "Add Chisel storage
engine design spec") shares **no ancestry** with `origin/main`'s root (`51b3731`,
"Initial release: Chisel v1.0.0"). `git merge-base origin/main design/swift-binding`
reports nothing. Rebase, cherry-pick ranges, and merge are all unavailable; the
only route is a file-level graft onto a branch cut from `origin/main`.

`main` already behaves as though the binding were present:

- ADR 0016 (Swift binding via UniFFI) is Accepted and indexed.
- `.gitignore` carries `swift/Chisel.xcframework/`, `swift/.build/`, `swift/.xcbuild/`
  and the object-file rules, added by SWIFT-2 specifically because the artifacts
  appeared on `main` while the sources did not.
- `ci.yml:29` justifies its macOS test matrix partly with "the project ships …
  a macOS/iOS Swift binding".
- ADR 0025 states: "The Swift binding has the mirror-image savepoint defect
  (SWIFT-3); the shape chosen here is the one to mirror when that binding lands."

So the decision to have this binding is not in question. Only the code is absent.

### Drift, measured rather than assumed

A throwaway worktree cut from `origin/main` with `chisel-ffi/` grafted in
compiles clean and passes all 18 of its Rust tests after exactly three mechanical
changes: the root-package rename, workspace membership, and a comment reference
to `chisel::crypto::Key`. The consumed API surface barely moved:

| Surface | Drift since `design/swift-binding` |
|---|---|
| `Chisel` public methods | +`rekey`, nothing removed or changed |
| `Stats`, `ChiselCounters`, `DefragStats`, `DefragOptions` | none |
| `Options`, `DrainInsertion` | none |
| `ChiselError` variants | +5: `InvalidArgon2Params`, `InvalidFreemapDepth`, `TxnCounterExhausted`, `InvalidNextHandle`, `HandleSpaceExhausted` |
| Root package name | `chisel` 0.1.0 → `chisel-storage` 1.0.0 (`[lib] name = "chisel"` preserved) |
| `crypto` module | now `pub(crate)`; `chisel::Key` is the public path |
| Python surface | +`rekey` (module-level), +`InvalidArgon2ParamsError`; `chisel.pyi` → `__init__.pyi` |

**The clean compile is misleading and must not be read as "the port is correct".**
Two defects hide behind it, described next.

### SWIFT-13 (new): the totality guard cannot be total, and currently mistiers

`chisel-ffi/src/error.rs` maps `chisel::ChiselError` onto a UniFFI error enum and
ends with `_ => Self::Poisoned { message }`. The wildcard is required —
`chisel::ChiselError` is `#[non_exhaustive]`, so no downstream crate may write an
exhaustive match over it. The accompanying test,
`every_engine_variant_maps_to_its_ffi_case`, is a hand-maintained `Vec` of 30
variant instances whose comment reads "Covers all 30 current engine variants."
The engine now has 35. The test passes anyway, because it only asserts that the
variants *it lists* map correctly — it has no way to discover ones it does not list.

The live consequence: all five new variants fall through to `Poisoned`. Four of
them are fatal per `ChiselError::is_fatal()`, so the tier is right by luck.
`InvalidArgon2Params` is **operational** — the Python binding raises
`InvalidArgon2ParamsError(OperationalError)` for it. A Swift caller who supplies an
out-of-range Argon2 cost parameter is currently told the database suffered a fatal
poisoning, and `ChiselError.category` (whose stated authority is the Python
tiering) reports `.fatal`. Reporting `.Poisoned` is doubly wrong: it also asserts
an engine state (`is_poisoned() == true`) that is not true.

The Python binding already fixed this exact fail-open class (I138): its catch-all
routes unmapped variants through `is_fatal()` rather than assuming a tier. The
Swift FFI predates that fix and never inherited it.

## Goals

1. `chisel-ffi/` and `swift/` live on `main`, build, and are covered by CI.
2. The binding is a workspace member, documented in the repo's own docs, and
   consumable by other projects over SwiftPM.
3. Every filed Swift finding is closed: issues #104, #121, #155 (SWIFT-1, -3, -4,
   -5, -6, -7, -8, -9, -10, -11, -12), plus SWIFT-13 above. SWIFT-2 is already fixed.
4. Python↔Swift parity holds against the *current* Python surface, including `rekey`.
5. The Swift test suite covers the hand-written wrapper layer, which is where every
   one of these defects lives.

## Non-goals

- Publishing to a Swift package registry. SwiftPM consumption is by Git URL + tag.
- Changing the engine crate's public API. The port adapts to `main`; `main` does not
  adapt to the port. (One exception is explicitly rejected below.)
- Restructuring the Python binding.
- Re-litigating ADR 0016's UniFFI-over-`Arc<Mutex<Chisel>>` design.
- Bit-reproducible builds of the xcframework (see Risks).

## Verification oracle

The work is done when, from a clean clone on a macOS host with all eight PRs
merged:

```
cargo test --workspace              # engine + bench + chisel-ffi
cargo clippy --workspace -- -D warnings
cargo fmt --check
swift/build-xcframework.sh
CHISEL_LOCAL_XCFRAMEWORK=1 swift test    # repo root; Package.swift moves there in PR 8
```

(Before PR 8 the last line is `cd swift && swift test`.)

all succeed, and:

- `cargo test -p chisel-ffi` runs a totality guard that **fails** if an engine
  `ChiselError` variant has no explicit FFI arm.
- No Swift-facing error reports `.fatal` for an engine variant `is_fatal()` calls false.
- `swift/PARITY.md` cites `python/chisel/__init__.pyi` and every cited Swift symbol
  resolves.
- Issues #104, #121, #155 are closed by the merged PRs.

## Design

### 1. Port mechanics

Branch `feat/swift-binding-port` is cut from `origin/main`. Files are copied from
`design/swift-binding` via `git archive | tar -x` — not cherry-picked — and
committed as a small number of *logical* commits (scaffold + FFI surface, Swift
wrapper layer, CI + workspace wiring, spec/plan docs), each commit body naming
`design/swift-binding` as the historical record.

The 24 original commits are not replayed. Every commit before the drift fixes
fails to compile against `main`'s engine, so a replayed branch would be
non-bisectable and CI-green only at the tip — the granularity would be cosmetic.

`docs/specs/2026-07-18-swift-binding-design.md` and
`docs/plans/2026-07-18-swift-binding.md` come across unchanged as historical
artifacts of the original build.

**Archive risk (act on this first).** `design/swift-binding` is not on `origin`,
and neither is the `archive/pre-1.0-full-history` tag. The 24 Swift commits and
the whole pre-1.0 dev lineage exist only on one machine. The repo is public and a
`pre-scrub-2026-05-04` tag indicates the v1.0.0 lineage was deliberately scrubbed,
so **pushing the old lineage to `origin` is not an acceptable backup** — it would
undo that scrub in public. Recommended instead: `git bundle create` the branch and
store the bundle outside the working tree. This is a prerequisite, not part of any PR.

### 2. Drift closure

- `chisel-ffi/Cargo.toml`: `chisel = { package = "chisel-storage", path = ".." }`.
  The rename is invisible to the source, which keeps `use chisel::…`.
- Root `Cargo.toml`: add `chisel-ffi` to `members`. It stays out of `default-members`
  (Apple-target `staticlib`/`cdylib`), and the surrounding comment is corrected to
  say so — this is SWIFT-10.
- Comment references to `chisel::crypto::Key` become `chisel::Key`.
- UniFFI bindings are regenerated; `swift/Sources/Chisel/Generated/chisel_ffi.swift`
  is a build product committed for SwiftPM's benefit and must be regenerated, never
  hand-edited.

### 3. Error mapping redesign (SWIFT-13)

Three changes to `chisel-ffi/src/error.rs`:

**Explicit arms for all five new variants**, each mapped to a named FFI case whose
tier matches `is_fatal()`. `InvalidArgon2Params` becomes an operational case named
to match Python's `InvalidArgon2ParamsError`.

**The catch-all stops guessing.** A single new case replaces the `Poisoned`
fallback:

```rust
Other { message: String, fatal: bool }
```

with the arm `_ => Self::Other { fatal: e.is_fatal(), message }`. This mirrors
Python's I138 fix — tier by `is_fatal()`, never by assumption — while being
strictly more honest than Python's reuse of its abstract bases: the tier travels
with the value, and `.Poisoned` stops being returned for databases that are not
poisoned. `ChiselError.category` in `Extensions.swift` reads the `fatal` field for
this case. Since the binding has never shipped, no caller depends on the old
behaviour.

**The guard becomes real.** `chisel-ffi` is a workspace-only crate (`publish = false`),
so the engine's source is always at a known path relative to its manifest. The test
reads `concat!(env!("CARGO_MANIFEST_DIR"), "/../src/error.rs")`, extracts the
`ChiselError` variant names from the enum body, and asserts that every one of them
appears in the mapping table. A variant added to the engine and not mapped here
fails the test by name.

This is a text scan, and it fails loudly rather than silently if the enum's
formatting changes — an acceptable trade for a guard that actually detects the
thing its name promises.

An engine-side `#[doc(hidden)]` exhaustive-match helper was considered and
**rejected**: it puts a test-only concern into the engine's public API surface,
and `#[doc(hidden)]` carries no semver guarantee, so downstream crates could come
to depend on it. Nothing in the engine crate changes for this port.

### 4. Findings

| Finding | Fix |
|---|---|
| SWIFT-1 | Reject tag `0` inside `chisel-ffi`'s tagged methods with a typed `InvalidTag` case, so the README's "every throwing call raises `ChiselError`" holds. Python raises `ValueError` here, so exact parity is impossible; the stronger Swift contract wins. |
| SWIFT-3 | `savepoint { }`'s catch arm calls `rollbackTo(name:)` **then** `release(name:)`. The engine deliberately keeps the mark after a rollback, so without the release a retried scope hits `DuplicateSavepoint`. Mirrors ADR 0025. |
| SWIFT-4 | `Transaction` and `Savepoint` become `final class` with a one-shot `finished` flag set on scope exit; every forwarder throws `.AlreadyFinished` once set. This is the shape ADR 0025 chose for Python and the shape the FFI already reserved a variant for. |
| SWIFT-5 | `ChiselDatabase.init` / `ChiselStore.init` `guard path.isFileURL`, throwing otherwise. Covered by a new file-backed test. |
| SWIFT-6 | Hand-written redacting `Debug` for `ChiselKey` and `Options`, mirroring `crypto::Key`'s existing impl; `Clone` dropped from `ChiselKey` unless a call site needs it. Closes #155. |
| SWIFT-7 | See §5. |
| SWIFT-8 | Both Swift `isPoisoned` docs state the Rust condition: poisoned **or** closed. |
| SWIFT-9 | `transaction`/`savepoint` docs state that the catch arm covers a throw from `body` **or** from `commit()`/`release()`. |
| SWIFT-10 | Folded into §2. |
| SWIFT-11 | PARITY.md regenerated against `python/chisel/__init__.pyi`, citing **symbol names** rather than line numbers, which is what rotted. |
| SWIFT-12 | The `DefragOptions` cast test asserts a real boundary (`u32::MAX as u64 + 1`) or the comment drops the truncation claim. |
| SWIFT-13 | See §3. |

`ChiselStore.transaction` takes `@escaping @Sendable (Transaction) throws -> R`.
Making `Transaction` a class means it is not `Sendable`; under Swift 6 strict
concurrency this is an error rather than a warning. `Package.swift` is
`swift-tools-version:5.9` (Swift 5 language mode), so this lands as a warning
today — but the fix must not depend on that. See Risks.

### 5. Parity and tests

`rekey` is added as a UniFFI free function (it is an offline whole-database
operation, not a method on an open handle, matching Python's module-level `rekey`)
with a Swift wrapper and a PARITY.md row.

The Swift suite grows from 156 lines to cover the hand-written wrapper layer,
which is where SWIFT-3, -5, and -8 all live and which `swift test` currently never
touches: `close()` then second `close()`; `isPoisoned` after `close()`; file-backed
open, commit, reopen, read-back through a temp-directory file URL; a non-file URL
rejected; passphrase-encrypted round trip plus wrong-key reopen; a `savepoint { }`
whose body throws, run twice in one transaction; `addKey`/`rotateKey`/`removeKey`;
the three runtime-config setters; an escaped `Transaction` rejected with
`.AlreadyFinished`.

### 6. Distribution

`Package.swift` moves to the repo root, with `path:` overrides keeping sources at
`swift/Sources/Chisel` and tests at `swift/Tests/ChiselTests`. SwiftPM requires the
manifest at the repository root; without this move the package cannot be consumed
by URL at all.

The binary target switches on an environment variable:

- unset (consumers): `.binaryTarget(url:checksum:)` pointing at a versioned
  GitHub release asset.
- `CHISEL_LOCAL_XCFRAMEWORK=1` (this repo's CI, and local development):
  `.binaryTarget(path: "swift/Chisel.xcframework")`, the gitignored artifact
  `build-xcframework.sh` produces.

The switch exists because there is **no release asset yet** — the repo has four
tags and zero GitHub releases. Without it, `swift test` in CI would try to fetch a
URL that does not exist, and a fresh clone could not build.

Release flow, mirroring `publish-crate.yml`'s shape (`on: release: [published]`,
a test gate, `environment: release` for human approval):

1. `swift/release/prepare.sh <version>` builds the xcframework, zips it, computes
   `swift package compute-checksum`, and rewrites the root `Package.swift`'s `url`
   and `checksum` for that tag.
2. The result is committed and tagged; the tag therefore carries a manifest that
   describes its own asset.
3. The release workflow rebuilds, uploads the zip as the release asset, and
   **fails the release** if the rebuilt checksum disagrees with the committed one.

### 7. CI

`.github/workflows/swift.yml` comes across from the branch with two changes: the
`pull_request: branches: [main]` filter is **dropped** (that filter matches the PR's
base, which silently gave every stacked PR zero checks until #151 removed it from
`ci.yml`), and `CHISEL_LOCAL_XCFRAMEWORK=1` is set for the Swift steps.

Its existing structure is kept: `cargo test -p chisel-ffi` (needed because root
`cargo test` honours `default-members` and skips the crate), an iOS cross-compile
check, the xcframework build, `swift test` as the hard gate, and a best-effort
ThreadSanitizer run that cannot redden the pipeline because some macOS hosts' SIP
policy blocks sanitizer injection into the xctest helper.

### 8. Documentation and ADRs

Per repo policy, docs and ADRs are updated **within** the PR that changes the
behaviour, not batched at the end.

- Root `README.md`: the workspace is four members, not three; a Swift binding
  section alongside the Python one; SwiftPM install instructions.
- `ARCHITECTURE.md` / `THEORY.md`: the binding's place in the layer map.
- ADR 0016 gains an `## Addendum (2026-08-23)` recording the port — its body is
  Accepted and is not rewritten.
- New ADRs for decisions this spec makes that 0016 does not cover: the
  `is_fatal()`-driven error fallback, and the SwiftPM distribution shape.

## PR stack

Each PR is independently CI-green and adversarially reviewed before the next.

| PR | Content | Closes |
|---|---|---|
| 1 | Graft + drift closure + workspace + `swift.yml` + spec/plan docs | SWIFT-10 |
| 2 | Error mapping redesign | SWIFT-13 |
| 3 | `rekey` parity | — |
| 4 | SWIFT-6, -1, -5, -3 | #155, part of #104 |
| 5 | SWIFT-4 lifetime guard | rest of #104 |
| 6 | Swift test build-out | SWIFT-7 |
| 7 | SWIFT-8, -9, -11, -12; PARITY regen; root docs | #121 |
| 8 | Root `Package.swift`, release workflow, ADRs | — |

## Risks and accepted consequences

**The totality guard is a source scan, not a compile error.** Rejecting the
engine-side helper means a new engine variant fails a test rather than failing to
compile, and the scan is coupled to `src/error.rs`'s formatting. Accepted: the
failure names the missing variant, a parse failure is itself a test failure, and
the alternative pollutes the engine's public API for a test-only concern.

**`Transaction` as a class is not `Sendable`.** `ChiselStore.transaction`'s body is
`@escaping @Sendable`. Under Swift 6 strict concurrency this is an error. PR 5 must
resolve it properly — most likely by having `ChiselStore` hand the closure a
distinct actor-isolated façade rather than sharing `Transaction` — and not rely on
the manifest's Swift 5 language mode to mute it.

**Rust staticlib builds are not bit-reproducible.** The release workflow's checksum
gate may therefore fail on a legitimate re-tag. If that proves flaky in practice,
the fallback is to upload the artifact produced by `prepare.sh` rather than a CI
rebuild. Recorded here so the first failure is recognised as expected, not as a
supply-chain alarm.

**`main` transiently carries filed defects.** Between PR 1 and PR 7 the binding is
on `main` with known issues. Accepted deliberately: the alternative fuses a
~4,000-line port with 12 semantic fixes into one diff, and this project's review
history shows adversarial review catches wrong *reasoning* far more often than
wrong behaviour — which requires diffs small enough to reason about.

**The archive is single-copy until bundled.** See §1. This is the only item that
risks unrecoverable loss and should be handled before any other work begins.
