# Bench Engine Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish the `bench/` subcrate, the uniform `Engine` trait, and a working `ChiselEngine` implementation — the foundation that PRs 3–7 of the benchmark-suite series will build on.

**Architecture:** New independent subcrate alongside `python/`, path-depending on the root `chisel` crate. Three small modules: `engine.rs` (trait + `Identifier` newtype + error alias), `chisel_engine.rs` (the `Chisel`-backed implementation), and `lib.rs` (re-exports). One integration smoke test exercises the trait end-to-end through `ChiselEngine`.

**Tech Stack:** Rust 2021, `chisel` (path dep), `tempfile` (dev dep, for the smoke test).

---

## Notes for the executing engineer

- This is PR 2 of the benchmark-suite series. PR 1 (counters instrumentation) is already merged on `main` — `Chisel::counters() -> Result<ChiselCounters>` is the public API surface this plan consumes via `internal_counters()`.
- Spec context: `docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md` §2 (architecture) and §2.2 (Engine trait surface). PR 2 implements only the bottom two layers (Engine trait + ChiselEngine), not Workload, Runner, or Reporter — those come in PRs 4-5.
- redb and SQLite engines are NOT part of PR 2. Their dependencies (redb, rusqlite, criterion) are NOT added in `bench/Cargo.toml` yet — they belong to PRs 3 and 4 respectively.
- The subcrate is an INDEPENDENT cargo crate, matching how `python/` is structured today. There is no `[workspace]` declaration in the repo and we are not adding one. Each crate has its own `target/` directory.
- TDD where it adds value: the smoke test (Task 4) is written before the bits it stresses. Tasks 1-3 are scaffolding/types/impl with verification by compilation; the smoke test is what proves the wiring works.
- If the executing engineer wants isolation, create a worktree before starting Task 1: `git worktree add .worktrees/bench-engine-foundation -b bench-engine-foundation`. All subsequent work happens inside the worktree.
- `cargo clippy -- -D warnings` and `cargo fmt -- --check` must pass at every commit boundary. The PR-1 lessons (doc-comment 2-space hanging indent for list items, never amend a commit) apply here too.

## File Structure

| File | New / Modify | Responsibility |
|------|--------------|----------------|
| `bench/Cargo.toml` | Create | Crate manifest. Path-deps on `chisel`. Dev-dep on `tempfile`. |
| `bench/.gitignore` | Create | Ignore `target/` and `.DS_Store`. |
| `bench/src/lib.rs` | Create | Crate root; re-exports `Engine`, `Identifier`, `EngineResult`, `ChiselEngine`. |
| `bench/src/engine.rs` | Create | `Engine` trait, `Identifier` newtype, `EngineResult` alias. No engine impls. |
| `bench/src/chisel_engine.rs` | Create | `ChiselEngine` struct + `Engine` impl + two constructors (file-backed, in-memory). |
| `bench/tests/smoke.rs` | Create | One integration test exercising the full Engine surface through ChiselEngine. |

No existing files are modified. PR 2 is purely additive.

## Why these decompositions

- `engine.rs` and `chisel_engine.rs` are split because the trait will have multiple impls in later PRs (RedbEngine, SqliteEngine in PR 3). Keeping the trait in its own file makes adding new engines trivially clean — the new file is `redb_engine.rs`, alongside the existing `chisel_engine.rs`, with the trait imported from `engine`.
- `lib.rs` is small and re-exports only what consumers need. Internal types (e.g. constructor helpers) stay private to their modules.
- `tests/smoke.rs` is an integration test (separate from `#[cfg(test)] mod tests` blocks in the source files) because it exercises the full crate-public surface — the same way a downstream consumer would.

---

## Task 1: Create the `bench/` subcrate scaffolding

Pure scaffolding — no logic. Verifies the new crate compiles before any types are defined.

**Files:**
- Create: `bench/Cargo.toml`
- Create: `bench/.gitignore`
- Create: `bench/src/lib.rs`

- [ ] **Step 1: Confirm baseline — full test suite green from repo root**

Run: `cargo test`
Expected: every existing test passes. If anything is red, stop and report.

- [ ] **Step 2: Create `bench/Cargo.toml`**

```toml
[package]
name = "chisel-bench"
version = "0.1.0"
edition = "2021"
description = "Benchmark harness for Chisel — Engine trait abstraction, workload generators, and cross-engine comparison runners. Internal use only; not published."
publish = false

[dependencies]
chisel = { path = ".." }

[dev-dependencies]
tempfile = "3"
```

`publish = false` is deliberate — the bench harness is internal tooling, never goes to crates.io. Same crate name shape as `chisel-py` (PyO3 binding has its own crate name distinct from the root crate).

- [ ] **Step 3: Create `bench/.gitignore`**

```
target/
.DS_Store
```

The `target/` line ignores cargo's per-crate build artifacts; `.DS_Store` is the Apple metadata file that turns up in checkouts on macOS.

- [ ] **Step 4: Create `bench/src/lib.rs` with a placeholder body**

```rust
// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PR 2 (this PR) lands only the bottom layer: the Engine trait and
// ChiselEngine. Subsequent PRs add the other engines (PR 3), the
// workload + runner + micro grid (PR 4), the reporter (PR 5),
// scenarios (PR 6), and CI integration (PR 7).
```

That's the entire file content for now. Re-exports come in Tasks 2 and 3.

- [ ] **Step 5: Verify the subcrate compiles**

Run: `cd bench && cargo build`
Expected: clean build. Cargo will compile `chisel` (the path-dep) into `bench/target/` because each crate has its own target directory in this repo. First build will take a minute or two.

- [ ] **Step 6: Commit**

```bash
git add bench/
git commit -m "bench: scaffold subcrate (PR 2 of bench-suite series)"
```

---

## Task 2: Define the `Engine` trait, `Identifier`, and `EngineResult`

The trait is the API contract that all engine implementations satisfy. PR 2 ships only one impl (`ChiselEngine`), but the trait is designed for the eventual three.

**Files:**
- Create: `bench/src/engine.rs`
- Modify: `bench/src/lib.rs` (add `pub mod engine` + re-exports)

- [ ] **Step 1: Create `bench/src/engine.rs`**

```rust
// Engine trait — uniform façade over chisel, redb, and sqlite.
//
// API mapping policy (per spec §2.3, "handle-as-natural-identifier"):
// each engine returns its native identifier on insert, and we use that
// identifier for subsequent reads/updates/deletes. Chisel's handle,
// redb's caller-generated monotonic key, and SQLite's rowid are all
// valid `Identifier(u64)` values; the trait does not synthesize an
// external key layer.
//
// Read takes `&self`; mutating methods take `&mut self`. This matches
// Chisel's post-F3 shape and fits redb / SQLite naturally.

use chisel::stats::ChiselCounters;
use std::error::Error;

/// Opaque identifier for a value stored in an engine.
///
/// Maps to the native identifier each engine returns on insert:
/// Chisel handle, redb caller-generated key, or SQLite rowid. The
/// wrapper exists so the harness operates on a uniform `u64`-shaped
/// identifier across engines without leaking engine-specific types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identifier(pub u64);

/// Trait-wide error type. Each engine impl boxes its native error
/// into this — `ChiselError`, `redb::Error`, `rusqlite::Error` all
/// implement `std::error::Error` and convert via the standard
/// `Box<dyn Error>` blanket `From` impl.
///
/// `Send + Sync` is included so a `Box<dyn Engine>` can be moved
/// across thread boundaries even though the engines themselves are
/// single-threaded — a future Criterion configuration may want to
/// drop this constraint or retain it; including it now is no
/// runtime cost and keeps options open.
pub type EngineResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Uniform façade over a transactional storage engine.
///
/// The trait excludes engine construction (each impl has its own
/// `new` / `open` constructors with engine-specific options), and
/// excludes durability-mode configuration (PR 3 will add that as
/// either constructor parameters or builder methods on each impl).
///
/// Method ordering: transaction control first, then the five CRUD
/// operations (4 mutating + 1 read), then introspection.
pub trait Engine {
    fn begin(&mut self) -> EngineResult<()>;
    fn commit(&mut self) -> EngineResult<()>;
    fn rollback(&mut self) -> EngineResult<()>;

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier>;
    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>>;
    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()>;
    fn delete(&mut self, id: Identifier) -> EngineResult<()>;
    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()>;

    /// Current size of the engine's backing file in bytes. For
    /// in-memory backings, returns the size of the in-memory
    /// representation. The runner reports deltas of this for the
    /// `file_size_delta` column of the diagnostic table.
    fn file_size_bytes(&self) -> EngineResult<u64>;

    /// Engine-internal counters. Returns `Some` for `ChiselEngine`
    /// (where the counters live as `Chisel::counters()`) and `None`
    /// for engines without instrumentation. The runner surfaces
    /// these as Chisel-only sub-columns in the output.
    fn internal_counters(&self) -> Option<ChiselCounters>;
}
```

- [ ] **Step 2: Update `bench/src/lib.rs` to export the new module**

Replace the entire contents of `bench/src/lib.rs` with:

```rust
// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PR 2 (this PR) lands only the bottom layer: the Engine trait and
// ChiselEngine. Subsequent PRs add the other engines (PR 3), the
// workload + runner + micro grid (PR 4), the reporter (PR 5),
// scenarios (PR 6), and CI integration (PR 7).

pub mod engine;

pub use engine::{Engine, EngineResult, Identifier};
```

- [ ] **Step 3: Verify it compiles**

Run: `cd bench && cargo build`
Expected: clean build. No warnings.

- [ ] **Step 4: Verify clippy and fmt are clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: silent (clean).

- [ ] **Step 5: Commit**

```bash
git add bench/src/lib.rs bench/src/engine.rs
git commit -m "bench: define Engine trait, Identifier, EngineResult"
```

---

## Task 3: Implement `ChiselEngine`

The first (and, in PR 2, only) `Engine` implementation. Wraps a `Chisel` instance and translates between the trait's `Identifier(u64)` and Chisel's `u64` handle.

**Files:**
- Create: `bench/src/chisel_engine.rs`
- Modify: `bench/src/lib.rs` (add `pub mod chisel_engine` + re-export)

- [ ] **Step 1: Create `bench/src/chisel_engine.rs`**

```rust
// ChiselEngine — Engine trait impl backed by the Chisel storage engine.
//
// Constructors:
//   ChiselEngine::open_file(path, cache_size) — file-backed.
//   ChiselEngine::open_in_memory(cache_size)  — Vec<u8>-backed; same
//                                              code path, no fsync
//                                              durability, used for
//                                              fast smoke tests.
//
// Identifier(u64) ↔ chisel handle is a 1:1 transparent mapping.
// All trait method bodies are 1-line delegations to the Chisel
// public API.

use crate::engine::{Engine, EngineResult, Identifier};
use chisel::stats::ChiselCounters;
use chisel::{Chisel, Options};
use std::path::Path;

pub struct ChiselEngine {
    db: Chisel,
}

impl ChiselEngine {
    /// Open or create a file-backed Chisel database.
    pub fn open_file(path: &Path, cache_size: usize) -> EngineResult<Self> {
        let db = Chisel::open(
            path,
            Options {
                cache_size,
                ..Default::default()
            },
        )?;
        Ok(Self { db })
    }

    /// Open an in-memory Chisel database. Same engine, no durability;
    /// for smoke tests and any benchmark that doesn't need a real file.
    pub fn open_in_memory(cache_size: usize) -> EngineResult<Self> {
        let db = Chisel::open_in_memory_with_options(Options {
            cache_size,
            ..Default::default()
        })?;
        Ok(Self { db })
    }
}

impl Engine for ChiselEngine {
    fn begin(&mut self) -> EngineResult<()> {
        Ok(self.db.begin()?)
    }

    fn commit(&mut self) -> EngineResult<()> {
        Ok(self.db.commit()?)
    }

    fn rollback(&mut self) -> EngineResult<()> {
        Ok(self.db.rollback()?)
    }

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier> {
        Ok(Identifier(self.db.allocate(value)?))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        Ok(self.db.read(id.0)?)
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        Ok(self.db.update(id.0, value)?)
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        Ok(self.db.delete(id.0)?)
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        // Allocate-and-collect rather than transmute: Identifier is
        // `#[repr(transparent)]`-shaped today (pure newtype) but we
        // don't depend on that — the conversion is cheap and the
        // intent is clear.
        let handles: Vec<u64> = ids.iter().map(|i| i.0).collect();
        Ok(self.db.delete_many(&handles)?)
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        Ok(self.db.stats()?.file_size_bytes)
    }

    fn internal_counters(&self) -> Option<ChiselCounters> {
        // Counters returns Result<ChiselCounters>. A poisoned engine
        // gives Err — surface as None so the runner doesn't propagate
        // poison through the Option-shaped trait method. The runner's
        // own happy-path checks will catch poison at the next call.
        self.db.counters().ok()
    }
}
```

- [ ] **Step 2: Update `bench/src/lib.rs`**

Replace the contents of `bench/src/lib.rs` with:

```rust
// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PR 2 (this PR) lands only the bottom layer: the Engine trait and
// ChiselEngine. Subsequent PRs add the other engines (PR 3), the
// workload + runner + micro grid (PR 4), the reporter (PR 5),
// scenarios (PR 6), and CI integration (PR 7).

pub mod chisel_engine;
pub mod engine;

pub use chisel_engine::ChiselEngine;
pub use engine::{Engine, EngineResult, Identifier};
```

- [ ] **Step 3: Verify it compiles**

Run: `cd bench && cargo build`
Expected: clean build, no warnings.

- [ ] **Step 4: Verify clippy and fmt are clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: silent (clean).

- [ ] **Step 5: Commit**

```bash
git add bench/src/lib.rs bench/src/chisel_engine.rs
git commit -m "bench: implement ChiselEngine"
```

---

## Task 4: Smoke test for `ChiselEngine` through the Engine trait

End-to-end integration test. Exercises every Engine trait method through `ChiselEngine`, including `internal_counters()` returning `Some`. Written as if it were a downstream consumer of the bench crate.

**Files:**
- Create: `bench/tests/smoke.rs`

- [ ] **Step 1: Create `bench/tests/smoke.rs`**

```rust
// Smoke test for the bench crate's Engine trait surface, exercised
// through ChiselEngine. Goal: every Engine method gets called once
// in a realistic sequence; internal_counters() returns Some with
// monotonically advancing values; the trait abstraction does not
// hide engine-specific bugs.
//
// Uses in-memory Chisel for speed — no real file, no fsync. The
// counters still advance because in-memory Chisel calls fsync as a
// no-op (the counter increments regardless, by design — see PR 1
// commit `a051600` and the note in `PageIo::fsync`).

use chisel_bench::{ChiselEngine, Engine, Identifier};

#[test]
fn smoke_full_lifecycle_through_engine_trait() {
    let mut engine = ChiselEngine::open_in_memory(64).expect("open in-memory");

    // Counters must be Some for ChiselEngine (the spec's contract).
    let baseline = engine.internal_counters().expect("Chisel exposes counters");

    // Allocate three values inside one transaction.
    engine.begin().expect("begin");
    let a: Identifier = engine.allocate(b"alpha").expect("allocate alpha");
    let b: Identifier = engine.allocate(b"beta").expect("allocate beta");
    let c: Identifier = engine.allocate(b"gamma").expect("allocate gamma");
    engine.commit().expect("commit");

    // Read them back outside any transaction (read takes &self).
    assert_eq!(engine.read(a).expect("read a"), b"alpha");
    assert_eq!(engine.read(b).expect("read b"), b"beta");
    assert_eq!(engine.read(c).expect("read c"), b"gamma");

    // Update and verify.
    engine.begin().expect("begin");
    engine.update(b, b"BETA").expect("update b");
    engine.commit().expect("commit");
    assert_eq!(engine.read(b).expect("read b'"), b"BETA");

    // Delete one, batch-delete two.
    engine.begin().expect("begin");
    engine.delete(a).expect("delete a");
    engine.delete_many(&[b, c]).expect("delete_many b,c");
    engine.commit().expect("commit");

    // Rollback path: begin, allocate, rollback. Resulting handle
    // must not be readable after rollback (it never became durable).
    engine.begin().expect("begin");
    let ghost: Identifier = engine.allocate(b"ghost").expect("allocate ghost");
    engine.rollback().expect("rollback");
    assert!(
        engine.read(ghost).is_err(),
        "rolled-back handle must not be readable"
    );

    // Counters advanced.
    let after = engine.internal_counters().expect("Chisel still exposes counters");
    assert!(
        after.fsync_calls > baseline.fsync_calls,
        "commits must advance fsync_calls"
    );
    assert!(
        after.pages_allocated > baseline.pages_allocated,
        "allocations must advance pages_allocated"
    );

    // file_size_bytes is non-zero (in-memory backing reports the
    // representation size; cannot be empty after this much work).
    let size = engine.file_size_bytes().expect("file_size_bytes");
    assert!(size > 0, "file_size_bytes must reflect allocated pages");
}
```

- [ ] **Step 2: Run the test**

Run: `cd bench && cargo test --test smoke`
Expected: PASS. One test, all assertions hold.

- [ ] **Step 3: Verify clippy and fmt are clean**

Run: `cd bench && cargo clippy --tests -- -D warnings && cargo fmt -- --check`
Expected: silent (clean). Note `--tests` so clippy compiles the test target too.

- [ ] **Step 4: Commit**

```bash
git add bench/tests/smoke.rs
git commit -m "bench: smoke test for ChiselEngine through Engine trait"
```

---

## Task 5: Final gate

Confirm the worktree is clean, all checks pass, and no regression at the root crate.

- [ ] **Step 1: Full Rust suite at root (regression check)**

Run: `cargo test`
Expected: every existing test still passes (271 from PR 1's accounting). The bench subcrate's tests are not run because each crate is independent.

- [ ] **Step 2: Bench subcrate test suite**

Run: `cd bench && cargo test`
Expected: 1 passed (smoke).

- [ ] **Step 3: Clippy at root**

Run: `cargo clippy -- -D warnings`
Expected: clean.

- [ ] **Step 4: Clippy in bench**

Run: `cd bench && cargo clippy --tests -- -D warnings`
Expected: clean.

- [ ] **Step 5: Fmt at root**

Run: `cargo fmt -- --check`
Expected: clean.

- [ ] **Step 6: Fmt in bench**

Run: `cd bench && cargo fmt -- --check`
Expected: clean.

- [ ] **Step 7: Confirm git state**

Run: `git status`
Expected: working tree clean. `git log --oneline -5` shows the four PR-2 commits in order:
1. bench: scaffold subcrate (PR 2 of bench-suite series)
2. bench: define Engine trait, Identifier, EngineResult
3. bench: implement ChiselEngine
4. bench: smoke test for ChiselEngine through Engine trait

If anything is unexpected, do NOT proceed; report.

---

## Done

PR 2 is complete when all five tasks above are done and gates 1-7 of Task 5 pass. The next step (out of scope for this plan) is `superpowers:finishing-a-development-branch` to merge to main.

PR 3 (RedbEngine + SqliteEngine) follows. It will modify `bench/Cargo.toml` to add `redb` and `rusqlite` as dependencies, add `bench/src/redb_engine.rs` and `bench/src/sqlite_engine.rs`, and add a cross-engine equivalence test. None of that is in PR 2.
