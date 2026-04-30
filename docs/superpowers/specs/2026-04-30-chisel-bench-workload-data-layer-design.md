# Chisel Bench — Workload Data Layer — Design

**Date:** 2026-04-30
**Status:** Design approved; implementation plan pending.
**Scope:** Add a pure data-layer module `bench/src/workload.rs` containing the `Operation` enum, the `Workload` struct, and six seeded generator functions covering the micro-grid's nine rows. No Runner, no Criterion glue, no engine interaction. PR 4a of the bench-suite series — the first half of the original PR 4 ("Micro grid"), split out so the data-layer abstractions land and get reviewed before the runner is built on top of them.

This spec follows on from `2026-04-25-chisel-benchmark-suite-design.md` (the overall bench-suite design) and `2026-04-30-chisel-bench-redb-sqlite-engines-design.md` (PR 3, which landed the three `Engine` impls).

The original PR 4 in the master spec covered both the Workload data layer and the Runner / 270-cell registration in one ~600 LOC PR. This split lets the data abstraction land in isolation: a clean ~250 LOC review focused on op-shape, generator determinism, and the engine-agnostic invariant. PR 4b will then add the Runner and Criterion bench targets.

## 1. Goals and Non-Goals

### Goals

- Land an engine-agnostic `Operation` enum and `Workload` struct in `bench/src/workload.rs`. The module imports nothing from `chisel`, `redb`, or `rusqlite`; this is verifiable by inspection of the file's `use` statements.
- Land six deterministic generator functions covering every micro-grid row: pre-population, allocate, read-random, update-random, delete-random, and delete-many. Each is a pure function of `(seed, parameters)` to `Workload`.
- Pin the PRNG: use `rand_chacha::ChaCha8Rng` (not `rand::rngs::StdRng`), so workload outputs survive `rand` minor-version bumps without silent drift.
- Workload generation is itself unit-tested without an engine: determinism, index-validity, op-mix correctness, no-replacement guarantees for delete generators, cross-seed independence.
- Use the alloc-order-index identifier-reference scheme: `Operation` variants reference records by their position in the running list of allocations rather than by engine-assigned identifiers (which differ per engine).

### Non-Goals (this PR)

- *The Runner.* Pre-population execution, cache warming, transaction granularity, Criterion `iter_batched`, and per-cell counter snapshotting are PR 4b.
- *Any `[[bench]]` target.* The workload module is a library with unit tests; no `bench/benches/` files yet.
- *The 270-cell registration code.* PR 4b.
- *Cold-cache machinery.* Fresh-open-per-iteration is purely the Runner's concern (PR 4b).
- *Zipfian access patterns.* The micro grid uses uniform-random selection. Zipfian (and the `rand_distr` dependency) lands with PR 6's scenario tier, where it has concrete consumers.
- *Log-normal or other size distributions.* `Operation::Allocate { size }` is a single value. Distributions are PR 6.
- *Mixed-op generators.* No "50% read / 50% update interleaved" generator. Each PR 4 generator emits one op family. PR 6 will add mixed generators when scenarios pin the requirements.
- *Explicit transaction-boundary `Operation` variants* (`Begin` / `Commit`). The Runner wraps tx boundaries around groups of ops. PR 6 may revisit if mutation-log scenarios need finer control.
- *Workload serialization.* No `serde` impl yet. The struct is shaped to allow one cleanly later (plain fields, no engine types).

## 2. Architecture — types and module layout

### 2.1 File structure

| File | Touch | Responsibility |
|------|-------|----------------|
| `bench/Cargo.toml` | Modify | Add `rand = "0.8"` and `rand_chacha = "0.3"` to `[dependencies]` |
| `bench/src/workload.rs` | Create | `Operation`, `Workload`, six generator functions, inline `#[cfg(test)]` tests |
| `bench/src/lib.rs` | Modify | `pub mod workload`; re-export `Operation` and `Workload` |

No other files are touched. The engine impls (`chisel_engine.rs`, `redb_engine.rs`, `sqlite_engine.rs`) are unaware of this module, and the trait in `engine.rs` is unchanged.

### 2.2 Dependency choices

- **`rand = "0.8"`** — provides the `Rng` and `SeedableRng` traits and the `rand::seq::index::sample` helper used by the without-replacement delete generators. Pinning the major-minor pair avoids accidental ecosystem-mismatch drift.
- **`rand_chacha = "0.3"`** — provides `ChaCha8Rng`, the pinned PRNG. Pair-version of `rand 0.8`. ChaCha8 is the cheapest variant of the family and produces byte-identical sequences across platforms (32/64-bit, x86/ARM, Linux/macOS) for any given seed — a property `StdRng` does not guarantee across `rand` minor-version updates, and on which bench-suite reproducibility silently depends.

Both are regular `[dependencies]` (not `[dev-dependencies]`) because `Workload` is part of the bench crate's public API surface and PR 4b's Runner will consume it from production code paths.

### 2.3 `Operation` enum

```rust
/// One unit of work the Runner executes against an Engine.
///
/// `alloc_index` is a position in the Runner's "allocations seen so
/// far" vector (per the alloc-order-index identifier scheme). Each
/// generator guarantees every emitted index is live (allocated and
/// not deleted) at that point in the sequence — so the Runner does
/// not need to filter or validate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    Allocate { size: usize },
    Read { alloc_index: usize },
    Update { alloc_index: usize, size: usize },
    Delete { alloc_index: usize },
    DeleteMany { alloc_indices: Vec<usize> },
}
```

Design notes:

- **No `Begin` / `Commit` variants.** Transaction boundaries are the Runner's responsibility. The same `gen_read_random` workload drives both micro-grid row 5 (1 op/tx) and row 6 (1000 ops/tx) without duplication; the Runner decides where to wrap `engine.begin()` / `engine.commit()`.
- **Four of five variants are `Copy`-eligible-as-fields**; only `DeleteMany` carries a `Vec`. `Operation` itself derives `Clone` (not `Copy`) for that reason. The other variants stay heap-pointer-free, so a Workload's `Vec<Operation>` is mostly dense integer data.
- **`Update.size` is independent of the original size**, even though micro-grid generators always pass the same value as the corresponding `Allocate`. The data shape generalizes for free to PR 6's document-store scenario, where update-with-different-size matters.
- **`DeleteMany.alloc_indices` is a `Vec<usize>`** rather than `(start, count)`. The list is generally non-contiguous (sampled without replacement from the live set), and the cost of the `Vec` is bounded — micro-grid worst case is 800 batches × 1000 indices × 8 B ≈ 6 MB total per workload, well within bench-time memory budget.
- **No filler bytes in `Allocate` / `Update`**. The Runner fills with `vec![0u8; size]`. Content is not part of what we measure (engines do not compress on the read/write path; if any did, we would want a separate compressibility cell).

### 2.4 `Workload` struct

```rust
/// A deterministic sequence of Operations plus enough metadata to
/// identify which generator produced it.
///
/// `name` is the Criterion BenchmarkId stem (e.g., "read_random@2KB").
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
```

Design notes:

- **`name: String`** (not `&'static str`) so generators synthesize names like `format!("read_random@{}B", size)` without needing a leaked-static interner. Cost: one `String` allocation per workload, irrelevant compared to op generation.
- **`prepop_count` is a field**, not derived from inspecting `ops`. Pre-population happens before the workload runs — the count is a workload-level property, not an op-level one. Storing it explicitly avoids the Runner having to infer it from "the largest `alloc_index` referenced," which would be a fragile reverse-engineering of the generator's intent.
- **Derived traits: `Clone, Debug, PartialEq, Eq`.** `Clone` lets the Runner consume a workload while tests still compare two workloads from same seed for equality. `Eq` lets the determinism test assert `gen_read_random(...) == gen_read_random(...)` directly. No `Hash` — `Vec<Operation>: Hash` would compile (since `Operation: Hash`), but no current consumer uses Workload as a hash-map key, and we keep derives to what is actually used. No `Copy` (it owns a Vec). No `serde` (deferred — the field shapes are deliberately serde-friendly when a consumer arrives).

## 3. Generator functions

Six free functions in `bench/src/workload.rs`. Each is a pure function of its arguments returning a `Workload`. All randomized generators construct their own `ChaCha8Rng::seed_from_u64(seed)` internally — no `&mut Rng` is passed across function boundaries, so the determinism contract stays surgical.

### 3.1 Signatures

```rust
/// Pre-population: `count` Allocates of `size` bytes each.
/// No randomness — returns the same Workload every call. `seed` is
/// fixed at 0 and `name` at "prepopulate" so it does not appear as
/// a varying axis in Criterion ids.
pub fn gen_prepopulate(count: usize, size: usize) -> Workload;

/// Row 1/2 (allocate × {1, 1000}/tx). `count` Allocate ops of `size`
/// bytes. No randomness; takes no seed (parallels `gen_prepopulate`,
/// where seedless and unambiguous beats symmetry-with-an-unused-arg).
/// The Workload's `seed` field is set to 0.
pub fn gen_allocate(count: usize, size: usize) -> Workload;

/// Row 3/4 (read warm/cold). `count` Read ops with alloc_indices
/// sampled uniformly with replacement from `0..prepop_count`.
pub fn gen_read_random(seed: u64, prepop_count: usize, count: usize) -> Workload;

/// Row 5/6 (update × {1, 1000}/tx). Same selection as read_random,
/// but Update with `size` payload.
pub fn gen_update_random(
    seed: u64,
    prepop_count: usize,
    count: usize,
    size: usize,
) -> Workload;

/// Row 7/8 (delete × {1, 1000}/tx). Sampled WITHOUT replacement so
/// no index is deleted twice. Requires `count <= prepop_count`;
/// panics on violation (this is a generator-author bug, not a runtime
/// condition).
pub fn gen_delete_random(
    seed: u64,
    prepop_count: usize,
    count: usize,
) -> Workload;

/// Row 9 (delete_many — single bulk call per op). Each Operation is
/// one DeleteMany carrying `batch_size` distinct indices; `batches`
/// total. Sampled without replacement across the entire workload, so
/// no index appears in two different batches. Requires
/// `batches * batch_size <= prepop_count`.
pub fn gen_delete_many(
    seed: u64,
    prepop_count: usize,
    batches: usize,
    batch_size: usize,
) -> Workload;
```

### 3.2 Sampling implementation

- Read and update generators use `rng.gen_range(0..prepop_count)` per op (uniform with replacement).
- Delete-random uses `rand::seq::index::sample(&mut rng, prepop_count, count)`, which Floyd-samples in O(count) time and returns an `IndexVec` of distinct indices. No need to materialize a full `Vec` of `0..prepop_count`.
- Delete-many uses one `rand::seq::index::sample(&mut rng, prepop_count, batches * batch_size)` call, then chunks the result into `batches` slices of length `batch_size`.

### 3.3 Naming convention

Generator-produced `Workload.name` strings:

| Generator | `name` |
|-----------|--------|
| `gen_prepopulate` | `"prepopulate"` |
| `gen_allocate` | `"allocate"` |
| `gen_read_random` | `"read_random"` |
| `gen_update_random` | `"update_random"` |
| `gen_delete_random` | `"delete_random"` |
| `gen_delete_many` | `"delete_many"` |

Size and other parameters are appended by the Runner (PR 4b) when constructing Criterion BenchmarkIds — keeping size out of the generator's `name` lets one `read_random` workload drive multiple cells if a future caller wants to reuse it.

## 4. Tests

All tests live inline in `#[cfg(test)] mod tests` at the bottom of `workload.rs`. They run as part of `cargo test -p chisel-bench`.

### 4.1 Test cases

| # | Test | What it verifies |
|---|------|------------------|
| 1 | `determinism_read_random` | Two calls to `gen_read_random` with same args produce equal Workloads (uses derived `PartialEq`) |
| 2 | `determinism_delete_many` | Same for `gen_delete_many` — covers the without-replacement path separately |
| 3 | `index_validity_uniform` | For `gen_read_random` and `gen_update_random`: every emitted alloc_index is `< prepop_count` |
| 4 | `index_validity_no_replacement` | For `gen_delete_random`: indices form a set of size `count` (no duplicates). For `gen_delete_many`: union of all batch indices forms a set of size `batches * batch_size`, all `< prepop_count` |
| 5 | `op_mix_correctness` | Each generator emits the right `Operation` variant and the expected count (e.g., `gen_allocate` produces only `Allocate`, exactly N of them) |
| 6 | `cross_seed_independence` | `gen_read_random(seed=1, ...)` and `gen_read_random(seed=2, ...)` produce different `ops` Vecs (guards against the most common workload-generator bug — forgetting to seed the RNG) |

No criterion benches, no engine touches, no I/O. The test file does not import `chisel`, `redb`, `rusqlite`, or `tempfile`.

### 4.2 What the tests do not cover

- **Statistical-distribution shape.** The micro-grid generators all use uniform sampling; we are not using Zipfian or log-normal distributions in PR 4a, so there is nothing to assert distribution-shape against.
- **Performance characteristics of generation itself.** Generation is fast by inspection (linear in op count, no I/O); we do not benchmark the generator. PR 6 may revisit if Zipfian generators show up in CI bench timing.
- **Engine interaction.** That is PR 4b's territory — the Runner consuming Workloads is its own test surface.

## 5. Acceptance criteria

PR 4a is mergeable when:

1. `cargo build -p chisel-bench` and `cargo test -p chisel-bench` pass on macOS and Linux.
2. `cargo clippy -p chisel-bench -- -D warnings` is clean.
3. `cargo fmt -- --check` is clean across the touched files.
4. The six tests in §4.1 pass.
5. `bench/src/workload.rs` contains no `use chisel::`, `use redb::`, or `use rusqlite::` lines (the engine-agnostic invariant). Verifiable by `grep`.
6. Public API: `bench::Operation` and `bench::Workload` are re-exported through `bench/src/lib.rs`. Generator functions stay namespaced as `bench::workload::gen_*`.

## 6. Build sequence relationship

PR 4a is the first half of the original PR 4 in the master spec's build sequence. The series after this PR lands becomes:

| # | PR | Status |
|---|----|--------|
| 1 | Instrumentation precursor | Landed |
| 2 | `bench/` subcrate + Engine trait + ChiselEngine | Landed |
| 3 | RedbEngine + SqliteEngine + equivalence tests | Landed |
| **4a** | **Workload data layer** | **This PR** |
| 4b | Runner + 270-cell registration + cold-cache machinery | Pending — own spec/plan |
| 5 | Markdown summary post-processor | Pending |
| 6 | Scenario tier (YCSB-A/B, mutation log, document store) | Pending |
| 7 | CI workflow | Pending |
| 8 | Cross-engine relative-performance tests (addendum) | Pending — own spec/plan |

The split does not change the master design's contracts; it only re-paginates them. CLAUDE.md and the master spec should be updated when PR 4a merges to reflect the 4a/4b split.

### 6.1 Rollback

PR 4a fails review or is reverted: the bench subcrate keeps compiling and PR 3's equivalence tests keep running. No engine-side dependency was introduced. PR 4b cannot start until 4a lands; nothing else is gated.

## 7. Open implementation-phase questions

These are deferred to the implementation plan:

- The exact constant for `gen_prepopulate`'s fixed seed (0 in the spec text, but the plan picks the documented value).
- Whether tests use `#[test]` directly or `proptest` for the validity-invariant checks. Default expectation: plain `#[test]` for predictability; `proptest` adds a build-time dep that does not currently appear in the chisel workspace.
- The exact `format!` strings for `Workload.name` (e.g., do generator authors include the seed? probably not — seed is a separate field — but the plan confirms).
- Doc-comment style for the generator functions (one-line summary vs full multi-paragraph). The plan picks a uniform style.

These are implementation details that do not affect the design contract.
