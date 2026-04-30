# Bench Workload Data Layer Implementation Plan (PR 4a)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land an engine-agnostic Workload data layer (`Operation` enum, `Workload` struct, six seeded generator functions) inside the existing `bench/` subcrate, with no engine imports and ~250 LOC including inline tests.

**Architecture:** New `bench/src/workload.rs` is a pure data module — generators are deterministic functions of `(seed, params)` returning a `Workload { name, seed, prepop_count, ops }`. Operations reference records by allocation-order index (alloc_index), so the data shape is engine-agnostic — the Runner (PR 4b) maintains the index-to-engine-identifier mapping at execution time. PRNG is pinned to `rand_chacha::ChaCha8Rng` so workload output survives `rand` minor-version bumps.

**Tech Stack:** Rust 2021 edition, `rand 0.8`, `rand_chacha 0.3`. No new build features. Tests are inline `#[cfg(test)] mod tests` — no `tests/` directory file, no proptest, no engine deps.

**Spec:** `docs/superpowers/specs/2026-04-30-chisel-bench-workload-data-layer-design.md`

---

## Task 1: Add `rand` and `rand_chacha` dependencies

**Files:**
- Modify: `bench/Cargo.toml`

- [ ] **Step 1: Edit `bench/Cargo.toml`**

Open `bench/Cargo.toml`. Append two lines to the `[dependencies]` section, immediately after the existing `rusqlite` line:

```toml
[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }
rand = "0.8"
rand_chacha = "0.3"
```

The `[dev-dependencies]` block (with `tempfile`) stays unchanged.

- [ ] **Step 2: Verify the bench subcrate still builds**

Run: `cd bench && cargo build`
Expected: clean build, "Finished" line. New crates `rand`, `rand_chacha`, and any transitive deps appear in `bench/Cargo.lock` (or workspace lockfile).

- [ ] **Step 3: Verify existing tests still pass**

Run: `cd bench && cargo test`
Expected: all PR 3 equivalence tests still pass (15 named tests). No new tests yet.

- [ ] **Step 4: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock
git commit -m "$(cat <<'EOF'
bench: add rand and rand_chacha deps for workload generators

Pinned versions: rand 0.8 + rand_chacha 0.3 (the matched-pair). Used by
the workload data layer in PR 4a. ChaCha8Rng (not StdRng) so workload
output stays byte-identical across rand minor-version bumps.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

If `bench/Cargo.lock` does not exist (workspace mode), substitute the workspace lockfile path. Run `git status` first to confirm which lockfile changed.

---

## Task 2: Scaffold `workload.rs` with `Operation` and `Workload` types

**Files:**
- Create: `bench/src/workload.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Write the failing scaffold test**

Create `bench/src/workload.rs` containing only this test (no types yet — we want a compile-fail first to confirm the test is wired):

```rust
// Pure data-layer for benchmark workloads. No engine interaction —
// a `Workload` can be built, inspected, and unit-tested without any
// of the engine impls compiled in. The Runner (PR 4b) consumes
// `Workload` values and drives them against an `Engine`.
//
// Determinism contract: `(seed, params) -> Workload` is a pure
// function. We pin `ChaCha8Rng` rather than `StdRng` so result
// reproducibility survives `rand` minor bumps.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_construct_and_compare() {
        let op_a: Operation = Operation::Read { alloc_index: 7 };
        let op_b: Operation = Operation::Read { alloc_index: 7 };
        assert_eq!(op_a, op_b);

        let w = Workload {
            name: "test".to_string(),
            seed: 42,
            prepop_count: 100,
            ops: vec![op_a],
        };
        assert_eq!(w.name, "test");
        assert_eq!(w.ops.len(), 1);
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs`**

Edit `bench/src/lib.rs`. Add the module declaration and re-exports next to the existing ones:

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
// PRs 1–2 + PR-A + PR 3 landed the Engine trait and all three engine
// impls. PR 4a (this PR) lands the Workload data layer. PR 4b adds
// the Runner + 270-cell registration. PRs 5–7 follow.

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod sqlite_engine;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
```

- [ ] **Step 3: Run the test, expect a compile error**

Run: `cd bench && cargo test workload::tests::types_construct_and_compare`
Expected: compile error — `cannot find type Operation in this scope` and `cannot find type Workload in this scope`.

This proves the test is reachable (lib.rs wired correctly) and that the types are not yet defined.

- [ ] **Step 4: Add `Operation` and `Workload` types to `workload.rs`**

Replace the contents of `bench/src/workload.rs` with:

```rust
// Pure data-layer for benchmark workloads. No engine interaction —
// a `Workload` can be built, inspected, and unit-tested without any
// of the engine impls compiled in. The Runner (PR 4b) consumes
// `Workload` values and drives them against an `Engine`.
//
// Determinism contract: `(seed, params) -> Workload` is a pure
// function. We pin `ChaCha8Rng` rather than `StdRng` so result
// reproducibility survives `rand` minor bumps.

/// One unit of work the Runner executes against an Engine.
///
/// `alloc_index` is a position in the Runner's "allocations seen so
/// far" vector (per the alloc-order-index identifier scheme). Each
/// generator guarantees every emitted index is live (allocated and
/// not deleted) at that point in the sequence — so the Runner does
/// not need to filter or validate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Allocate a value of `size` bytes. The Runner fills the payload
    /// with `0u8` — content compressibility is out of scope.
    Allocate { size: usize },
    /// Read the value at allocation index `alloc_index`.
    Read { alloc_index: usize },
    /// Replace the value at `alloc_index` with `size` filler bytes.
    /// Independent of the original value's size; micro-grid generators
    /// happen to keep them equal but the type does not require it.
    Update { alloc_index: usize, size: usize },
    /// Delete the value at `alloc_index`. Generators do not emit
    /// further references to that index after a Delete.
    Delete { alloc_index: usize },
    /// Bulk-delete a list of indices in a single Operation. Same
    /// liveness contract as `Delete`. The only variant that carries
    /// a heap allocation, hence Operation derives Clone but not Copy.
    DeleteMany { alloc_indices: Vec<usize> },
}

/// A deterministic sequence of Operations plus enough metadata to
/// identify which generator produced it.
///
/// `name` is the Criterion BenchmarkId stem (e.g., "read_random").
/// The Runner appends size and other parameters when constructing
/// final BenchmarkIds — keeping size out of `name` lets one workload
/// drive multiple cells if a future caller wants to reuse it.
///
/// `seed` is preserved so logs and reports can answer "exactly what
/// ran?" `prepop_count` records how many records the workload assumes
/// already exist when execution starts; the Runner uses it to size
/// the pre-population step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    pub name: String,
    pub seed: u64,
    pub prepop_count: usize,
    pub ops: Vec<Operation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_construct_and_compare() {
        let op_a: Operation = Operation::Read { alloc_index: 7 };
        let op_b: Operation = Operation::Read { alloc_index: 7 };
        assert_eq!(op_a, op_b);

        let w = Workload {
            name: "test".to_string(),
            seed: 42,
            prepop_count: 100,
            ops: vec![op_a],
        };
        assert_eq!(w.name, "test");
        assert_eq!(w.ops.len(), 1);
    }
}
```

- [ ] **Step 5: Run the test, expect pass**

Run: `cd bench && cargo test workload::tests::types_construct_and_compare`
Expected: 1 passed.

- [ ] **Step 6: Verify clippy and fmt are clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/workload.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: scaffold workload data-layer types

Add Operation enum + Workload struct in bench/src/workload.rs and
re-export through lib.rs. No generators yet — those land per-generator
in subsequent commits to keep diffs reviewable.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Implement `gen_prepopulate`

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing test**

Add this test to the `tests` module in `bench/src/workload.rs` (above the closing `}` of `mod tests`):

```rust
    #[test]
    fn gen_prepopulate_shape() {
        let w = gen_prepopulate(10, 32);
        assert_eq!(w.name, "prepopulate");
        assert_eq!(w.seed, 0);
        assert_eq!(w.prepop_count, 0);
        assert_eq!(w.ops.len(), 10);
        for op in &w.ops {
            assert!(matches!(op, Operation::Allocate { size: 32 }));
        }
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::gen_prepopulate_shape`
Expected: compile error — `cannot find function gen_prepopulate in this scope`.

- [ ] **Step 3: Implement `gen_prepopulate`**

Add this function to `bench/src/workload.rs`, immediately above the `#[cfg(test)]` line:

```rust
/// Pre-population: `count` Allocate ops of `size` bytes each.
/// No randomness — the Workload is fully determined by its arguments.
/// `seed` is fixed at 0 and `name` at "prepopulate" so that
/// pre-population does not appear as a varying axis in Criterion ids.
/// `prepop_count` is 0 because the workload itself does the populating
/// — it does not assume any pre-existing records.
pub fn gen_prepopulate(count: usize, size: usize) -> Workload {
    let ops = (0..count).map(|_| Operation::Allocate { size }).collect();
    Workload {
        name: "prepopulate".to_string(),
        seed: 0,
        prepop_count: 0,
        ops,
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cd bench && cargo test workload::tests::gen_prepopulate_shape`
Expected: 1 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: gen_prepopulate generator + shape test

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Implement `gen_allocate`

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing test**

Add this test to the `tests` module:

```rust
    #[test]
    fn gen_allocate_shape() {
        let w = gen_allocate(10, 64);
        assert_eq!(w.name, "allocate");
        assert_eq!(w.seed, 0);
        assert_eq!(w.prepop_count, 0);
        assert_eq!(w.ops.len(), 10);
        for op in &w.ops {
            assert!(matches!(op, Operation::Allocate { size: 64 }));
        }
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::gen_allocate_shape`
Expected: compile error — `cannot find function gen_allocate in this scope`.

- [ ] **Step 3: Implement `gen_allocate`**

Add this function to `bench/src/workload.rs`, immediately above the `#[cfg(test)]` line:

```rust
/// Row 1/2 of the micro grid (allocate, 1 op/tx and 1000 ops/tx).
/// `count` Allocate ops of `size` bytes. No randomness; takes no
/// seed (parallels `gen_prepopulate`, where seedless and unambiguous
/// beats symmetry-with-an-unused-arg). The Workload's `seed` field
/// is set to 0; `prepop_count` is 0 because rows 1/2 measure
/// allocate against an empty database.
pub fn gen_allocate(count: usize, size: usize) -> Workload {
    let ops = (0..count).map(|_| Operation::Allocate { size }).collect();
    Workload {
        name: "allocate".to_string(),
        seed: 0,
        prepop_count: 0,
        ops,
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cd bench && cargo test workload::tests::gen_allocate_shape`
Expected: 1 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: gen_allocate generator + shape test

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Implement `gen_read_random` (first randomized generator)

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Add `use` statements for `rand` traits and `ChaCha8Rng`**

Edit `bench/src/workload.rs`. Add these `use` statements right after the file's header doc comment (before the `Operation` enum):

```rust
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
```

The `rand::seq::index::sample` call in later tasks uses a fully-qualified path, so no separate `use` for that.

- [ ] **Step 2: Write three failing tests covering determinism, validity, and cross-seed independence**

Add these tests to the `tests` module:

```rust
    #[test]
    fn gen_read_random_determinism() {
        let a = gen_read_random(42, 1000, 100);
        let b = gen_read_random(42, 1000, 100);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_read_random_validity() {
        let prepop = 1000;
        let w = gen_read_random(1, prepop, 500);
        assert_eq!(w.name, "read_random");
        assert_eq!(w.seed, 1);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), 500);
        for op in &w.ops {
            match op {
                Operation::Read { alloc_index } => {
                    assert!(*alloc_index < prepop, "out-of-range index {}", alloc_index);
                }
                other => panic!("expected Read, got {:?}", other),
            }
        }
    }

    #[test]
    fn gen_read_random_cross_seed_independence() {
        let a = gen_read_random(1, 1000, 100);
        let b = gen_read_random(2, 1000, 100);
        assert_ne!(a.ops, b.ops);
    }
```

- [ ] **Step 3: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::gen_read_random`
Expected: compile error — `cannot find function gen_read_random in this scope`.

- [ ] **Step 4: Implement `gen_read_random`**

Add this function above the `#[cfg(test)]` line:

```rust
/// Row 3/4 of the micro grid (read warm/cold). `count` Read ops with
/// alloc_indices sampled uniformly with replacement from
/// `0..prepop_count`. The same alloc_index may appear multiple times
/// in the workload — that is intentional, mirroring real read access
/// where popular records get hit repeatedly.
pub fn gen_read_random(seed: u64, prepop_count: usize, count: usize) -> Workload {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let ops = (0..count)
        .map(|_| Operation::Read {
            alloc_index: rng.gen_range(0..prepop_count),
        })
        .collect();
    Workload {
        name: "read_random".to_string(),
        seed,
        prepop_count,
        ops,
    }
}
```

- [ ] **Step 5: Run all three tests, expect pass**

Run: `cd bench && cargo test workload::tests::gen_read_random`
Expected: 3 passed.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: gen_read_random generator + determinism/validity/seed tests

First randomized generator. ChaCha8Rng seeded from u64; uniform-random
sampling with replacement. Tests cover determinism, index validity,
and cross-seed independence — the three properties every randomized
generator must satisfy.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Implement `gen_update_random`

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn gen_update_random_determinism() {
        let a = gen_update_random(42, 1000, 100, 64);
        let b = gen_update_random(42, 1000, 100, 64);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_update_random_validity() {
        let prepop = 1000;
        let w = gen_update_random(1, prepop, 500, 256);
        assert_eq!(w.name, "update_random");
        assert_eq!(w.seed, 1);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), 500);
        for op in &w.ops {
            match op {
                Operation::Update { alloc_index, size } => {
                    assert!(*alloc_index < prepop, "out-of-range index {}", alloc_index);
                    assert_eq!(*size, 256);
                }
                other => panic!("expected Update, got {:?}", other),
            }
        }
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::gen_update_random`
Expected: compile error — `cannot find function gen_update_random`.

- [ ] **Step 3: Implement `gen_update_random`**

Add above the `#[cfg(test)]` line:

```rust
/// Row 5/6 of the micro grid (update, 1 op/tx and 1000 ops/tx).
/// Same selection scheme as `gen_read_random` — uniform random with
/// replacement — but emits Update ops with `size` bytes of filler.
pub fn gen_update_random(
    seed: u64,
    prepop_count: usize,
    count: usize,
    size: usize,
) -> Workload {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let ops = (0..count)
        .map(|_| Operation::Update {
            alloc_index: rng.gen_range(0..prepop_count),
            size,
        })
        .collect();
    Workload {
        name: "update_random".to_string(),
        seed,
        prepop_count,
        ops,
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cd bench && cargo test workload::tests::gen_update_random`
Expected: 2 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: gen_update_random generator + determinism/validity tests

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Implement `gen_delete_random` (first without-replacement generator)

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn gen_delete_random_determinism() {
        let a = gen_delete_random(42, 1000, 500);
        let b = gen_delete_random(42, 1000, 500);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_delete_random_no_replacement() {
        use std::collections::HashSet;
        let prepop = 1000;
        let count = 500;
        let w = gen_delete_random(1, prepop, count);
        assert_eq!(w.name, "delete_random");
        assert_eq!(w.seed, 1);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), count);

        let mut seen: HashSet<usize> = HashSet::new();
        for op in &w.ops {
            match op {
                Operation::Delete { alloc_index } => {
                    assert!(*alloc_index < prepop, "out-of-range index {}", alloc_index);
                    assert!(seen.insert(*alloc_index), "duplicate index {}", alloc_index);
                }
                other => panic!("expected Delete, got {:?}", other),
            }
        }
        assert_eq!(seen.len(), count);
    }

    #[test]
    #[should_panic(expected = "exceeds prepop_count")]
    fn gen_delete_random_panics_on_overcount() {
        // count > prepop_count is a generator-author bug; we panic
        // rather than silently produce an invalid workload.
        let _ = gen_delete_random(0, 10, 11);
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::gen_delete_random`
Expected: compile error — `cannot find function gen_delete_random`.

- [ ] **Step 3: Implement `gen_delete_random`**

Add above the `#[cfg(test)]` line:

```rust
/// Row 7/8 of the micro grid (delete, 1 op/tx and 1000 ops/tx).
/// Sampled WITHOUT replacement so no index is deleted twice within
/// the workload — uses Floyd-style sampling via `rand::seq::index::sample`,
/// O(count) time, no need to materialize the full `0..prepop_count` Vec.
///
/// Panics if `count > prepop_count`. This is a generator-author bug
/// (you cannot delete more records than exist), not a runtime
/// condition; failing loud at construction time beats silently
/// producing a malformed workload.
pub fn gen_delete_random(seed: u64, prepop_count: usize, count: usize) -> Workload {
    assert!(
        count <= prepop_count,
        "gen_delete_random: count ({}) exceeds prepop_count ({})",
        count,
        prepop_count
    );
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let indices = rand::seq::index::sample(&mut rng, prepop_count, count);
    let ops = indices
        .iter()
        .map(|alloc_index| Operation::Delete { alloc_index })
        .collect();
    Workload {
        name: "delete_random".to_string(),
        seed,
        prepop_count,
        ops,
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cd bench && cargo test workload::tests::gen_delete_random`
Expected: 3 passed (including the `#[should_panic]` test).

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: gen_delete_random generator + no-replacement + panic tests

First without-replacement generator. Uses rand::seq::index::sample
(Floyd's algorithm) — O(count) time with no full-range Vec materialized.
Panics on count > prepop_count: caller bug, not runtime condition.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Implement `gen_delete_many`

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn gen_delete_many_determinism() {
        let a = gen_delete_many(7, 10_000, 5, 200);
        let b = gen_delete_many(7, 10_000, 5, 200);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_delete_many_cross_batch_no_replacement() {
        use std::collections::HashSet;
        let prepop = 1000;
        let batches = 5;
        let batch_size = 100;
        let w = gen_delete_many(2, prepop, batches, batch_size);
        assert_eq!(w.name, "delete_many");
        assert_eq!(w.seed, 2);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), batches);

        let mut seen: HashSet<usize> = HashSet::new();
        for op in &w.ops {
            match op {
                Operation::DeleteMany { alloc_indices } => {
                    assert_eq!(alloc_indices.len(), batch_size);
                    for &i in alloc_indices {
                        assert!(i < prepop, "out-of-range index {}", i);
                        assert!(seen.insert(i), "duplicate index {} across batches", i);
                    }
                }
                other => panic!("expected DeleteMany, got {:?}", other),
            }
        }
        assert_eq!(seen.len(), batches * batch_size);
    }

    #[test]
    #[should_panic(expected = "exceeds prepop_count")]
    fn gen_delete_many_panics_on_overcount() {
        let _ = gen_delete_many(0, 10, 3, 4); // 3 * 4 = 12 > 10
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::gen_delete_many`
Expected: compile error — `cannot find function gen_delete_many`.

- [ ] **Step 3: Implement `gen_delete_many`**

Add above the `#[cfg(test)]` line:

```rust
/// Row 9 of the micro grid (delete_many — bulk-delete primitive).
/// Each Operation is one DeleteMany carrying `batch_size` distinct
/// indices; `batches` total. Sampled without replacement across the
/// entire workload — no index appears in two different batches —
/// via a single `rand::seq::index::sample` call chunked into batches.
///
/// Panics if `batches * batch_size > prepop_count` (same rationale
/// as `gen_delete_random`: caller bug, fail loud).
pub fn gen_delete_many(
    seed: u64,
    prepop_count: usize,
    batches: usize,
    batch_size: usize,
) -> Workload {
    let total = batches * batch_size;
    assert!(
        total <= prepop_count,
        "gen_delete_many: batches*batch_size ({}) exceeds prepop_count ({})",
        total,
        prepop_count
    );
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all = rand::seq::index::sample(&mut rng, prepop_count, total).into_vec();
    let ops = all
        .chunks(batch_size)
        .map(|chunk| Operation::DeleteMany {
            alloc_indices: chunk.to_vec(),
        })
        .collect();
    Workload {
        name: "delete_many".to_string(),
        seed,
        prepop_count,
        ops,
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cd bench && cargo test workload::tests::gen_delete_many`
Expected: 3 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: gen_delete_many generator + cross-batch no-replacement test

Bulk-delete generator for micro-grid row 9. Single sample call yielding
batches*batch_size distinct indices, then chunked into batches — so
the cross-batch no-replacement guarantee is structural, not asserted.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Verify acceptance criteria

**Files:**
- Read-only checks across `bench/src/workload.rs` and the bench subcrate.

- [ ] **Step 1: Verify the engine-agnostic invariant**

Run from the repo root:
```bash
grep -n "use chisel::\|use redb::\|use rusqlite::" bench/src/workload.rs
```
Expected: no output. The data-layer module must not import from any engine crate. If grep prints anything, the abstraction has leaked and the file needs revisiting.

- [ ] **Step 2: Run the full bench test suite**

Run: `cd bench && cargo test`
Expected: all pre-existing tests still pass (15 equivalence tests from PR 3 + the new workload tests). Count the new ones: 14 in total across tasks 2–8 (1 + 1 + 1 + 3 + 2 + 3 + 3).

- [ ] **Step 3: Run clippy with deny-warnings on the bench subcrate**

Run: `cd bench && cargo clippy --all-targets -- -D warnings`
Expected: no output (clippy clean).

- [ ] **Step 4: Run cargo fmt check**

Run from the repo root: `cargo fmt -- --check`
Expected: no diff. If diff appears, run `cargo fmt` and amend/commit the formatting fix as a separate commit titled `bench: fmt`.

- [ ] **Step 5: Run the full workspace test to confirm nothing else broke**

Run from the repo root: `cargo test`
Expected: full suite passes (root chisel crate, python crate, bench crate). No regressions.

- [ ] **Step 6: Confirm public API shape**

Run: `cd bench && cargo doc --no-deps --quiet 2>&1 | tail -5`
Expected: no warnings about missing docs (every public function has a doc comment per task 3-8).

Then verify the re-exports landed:
```bash
grep -E "pub use workload::\{Operation, Workload\}" bench/src/lib.rs
```
Expected: matches one line.

- [ ] **Step 7: No commit needed if all checks pass**

If steps 1–6 all pass with no diff, do nothing — task 8's commit was the final code-bearing commit. The plan is complete.

If step 4 produced fmt diff, commit it:
```bash
git add -u
git commit -m "$(cat <<'EOF'
bench: cargo fmt

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final state after all tasks

- `bench/Cargo.toml` has `rand 0.8` and `rand_chacha 0.3` in `[dependencies]`.
- `bench/src/workload.rs` contains: `Operation` enum, `Workload` struct, six generator functions, 14 inline tests, no engine imports.
- `bench/src/lib.rs` declares `pub mod workload` and re-exports `Operation` and `Workload`.
- `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt -- --check` all clean.
- 9 commits authored, each meaningful on its own.

PR 4b can now begin: it consumes `Workload` values, drives them against the engines, and adds the 270-cell Criterion registration.
