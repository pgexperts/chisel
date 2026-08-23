# Swift Binding Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the Chisel Swift/UniFFI binding on `main` as a fully tested, CI-gated, SwiftPM-consumable workspace member with every filed defect closed.

**Architecture:** `chisel-ffi` is the only crate speaking the C ABI; it wraps `chisel::Chisel` in `Mutex<Option<Chisel>>` and exports a flat synchronous surface to UniFFI. All ergonomics (transaction closures, the async actor) live in the hand-written Swift layer above the generated bindings. The engine crate is not modified by any task in this plan.

**Tech Stack:** Rust 1.82 (MSRV), UniFFI 0.29, Swift 6.0.3 / swift-tools-version 5.9, XCTest, GitHub Actions on `macos-14`.

**Spec:** [`docs/specs/2026-08-23-swift-binding-port-design.md`](../specs/2026-08-23-swift-binding-port-design.md)

## Global Constraints

These apply to **every** task. They are not repeated per task.

- **MSRV is 1.82.** `chisel-ffi` inherits it via the path dependency. Do not introduce a dependency requiring edition2024 / Rust 1.85+.
- **Never write comma-grouped `case .A, .B, .C:` arms in a Swift `switch` over `ChiselError`.** It compiles and then SIGSEGVs at runtime in **debug builds only** — a Swift 6.0.3 `-Onone` codegen bug on a large mixed-arity enum with payloads. One `case:` per line, always. This is documented in `swift/Sources/Chisel/Extensions.swift`'s header; do not "clean it up".
- **The engine crate (`src/`) is not modified.** If a task appears to require an engine change, stop and escalate rather than editing `src/`.
- **`swift/Sources/Chisel/Generated/chisel_ffi.swift` is a build product.** Regenerate it with `swift/build-xcframework.sh`. Never hand-edit it. It is committed so SwiftPM consumers do not need Rust.
- **Rust doc-comment list items use a 2-space hanging indent.** A 4-space indent trips clippy's `doc_overindented_list_items` and `cargo clippy -- -D warnings` fails.
- **No references to Claude, Anthropic, or AI assistance** in commit messages, code comments, or documentation.
- **Every commit message ends with:** `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`
- **Gating checks** (must pass before any PR is opened):
  ```
  cargo test                                # engine + bench; honours default-members
  cargo test -p chisel-ffi                  # NOT covered by the line above
  cargo clippy --workspace -- -D warnings   # --workspace is correct here: linting does not link
  cargo fmt --check
  ```
  **Never `cargo test --workspace`.** It pulls in `chisel-py`, whose pyo3
  `extension-module` feature deliberately leaves Python symbols undefined for the
  interpreter to resolve at load time; only maturin supplies the linker flags that
  make that work. This is exactly what `default-members` exists to prevent, and the
  repo's own README documents `cargo test` for the same reason.
  plus, for any task touching `swift/` or `chisel-ffi/`:
  ```
  swift/build-xcframework.sh && (cd swift && swift test)
  ```
- **PR workflow:** each PR group below is its own PR with `--base main`. Never amend or force-push a branch whose PR has been opened; open a new PR for follow-up work.
- **Base branch:** `feat/swift-binding-port`, already cut from `origin/main` at `67cc396`.

## Prerequisite (do this before Task 1)

`design/swift-binding` exists only on this machine and is not on `origin`. A bundle was created at
`<scratchpad>/chisel-swift-binding-archive.bundle` (2.2 MB, verified complete). **Move it somewhere durable
before starting.** Do not push the branch to `origin`: the repo is public and the `pre-scrub-2026-05-04`
tag indicates the pre-1.0 lineage was deliberately scrubbed.

## File Structure

**Created by the graft (Task 1), unmodified from `design/swift-binding`:**

| File | Responsibility |
|---|---|
| `chisel-ffi/Cargo.toml` | Crate manifest; `staticlib`+`cdylib`+`lib`, uniffi-bindgen bin |
| `chisel-ffi/src/lib.rs` | UniFFI scaffolding, module wiring, `library_version()`, `Send` proof |
| `chisel-ffi/src/types.rs` | FFI value records/enums + engine conversions; the only file knowing both shapes |
| `chisel-ffi/src/error.rs` | The `chisel::ChiselError` → UniFFI error enum translation |
| `chisel-ffi/src/database.rs` | `NativeDatabase`, the exported object; `with_db` is the single lock point |
| `chisel-ffi/src/bin/uniffi-bindgen.rs` | Bindgen entry point |
| `swift/Sources/Chisel/ChiselDatabase.swift` | Sync wrapper: `ChiselDatabase`, `Transaction`, `Savepoint` |
| `swift/Sources/Chisel/ChiselStore.swift` | Async actor over a private serial queue |
| `swift/Sources/Chisel/Extensions.swift` | `ChiselError.category` / `.isPoisoning` / `.message` |
| `swift/Sources/Chisel/Keychain.swift` | Keychain-backed `ChiselKey` retrieval (Swift-only addition) |
| `swift/Sources/Chisel/Generated/chisel_ffi.swift` | UniFFI output (build product, committed) |
| `swift/Package.swift` | SPM manifest (moves to repo root in Task 18) |
| `swift/build-xcframework.sh` | Builds all 5 Apple slices, lipos, generates bindings |
| `swift/Tests/ChiselTests/*.swift` | XCTest suite |
| `swift/PARITY.md` | Python↔Swift parity table |
| `swift/README.md` | Binding usage docs |

**Modified on `main`:**

| File | Change |
|---|---|
| `Cargo.toml` | `chisel-ffi` added to `members`; SWIFT-10 comment corrections |
| `README.md` | Four workspace members; Swift binding section; SwiftPM install |
| `ARCHITECTURE.md`, `THEORY.md` | Binding's place in the layer map |
| `docs/adr/0016-swift-binding-via-uniffi.md` | Addendum recording the port |
| `docs/adr/README.md` | Regenerated index (never hand-edited) |

**Created new:**

| File | Responsibility |
|---|---|
| `.github/workflows/swift.yml` | macOS gate: `cargo test -p chisel-ffi`, iOS cross-compile, xcframework, `swift test` |
| `.github/workflows/swift-release.yml` | Builds + uploads the xcframework release asset |
| `swift/release/prepare.sh` | Builds, zips, checksums, rewrites root `Package.swift` |
| `Package.swift` (repo root) | SPM manifest with env-switched binary target |
| `docs/adr/00NN-*.md` | Two new ADRs (error fallback, distribution shape) |

---
# PR 1 — Port onto the published lineage

Lands the binding compiling and tested. No behaviour is fixed here; defects come later so a
port regression stays distinguishable from a fix regression.

### Task 1: Graft the crates and wire the workspace

**Files:**
- Create: `chisel-ffi/` (6 files), `swift/` (16 files), `docs/specs/2026-07-18-swift-binding-design.md`, `docs/plans/2026-07-18-swift-binding.md`
- Modify: `chisel-ffi/Cargo.toml` (dependency rename), `Cargo.toml` (members + SWIFT-10 comments)

**Interfaces:**
- Consumes: nothing.
- Produces: the crate `chisel-ffi` with `NativeDatabase`, `ChiselError`, `FfiResult<T>`, `Options`, `Stats`, `Counters`, `DefragOptions`, `DefragStats`, `DrainInsertion`, `ChiselKey`, `Argon2Params`, `TagDropProgress`, and `library_version() -> String`. Every later task builds on these exact names.

- [ ] **Step 1: Graft the files**

```bash
git archive design/swift-binding chisel-ffi | tar -x -C .
git archive design/swift-binding swift | tar -x -C .
git archive design/swift-binding \
  docs/specs/2026-07-18-swift-binding-design.md \
  docs/plans/2026-07-18-swift-binding.md | tar -x -C .
```

- [ ] **Step 2: Verify the graft is complete and nothing else moved**

Run: `git status --short`
Expected: exactly `?? chisel-ffi/`, `?? swift/`, and the two `?? docs/` files. No modifications to `src/`, `python/`, or `bench/`. If `src/` shows changes, the archive extracted the wrong tree — reset and redo.

- [ ] **Step 3: Rename the engine dependency**

`main` renamed the root package `chisel` → `chisel-storage` (v1.0.0) while keeping `[lib] name = "chisel"`. The `package =` key preserves every `use chisel::…` in the source unchanged.

In `chisel-ffi/Cargo.toml`, replace:

```toml
chisel = { path = ".." }
```

with:

```toml
# The root package is published as "chisel-storage" (the bare "chisel" name was
# taken on crates.io); its [lib] name is still "chisel", so `package =` here
# keeps every `use chisel::...` in this crate compiling unchanged.
chisel = { package = "chisel-storage", path = ".." }
```

- [ ] **Step 4: Add the workspace member and correct the stale comments (SWIFT-10)**

In the root `Cargo.toml`, replace:

```toml
members = [".", "python", "bench"]
```

with:

```toml
members = [".", "python", "bench", "chisel-ffi"]
```

In the header comment above it, replace the phrase `the engine, the PyO3 binding, and the bench harness in one shot` with `the engine, the PyO3 binding, the bench harness, and the UniFFI binding in one shot`.

Then replace the opening sentence of the `default-members` paragraph — `` `default-members` excludes `python` from `cargo build` / `cargo test` from the workspace root. `` — with:

```
# `default-members` excludes BOTH `python` and `chisel-ffi` from `cargo build` /
# `cargo test` at the workspace root, for different reasons.
#
# `chisel-ffi` builds `staticlib`/`cdylib` artifacts for Apple targets and is
# consumed by the Swift package, not by any Rust binary here; a root `cargo
# test` therefore does NOT run its unit tests. That gap is covered by the
# explicit `cargo test -p chisel-ffi` step in .github/workflows/swift.yml,
# which is the only CI job on a macOS runner. Anyone running the documented
# local pre-push checks must run that command too or they are shipping an
# untested FFI crate.
#
# The `python` exclusion is a linker constraint:
```

(The nine existing lines about pyo3's `extension-module` follow unchanged.)

- [ ] **Step 5: Verify the crate compiles and its tests pass against `main`'s engine**

Run: `cargo test -p chisel-ffi`
Expected: PASS, 18 tests. If `From<chisel::ChiselError>` fails to compile, the graft picked up a stale tree — recheck Step 1.

- [ ] **Step 6: Verify the workspace is clean**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --check`
Expected: both clean. `--workspace` overrides `default-members`, so this does lint `chisel-ffi`.

- [ ] **Step 7: Commit**

```bash
git add chisel-ffi Cargo.toml docs/specs/2026-07-18-swift-binding-design.md docs/plans/2026-07-18-swift-binding.md
git commit -m "feat(ffi): port the chisel-ffi UniFFI crate onto the published lineage

Grafted from the local design/swift-binding branch, which shares no ancestry
with main (separate roots: the v1.0.0 squash vs the pre-1.0 dev lineage), so
the 24 original commits cannot be replayed. That branch remains the
historical record.

Two changes were needed to build against main's engine: the root package is
now chisel-storage, and chisel-ffi joins the workspace. The consumed API
surface was otherwise unchanged.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Land the Swift package and its CI gate

**Files:**
- Create: `.github/workflows/swift.yml`
- Already grafted in Task 1: `swift/`

**Interfaces:**
- Consumes: `chisel-ffi` from Task 1.
- Produces: `ChiselDatabase`, `ChiselStore`, `Transaction`, `Savepoint`, `ChiselError.category`/`.isPoisoning`/`.message`, `ChiselKey.keychain(service:account:)`. A green `swift test`.

- [ ] **Step 1: Build the xcframework and confirm the Swift suite passes**

Run: `swift/build-xcframework.sh && (cd swift && swift test)`
Expected: PASS, 12 test methods across 6 files. This is the first time the grafted Swift layer meets `main`'s engine; a failure here is a real drift signal, not a flake.

- [ ] **Step 2: Add the CI workflow**

Create `.github/workflows/swift.yml` with the content of `git show design/swift-binding:.github/workflows/swift.yml`, changing **only** the trigger block. Replace:

```yaml
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
```

with:

```yaml
on:
  push:
    branches: [main]
  # Deliberately NOT filtered to `branches: [main]`, matching ci.yml. That
  # filter matches the PR's BASE, so a stacked PR — which is how this
  # binding's fixes land — got no CI at all, silently: the PR showed no
  # checks, reading as "nothing to run" rather than "never triggered". See
  # the same note in ci.yml (issue #151).
  pull_request:
```

- [ ] **Step 3: Verify the workflow parses**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/swift.yml')); print('ok')"`
Expected: `ok`

- [ ] **Step 4: Commit**

```bash
git add swift .github/workflows/swift.yml
git commit -m "feat(swift): port the Swift package and add its macOS CI gate

swift.yml runs the checks a root \`cargo test\` cannot: chisel-ffi's own unit
tests (excluded by default-members), the iOS cross-compile slices, the
xcframework build, and \`swift test\` as the hard gate. The ThreadSanitizer
step stays best-effort because some macOS hosts' SIP policy blocks sanitizer
injection into the xctest helper, which is a host limitation rather than a
code fault.

The pull_request trigger drops the branches filter, matching ci.yml — that
filter matches the PR base and silently gave stacked PRs zero checks.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

- [ ] **Step 5: Open the PR**

```bash
git push -u origin feat/swift-binding-port
gh pr create --base main --title "Port the Swift/UniFFI binding onto the published lineage" --body "$(cat <<'BODY'
Lands `chisel-ffi/` and `swift/` on `main` for the first time. ADR 0016 already
records the design; only the code was missing.

The binding lived on `design/swift-binding`, which shares no ancestor with
`main` — separate roots. A graft is the only available mechanism; the branch
remains the historical record for the original 24 commits.

Drift against `main`'s engine turned out to be two mechanical changes: the root
package is now `chisel-storage`, and `chisel-ffi` joins the workspace. The
consumed API surface (`Chisel`'s methods, `Stats`, `Counters`, `DefragStats`,
`DefragOptions`) was otherwise unchanged.

Known defects are deliberately NOT fixed here — see #104, #121, #155, and the
`SWIFT-13` finding in the design spec. They land as reviewable follow-ups so a
port regression stays distinguishable from a fix regression.

Spec: `docs/specs/2026-08-23-swift-binding-port-design.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 2 — Error mapping: close SWIFT-13

Five engine variants added since the binding was written currently fall through
`_ => Poisoned`. Four are fatal so the tier is right by luck; `InvalidArgon2Params`
is **operational**, so a Swift caller who mistunes an Argon2 cost parameter is told
the database suffered a fatal poisoning. The guard that should have caught this is a
hand-maintained list that cannot detect a variant it does not already name.

Branch: `fix/swift-13-error-mapping`, cut from the PR 1 branch.

### Task 3: Map the five new variants and replace the guessing fallback

**Files:**
- Modify: `chisel-ffi/src/error.rs` (enum, `message()`, `From` impl, `ffi_name` in tests)

**Interfaces:**
- Consumes: `chisel-ffi` from Task 1.
- Produces: FFI `ChiselError` cases `InvalidArgon2Params { message }`, `InvalidFreemapDepth { message }`, `InvalidNextHandle { message }`, `TxnCounterExhausted { message }`, `HandleSpaceExhausted { message }`, and `Other { message: String, fatal: bool }`. Tasks 5 and 11 switch over these exact names.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `chisel-ffi/src/error.rs`:

```rust
    // SWIFT-13. The engine's `is_fatal()` is the authority on whether a
    // variant means "the database is intact" or "integrity is in question".
    // Before this fix all five of these fell through `_ => Poisoned`, which
    // reports fatal unconditionally — right by luck for four of them and
    // flatly wrong for InvalidArgon2Params, an out-of-range cost parameter
    // rejected before the file is touched.
    #[test]
    fn new_engine_variants_map_to_their_own_cases_at_the_right_tier() {
        use chisel::ChiselError as E;

        let operational = E::InvalidArgon2Params {
            m_cost: 1,
            t_cost: 1,
            p_cost: 1,
        };
        assert!(!operational.is_fatal(), "engine tiering changed under us");
        assert_eq!(ffi_name(&ChiselError::from(operational)), "InvalidArgon2Params");

        let fatal: Vec<(E, &str)> = vec![
            (
                E::InvalidFreemapDepth { stored: 9, max: 3 },
                "InvalidFreemapDepth",
            ),
            (
                E::InvalidNextHandle { stored: 9, max: 3 },
                "InvalidNextHandle",
            ),
            (E::TxnCounterExhausted { current: 7 }, "TxnCounterExhausted"),
            (
                E::HandleSpaceExhausted { current: 7 },
                "HandleSpaceExhausted",
            ),
        ];
        for (engine, expected) in fatal {
            assert!(engine.is_fatal(), "{expected} should be fatal per the engine");
            assert_eq!(ffi_name(&ChiselError::from(engine)), expected);
        }
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p chisel-ffi new_engine_variants_map_to_their_own_cases_at_the_right_tier`
Expected: FAIL — `assertion failed: left == right`, with `"Poisoned"` on the left for the first assertion. That failure *is* the bug.

- [ ] **Step 3: Add the five variants and the tiered fallback to the enum**

In `chisel-ffi/src/error.rs`, add `InvalidArgon2Params` to the **operational** group (immediately after `InvalidSuperblockCount`):

```rust
    InvalidArgon2Params {
        message: String,
    },
```

Add the four fatal variants to the **fatal** group (immediately after `InvalidPageId`):

```rust
    InvalidFreemapDepth {
        message: String,
    },
    InvalidNextHandle {
        message: String,
    },
    TxnCounterExhausted {
        message: String,
    },
    HandleSpaceExhausted {
        message: String,
    },
```

At the very end of the enum, after `DecryptionFailed`, add the fallback:

```rust
    /// An engine variant added AFTER this crate was last updated. `fatal`
    /// carries the engine's own `is_fatal()` verdict, so the caller-facing
    /// tier is never guessed.
    ///
    /// This replaces a `_ => Poisoned` fallback that reported every unmapped
    /// variant as a fatal poisoning. That was wrong twice over: it mistiers
    /// operational variants (an out-of-range Argon2 parameter is not a
    /// corrupt database), and `Poisoned` additionally asserts an engine
    /// state — `is_poisoned() == true` — that is not true for a handle that
    /// merely rejected a bad argument.
    ///
    /// Reaching this case is not an error, but it does mean the mapping is
    /// stale; `every_engine_variant_has_an_explicit_ffi_arm` fails as soon as
    /// a variant exists with no arm of its own.
    Other {
        message: String,
        fatal: bool,
    },
```

- [ ] **Step 4: Add the explicit `From` arms and retire the wildcard**

In `impl From<chisel::ChiselError> for ChiselError`, add after the `E::InvalidSuperblockCount` arm:

```rust
            E::InvalidArgon2Params { .. } => Self::InvalidArgon2Params { message },
```

and after the `E::InvalidPageId` arm:

```rust
            E::InvalidFreemapDepth { .. } => Self::InvalidFreemapDepth { message },
            E::InvalidNextHandle { .. } => Self::InvalidNextHandle { message },
            E::TxnCounterExhausted { .. } => Self::TxnCounterExhausted { message },
            E::HandleSpaceExhausted { .. } => Self::HandleSpaceExhausted { message },
```

Then replace the whole trailing wildcard arm — the comment block plus `_ => Self::Poisoned { message },` — with:

```rust
            // `chisel::ChiselError` is `#[non_exhaustive]`, so this match
            // needs a wildcard even though every variant that exists today is
            // listed explicitly above.
            //
            // It does NOT guess the tier. The previous version routed
            // everything here to `Poisoned` on a fail-toward-fatal argument,
            // which mistiered every future operational variant and claimed an
            // engine state that was not true. `is_fatal()` is the engine's own
            // answer to exactly this question, so ask it — the same fix the
            // Python binding made for the same fail-open class (I138).
            //
            // `message` (captured above from the still-real `Display`) is
            // preserved, so a not-yet-mapped variant still surfaces its actual
            // diagnostic text.
            other => Self::Other {
                fatal: other.is_fatal(),
                message,
            },
```

- [ ] **Step 5: Extend `message()` and `ffi_name`**

In `ChiselError::message()`, add these to the or-pattern chain (they bind `message` identically, so they join the existing chain):

```rust
            | Self::InvalidArgon2Params { message }
            | Self::InvalidFreemapDepth { message }
            | Self::InvalidNextHandle { message }
            | Self::TxnCounterExhausted { message }
            | Self::HandleSpaceExhausted { message }
            | Self::Other { message, .. }
```

In the test helper `ffi_name`, add the six arms (it has no wildcard, so it will not compile until you do):

```rust
            ChiselError::InvalidArgon2Params { .. } => "InvalidArgon2Params",
            ChiselError::InvalidFreemapDepth { .. } => "InvalidFreemapDepth",
            ChiselError::InvalidNextHandle { .. } => "InvalidNextHandle",
            ChiselError::TxnCounterExhausted { .. } => "TxnCounterExhausted",
            ChiselError::HandleSpaceExhausted { .. } => "HandleSpaceExhausted",
            ChiselError::Other { .. } => "Other",
```

- [ ] **Step 6: Update the legacy coverage test's five new cases and its count**

The existing `every_engine_variant_maps_to_its_ffi_case` asserts `cases.len() == 30` with the message `"expected all 30 current engine variants to be covered"`. Add the five new pairs to its `cases` vec:

```rust
            (
                E::InvalidArgon2Params {
                    m_cost: 1,
                    t_cost: 1,
                    p_cost: 1,
                },
                "InvalidArgon2Params",
            ),
            (
                E::InvalidFreemapDepth { stored: 9, max: 3 },
                "InvalidFreemapDepth",
            ),
            (
                E::InvalidNextHandle { stored: 9, max: 3 },
                "InvalidNextHandle",
            ),
            (E::TxnCounterExhausted { current: 7 }, "TxnCounterExhausted"),
            (
                E::HandleSpaceExhausted { current: 7 },
                "HandleSpaceExhausted",
            ),
```

and change the count assertion to `35` with the message `"expected all 35 current engine variants to be covered"`.

Also update its "no silently-dropped error" guard, which currently special-cases `Poisoned`. Replace:

```rust
            if expected != "Poisoned" {
                assert_ne!(
                    name, "Poisoned",
                    "{expected} fell through to the wildcard instead of its own arm"
                );
            }
```

with:

```rust
            // The wildcard's target is now `Other`, never `Poisoned`. Any
            // variant surfacing as `Other` fell through instead of reaching
            // its own arm — which is exactly what this test exists to catch.
            assert_ne!(
                name, "Other",
                "{expected} fell through to the wildcard instead of its own arm"
            );
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p chisel-ffi`
Expected: PASS, 19 tests (18 existing plus this one; Task 4 adds the twentieth). Confirm `new_engine_variants_map_to_their_own_cases_at_the_right_tier` passes.

- [ ] **Step 8: Commit**

```bash
git add chisel-ffi/src/error.rs
git commit -m "fix(ffi): tier unmapped engine errors by is_fatal instead of assuming fatal

Five variants added to the engine since this crate was written fell through
the wildcard to Poisoned. Four are fatal so the tier was right by accident;
InvalidArgon2Params is operational, so a caller who supplied an out-of-range
cost parameter was told the database had suffered a fatal poisoning. Poisoned
also asserts is_poisoned() == true, which was not the case.

All five now have explicit arms. The wildcard becomes Other { message, fatal }
and reads the engine's own is_fatal() rather than guessing — the fix the
Python binding already made for this fail-open class (I138).

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Make the totality guard actually total

**Files:**
- Modify: `chisel-ffi/src/error.rs` (tests module)

**Interfaces:**
- Consumes: the `From` impl from Task 3.
- Produces: `every_engine_variant_has_an_explicit_ffi_arm`, the test that fails when a future engine variant has no arm.

- [ ] **Step 1: Write the failing test**

The existing guard is a hand-maintained `Vec` — it can only check variants it already names, which is why five went unnoticed. This one derives the variant set from the engine's source. Append to the `tests` module:

```rust
    // SWIFT-13. `chisel::ChiselError` is `#[non_exhaustive]`, so no downstream
    // crate may match it exhaustively and the compiler cannot enforce this.
    // The list-based test above can only check variants it already names —
    // which is precisely how five variants went unmapped.
    //
    // `chisel-ffi` is `publish = false` and workspace-only, so the engine's
    // source is always at a fixed path relative to this manifest. Parsing it
    // is the only way to learn the true variant set without adding a
    // test-only helper to the engine's public API.
    //
    // A parse that finds implausibly few variants is itself a failure, so a
    // reformatting of `src/error.rs` breaks this loudly rather than quietly
    // passing on an empty set.
    #[test]
    fn every_engine_variant_has_an_explicit_ffi_arm() {
        let engine_src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../src/error.rs"
        ))
        .expect("engine src/error.rs must be readable from within the workspace");

        let enum_body = engine_src
            .split_once("pub enum ChiselError {")
            .expect("engine error.rs must declare `pub enum ChiselError {`")
            .1
            .split_once("\n}")
            .expect("the ChiselError enum must end with a line-start closing brace")
            .0;

        // Variant lines are exactly those at 4-space indent starting with an
        // uppercase ASCII letter. Doc comments (`///`), attributes (`#[`),
        // and field lines (8-space indent, lowercase) are all excluded.
        let variants: Vec<String> = enum_body
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                line.len() - trimmed.len() == 4
                    && trimmed.starts_with(|c: char| c.is_ascii_uppercase())
            })
            .map(|line| {
                line.trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect()
            })
            .collect();

        assert!(
            variants.len() >= 30,
            "parsed only {} variants out of src/error.rs — the parser is \
             broken, not the enum. Fix this test before trusting it.",
            variants.len()
        );

        // Our own source, so the check tracks the real `From` impl rather
        // than a second list that can drift from it.
        let from_impl = include_str!("error.rs")
            .split_once("impl From<chisel::ChiselError> for ChiselError {")
            .expect("this file must contain the From impl")
            .1
            .split_once("\n}")
            .expect("the From impl must end with a line-start closing brace")
            .0;

        let unmapped: Vec<&String> = variants
            .iter()
            .filter(|v| !from_impl.contains(&format!("E::{v}")))
            .collect();

        assert!(
            unmapped.is_empty(),
            "engine ChiselError variants with no explicit arm in \
             `From<chisel::ChiselError>`: {unmapped:?}\n\
             Add an arm for each, plus a matching FFI case, a `message()` \
             arm, an `ffi_name` arm, and a `category` arm in \
             swift/Sources/Chisel/Extensions.swift. The `Other` fallback \
             exists for variants added after this crate was last updated — \
             it is not a resting place for one that already exists."
        );
    }
```

- [ ] **Step 2: Run it to verify it passes now, and would have failed before**

Run: `cargo test -p chisel-ffi every_engine_variant_has_an_explicit_ffi_arm`
Expected: PASS (Task 3 added all five arms).

To confirm the test has teeth rather than passing vacuously, temporarily comment out the `E::TxnCounterExhausted { .. } => …` arm added in Task 3 and rerun:
Expected: FAIL, naming `["TxnCounterExhausted"]`. **Restore the arm afterwards** and rerun to confirm PASS.

- [ ] **Step 3: Delete the superseded count assertion**

The list-based test's `assert_eq!(cases.len(), 35, ...)` is now redundant — the scanning test enforces coverage, and a hardcoded count is exactly the thing that went stale. Remove that assertion and its message, keeping the rest of the test (it still verifies each variant maps to the *correct* case, which the scan does not check).

- [ ] **Step 4: Run the full crate suite**

Run: `cargo test -p chisel-ffi && cargo clippy --workspace -- -D warnings && cargo fmt --check`
Expected: PASS, 20 tests; clippy and fmt clean.

- [ ] **Step 5: Commit**

```bash
git add chisel-ffi/src/error.rs
git commit -m "test(ffi): derive the error-mapping guard from the engine's source

The old guard was a hand-maintained list of 30 variant instances whose
comment claimed it covered them all. The engine had 35. A list cannot detect
a variant it does not already name, which is how five went unmapped while a
test called every_engine_variant_maps_to_its_ffi_case passed.

The new test parses the enum out of src/error.rs and asserts each variant has
an explicit arm in this crate's From impl. It is a text scan and fails loudly
if the parse degrades, which is the trade for a guard the language otherwise
forbids: #[non_exhaustive] blocks an exhaustive match from downstream, and a
test-only helper on the engine's public API would outlive its purpose.

The hardcoded count assertion is gone — it was the stale thing.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: Classify the new cases on the Swift side

**Files:**
- Modify: `swift/Sources/Chisel/Extensions.swift` (`category`, `message`)
- Modify: `swift/Tests/ChiselTests/ErrorTests.swift`
- Regenerate: `swift/Sources/Chisel/Generated/chisel_ffi.swift`

**Interfaces:**
- Consumes: the FFI cases from Task 3.
- Produces: `ChiselError.category` and `.message` total over the new cases.

- [ ] **Step 1: Regenerate the bindings**

Run: `swift/build-xcframework.sh`
Expected: succeeds; `git diff --stat swift/Sources/Chisel/Generated/chisel_ffi.swift` shows the six new cases.

- [ ] **Step 2: Run the Swift build to see the compile-time tripwire fire**

Run: `cd swift && swift build`
Expected: FAIL — `switch must be exhaustive` in `Extensions.swift`, twice (`category` and `message`). Both switches deliberately have no `default:` arm precisely so this happens.

- [ ] **Step 3: Classify the new cases**

**Do not use comma-grouped `case` arms** — see Global Constraints.

In `category`, add to the operational group:

```swift
        case .InvalidArgon2Params: return .operational
```

to the fatal group:

```swift
        case .InvalidFreemapDepth: return .fatal
        case .InvalidNextHandle: return .fatal
        case .TxnCounterExhausted: return .fatal
        case .HandleSpaceExhausted: return .fatal
```

and, at the end, the fallback — the one case whose tier is data rather than a constant:

```swift
        // The Rust side sets `fatal` from the engine's own is_fatal(), so an
        // engine variant added after this binding was last regenerated still
        // reaches the caller at the right tier instead of being guessed.
        case .Other(_, let fatal): return fatal ? .fatal : .operational
```

In `message`, add:

```swift
        case .InvalidArgon2Params(let m): return m
        case .InvalidFreemapDepth(let m): return m
        case .InvalidNextHandle(let m): return m
        case .TxnCounterExhausted(let m): return m
        case .HandleSpaceExhausted(let m): return m
        case .Other(let m, _): return m
```

- [ ] **Step 4: Write the failing test**

Add to `swift/Tests/ChiselTests/ErrorTests.swift`:

```swift
    // The five variants the engine grew after this binding was written. Before
    // the SWIFT-13 fix every one of them arrived as `.Poisoned`, so
    // InvalidArgon2Params — a rejected cost parameter, with the database
    // untouched — was reported to callers as a fatal poisoning.
    func testVariantsAddedAfterTheBindingClassifyCorrectly() {
        XCTAssertEqual(ChiselError.InvalidArgon2Params(message: "x").category, .operational)
        XCTAssertEqual(ChiselError.InvalidFreemapDepth(message: "x").category, .fatal)
        XCTAssertEqual(ChiselError.InvalidNextHandle(message: "x").category, .fatal)
        XCTAssertEqual(ChiselError.TxnCounterExhausted(message: "x").category, .fatal)
        XCTAssertEqual(ChiselError.HandleSpaceExhausted(message: "x").category, .fatal)
    }

    // `.Other` carries the engine's is_fatal() verdict rather than a constant,
    // so a future unmapped variant still reaches the caller at the right tier.
    func testOtherCarriesItsTier() {
        XCTAssertEqual(ChiselError.Other(message: "x", fatal: false).category, .operational)
        XCTAssertEqual(ChiselError.Other(message: "x", fatal: true).category, .fatal)
        XCTAssertTrue(ChiselError.Other(message: "x", fatal: true).isPoisoning)
        XCTAssertEqual(ChiselError.Other(message: "boom", fatal: true).message, "boom")
    }
```

- [ ] **Step 5: Run the Swift suite**

Run: `cd swift && swift test`
Expected: PASS, 14 test methods.

- [ ] **Step 6: Run the debug-build check that the SIGSEGV bug hides behind**

Run: `cd swift && swift test 2>&1 | tail -5`
Expected: PASS, no SIGSEGV. If the suite crashes here but passes under `swift test -c release`, a comma-grouped `case` arm was introduced — see Global Constraints.

- [ ] **Step 7: Commit and open the PR**

```bash
git add swift/Sources/Chisel/Extensions.swift swift/Sources/Chisel/Generated/chisel_ffi.swift swift/Tests/ChiselTests/ErrorTests.swift
git commit -m "fix(swift): classify the five late engine variants and the Other fallback

Extensions.swift's category/message switches have no default arm by design,
so regenerating the bindings broke the build until each new case was
classified — the tripwire working as intended.

.Other reads its tier from the payload rather than a constant, so a variant
added after the next regeneration still reaches callers correctly tiered.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin fix/swift-13-error-mapping
gh pr create --base main --title "Tier unmapped engine errors by is_fatal (SWIFT-13)" --body "$(cat <<'BODY'
Five `ChiselError` variants added to the engine since the binding was written
fell through `_ => Poisoned`. Four are fatal, so the tier was right by
accident. `InvalidArgon2Params` is operational — an out-of-range cost
parameter, rejected before the file is opened — and callers were told the
database had suffered a fatal poisoning.

The guard that should have caught this, `every_engine_variant_maps_to_its_ffi_case`,
is a hand-maintained list of 30 instances whose comment claimed it covered
them all. A list cannot detect a variant it does not name.

- All five variants get explicit arms.
- The wildcard becomes `Other { message, fatal }` and reads the engine's own
  `is_fatal()`, matching the Python binding's I138 fix for the same fail-open class.
- The guard now parses the engine's enum out of `src/error.rs` and fails
  naming any variant with no arm.

No engine code changed.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 3 — Close the parity gap: `rekey`

`main` grew `Chisel::rekey` and a module-level Python `rekey(path, key)` after the
binding was written. Branch: `feat/swift-rekey`, cut from the PR 2 branch.

### Task 6: Export `rekey` through the FFI and the Swift layer

**Files:**
- Modify: `chisel-ffi/src/lib.rs` (free function + test)
- Create: `swift/Sources/Chisel/Rekey.swift`
- Modify: `swift/Tests/ChiselTests/` (new `RekeyTests.swift`), `swift/PARITY.md`
- Regenerate: `swift/Sources/Chisel/Generated/chisel_ffi.swift`

**Interfaces:**
- Consumes: `ChiselKey`, `Argon2Params`, `FfiResult` from Task 1; `ChiselError` cases from Task 3.
- Produces: Rust `pub fn rekey(path: String, key: ChiselKey, argon2_params: Option<Argon2Params>) -> FfiResult<()>`; Swift `public func rekey(at:key:argon2Params:) throws`.

- [ ] **Step 1: Write the failing Rust test**

Append a `tests` module to `chisel-ffi/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    // `use super::*` is the whole import list: lib.rs already re-exports
    // ChiselError, ChiselKey, Options and NativeDatabase, so naming them again
    // here is E0252 (defined multiple times), not a harmless duplicate.
    use super::*;

    // rekey takes a PATH, not an open handle: it builds a replacement file
    // beside the original and publishes it with a rename, so a handle opened
    // beforehand would afterwards refer to the original, now-unlinked inode.
    // Close, rekey, reopen — which is exactly what this test does.
    #[test]
    fn rekey_rewrites_under_a_fresh_data_key_and_preserves_content() {
        let dir = std::env::temp_dir().join(format!("chisel-ffi-rekey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rk.db");
        let path_s: String = path.to_string_lossy().into();
        let key = || ChiselKey::Passphrase {
            phrase: "correct horse".into(),
        };

        let mut opts = Options::default_ffi();
        opts.encryption_key = Some(key());
        let db = NativeDatabase::new(Some(path_s.clone()), opts).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"secret".to_vec()).unwrap();
        db.set_root_name("r".into(), h).unwrap();
        db.commit().unwrap();
        db.close().unwrap();

        rekey(path_s.clone(), key(), None).expect("rekey succeeds");

        // Same credential still opens it, and the value survived the rewrite.
        let mut reopen = Options::default_ffi();
        reopen.encryption_key = Some(key());
        let db = NativeDatabase::new(Some(path_s), reopen).unwrap();
        let root = db.get_root_name("r".into()).unwrap().expect("root survived");
        assert_eq!(db.read(root).unwrap(), b"secret".to_vec());
        db.close().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rekey_on_a_missing_path_is_database_file_not_found() {
        let missing = std::env::temp_dir().join("chisel-ffi-definitely-absent.db");
        assert!(matches!(
            rekey(
                missing.to_string_lossy().into(),
                ChiselKey::Passphrase { phrase: "x".into() },
                None
            ),
            Err(ChiselError::DatabaseFileNotFound { .. })
        ));
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p chisel-ffi rekey`
Expected: FAIL to compile — `cannot find function 'rekey' in this scope`.

- [ ] **Step 3: Export the free function**

Add to `chisel-ffi/src/lib.rs`, after `library_version()`:

```rust
/// Re-encrypt an entire database under a freshly generated data key.
///
/// Takes a path rather than an open handle, mirroring the constructor: the
/// whole file is rewritten and published by rename, so a handle opened
/// beforehand would afterwards refer to the original, now-unlinked inode.
/// Close any open database first, call this, then reopen.
///
/// Use this only when the DATA key itself is believed compromised. If a
/// credential leaked, `rotateKey` is the right tool — it is O(1) and touches
/// only the superblock.
///
/// Every credential other than `key` is revoked: each key slot's wrapping key
/// derives from its own credential and only `key` was supplied. Add the others
/// back with `addKey` afterwards.
///
/// `argon2Params` is a Swift-only addition over the Python surface, which
/// exposes no equivalent. `None` inherits the cost parameters of the slot
/// `key` unlocks, so a deliberately hardened database is not silently
/// downgraded; supplying a value re-hardens as part of the rewrite, which is
/// the natural moment to do it.
#[uniffi::export]
pub fn rekey(
    path: String,
    key: ChiselKey,
    argon2_params: Option<Argon2Params>,
) -> FfiResult<()> {
    chisel::Chisel::rekey(
        std::path::Path::new(&path),
        &key.into_engine(),
        argon2_params.map(Into::into),
    )
    .map_err(Into::into)
}
```

**No new `use` statement is needed.** `lib.rs` already has `pub use types::{Argon2Params, ChiselKey, …}`, which brings both names into this module's scope; adding `use crate::types::{Argon2Params, ChiselKey};` alongside it is `E0252: the name is defined multiple times`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p chisel-ffi`
Expected: PASS, 22 tests.

- [ ] **Step 5: Regenerate bindings and add the Swift wrapper**

Run: `swift/build-xcframework.sh`

Create `swift/Sources/Chisel/Rekey.swift`:

```swift
// Rekey.swift — the one operation that is not a method on an open handle.
//
// `rekey` rewrites the whole database and publishes it with an atomic rename,
// so it takes a path: a handle opened beforehand would afterwards refer to the
// original, now-unlinked inode. This mirrors the Python binding, where `rekey`
// is a module-level function rather than a `Chisel` method for the same reason.
import Foundation

/// Re-encrypt an entire database under a freshly generated data key.
///
/// Close any open handle to `path` first, call this, then reopen — the file is
/// replaced by rename, so an existing handle would keep pointing at the old
/// inode.
///
/// Use this only when the DATA key itself is believed compromised. For a leaked
/// credential, `ChiselDatabase.rotateKey(old:new:)` is the right tool: it is
/// O(1) and touches only the superblock.
///
/// Every credential other than `key` is revoked, because each key slot's
/// wrapping key derives from its own credential and only `key` was supplied.
/// Re-add the others with `addKey(existing:new:)` afterwards.
///
/// - Parameter argon2Params: `nil` inherits the cost parameters of the slot
///   `key` unlocks, so a hardened database is not silently downgraded.
/// - Throws: `.DatabaseFileNotFound` if `path` names nothing,
///   `.EncryptionNotSupported` if the database is plaintext,
///   `.InvalidEncryptionKey` if `key` unlocks no slot, `.InvalidArgon2Params`
///   if `argon2Params` carries unusable cost values.
public func rekey(at path: URL, key: ChiselKey, argon2Params: Argon2Params? = nil) throws {
    // `URL.path` on a non-file URL silently discards the scheme and host, so
    // `https://example.com/prod.db` would rekey `/prod.db` on the local disk.
    // Reject anything that is not a file URL at the boundary. PR 4 extracts
    // this into a shared helper once the initializers need it too (SWIFT-5).
    guard path.isFileURL else {
        throw ChiselError.DatabaseFileNotFound(
            message: "rekey requires a file URL, got scheme \(path.scheme ?? "none")"
        )
    }
    try Chisel.rekey(path: path.path, key: key, argon2Params: argon2Params)
}
```

- [ ] **Step 6: Write the Swift test**

Create `swift/Tests/ChiselTests/RekeyTests.swift`:

```swift
import XCTest
import Foundation
@testable import Chisel

final class RekeyTests: XCTestCase {
    func testRekeyPreservesContentUnderTheSameCredential() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("chisel-swift-rekey-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("rk.db")
        let key = ChiselKey.Passphrase(phrase: "correct horse")

        var opts = Options()
        opts.encryptionKey = key
        let db = try ChiselDatabase(path: path, options: opts)
        let h = try db.transaction { txn -> Handle in
            let h = try txn.allocate(Data("secret".utf8))
            try txn.setRootName("r", h)
            return h
        }
        _ = h
        try db.close()

        try rekey(at: path, key: key)

        var reopen = Options()
        reopen.encryptionKey = key
        let again = try ChiselDatabase(path: path, options: reopen)
        let root = try XCTUnwrap(try again.getRootName("r"))
        XCTAssertEqual(try again.read(root), Data("secret".utf8))
        try again.close()
    }

    // A non-file URL must be refused at the boundary rather than silently
    // rekeying the path component on the local filesystem.
    func testRekeyRejectsNonFileURL() {
        let url = URL(string: "https://example.com/prod.db")!
        XCTAssertThrowsError(try rekey(at: url, key: .Passphrase(phrase: "x"))) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
        }
    }
}
```

- [ ] **Step 7: Run the Swift suite**

Run: `cd swift && swift test`
Expected: PASS, 16 test methods.

- [ ] **Step 8: Add the PARITY.md row**

In `swift/PARITY.md`, add to the module-level table (directly under the `open(path, **opts)` row):

```
| `rekey(path, key)` | `rekey(at:key:argon2Params:)` | Rekey.swift — `rekey(at:key:argon2Params:)` |
```

and add to the "Swift-only additions" section:

```
- `rekey(at:key:argon2Params:)`'s `argon2Params` parameter — Python's `rekey`
  exposes no cost-parameter argument. `nil` reproduces Python's behaviour
  exactly (inherit the unlocking slot's parameters); a supplied value
  re-hardens during the rewrite, which is the natural moment to do it.
```

- [ ] **Step 9: Commit and open the PR**

```bash
git add chisel-ffi/src/lib.rs swift/Sources/Chisel/Rekey.swift swift/Sources/Chisel/Generated/chisel_ffi.swift swift/Tests/ChiselTests/RekeyTests.swift swift/PARITY.md
git commit -m "feat(swift): expose rekey, closing the parity gap with Python

The engine and the Python binding both grew rekey after this binding was
written. It is a free function rather than a method because it replaces the
file by rename — a handle opened beforehand would afterwards refer to the
original, now-unlinked inode.

argon2Params goes beyond Python's surface deliberately: a rekey already
rewrites every page, so it is the natural moment to re-harden, and nil
reproduces Python's inherit-the-slot behaviour exactly.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin feat/swift-rekey
gh pr create --base main --title "Expose rekey in the Swift binding" --body "$(cat <<'BODY'
Closes the one Python↔Swift parity gap that opened while the binding sat
unmerged: `main` grew `Chisel::rekey` and a module-level Python
`rekey(path, key)`.

Exposed as a UniFFI free function, matching Python's shape and for the same
reason — it publishes the rewritten database by rename, so it takes a path
rather than an open handle.

`argon2Params` is a deliberate addition over Python: `nil` reproduces Python's
behaviour, and a supplied value re-hardens during a rewrite that is already
touching every page. Recorded in PARITY.md as a Swift-only addition.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 4 — Security and correctness: SWIFT-6, -1, -5, -3

Branch: `fix/swift-security-correctness`, cut from the PR 3 branch. Closes #155 and the
correctness half of #104.

### Task 7: Stop `Debug` from printing plaintext key material (SWIFT-6)

**Files:**
- Modify: `chisel-ffi/src/types.rs`

**Interfaces:**
- Consumes: `ChiselKey`, `Options` from Task 1.
- Produces: `ChiselKey` and `Options` with hand-written redacting `Debug`; `ChiselKey` no longer `Clone`.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `chisel-ffi/src/types.rs`:

```rust
    // The engine hand-writes Debug for crypto::Key to print `<redacted>`;
    // this crate derived it, so `{:?}` on a ChiselKey — or on any struct
    // embedding one, which Options does — printed the user's passphrase
    // verbatim. Nothing formats these today, which is what makes it a
    // standing trap rather than a live leak: any future tracing::debug!(?o),
    // assert_eq! failure message, or Debug derive on an enclosing type turns
    // it into one. On iOS those destinations are routinely uploaded.
    #[test]
    fn debug_never_prints_key_material() {
        let raw = ChiselKey::Raw {
            bytes: b"0123456789abcdef".to_vec(),
        };
        let printed = format!("{raw:?}");
        assert!(!printed.contains("0123456789abcdef"), "raw key leaked: {printed}");
        assert!(printed.contains("redacted"), "expected a redaction marker: {printed}");

        let phrase = ChiselKey::Passphrase {
            phrase: "correct horse battery staple".into(),
        };
        let printed = format!("{phrase:?}");
        assert!(!printed.contains("correct horse"), "passphrase leaked: {printed}");

        let mut opts = Options::default_ffi();
        opts.encryption_key = Some(ChiselKey::Passphrase {
            phrase: "correct horse battery staple".into(),
        });
        let printed = format!("{opts:?}");
        assert!(
            !printed.contains("correct horse"),
            "passphrase leaked through Options: {printed}"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p chisel-ffi debug_never_prints_key_material`
Expected: FAIL — `raw key leaked: Raw { bytes: [48, 49, ...] }`. (The `Raw` arm leaks as a byte array; the `Passphrase` arm leaks as literal text.)

- [ ] **Step 3: Replace the derives with a redacting impl**

In `chisel-ffi/src/types.rs`, change the `ChiselKey` derive from:

```rust
#[derive(uniffi::Enum, Clone, Debug)]
pub enum ChiselKey {
```

to:

```rust
// No `Debug` derive and no `Clone`, both deliberate.
//
// `Debug` is hand-written below to redact the payload, mirroring the engine's
// own `impl Debug for crypto::Key` (src/crypto/mod.rs). Deriving it printed
// the plaintext secret, directly against the discipline the surrounding code
// keeps: `into_engine` wraps everything in `Zeroizing`, and the root manifest
// pins `zeroize` specifically to wipe key material on drop.
//
// `Clone` is dropped because a cloned `ChiselKey` holds a plain `Vec<u8>` /
// `String` that is dropped WITHOUT wiping — the same leak in the other
// direction. No call site needs it: every consumer takes the value and
// `into_engine(self)` consumes it.
#[derive(uniffi::Enum)]
pub enum ChiselKey {
```

Add immediately after the enum:

```rust
impl std::fmt::Debug for ChiselKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The variant name is safe and useful (it says which credential KIND
        // was supplied); the payload never is. Byte length is deliberately
        // omitted too — key length is a fingerprint.
        match self {
            ChiselKey::Raw { .. } => f.write_str("ChiselKey::Raw(<redacted>)"),
            ChiselKey::Passphrase { .. } => f.write_str("ChiselKey::Passphrase(<redacted>)"),
        }
    }
}
```

Then change the `Options` derive from `#[derive(uniffi::Record, Clone, Debug)]` to:

```rust
// `Debug` is hand-written below rather than derived: `Options` embeds
// `Option<ChiselKey>`, so a derived impl is only as safe as its fields and a
// future field could reintroduce the leak silently. Spelling it out makes the
// redaction a property of this type rather than an accident of its members.
#[derive(uniffi::Record, Clone)]
pub struct Options {
```

and add after the `impl Options { … }` block:

```rust
impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("cache_max_bytes", &self.cache_max_bytes)
            .field("spillway_max_bytes", &self.spillway_max_bytes)
            .field("drain_insertion", &self.drain_insertion)
            .field("create_if_missing", &self.create_if_missing)
            .field("read_only", &self.read_only)
            .field("superblock_count", &self.superblock_count)
            // Presence is operationally useful ("was this DB opened
            // encrypted?"); the credential itself is not printable.
            .field(
                "encryption_key",
                &self.encryption_key.as_ref().map(|_| "<redacted>"),
            )
            .field("argon2_params", &self.argon2_params)
            .finish()
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p chisel-ffi`
Expected: PASS. If the build fails with `ChiselKey: Clone` required, a call site does need it — find it, and if it is genuinely necessary keep `Clone` but wrap the payloads in `Zeroizing` first. Do not silently restore the derive.

- [ ] **Step 5: Commit**

```bash
git add chisel-ffi/src/types.rs
git commit -m "fix(ffi): redact key material in Debug and drop Clone from ChiselKey

ChiselKey derived Debug on the plaintext secret, and Options embeds one, so
any {:?} printed the user's passphrase. Nothing formatted them today, which
made it a standing trap rather than a live leak — the next tracing::debug!,
assert_eq! failure, or Debug derive on an enclosing type would have been the
first disclosure, and on iOS those destinations are uploaded off-device.

The engine already hand-writes this correctly for crypto::Key; the FFI layer
just never inherited the discipline. Clone goes for the mirror reason: a
cloned key holds an unwiped Vec<u8>/String. No call site needed it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Give a zero tag a typed error (SWIFT-1)

**Files:**
- Modify: `chisel-ffi/src/error.rs` (new `InvalidTag` case + `ffi_name`), `chisel-ffi/src/database.rs` (five method signatures)
- Modify: `swift/Sources/Chisel/Extensions.swift`, `swift/Tests/ChiselTests/ErrorTests.swift`
- Regenerate: `swift/Sources/Chisel/Generated/chisel_ffi.swift`

**Interfaces:**
- Consumes: `ChiselError` from Task 3.
- Produces: FFI case `InvalidTag { message }`; `NativeDatabase` tag-taking methods accept `u32` and validate.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `chisel-ffi/src/database.rs`:

```rust
    // UniFFI lowers Tag to a bare u32 alias with no validation, so a Swift
    // caller can pass 0. Rejection happened only inside the custom_type! lift,
    // via the engine's TryFrom<u32> — whose ZeroTagError is NOT a
    // chisel::ChiselError, so it never reached the FFI error enum and escaped
    // as an untyped failure. swift/README.md promises "every throwing call
    // raises ChiselError"; Python raises a documented error for this input.
    #[test]
    fn zero_tag_is_a_typed_error_on_every_tag_taking_method() {
        let db = NativeDatabase::new_in_memory(Options::default_ffi()).unwrap();
        db.begin().unwrap();
        let h = db.allocate(b"v".to_vec()).unwrap();

        assert!(matches!(
            db.allocate_tagged(b"v".to_vec(), 0),
            Err(ChiselError::InvalidTag { .. })
        ));
        assert!(matches!(
            db.handles_with_tag(0),
            Err(ChiselError::InvalidTag { .. })
        ));
        assert!(matches!(
            db.delete_tagged(h, 0),
            Err(ChiselError::InvalidTag { .. })
        ));
        assert!(matches!(
            db.delete_with_tag(0, 10),
            Err(ChiselError::InvalidTag { .. })
        ));

        // A nonzero tag still works — the guard rejects only 0.
        let tagged = db.allocate_tagged(b"v".to_vec(), 7).unwrap();
        assert_eq!(db.handles_with_tag(7).unwrap(), vec![tagged]);
        db.rollback().unwrap();
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p chisel-ffi zero_tag_is_a_typed_error_on_every_tag_taking_method`
Expected: FAIL to compile — `expected chisel::Tag, found integer`. That signature mismatch is the finding: the type never let a `0` be rejected in a typed way.

- [ ] **Step 3: Add the FFI error case**

In `chisel-ffi/src/error.rs`, add to the operational group (after `AlreadyFinished`):

```rust
    // FFI-only, like `Closed` and `AlreadyFinished`: the engine has no
    // equivalent variant because its `Tag` type makes zero unrepresentable.
    // UniFFI cannot express that — it lowers `Tag` to a bare `u32` alias — so
    // the check has to live at this boundary, mirroring the Python binding,
    // which validates in `python/src/db.rs` rather than relying on the lift.
    InvalidTag {
        message: String,
    },
```

Add the `message()` or-pattern arm `| Self::InvalidTag { message }` and the `ffi_name` arm:

```rust
            ChiselError::InvalidTag { .. } => "InvalidTag",
```

- [ ] **Step 4: Validate at the boundary instead of in the lift**

In `chisel-ffi/src/database.rs`, add a helper next to `closed_err`:

```rust
// Tag validation lives here rather than in the `custom_type!` lift because a
// lift failure surfaces to Swift as an untyped UniFFI internal error, not as a
// `ChiselError` — and the binding's documented contract is that every throwing
// call raises `ChiselError`. Taking `u32` and converting explicitly is what
// makes the rejection expressible.
fn tag_from(raw: u32) -> Result<chisel::Tag, ChiselError> {
    chisel::Tag::new(raw).ok_or_else(|| ChiselError::InvalidTag {
        message: "tag must be non-zero (0 means untagged)".into(),
    })
}
```

Change these five exported methods to take `u32` and convert (bodies otherwise unchanged):

```rust
    pub fn allocate_tagged(&self, value: Vec<u8>, tag: u32) -> FfiResult<chisel::Handle> {
        let tag = tag_from(tag)?;
        self.with_db(|db| db.allocate_tagged(&value, tag))
    }

    pub fn handles_with_tag(&self, tag: u32) -> FfiResult<Vec<chisel::Handle>> {
        let tag = tag_from(tag)?;
        self.with_db(|db| db.handles_with_tag(tag))
    }

    pub fn delete_tagged(&self, handle: chisel::Handle, tag: u32) -> FfiResult<()> {
        let tag = tag_from(tag)?;
        self.with_db(|db| db.delete_tagged(handle, tag))
    }

    pub fn delete_with_tag(&self, tag: u32, max: u64) -> FfiResult<TagDropProgress> {
        let tag = tag_from(tag)?;
        self.with_db(|db| db.delete_with_tag(tag, max as usize))
            .map(TagDropProgress::from)
    }
```

`tag(&self, handle)` keeps returning `Option<chisel::Tag>` — lowering a `Tag` to `u32` is infallible, and `Tag` is a `typealias` for `UInt32` in Swift, so the Swift-visible types are identical either way.

- [ ] **Step 5: Fix the existing test that passes a `chisel::Tag`**

In `tag_and_root_name_round_trip`, replace `let tag = chisel::Tag::new(7).unwrap();` and its uses so the tag-taking calls pass `7u32` while the `db.tag(h)` assertion still compares against `chisel::Tag::new(7).unwrap()`:

```rust
        let tag = chisel::Tag::new(7).unwrap();
        let h = db.allocate_tagged(b"v".to_vec(), 7).unwrap();
        assert_eq!(db.tag(h).unwrap(), Some(tag));
        assert_eq!(db.handles_with_tag(7).unwrap(), vec![h]);
```

- [ ] **Step 6: Run the Rust tests**

Run: `cargo test -p chisel-ffi`
Expected: PASS.

- [ ] **Step 7: Regenerate, classify, and test on the Swift side**

Run: `swift/build-xcframework.sh`
Expected: `cd swift && swift build` then FAILS with a non-exhaustive switch in `Extensions.swift` — the tripwire again.

Add to `category` (operational group) and `message`, one case per line:

```swift
        case .InvalidTag: return .operational
```

```swift
        case .InvalidTag(let m): return m
```

Add to `swift/Tests/ChiselTests/ErrorTests.swift`:

```swift
    // A zero tag is a caller mistake with the database untouched — the same
    // shape as every other operational case, and catchable as ChiselError,
    // which is what swift/README.md promises for every throwing call.
    func testZeroTagIsOperationalAndCatchable() throws {
        let db = try ChiselDatabase(path: nil)
        try db.transaction { txn in
            XCTAssertThrowsError(try txn.allocateTagged(Data("x".utf8), tag: 0)) { error in
                guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
                XCTAssertEqual(e.category, .operational)
            }
            XCTAssertThrowsError(try txn.handlesWithTag(0))
            // A nonzero tag is unaffected.
            _ = try txn.allocateTagged(Data("x".utf8), tag: 7)
        }
    }
```

Run: `cd swift && swift test`
Expected: PASS, 17 test methods.

- [ ] **Step 8: Commit**

```bash
git add chisel-ffi/src/error.rs chisel-ffi/src/database.rs swift/Sources/Chisel/Extensions.swift swift/Sources/Chisel/Generated/chisel_ffi.swift swift/Tests/ChiselTests/ErrorTests.swift
git commit -m "fix(ffi): reject a zero tag with a typed error at the FFI boundary

UniFFI lowers Tag to a bare UInt32 alias, so allocateTagged(value, tag: 0)
compiled and could not fail with any ChiselError — the type system made that
impossible. Rejection happened only inside the custom_type! lift, via the
engine's TryFrom<u32>, whose ZeroTagError is not a chisel::ChiselError and so
never reached the FFI error enum; it escaped as an untyped failure past a
documented contract that every throwing call raises ChiselError.

The tag-taking methods now take u32 and convert explicitly, which is what
makes the rejection expressible. Python validates at the same boundary for
the same reason.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: Reject non-file URLs at the boundary (SWIFT-5)

**Files:**
- Modify: `swift/Sources/Chisel/ChiselDatabase.swift`, `swift/Sources/Chisel/Rekey.swift`
- Create: `swift/Sources/Chisel/FileURL.swift`
- Modify: `swift/Tests/ChiselTests/DatabaseTests.swift`

**Interfaces:**
- Consumes: `ChiselDatabase.init`, `ChiselStore.init`, `rekey(at:)` from earlier tasks.
- Produces: `internal func chiselFilePath(_ url: URL) throws -> String`, used by all three entry points.

- [ ] **Step 1: Write the failing test**

Add to `swift/Tests/ChiselTests/DatabaseTests.swift`:

```swift
    // `URL.path` on a non-file URL silently drops the scheme and host, so
    // `https://example.com/prod.db` created and flocked `/prod.db` on the
    // local filesystem, and `URL(string: "data.db")!` resolved against the
    // process cwd — which on iOS is not a writable container location. Both
    // succeeded silently instead of failing at the boundary.
    func testNonFileURLIsRejected() {
        XCTAssertThrowsError(
            try ChiselDatabase(path: URL(string: "https://example.com/prod.db")!)
        )
        XCTAssertThrowsError(
            try ChiselDatabase(path: URL(string: "data.db")!)
        )
    }

    func testFileURLOpensCommitsAndReopens() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("chisel-swift-file-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("db.chisel")

        let db = try ChiselDatabase(path: path)
        try db.transaction { txn in
            let h = try txn.allocate(Data("durable".utf8))
            try txn.setRootName("root", h)
        }
        try db.close()

        let reopened = try ChiselDatabase(path: path)
        let root = try XCTUnwrap(try reopened.getRootName("root"))
        XCTAssertEqual(try reopened.read(root), Data("durable".utf8))
        try reopened.close()
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd swift && swift test --filter testNonFileURLIsRejected`
Expected: FAIL — no error thrown. (Depending on sandbox permissions the `https:` case may instead fail while *creating* `/prod.db`, which is the same defect wearing a different mask.)

- [ ] **Step 3: Add the shared helper**

Create `swift/Sources/Chisel/FileURL.swift`:

```swift
// FileURL.swift — the one place a caller-supplied URL becomes a filesystem
// path. Every entry point that accepts a URL routes through it.
//
// `URL.path` is not a safe conversion: on a non-file URL it silently returns
// the path component with the scheme and host discarded, so a URL that names a
// remote resource turns into a local path that the engine will happily create
// and flock.
import Foundation

/// Convert a caller-supplied file URL to the path the engine expects.
///
/// - Throws: `.DatabaseFileNotFound` if `url` is not a file URL. Reusing that
///   case rather than adding one keeps the FFI error surface aligned with the
///   engine's; the message names the actual problem.
func chiselFilePath(_ url: URL) throws -> String {
    guard url.isFileURL else {
        throw ChiselError.DatabaseFileNotFound(
            message: """
                database path must be a file URL, got scheme \
                \(url.scheme ?? "none") — build it with URL(fileURLWithPath:)
                """
        )
    }
    return url.path
}
```

- [ ] **Step 4: Route the three entry points through it**

In `ChiselDatabase.swift`, replace the initializer body:

```swift
    /// `path: nil` opens an in-memory database. A non-nil `path` must be a
    /// file URL — see `chiselFilePath`.
    public init(path: URL?, options: Options = Options()) throws {
        self.native = try NativeDatabase(path: path.map(chiselFilePath), options: options)
    }
```

Note `path.map(chiselFilePath)` needs the throwing form; use:

```swift
        let resolved = try path.map { try chiselFilePath($0) }
        self.native = try NativeDatabase(path: resolved, options: options)
```

`ChiselStore.init` already funnels through `ChiselDatabase(path:options:)`, so it inherits the guard with no change.

In `Rekey.swift`, replace the inline guard added in Task 6 with the shared helper:

```swift
    try Chisel.rekey(path: chiselFilePath(path), key: key, argon2Params: argon2Params)
```

and delete the now-duplicated `guard path.isFileURL` block and its comment.

- [ ] **Step 5: Run the tests**

Run: `cd swift && swift test`
Expected: PASS, 19 test methods (including `RekeyTests.testRekeyRejectsNonFileURL`, which now exercises the shared helper).

- [ ] **Step 6: Commit**

```bash
git add swift/Sources/Chisel/FileURL.swift swift/Sources/Chisel/ChiselDatabase.swift swift/Sources/Chisel/Rekey.swift swift/Tests/ChiselTests/DatabaseTests.swift
git commit -m "fix(swift): require a file URL at every path-taking entry point

URL.path on a non-file URL silently discards the scheme and host, so
ChiselDatabase(path: URL(string: \"https://example.com/prod.db\")!) created and
flocked /prod.db locally, and a scheme-less URL resolved against the process
cwd — not a writable location on iOS. Both succeeded quietly.

No Swift test had ever passed a non-nil path, so the conversion had zero
coverage on the Swift side. There is now a file-backed open/commit/reopen test
as well as the rejection test.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 10: Release the savepoint on the throw path (SWIFT-3)

**Files:**
- Modify: `swift/Sources/Chisel/ChiselDatabase.swift` (`savepoint`)
- Modify: `swift/Tests/ChiselTests/DatabaseTests.swift`

**Interfaces:**
- Consumes: `Transaction.savepoint(_:_:)` from Task 2.
- Produces: a savepoint scope that is symmetric on both exit paths.

- [ ] **Step 1: Write the failing test**

The engine deliberately keeps the mark after a rollback (`src/transaction/savepoints.rs`: "The named savepoint itself remains on the stack and can be rolled back to again or released"), so a scope that only rolls back leaks the name for the rest of the transaction.

Add to `swift/Tests/ChiselTests/DatabaseTests.swift`:

```swift
    // ADR 0025 chose this shape for Python and names SWIFT-3 as the
    // mirror-image defect to fix the same way. The engine keeps a savepoint
    // mark after a rollback_to, so a scope that rolls back without releasing
    // leaves the name occupied — and the second attempt of a retry loop fails
    // with DuplicateSavepoint, which reads as a bug in the caller's own loop.
    func testSavepointScopeIsReusableAfterItsBodyThrows() throws {
        struct Boom: Error {}
        let db = try ChiselDatabase(path: nil)
        try db.transaction { txn in
            for attempt in 0..<2 {
                XCTAssertThrowsError(
                    try txn.savepoint("attempt") { _ in throw Boom() },
                    "attempt \(attempt) should surface Boom, not DuplicateSavepoint"
                ) { error in
                    XCTAssertTrue(error is Boom, "got \(error) on attempt \(attempt)")
                }
            }
            // And the scope still works when the body succeeds.
            let h = try txn.savepoint("attempt") { _ in
                try txn.allocate(Data("kept".utf8))
            }
            XCTAssertEqual(try txn.read(h), Data("kept".utf8))
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd swift && swift test --filter testSavepointScopeIsReusableAfterItsBodyThrows`
Expected: FAIL on attempt 1 — the error is `ChiselError.DuplicateSavepoint`, not `Boom`, because the first attempt left `"attempt"` on the stack.

- [ ] **Step 3: Make the scope symmetric**

In `ChiselDatabase.swift`, replace the `catch` arm of `savepoint`:

```swift
        } catch {
            try? native.rollbackTo(name: name)
            throw error
        }
```

with:

```swift
        } catch {
            // Both calls, in this order. The engine keeps the mark after a
            // rollback_to — deliberately, so a caller can roll back to the
            // same point twice — which means rolling back alone leaves the
            // name occupied for the rest of the transaction and the next
            // `savepoint(name)` throws DuplicateSavepoint. Releasing after
            // the rollback makes the scope symmetric on both exit paths,
            // which is the whole point of the closure form.
            //
            // Both are best-effort: a cleanup failure must not mask the error
            // the caller actually needs to see.
            try? native.rollbackTo(name: name)
            try? native.release(name: name)
            throw error
        }
```

- [ ] **Step 4: Run the tests**

Run: `cd swift && swift test`
Expected: PASS, 20 test methods.

- [ ] **Step 5: Commit and open the PR**

```bash
git add swift/Sources/Chisel/ChiselDatabase.swift swift/Tests/ChiselTests/DatabaseTests.swift
git commit -m "fix(swift): release a savepoint after rolling back to it

The engine keeps a savepoint mark after rollback_to, on purpose. The scope's
catch arm rolled back without releasing, so the name stayed occupied and the
second iteration of a retry loop failed with DuplicateSavepoint — which a
caller reads as a bug in their own loop, not as leaked scope state.

ADR 0025 settled this shape for the Python binding and names this as the
mirror-image defect to fix the same way.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin fix/swift-security-correctness
gh pr create --base main --title "Swift binding: redact keys, reject zero tags and non-file URLs, fix savepoint scopes" --body "$(cat <<'BODY'
Closes #155 and the correctness half of #104.

- **SWIFT-6** — `ChiselKey` derived `Debug` on the plaintext secret and
  `Options` embeds one, so any `{:?}` printed the user's passphrase. Nothing
  formats them today; the next `tracing::debug!` or `assert_eq!` failure would
  have been the first disclosure, and on iOS those destinations are uploaded
  off-device. Now hand-written and redacting, matching the engine's own
  `crypto::Key`. `Clone` dropped for the mirror reason.
- **SWIFT-1** — a zero tag could not fail with any `ChiselError`; the type
  system made it impossible. Rejection lived in the `custom_type!` lift, whose
  `ZeroTagError` is not a `chisel::ChiselError`. Now validated at the boundary,
  as Python does.
- **SWIFT-5** — `URL.path` on a non-file URL silently discards the scheme, so a
  remote-looking URL created and flocked a local file. All three path-taking
  entry points route through one guard.
- **SWIFT-3** — the savepoint scope rolled back without releasing, so a retry
  loop's second attempt failed with `DuplicateSavepoint`. ADR 0025 named this
  as the mirror of the Python defect and chose the shape.

No engine code changed.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 5 — Enforce the transaction lifetime (SWIFT-4)

Branch: `fix/swift-4-lifetime-guard`, cut from the PR 4 branch. Closes the rest of #104.

`Transaction` is documented as a non-escaping façade and nothing enforces it. The FFI has
reserved an `AlreadyFinished` case for this guard since the beginning and no code path has
ever constructed it.

**Design note — deviation from the review's suggested fix.** The review proposed making
`Transaction` a reference type. This plan keeps it a `struct` and moves only the flag into a
shared reference. Copying the struct out of the closure copies the *reference*, so an escaped
copy observes the same `finished` flag and throws exactly as a class would — while avoiding a
Swift 6 strict-concurrency problem the class version creates: `ChiselStore.transaction` takes
`@escaping @Sendable (Transaction) throws -> R`, and changing `Transaction`'s kind there is a
concurrency-checking change, not just a storage change. Same guarantee, smaller blast radius.

### Task 11: Make a finished transaction refuse further work

**Files:**
- Modify: `swift/Sources/Chisel/ChiselDatabase.swift` (`Transaction`, `transaction`, `savepoint`, `Savepoint` doc)
- Modify: `swift/Tests/ChiselTests/DatabaseTests.swift`

**Interfaces:**
- Consumes: `Transaction` from Task 2, `.AlreadyFinished` from Task 1.
- Produces: `Transaction` whose forwarders throw `.AlreadyFinished` once its scope has exited.

- [ ] **Step 1: Write the failing test**

```swift
    // `var escaped: Transaction?; try db.transaction { escaped = $0 }` compiles
    // today, and calling through the escaped copy afterwards did NOT error:
    // NativeDatabase.with_db just takes the lock and dispatches to whatever
    // transaction the engine currently has open, so a write meant for the
    // finished transaction silently joined and committed with the next one.
    //
    // Python guards this with a per-object finished flag raising
    // AlreadyFinishedError (ADR 0025). The FFI reserved the matching case from
    // the start and nothing ever constructed it.
    func testEscapedTransactionRefusesWork() throws {
        let db = try ChiselDatabase(path: nil)
        var escaped: Transaction?
        try db.transaction { txn in escaped = txn }
        let stale = try XCTUnwrap(escaped)

        XCTAssertThrowsError(try stale.allocate(Data("late".utf8))) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
            // Pattern-match the case rather than comparing a string.
            // `String(describing:)` would return the MESSAGE here, not the
            // case name — Extensions.swift makes ChiselError
            // CustomStringConvertible with `description == message`.
            guard case .AlreadyFinished = e else {
                return XCTFail("expected .AlreadyFinished, got \(e)")
            }
        }
        XCTAssertThrowsError(try stale.read(Handle(0)))
        XCTAssertThrowsError(try stale.handles())
    }

    // A write through a stale handle must not join a LATER transaction — the
    // specific silent-corruption path SWIFT-4 describes.
    func testEscapedTransactionCannotJoinTheNextTransaction() throws {
        let db = try ChiselDatabase(path: nil)
        var escaped: Transaction?
        try db.transaction { txn in escaped = txn }
        let stale = try XCTUnwrap(escaped)

        try db.transaction { txn in
            XCTAssertThrowsError(try stale.allocate(Data("intruder".utf8)))
            _ = try txn.allocate(Data("legitimate".utf8))
        }
        let all = try db.handles()
        XCTAssertEqual(all.count, 1, "the stale write must not have joined")
    }

    // The guard must not fire during the scope it belongs to.
    func testTransactionWorksNormallyInsideItsScope() throws {
        let db = try ChiselDatabase(path: nil)
        let h = try db.transaction { txn in try txn.allocate(Data("fine".utf8)) }
        XCTAssertEqual(try db.read(h), Data("fine".utf8))
    }
```

No string-matching helper is needed or wanted: assertions that care *which* case
use `guard case .X = e`. `String(describing:)` is a trap here — `Extensions.swift`
declares `ChiselError: CustomStringConvertible` with `description == message`, so
it yields the engine's diagnostic text rather than the case name.

- [ ] **Step 2: Run it to verify it fails**

Run: `cd swift && swift test --filter testEscapedTransactionRefusesWork`
Expected: FAIL — `XCTAssertThrowsError` fails because the allocate *succeeds*. That success is the defect.

- [ ] **Step 3: Add the shared one-shot state**

In `ChiselDatabase.swift`, add above `public struct Transaction`:

```swift
/// One-shot liveness flag shared by every copy of a `Transaction` value.
///
/// `Transaction` stays a struct so its value semantics and its
/// `@Sendable`-closure usage in `ChiselStore` are unchanged; only the flag is
/// a reference. Copying the struct out of its closure copies this reference,
/// so an escaped copy observes the same `finished` and refuses work exactly as
/// a class-based façade would.
///
/// Not synchronized, and does not need to be: the sync `ChiselDatabase` is
/// thread-confined by contract, and `ChiselStore` funnels every access through
/// one serial queue — the same confinement invariant `DatabaseBox` documents.
final class TransactionState {
    var finished = false
}
```

- [ ] **Step 4: Guard every forwarder**

Change the `Transaction` declaration to:

```swift
public struct Transaction {
    let native: NativeDatabase
    let state: TransactionState

    /// The engine object, or a throw if this transaction's scope has exited.
    ///
    /// Every forwarder below goes through here. The FFI has reserved
    /// `.AlreadyFinished` for exactly this guard since the binding was written
    /// and nothing ever raised it; this is the Python shape ADR 0025 chose.
    private func live() throws -> NativeDatabase {
        guard !state.finished else {
            throw ChiselError.AlreadyFinished(
                message: "transaction has already been committed or rolled back"
            )
        }
        return native
    }
```

Then change every forwarder from `try native.X(…)` to `try live().X(…)`. All 21 of them:

```swift
    public func allocate(_ value: Data) throws -> Handle { try live().allocate(value: value) }
    public func read(_ handle: Handle) throws -> Data { try live().read(handle: handle) }
    public func update(_ handle: Handle, _ value: Data) throws { try live().update(handle: handle, value: value) }
    public func delete(_ handle: Handle) throws { try live().delete(handle: handle) }
    public func deleteMany(_ handles: [Handle]) throws { try live().deleteMany(handles: handles) }
    public func allocateTagged(_ value: Data, tag: Tag) throws -> Handle { try live().allocateTagged(value: value, tag: tag) }
    public func tag(_ handle: Handle) throws -> Tag? { try live().tag(handle: handle) }
    public func handlesWithTag(_ tag: Tag) throws -> [Handle] { try live().handlesWithTag(tag: tag) }
    public func deleteTagged(_ handle: Handle, tag: Tag) throws { try live().deleteTagged(handle: handle, tag: tag) }
    public func deleteWithTag(_ tag: Tag, max: UInt64) throws -> TagDropProgress { try live().deleteWithTag(tag: tag, max: max) }
    public func clientByte(_ handle: Handle) throws -> UInt8 { try live().clientByte(handle: handle) }
    public func setClientByte(_ handle: Handle, _ byte: UInt8) throws { try live().setClientByte(handle: handle, byte: byte) }
    public func setRootName(_ name: String, _ handle: Handle) throws { try live().setRootName(name: name, handle: handle) }
    public func getRootName(_ name: String) throws -> Handle? { try live().getRootName(name: name) }
    public func clearRootName(_ name: String) throws { try live().clearRootName(name: name) }
    public func handles() throws -> [Handle] { try live().handles() }
    public func stats() throws -> Stats { try live().stats() }
    public func counters() throws -> Counters { try live().counters() }
    public func defrag(_ options: DefragOptions = DefragOptions()) throws -> DefragStats { try live().defrag(options: options) }
```

And in `savepoint`, change the opening `try native.savepoint(name: name)` to `try live().savepoint(name: name)`. Its `release`/`rollbackTo` calls stay on `native` — they are cleanup running while the scope is still live, and routing them through `live()` would make cleanup depend on the very flag it is unwinding.

- [ ] **Step 5: Mark the transaction finished on both exit paths**

In `ChiselDatabase.transaction`, replace the body:

```swift
    public func transaction<R>(_ body: (Transaction) throws -> R) throws -> R {
        try native.begin()
        let state = TransactionState()
        let txn = Transaction(native: native, state: state)
        // Both exits, including the commit-failure path. After this returns,
        // any copy of `txn` that escaped the closure refuses work.
        defer { state.finished = true }
        do {
            let result = try body(txn)
            try native.commit()
            return result
        } catch {
            try? native.rollback()
            throw error
        }
    }
```

- [ ] **Step 6: Document why `Savepoint` needs no guard**

`Savepoint` carries no operations — it exposes only `name`, and all release/rollback control is folded into the closure — so a "finished" flag on it would guard nothing. Replace the comment above `public struct Savepoint` with:

```swift
/// A named savepoint scope. It carries no operations of its own — use the
/// enclosing `Transaction` for reads and writes, same engine, same active
/// transaction. This type exists only so `savepoint { }` has something
/// distinct from `Transaction` to hand the body.
///
/// Deliberately has NO one-shot `finished` guard, unlike `Transaction`: it has
/// no forwarders for such a guard to protect. Python's `Savepoint` does need
/// one (ADR 0025) because it exposes `release` / `rollback_to` as callable
/// methods; here those are reachable only through the closure's own exit
/// paths, so an escaped `Savepoint` can do nothing at all with it. If this
/// type ever gains an operation, it needs the `Transaction` treatment first.
```

- [ ] **Step 7: Run the tests**

Run: `cd swift && swift test`
Expected: PASS, 23 test methods.

- [ ] **Step 8: Verify strict concurrency is clean**

Run: `cd swift && swift build -Xswiftc -strict-concurrency=complete 2>&1 | grep -E 'error|warning: .*Sendable' | head -20`
Expected: no errors. Warnings about `DatabaseBox`'s `@unchecked Sendable` are pre-existing and expected. If `TransactionState` is flagged as non-Sendable crossing a boundary, that is a real finding — stop and escalate rather than adding `@unchecked`.

- [ ] **Step 9: Commit and open the PR**

```bash
git add swift/Sources/Chisel/ChiselDatabase.swift swift/Tests/ChiselTests/DatabaseTests.swift
git commit -m "fix(swift): a finished transaction refuses further work

Transaction was documented as a non-escaping facade with nothing enforcing it.
An escaped copy did not error: with_db takes the lock and dispatches to
whichever transaction the engine currently has open, so a write meant for the
finished transaction silently joined and committed with the next one.

The FFI reserved AlreadyFinished for this guard from the start and no code
path ever constructed it. It does now.

Transaction stays a struct with the flag behind a shared reference rather than
becoming a class: copies still observe the same flag, and ChiselStore's
@Sendable closure signature is untouched.

Savepoint is deliberately exempt and says so — it has no forwarders to guard.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin fix/swift-4-lifetime-guard
gh pr create --base main --title "A finished Swift transaction refuses further work (SWIFT-4)" --body "$(cat <<'BODY'
Closes #104.

`var escaped: Transaction?; try db.transaction { escaped = $0 }` compiled, and
calling through the escaped copy afterwards did not error — `with_db` dispatches
to whatever transaction the engine currently has open, so a write meant for the
finished transaction silently joined and committed with the next one.

The FFI has reserved `AlreadyFinished` for exactly this guard since the binding
was written; nothing ever raised it. ADR 0025 settled the shape for Python.

Deviation from the review's suggested fix: `Transaction` stays a struct and only
the flag becomes a shared reference. Copies observe the same flag, so the guarantee
is identical, and `ChiselStore.transaction`'s `@escaping @Sendable (Transaction)`
signature is untouched — changing `Transaction`'s kind there is a
concurrency-checking change rather than a storage one. Verified under
`-strict-concurrency=complete`.

`Savepoint` is exempt and documents why: it has no forwarders to guard.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 6 — Cover the hand-written Swift layer (SWIFT-7)

Branch: `test/swift-7-wrapper-coverage`, cut from the PR 5 branch.

The Swift suite was 156 lines and never called `close()`, `isPoisoned`, key management, the
config setters, or opened an encrypted database. Every defect in PRs 4 and 5 lived in that
untested layer. Some gaps are already closed by tests those PRs added (file-backed open,
savepoint throw path, escaped transaction); this PR closes the rest.

### Task 12: Lifecycle, poison, and runtime configuration

**Files:**
- Create: `swift/Tests/ChiselTests/LifecycleTests.swift`
- Modify: `swift/Tests/ChiselTests/StoreTests.swift`

**Interfaces:**
- Consumes: `ChiselDatabase`, `ChiselStore` from Task 2.
- Produces: coverage only; no source changes.

- [ ] **Step 1: Write the tests**

Create `swift/Tests/ChiselTests/LifecycleTests.swift`:

```swift
// LifecycleTests.swift — close / double-close / post-close behaviour and the
// runtime config setters, none of which the suite exercised before SWIFT-7.
// The Rust side has equivalents (use_after_close_is_closed_error,
// double_close_is_closed_error); it is precisely the hand-written Swift
// wrapper that was untested.
import XCTest
import Foundation
@testable import Chisel

final class LifecycleTests: XCTestCase {
    func testCloseThenUseIsClosedError() throws {
        let db = try ChiselDatabase(path: nil)
        try db.close()
        XCTAssertThrowsError(try db.handles()) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
        }
    }

    func testDoubleCloseIsClosedError() throws {
        let db = try ChiselDatabase(path: nil)
        try db.close()
        XCTAssertThrowsError(try db.close())
    }

    // SWIFT-8's substance as an executable assertion: a cleanly closed handle
    // reports isPoisoned == true, because the flag answers "can this handle
    // still do work", not "is the file corrupt". A caller reading it as the
    // latter after a clean shutdown concludes the database is damaged.
    func testIsPoisonedIsTrueAfterACleanClose() throws {
        let db = try ChiselDatabase(path: nil)
        XCTAssertFalse(db.isPoisoned)
        try db.close()
        XCTAssertTrue(db.isPoisoned, "a closed handle reports poisoned; see the doc comment")
    }

    func testRuntimeConfigSettersApplyOutsideATransaction() throws {
        let db = try ChiselDatabase(path: nil)
        try db.setCacheMaxBytes(4 * 1024 * 1024)
        try db.setSpillwayMaxBytes(0)
        try db.setDrainInsertion(.mru)
        try db.setDrainInsertion(.lruTail)
    }

    // Each setter refuses while a transaction is open — the engine's
    // TransactionInProgress, surfaced through the wrapper.
    func testRuntimeConfigSettersRefuseInsideATransaction() throws {
        let db = try ChiselDatabase(path: nil)
        try db.transaction { _ in
            XCTAssertThrowsError(try db.setCacheMaxBytes(1024 * 1024))
            XCTAssertThrowsError(try db.setSpillwayMaxBytes(0))
            XCTAssertThrowsError(try db.setDrainInsertion(.mru))
        }
    }

    func testStatsAndCountersAndFileSizeAreReachable() throws {
        let db = try ChiselDatabase(path: nil)
        XCTAssertEqual(try db.stats().handleCount, 0)
        _ = try db.counters()
        _ = try db.fileSizeBytes()
    }
}
```

Add to `swift/Tests/ChiselTests/StoreTests.swift` — the async layer had exactly one test:

```swift
    func testStoreCloseThenUseThrows() async throws {
        let store = try await ChiselStore(path: nil)
        try await store.close()
        do {
            _ = try await store.handles()
            XCTFail("expected a throw after close")
        } catch let e as ChiselError {
            XCTAssertEqual(e.category, .operational)
        }
    }

    func testStoreIsPoisonedAfterClose() async throws {
        let store = try await ChiselStore(path: nil)
        let before = await store.isPoisoned()
        XCTAssertFalse(before)
        try await store.close()
        let after = await store.isPoisoned()
        XCTAssertTrue(after)
    }

    func testStoreRollsBackOnThrow() async throws {
        struct Boom: Error {}
        let store = try await ChiselStore(path: nil)
        do {
            _ = try await store.transaction { txn -> Handle in
                _ = try txn.allocate(Data("x".utf8))
                throw Boom()
            }
            XCTFail("expected Boom")
        } catch is Boom {
            // expected
        }
        let all = try await store.handles()
        XCTAssertTrue(all.isEmpty, "the throwing transaction must have rolled back")
    }
```

- [ ] **Step 2: Run the tests**

Run: `cd swift && swift test`
Expected: PASS, 32 test methods. A failure in `testIsPoisonedIsTrueAfterACleanClose` means the Rust `is_poisoned` no longer treats `None` as poisoned — that is a behaviour change, not a bad test.

- [ ] **Step 3: Commit**

```bash
git add swift/Tests/ChiselTests/LifecycleTests.swift swift/Tests/ChiselTests/StoreTests.swift
git commit -m "test(swift): cover close, poison, and runtime config on both layers

The suite never called close(), isPoisoned, or the config setters, on either
the sync wrapper or the async store — and the async store had exactly one test
of any kind. The Rust side has equivalents; it was the hand-written Swift
layer that was uncovered, which is where every defect in the preceding PRs
turned out to live.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 13: Encryption and key management

**Files:**
- Create: `swift/Tests/ChiselTests/EncryptionTests.swift`

**Interfaces:**
- Consumes: `ChiselDatabase`, `ChiselKey`, `Options` from Task 2; `chiselFilePath` from Task 9.
- Produces: coverage only.

- [ ] **Step 1: Write the tests**

Create `swift/Tests/ChiselTests/EncryptionTests.swift`:

```swift
// EncryptionTests.swift — the encrypted-open and key-management surface,
// which had no Swift coverage at all: addKey / rotateKey / removeKey were
// reachable only through Rust tests, and no Swift test ever set
// Options.encryptionKey.
//
// File-backed throughout, deliberately: credentials round-trip through the
// on-disk superblock's wrapped-key slots, and an in-memory database has no
// analogue to reopen with a different key.
import XCTest
import Foundation
@testable import Chisel

final class EncryptionTests: XCTestCase {
    private func tempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("chisel-swift-enc-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }

    private func opened(at path: URL, key: ChiselKey) throws -> ChiselDatabase {
        var opts = Options()
        opts.encryptionKey = key
        return try ChiselDatabase(path: path, options: opts)
    }

    func testEncryptedRoundTripAndWrongKeyRejected() throws {
        let path = try tempDir().appendingPathComponent("enc.db")
        let good = ChiselKey.Passphrase(phrase: "correct horse battery staple")

        let db = try opened(at: path, key: good)
        try db.transaction { txn in
            let h = try txn.allocate(Data("classified".utf8))
            try txn.setRootName("root", h)
        }
        try db.close()

        let reopened = try opened(at: path, key: good)
        let root = try XCTUnwrap(try reopened.getRootName("root"))
        XCTAssertEqual(try reopened.read(root), Data("classified".utf8))
        try reopened.close()

        XCTAssertThrowsError(
            try opened(at: path, key: .Passphrase(phrase: "wrong"))
        ) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            // A wrong credential is operational: supply the right key and retry.
            XCTAssertEqual(e.category, .operational)
        }
    }

    func testAddRotateAndRemoveKey() throws {
        let path = try tempDir().appendingPathComponent("keys.db")
        let first = ChiselKey.Passphrase(phrase: "first credential")
        let second = ChiselKey.Passphrase(phrase: "second credential")
        let third = ChiselKey.Passphrase(phrase: "third credential")

        let db = try opened(at: path, key: first)
        try db.transaction { txn in
            let h = try txn.allocate(Data("payload".utf8))
            try txn.setRootName("root", h)
        }
        try db.addKey(existing: first, new: second)
        try db.close()

        // Both credentials now open it.
        let viaSecond = try opened(at: path, key: second)
        XCTAssertNotNil(try viaSecond.getRootName("root"))
        // Rotate the second into a third, then retire the first.
        try viaSecond.rotateKey(old: second, new: third)
        try viaSecond.removeKey(first)
        try viaSecond.close()

        let viaThird = try opened(at: path, key: third)
        XCTAssertNotNil(try viaThird.getRootName("root"))
        try viaThird.close()

        // The retired credentials no longer open it.
        XCTAssertThrowsError(try opened(at: path, key: first))
        XCTAssertThrowsError(try opened(at: path, key: second))
    }

    // Removing the only remaining credential would make the database
    // permanently unopenable, so the engine refuses.
    func testRemovingTheLastKeyIsRefused() throws {
        let path = try tempDir().appendingPathComponent("last.db")
        let only = ChiselKey.Passphrase(phrase: "the only one")
        let db = try opened(at: path, key: only)
        XCTAssertThrowsError(try db.removeKey(only)) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
        }
        try db.close()
    }

    // A raw 32-byte key takes the HKDF path rather than Argon2; both must work.
    func testRawKeyRoundTrip() throws {
        let path = try tempDir().appendingPathComponent("raw.db")
        let raw = ChiselKey.Raw(bytes: Data(repeating: 0xAB, count: 32))
        let db = try opened(at: path, key: raw)
        try db.transaction { txn in
            let h = try txn.allocate(Data("bytes".utf8))
            try txn.setRootName("root", h)
        }
        try db.close()

        let again = try opened(at: path, key: .Raw(bytes: Data(repeating: 0xAB, count: 32)))
        XCTAssertNotNil(try again.getRootName("root"))
        try again.close()
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cd swift && swift test`
Expected: PASS, 36 test methods. `testAddRotateAndRemoveKey` is the slowest — each passphrase open runs Argon2id.

- [ ] **Step 3: Commit and open the PR**

```bash
git add swift/Tests/ChiselTests/EncryptionTests.swift
git commit -m "test(swift): cover encrypted open and the key-management surface

No Swift test had ever set Options.encryptionKey or called addKey, rotateKey,
or removeKey — the whole credential surface was reachable only through the
Rust tests, so the Swift wrapper's conversions and error mapping for it were
unverified. Adds a passphrase round trip with a wrong-key rejection, a
three-credential add/rotate/remove sequence, the last-key refusal, and a raw
32-byte key (the HKDF path rather than Argon2).

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin test/swift-7-wrapper-coverage
gh pr create --base main --title "Cover the hand-written Swift wrapper layer (SWIFT-7)" --body "$(cat <<'BODY'
The Swift suite was 156 lines across 6 files and never touched `close()`,
`isPoisoned`, encrypted open, key management, the runtime config setters, or a
file-backed path. The Rust side had equivalents for most of it — it was
specifically the hand-written Swift layer that was untested, which is where every
defect fixed in the preceding PRs turned out to live.

Adds lifecycle/poison/config coverage on both the sync and async layers (the
async store previously had exactly one test of any kind) and a full
encryption/key-management suite.

Coverage-only; no source changes.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 7 — Documentation accuracy: SWIFT-8, -9, -11, -12

Branch: `docs/swift-accuracy`, cut from the PR 6 branch. Closes #121.

### Task 14: Correct the four inaccurate comments

**Files:**
- Modify: `swift/Sources/Chisel/ChiselDatabase.swift` (`isPoisoned`, `transaction`, `savepoint` docs)
- Modify: `swift/Sources/Chisel/ChiselStore.swift` (`isPoisoned` doc)
- Modify: `chisel-ffi/src/types.rs` (SWIFT-12 test comment)

**Interfaces:** none — comment and test-comment changes only.

- [ ] **Step 1: SWIFT-8 — say that a closed handle also reports poisoned**

The Rust side documents both conditions and its test asserts the closed case; both Swift docs dropped the second half. Replace `ChiselDatabase.swift`'s:

```swift
    /// True if a fatal error has poisoned the underlying engine handle.
```

with:

```swift
    /// True if this handle can no longer do work — either a fatal error has
    /// poisoned the engine, OR the handle has already been closed.
    ///
    /// The second condition is easy to misread: a database closed cleanly
    /// reports `true` here. The flag answers "can this handle still work",
    /// not "is the file on disk damaged", so a diagnostics screen that checks
    /// it after shutdown — or the drop-and-reopen recovery flow in
    /// swift/README.md, which closes first — must not conclude the database
    /// is corrupt from a `true` alone.
```

Replace `ChiselStore.swift`'s:

```swift
    /// Whether the engine has poisoned itself (a fatal integrity/I-O failure).
```

with:

```swift
    /// True if this store can no longer do work — either a fatal error has
    /// poisoned the engine, OR it has already been closed. A cleanly closed
    /// store reports `true`; see `ChiselDatabase.isPoisoned`.
```

- [ ] **Step 2: SWIFT-9 — say that the catch arm also covers commit failure**

The commit sits inside the `do`, so a body that returns normally and a commit that then fails still runs the catch arm — issuing a rollback on an engine that just poisoned itself on the commit path, and rethrowing the commit error rather than "what `body` threw". Replace `transaction`'s doc:

```swift
    /// Run `body` inside a transaction. Commits on normal return; on a thrown
    /// error, rolls back (best-effort — a rollback failure must not mask the
    /// original error, so it's swallowed via `try?`) and rethrows what `body`
    /// threw.
```

with:

```swift
    /// Run `body` inside a transaction, committing on normal return.
    ///
    /// The error path covers a throw from `body` OR from the `commit()` that
    /// follows it, and rethrows whichever one threw — the commit is inside the
    /// guarded region deliberately, so a failed commit still attempts a
    /// rollback. Note that a commit failure has already poisoned the engine
    /// (`Chisel::commit`: a failure inside the fsync/superblock protocol is
    /// fatal), so that rollback is expected to fail too; it is best-effort via
    /// `try?` for that reason, and a cleanup failure never masks the error the
    /// caller needs.
    ///
    /// Anything added to the error path therefore runs for commit failures as
    /// well as caller failures — do not put "user code failed" handling there.
```

Replace `savepoint`'s doc:

```swift
    /// Nested rollback point within the active transaction. Releases (keeps
    /// the work, drops the rollback target) on normal return; on a thrown
    /// error, rolls back to `name` — undoing only what happened since this
    /// savepoint, not the whole transaction — then rethrows.
```

with:

```swift
    /// Nested rollback point within the active transaction. Releases on normal
    /// return, keeping the work and dropping the rollback target.
    ///
    /// The error path covers a throw from `body` OR from the `release()` that
    /// follows it, and rethrows whichever one threw. It rolls back to `name` —
    /// undoing only what happened since this savepoint, not the whole
    /// transaction — and then releases the name, so the scope is symmetric and
    /// the same name can be used again in this transaction.
```

- [ ] **Step 3: SWIFT-12 — make the cast test match its claim**

`1_000_000` is ~0.02% of `u32::MAX`, so it round-trips identically through a 32-bit `usize`; the test cannot fail for the reason its comment gives, on any platform. In `chisel-ffi/src/types.rs`, replace the comment and the `large` case of `defrag_options_convert_usize_boundary`:

```rust
    // Exercises the u64 -> usize cast in DefragOptions::from at the only
    // boundary that can actually detect truncation: one past u32::MAX. On a
    // 64-bit usize this round-trips; on a 32-bit usize the cast wraps to 0,
    // silently turning "no limit" semantics into a zero budget. The previous
    // version asserted 1_000_000, which is ~0.02% of u32::MAX and therefore
    // round-trips identically on both, so it could not fail for the reason its
    // comment claimed.
    #[test]
    fn defrag_options_convert_usize_boundary() {
        let zero = DefragOptions {
            sparse_threshold: 0.5,
            max_values: 0,
        };
        let e: chisel::DefragOptions = zero.into();
        assert_eq!(e.sparse_threshold, 0.5);
        assert_eq!(e.max_values, 0usize);

        let past_u32 = u32::MAX as u64 + 1;
        let large = DefragOptions {
            sparse_threshold: 0.5,
            max_values: past_u32,
        };
        let e: chisel::DefragOptions = large.into();
        // usize is 64-bit on every target this crate builds for today (all
        // five Apple slices in build-xcframework.sh are 64-bit). The
        // assertion is written against the cast's real contract rather than
        // that assumption, so it becomes a genuine failure rather than a
        // silent truncation if a 32-bit target is ever added.
        assert_eq!(e.max_values, past_u32 as usize);
        assert_eq!(
            e.max_values as u64, past_u32,
            "u64 -> usize truncated: this target's usize is too narrow for the FFI contract"
        );
    }
```

- [ ] **Step 4: Verify**

Run: `cargo test -p chisel-ffi && (cd swift && swift build)`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add swift/Sources/Chisel/ChiselDatabase.swift swift/Sources/Chisel/ChiselStore.swift chisel-ffi/src/types.rs
git commit -m "docs(swift): correct four comments that described code that does not exist

isPoisoned's docs claimed only the poison condition; the Rust doc they wrap
says a closed handle also reports true, and its test asserts it. A caller
checking health after a clean shutdown read true and concluded corruption.

transaction/savepoint claimed to rethrow 'what body threw'. The commit and
release calls are inside the guarded region, so the error path also runs for a
failed commit and rethrows that instead — and does so against an engine the
commit failure has already poisoned.

The DefragOptions cast test asserted 1_000_000 against a comment claiming a
32-bit truncation boundary. It is 0.02% of u32::MAX and round-trips on every
platform, so the test could not fail for its stated reason. Now asserts one
past u32::MAX.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 15: Regenerate PARITY.md and integrate the binding into the repo docs

**Files:**
- Modify: `swift/PARITY.md`, `README.md`, `ARCHITECTURE.md`, `THEORY.md`

**Interfaces:** none — documentation only.

- [ ] **Step 1: SWIFT-11 — replace rotting line numbers with symbol names**

Eight `ChiselDatabase.swift` citations point at blank lines and mid-comment text, in a table whose header claims it was "verified by reading each file directly". The drift is confined to the block that shifted when a seven-line comment was inserted — which is the argument against line numbers, not against that particular edit.

In `swift/PARITY.md`, replace every `Location` cell of the form `ChiselDatabase.swift:NN` / `ChiselStore.swift:NN` with a symbol reference: `ChiselDatabase.read(_:)`, `ChiselStore.handles()`, and so on. Do the same for the `chisel_ffi.swift` citations (`NativeDatabase.begin()` rather than `Generated:819`).

Replace the header sentence:

```
verified by reading each file directly, not inferred from the plan.
```

with:

```
verified by reading each file directly, not inferred from the plan. Locations
are SYMBOL names, not line numbers: an earlier revision of this table cited
lines, and inserting one comment block into ChiselDatabase.swift silently
invalidated eight of them while the "verified" claim above stayed put.
```

- [ ] **Step 2: Update the source-of-truth reference**

`main` renamed the stub file. Replace `python/chisel/chisel.pyi` with `python/chisel/__init__.pyi` throughout `PARITY.md`, and update the count sentence to match the current file:

Run: `git show origin/main:python/chisel/__init__.pyi | grep -cE '^\s+def [a-z_]+'`
Use that number in the header rather than the stale one.

- [ ] **Step 3: Verify every symbol cited actually exists**

Run:

```bash
cd swift && for sym in $(grep -oE '(ChiselDatabase|ChiselStore|NativeDatabase|Transaction|Savepoint)\.[a-zA-Z]+' PARITY.md | cut -d. -f2 | sort -u); do
  grep -qE "func $sym|var $sym|let $sym" -r Sources/Chisel || echo "MISSING: $sym"
done
```
Expected: no `MISSING:` lines.

- [ ] **Step 4: Integrate into the repo's own docs**

In `README.md`:
- Replace `Cargo workspace with three members` with `Cargo workspace with four members`, and add `the `chisel-ffi/` UniFFI binding` to the member list in that paragraph.
- In the intro paragraph, after the PyO3 sentence, add: `A Swift package for iOS and macOS ships alongside it; see [`swift/README.md`](swift/README.md).`
- Add a `### Swift binding` section after the existing `### Python binding` section:

```markdown
### Swift binding

`chisel-ffi/` is a UniFFI wrapper compiled as a static library for five Apple
targets and packaged as an xcframework; `swift/Sources/Chisel/` is the
hand-written ergonomic layer over the generated bindings, providing a
synchronous `ChiselDatabase` and an async `ChiselStore` actor.

```bash
swift/build-xcframework.sh   # builds all five slices and regenerates bindings
swift test                   # from the repo root
```

The binding is gated by `.github/workflows/swift.yml` on a macOS runner, which
also runs `cargo test -p chisel-ffi` — a root `cargo test` honours
`default-members` and skips it. See [`swift/README.md`](swift/README.md) for
usage and [`swift/PARITY.md`](swift/PARITY.md) for the Python↔Swift map.
```

- In the licence line, extend `the `python/` PyO3 binding and the `bench/` benchmark subcrate` to include `the `chisel-ffi/` UniFFI binding and the `swift/` package`.

In `ARCHITECTURE.md` and `THEORY.md`, add the binding to the layer map wherever the Python binding is described, stating the one structural fact a reader needs: `chisel-ffi` is the only crate that speaks the C ABI, it wraps `chisel::Chisel` in `Mutex<Option<Chisel>>` because the engine is `Send + !Sync`, and every ergonomic construct (transaction closures, the async actor) lives in Swift rather than in Rust.

- [ ] **Step 5: Verify no dangling references**

Run: `grep -rn 'chisel\.pyi' swift/ README.md ARCHITECTURE.md THEORY.md || echo "clean"`
Expected: `clean`

Run: `grep -rn 'three members' README.md ARCHITECTURE.md THEORY.md || echo "clean"`
Expected: `clean`

- [ ] **Step 6: Commit and open the PR**

```bash
git add swift/PARITY.md README.md ARCHITECTURE.md THEORY.md
git commit -m "docs: cite symbols not lines in PARITY.md, and admit the binding to the repo docs

Eight PARITY.md citations pointed at blank lines and mid-comment text, under a
header claiming the table was verified by reading the files. The drift came
entirely from one inserted comment block, which is the argument against line
numbers rather than against that edit. Locations are now symbol names, and the
source-of-truth reference follows main's rename to __init__.pyi.

README/ARCHITECTURE/THEORY described a three-member workspace and a
Python-only binding story.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin docs/swift-accuracy
gh pr create --base main --title "Swift binding documentation accuracy (SWIFT-8, -9, -11, -12)" --body "$(cat <<'BODY'
Closes #121.

- **SWIFT-8** — `isPoisoned`'s docs claimed only the poison condition. The Rust
  doc they wrap says a closed handle also reports `true`, and its test asserts
  it. A caller checking health after a clean shutdown read `true` and concluded
  the file was corrupt.
- **SWIFT-9** — `transaction`/`savepoint` claimed to rethrow "what `body` threw".
  The commit and release calls are inside the guarded region, so the error path
  also runs for a failed commit, against an engine that failure has already
  poisoned.
- **SWIFT-11** — eight PARITY.md citations pointed at blank lines. Now symbol
  names, with the header saying why.
- **SWIFT-12** — the `DefragOptions` cast test asserted `1_000_000` against a
  comment claiming a 32-bit truncation boundary; that value round-trips on every
  platform, so the test could not fail for its stated reason.

Also brings README/ARCHITECTURE/THEORY into line: the workspace has four members
and the binding is no longer undocumented.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---
# PR 8 — SwiftPM distribution

Branch: `feat/swift-distribution`, cut from the PR 7 branch.

`Package.swift` lives in `swift/` and its binary target points at a **gitignored** local
xcframework, so nobody can consume the package by URL: SwiftPM requires the manifest at the
repository root, and the artifact does not exist until someone runs the build script. The
repo has four tags and **zero** GitHub releases, so the manifest must work before any release
asset exists.

### Task 16: Move the manifest to the repo root with an env-switched binary target

**Files:**
- Create: `Package.swift` (repo root)
- Delete: `swift/Package.swift`
- Modify: `.github/workflows/swift.yml`, `.gitignore`

**Interfaces:**
- Produces: a root manifest resolvable both from a clone (local artifact) and by URL (release asset).

- [ ] **Step 1: Create the root manifest**

```swift
// swift-tools-version:5.9
import PackageDescription
import Foundation

// The binary target has two modes because the artifact it names cannot exist in
// both places at once.
//
//   - CONSUMERS resolve `Chisel.xcframework.zip` from a GitHub release asset by
//     URL and checksum. That is the only form SwiftPM can fetch, and it is why
//     this manifest must live at the repository root rather than in `swift/`.
//   - THIS REPO'S CI and local development set CHISEL_LOCAL_XCFRAMEWORK=1 and
//     use the artifact `swift/build-xcframework.sh` just produced. Without this
//     branch, `swift test` on a PR would try to fetch a release that does not
//     exist yet — the repo has zero releases today — and a fresh clone could
//     not build at all.
//
// `swift/release/prepare.sh` rewrites the url and checksum below when a release
// is cut, so the tag carries a manifest describing its own asset.
let useLocalBinary = ProcessInfo.processInfo.environment["CHISEL_LOCAL_XCFRAMEWORK"] != nil

let binaryTarget: Target = useLocalBinary
    ? .binaryTarget(name: "ChiselFFI", path: "swift/Chisel.xcframework")
    : .binaryTarget(
        name: "ChiselFFI",
        url: "https://github.com/pgexperts/chisel/releases/download/v1.0.0/Chisel.xcframework.zip",
        checksum: "0000000000000000000000000000000000000000000000000000000000000000"
    )

let package = Package(
    name: "Chisel",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "Chisel", targets: ["Chisel"]),
    ],
    targets: [
        binaryTarget,
        .target(
            name: "Chisel",
            dependencies: ["ChiselFFI"],
            path: "swift/Sources/Chisel"
        ),
        .testTarget(
            name: "ChiselTests",
            dependencies: ["Chisel"],
            path: "swift/Tests/ChiselTests"
        ),
    ]
)
```

The placeholder checksum is deliberate and is replaced by `prepare.sh` at the first release. Until then, only the `CHISEL_LOCAL_XCFRAMEWORK=1` path is usable — which is documented in the README step below.

- [ ] **Step 2: Remove the old manifest and update ignores**

```bash
git rm swift/Package.swift
```

Add to `.gitignore`:

```
# SwiftPM resolves the root manifest, so its build directory is at the root too.
/.build/
/.swiftpm/
```

- [ ] **Step 3: Verify both modes**

Run: `swift/build-xcframework.sh && CHISEL_LOCAL_XCFRAMEWORK=1 swift test`
Expected: PASS, 36 test methods, run from the **repo root**.

Run: `swift package dump-package > /dev/null && echo "manifest parses in consumer mode"`
Expected: prints the message. (Parsing does not fetch the asset; resolution would.)

- [ ] **Step 4: Point CI at the local artifact**

In `.github/workflows/swift.yml`, replace the three `cd swift && swift …` steps with root-relative ones carrying the environment variable:

```yaml
      # Hard gate: this must pass for the pipeline to be green. The env var
      # selects the xcframework just built above; without it the manifest
      # resolves the release asset by URL, which is wrong for testing the
      # working tree and impossible before the first release exists.
      - name: Swift tests
        env:
          CHISEL_LOCAL_XCFRAMEWORK: "1"
        run: swift test

      - name: Swift tests (ThreadSanitizer, best-effort)
        continue-on-error: true
        env:
          CHISEL_LOCAL_XCFRAMEWORK: "1"
        run: swift test --sanitize=thread

      - name: Swift build (release config)
        env:
          CHISEL_LOCAL_XCFRAMEWORK: "1"
        run: swift build -c release
```

- [ ] **Step 5: Commit**

```bash
git add Package.swift .gitignore .github/workflows/swift.yml
git rm --cached swift/Package.swift 2>/dev/null || true
git commit -m "feat(swift): move the SPM manifest to the repo root for URL consumption

SwiftPM resolves Package.swift from the repository root only, so with the
manifest in swift/ the package could not be added by URL at all — and its
binary target pointed at a gitignored local artifact that does not exist until
the build script runs.

The binary target now switches on CHISEL_LOCAL_XCFRAMEWORK: this repo's CI and
local development use the freshly built artifact, consumers fetch a release
asset. The switch is load-bearing rather than a convenience — the repo has zero
releases, so without it CI would try to fetch a URL that does not exist.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 17: Release preparation script and workflow

**Files:**
- Create: `swift/release/prepare.sh`, `.github/workflows/swift-release.yml`
- Modify: `README.md`

**Interfaces:**
- Produces: `swift/release/prepare.sh <version>`, and a workflow gating the release on a checksum match.

- [ ] **Step 1: Write the prepare script**

Create `swift/release/prepare.sh` (`chmod +x`):

```bash
#!/usr/bin/env bash
# Prepare a Swift release: build the xcframework, zip it, compute its SwiftPM
# checksum, and rewrite the root Package.swift so the tag carries a manifest
# describing its own asset.
#
# The ordering matters. A consumer who pins tag vX.Y.Z gets THAT tag's
# Package.swift, so the url and checksum must already be correct when the tag is
# created — they cannot be patched in afterwards by the release workflow.
#
# Usage:  swift/release/prepare.sh v1.1.0
# Then:   git commit -am "release: vX.Y.Z Swift artifacts" && git tag vX.Y.Z
#         git push && git push --tags, then publish the release on GitHub.
set -euo pipefail

VERSION="${1:?usage: prepare.sh <version-tag>, e.g. v1.1.0}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ZIP="$ROOT/swift/Chisel.xcframework.zip"
REPO="https://github.com/pgexperts/chisel"

echo "==> building the xcframework"
"$ROOT/swift/build-xcframework.sh"

echo "==> zipping"
rm -f "$ZIP"
(cd "$ROOT/swift" && zip -qr "$ZIP" Chisel.xcframework)

echo "==> computing checksum"
CHECKSUM="$(cd "$ROOT" && swift package compute-checksum "$ZIP")"
URL="$REPO/releases/download/$VERSION/Chisel.xcframework.zip"

echo "==> rewriting Package.swift"
# Only the two lines inside the consumer branch of the binary target.
/usr/bin/sed -i '' \
  -e "s|url: \"https://github.com/pgexperts/chisel/releases/download/[^\"]*\"|url: \"$URL\"|" \
  -e "s|checksum: \"[0-9a-f]*\"|checksum: \"$CHECKSUM\"|" \
  "$ROOT/Package.swift"

cat <<MSG

Prepared $VERSION
  asset     $ZIP
  url       $URL
  checksum  $CHECKSUM

Next:
  git commit -am "release: $VERSION Swift artifacts"
  git tag $VERSION && git push && git push --tags
  Publish the release on GitHub; swift-release.yml uploads the asset.

The zip is NOT committed — it is built again by the workflow and uploaded there.
MSG
```

- [ ] **Step 2: Add the zip to `.gitignore`**

```
swift/Chisel.xcframework.zip
```

- [ ] **Step 3: Write the release workflow**

Create `.github/workflows/swift-release.yml`:

```yaml
name: Swift release

# Mirrors publish-crate.yml: fires on a PUBLISHED GitHub Release, a deliberate
# "ship this" action, so a stray or draft tag never uploads an artifact.
on:
  release:
    types: [published]

jobs:
  # A release can be cut against a commit that never went through swift.yml
  # (a hand-pushed tag), so re-verify rather than trusting the release event.
  test-gate:
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test -p chisel-ffi
      - name: Build xcframework
        run: swift/build-xcframework.sh
      - name: Swift tests
        env:
          CHISEL_LOCAL_XCFRAMEWORK: "1"
        run: swift test

  upload:
    needs: test-gate
    runs-on: macos-14
    permissions:
      contents: write # required to upload a release asset
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2

      - name: Build and zip the xcframework
        run: |
          swift/build-xcframework.sh
          (cd swift && zip -qr Chisel.xcframework.zip Chisel.xcframework)

      # The tag's own Package.swift must describe the asset consumers will
      # fetch, so the committed checksum is the contract and this rebuild is
      # checked against it.
      #
      # NOTE: Rust staticlib builds are not bit-reproducible, so this gate can
      # fail on a legitimate re-tag. If that happens, it is not a supply-chain
      # alarm: re-run swift/release/prepare.sh, commit the new checksum, and
      # move the tag. If it proves chronic, switch this job to uploading the
      # artifact prepare.sh produced rather than rebuilding here.
      - name: Verify the committed checksum matches the built artifact
        run: |
          BUILT="$(swift package compute-checksum swift/Chisel.xcframework.zip)"
          COMMITTED="$(grep -oE 'checksum: "[0-9a-f]{64}"' Package.swift | grep -oE '[0-9a-f]{64}')"
          echo "built:     $BUILT"
          echo "committed: $COMMITTED"
          if [ "$BUILT" != "$COMMITTED" ]; then
            echo "::error::Package.swift's checksum does not match the built xcframework."
            echo "::error::Re-run swift/release/prepare.sh, commit, and move the tag."
            exit 1
          fi

      - name: Attach the asset to the release
        env:
          GH_TOKEN: ${{ github.token }}
        run: gh release upload "${{ github.event.release.tag_name }}" swift/Chisel.xcframework.zip --clobber
```

- [ ] **Step 4: Verify both files**

Run: `bash -n swift/release/prepare.sh && python3 -c "import yaml; yaml.safe_load(open('.github/workflows/swift-release.yml')); print('ok')"`
Expected: `ok`

- [ ] **Step 5: Document consumption in the README**

Add to the `### Swift binding` section added in Task 15:

````markdown
**Consuming it from another package.** Add the dependency:

```swift
.package(url: "https://github.com/pgexperts/chisel.git", from: "1.1.0")
```

SwiftPM fetches a prebuilt xcframework from the release asset, so consumers need
no Rust toolchain. Note that this requires a **published GitHub release** — until
one exists, build from a clone with `swift/build-xcframework.sh` and set
`CHISEL_LOCAL_XCFRAMEWORK=1`, which is also how this repo's own CI runs.

To cut a release: `swift/release/prepare.sh vX.Y.Z`, commit, tag, push, then
publish the release on GitHub. `swift-release.yml` re-runs the tests, verifies
the committed checksum against a rebuild, and uploads the asset.
````

- [ ] **Step 6: Commit**

```bash
git add swift/release/prepare.sh .github/workflows/swift-release.yml .gitignore README.md
git commit -m "feat(swift): release preparation script and asset upload workflow

A consumer pinning a tag gets that tag's Package.swift, so its url and checksum
must be correct before the tag exists — they cannot be patched in afterwards.
prepare.sh builds, zips, checksums, and rewrites the manifest so the ordering
works out; the workflow re-runs the tests, verifies the committed checksum
against a rebuild, and uploads.

The checksum gate can fail on a legitimate re-tag because Rust staticlib builds
are not bit-reproducible. The workflow says so, so the first occurrence reads as
expected rather than as a supply-chain alarm.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 18: Record the decisions as ADRs

**Files:**
- Create: two files under `docs/adr/`
- Modify: `docs/adr/0016-swift-binding-via-uniffi.md` (addendum), `docs/adr/README.md` (regenerated)

**Interfaces:** none.

- [ ] **Step 1: Get the next ADR ids**

Run: `python3 ~/.claude/skills/codebase-memory/scripts/adr_index.py docs/adr --next`
Expected: prints the next id. **Never derive the id by eye.** Run it again after writing the first record to get the second id.

- [ ] **Step 2: Write the error-fallback ADR**

Copy `references/template.md` from the codebase-memory skill to `docs/adr/NNNN-tier-unmapped-engine-errors-by-is-fatal.md` and fill it in:

- **Context:** `chisel::ChiselError` is `#[non_exhaustive]`, so a downstream binding cannot match it exhaustively; both bindings need a fallback. The Swift FFI's routed everything to `Poisoned` on a fail-toward-fatal argument. Five variants accumulated there, one of them operational.
- **Decision:** the fallback carries the engine's own `is_fatal()` verdict — FFI case `Other { message, fatal }` — rather than assuming a tier. The mapping guard derives the engine's variant set from `src/error.rs` rather than from a hand-maintained list.
- **Alternatives considered:** keep fail-toward-fatal (rejected: mistiers every future operational variant and asserts an engine state that is not true); add a `#[doc(hidden)]` exhaustive-match helper to the engine (rejected: puts a test-only concern in the public API, and `#[doc(hidden)]` is no semver guarantee).
- **Consequences:** Swift's `category` for the fallback is data rather than a constant. The guard is a text scan coupled to `src/error.rs`'s formatting and fails loudly if that changes.

- [ ] **Step 3: Write the distribution ADR**

`docs/adr/NNNN-swiftpm-distribution-via-release-asset.md`:

- **Context:** SwiftPM resolves only a root `Package.swift`, and a binary target must name an artifact SwiftPM can fetch. The repo had the manifest in `swift/` pointing at a gitignored local build, so the package was unconsumable. There are no releases yet.
- **Decision:** root manifest; binary target switches on `CHISEL_LOCAL_XCFRAMEWORK`; the tag carries its own url and checksum, written by `prepare.sh` before tagging; the release workflow verifies a rebuild against the committed checksum.
- **Alternatives considered:** commit the xcframework (rejected: ~974 MB of build output, the exact thing SWIFT-2's `.gitignore` rules exist to prevent); have the workflow patch `Package.swift` after upload (rejected: the tag would carry a manifest describing a different artifact than the one consumers pinning that tag receive).
- **Consequences:** cutting a release is a scripted two-phase operation rather than a tag push. The checksum gate can fail on a legitimate re-tag because Rust staticlib builds are not bit-reproducible; the fallback is to upload `prepare.sh`'s artifact rather than a rebuild.

- [ ] **Step 4: Add the addendum to ADR 0016**

ADR 0016 is Accepted; **do not rewrite its body**. Append:

```markdown
## Addendum (2026-08-23)

The binding described above landed on `main` on this date. It had been written on
`design/swift-binding`, a branch with no common ancestor with the published
lineage, so it arrived as a file graft rather than a merge; that branch remains
the record of the original 24 commits.

The design held up against a month of engine drift: the consumed API surface
needed no changes beyond the root package's rename to `chisel-storage`. Two
decisions taken during the port are recorded separately — see the ADRs on the
`is_fatal()`-driven error fallback and on SwiftPM distribution.
```

- [ ] **Step 5: Regenerate the index and verify**

Run: `python3 ~/.claude/skills/codebase-memory/scripts/adr_index.py docs/adr`
Expected: exit 0; `docs/adr/README.md` regenerated. **Never edit README.md by hand.**

- [ ] **Step 6: Final full verification**

Run:

```bash
cargo test && \
cargo test -p chisel-ffi && \
cargo clippy --workspace -- -D warnings && \
cargo fmt --check && \
swift/build-xcframework.sh && \
CHISEL_LOCAL_XCFRAMEWORK=1 swift test
```
Expected: all green. This is the spec's verification oracle.

- [ ] **Step 7: Commit and open the PR**

```bash
git add docs/adr
git commit -m "docs(adr): record the error-fallback and SwiftPM distribution decisions

ADR 0016 gets an addendum noting the port; its Accepted body is unchanged.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"

git push -u origin feat/swift-distribution
gh pr create --base main --title "Make the Swift package consumable over SwiftPM" --body "$(cat <<'BODY'
`Package.swift` lived in `swift/` and its binary target pointed at a gitignored
local artifact, so the package could not be added by URL at all: SwiftPM
resolves only a root manifest, and the artifact does not exist until the build
script runs.

- Root `Package.swift` with `path:` overrides keeping sources where they are.
- The binary target switches on `CHISEL_LOCAL_XCFRAMEWORK`. This is load-bearing,
  not a convenience: the repo has zero releases, so without the local branch CI
  would try to fetch a URL that does not exist.
- `prepare.sh` writes the url and checksum before tagging, because a consumer
  pinning a tag gets that tag's manifest — it cannot be patched afterwards.
- `swift-release.yml` re-runs the tests, verifies a rebuild against the committed
  checksum, and uploads the asset.

The checksum gate can fail on a legitimate re-tag because Rust staticlib builds
are not bit-reproducible. The workflow documents that, with the fallback, so the
first occurrence is not read as a supply-chain alarm.

Two ADRs record the decisions; ADR 0016 gets an addendum rather than a rewrite.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---

## Closing out

After PR 8 merges:

- [ ] Close #104, #121, #155 if the PR bodies did not auto-close them.
- [ ] Confirm `swift.yml` ran on each PR — a PR showing no checks means the `branches:` filter crept back in (see #151).
- [ ] Confirm the archive bundle from the Prerequisite section is in its durable home.
