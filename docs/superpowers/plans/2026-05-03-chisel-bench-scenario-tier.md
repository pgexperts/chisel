# Bench Scenario Tier Implementation Plan (PR 6)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land four end-to-end YCSB-style scenarios (YCSB-A, YCSB-B, mutation log, document store) running once per (scenario, strict-mode) cell = 12 cells total. Add Zipfian/log-normal/op-mix primitives to the workload data layer. Run cells via inline `Instant::now()` timing (NOT Criterion) to fit the 1-6 minute master-spec runtime budget. Extend PR 5's post-processor to render the scenario table in `summary.md` and add a `scenarios` top-level key to `results.json`.

**Architecture:** Library/binary split mirrors PR 4b. Workload primitives (`zipfian_indices`, `lognormal_sizes`, `mix_operations`, `OpKind`) extend `bench/src/workload.rs`. The four scenario generators live in a new `bench/src/scenarios.rs` module — each is a thin composition of primitives + spec parameters. `run_scenario_cell` in `bench/src/runner.rs` opens a fresh engine, populates untimed, runs the workload with per-op `Instant::now()` instrumentation, captures aux metrics. `bench/benches/scenarios.rs` is a `harness = false` bench binary with its own `main` that iterates 12 cells and streams JSONL.

**Tech Stack:** Rust 2021. New dep: `rand_distr = "0.4"` (paired with existing `rand 0.8`). Reuses serde/serde_json/tempfile/chrono from earlier PRs. The post-processor extension reuses PR 5's `summary` module.

**Spec:** `docs/superpowers/specs/2026-05-03-chisel-bench-scenario-tier-design.md`

---

## Task 1: `Cargo.toml` — add `rand_distr` dependency

**Files:**
- Modify: `bench/Cargo.toml`

The `[[bench]] scenarios` target is deferred to Task 10 (when `bench/benches/scenarios.rs` exists). Cargo validates target sources at parse time, so declaring a `[[bench]]` for a missing file would break the build.

- [ ] **Step 1: Edit `bench/Cargo.toml`**

The current `[dependencies]` section (after PR 5) is:

```toml
[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }
rand = "0.8"
rand_chacha = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tempfile = "3"
chrono = "0.4"
walkdir = "2"
clap = { version = "4", features = ["derive"] }
hostname = "0.4"
```

Add `rand_distr = "0.4"` (pair-version of `rand 0.8`). The result:

```toml
[dependencies]
chisel = { path = ".." }
redb = "2"
rusqlite = { version = "0.31", features = ["bundled"] }
rand = "0.8"
rand_chacha = "0.3"
rand_distr = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tempfile = "3"
chrono = "0.4"
walkdir = "2"
clap = { version = "4", features = ["derive"] }
hostname = "0.4"
```

The `[dev-dependencies]`, `[[bench]] micro_grid`, and `[[bin]] summarize` sections stay untouched.

- [ ] **Step 2: Verify the bench subcrate still builds**

Run: `cd bench && cargo build`
Expected: clean build, `Finished` line. New crate `rand_distr` and any transitive deps appear in `bench/Cargo.lock`.

- [ ] **Step 3: Verify existing tests still pass**

Run: `cd bench && cargo test`
Expected: 36 lib + 15 equivalence + 1 lib smoke + 1 runner smoke + 1 summarize smoke = 54 tests, all passing.

- [ ] **Step 4: Commit**

```bash
git add bench/Cargo.toml bench/Cargo.lock
git commit -m "$(cat <<'EOF'
bench: add rand_distr dep for the scenario tier

rand_distr 0.4 is the pair-version of rand 0.8 (already in deps from
PR 4a). Provides Zipf and LogNormal samplers for the YCSB-style
scenarios in PR 6. The [[bench]] scenarios target declaration is
deferred to task 10 — Cargo validates target sources at parse time.
EOF
)"
```

---

## Task 2: `workload.rs` — `OpKind` enum + `zipfian_indices` + 2 tests

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module at the bottom of `bench/src/workload.rs`:

```rust
    #[test]
    fn zipfian_indices_determinism() {
        let a = zipfian_indices(42, 1000, 100, 0.99);
        let b = zipfian_indices(42, 1000, 100, 0.99);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1000);
    }

    #[test]
    fn zipfian_indices_distribution_is_skewed() {
        // Theta=0.99 (YCSB default) means heavy skew. Over 10K samples
        // from [0, 100), the top decile (indices 0..10) should receive
        // ~75% of accesses per spec §4.1. Allow generous tolerance.
        let samples = zipfian_indices(7, 10_000, 100, 0.99);
        let top_decile = samples.iter().filter(|&&i| i < 10).count();
        let pct = (top_decile as f64) / 10_000.0;
        assert!(
            pct > 0.50,
            "zipfian θ=0.99 over 10K samples expected ≥50% in top decile, got {pct:.3}"
        );
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::zipfian_indices`
Expected: compile error — `cannot find function zipfian_indices in this scope`.

- [ ] **Step 3: Add the `OpKind` enum and `zipfian_indices` function**

Edit `bench/src/workload.rs`. The `use` lines at the top currently include:

```rust
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
```

Add `use rand_distr::{Distribution, Zipf};`:

```rust
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Zipf};
```

Add the `OpKind` enum and `zipfian_indices` function. Place them near the top of the file, after the existing type definitions (Operation, Workload) but before the existing micro-grid generators. The `OpKind` enum:

```rust
/// Tag for the four mutating-or-reading operation kinds, used by
/// `mix_operations` to pick which `Operation` variant to emit.
/// `DeleteMany` is intentionally absent — scenarios use the single
/// `Delete` variant per master spec §4.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Allocate,
    Read,
    Update,
    Delete,
}
```

Then the `zipfian_indices` function:

```rust
/// Zipfian-distributed random indices into [0, prepop_count). `theta`
/// controls skew: 0.0 = uniform, 0.99 = YCSB default (heavy skew,
/// ~75% of accesses to ~10% of records).
///
/// Returns Vec<usize> of length `count`. Uses `rand_distr::Zipf`,
/// which models the Zipf distribution Z(N, s). The `Zipf` constructor
/// takes (n, s) where s is the exponent: s=0 is uniform, s=1.0+ is
/// heavily skewed. We translate YCSB's `theta` to s via `s = theta`
/// (rand_distr's parameterization matches YCSB's directly).
///
/// Deterministic: same (seed, count, prepop_count, theta) → same Vec.
/// Uses ChaCha8Rng seeded from `seed` for cross-platform reproducibility.
pub fn zipfian_indices(
    seed: u64,
    count: usize,
    prepop_count: usize,
    theta: f64,
) -> Vec<usize> {
    assert!(prepop_count > 0, "zipfian_indices: prepop_count must be > 0");
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let zipf = Zipf::new(prepop_count as u64, theta).unwrap();
    (0..count)
        .map(|_| {
            // Zipf samples in [1, n], convert to [0, n-1] by subtracting 1.
            let v = zipf.sample(&mut rng) as usize;
            v.saturating_sub(1).min(prepop_count - 1)
        })
        .collect()
}
```

Note: `rand_distr::Zipf::sample` returns `f64`, which we cast to `usize`. Zipf samples in `[1, n]`, so we subtract 1 to land in `[0, n-1]`. The `.min(prepop_count - 1)` is defensive — Zipf shouldn't return n+1, but float-to-int conversion can produce edge values; clamp to be safe.

- [ ] **Step 4: Run tests, expect both passing**

Run: `cd bench && cargo test workload::tests::zipfian_indices`
Expected: 2 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: add OpKind enum + zipfian_indices to workload primitives

OpKind tags the four mutating-or-reading operation kinds (no DeleteMany
— scenarios use single Delete per master spec §4.3). zipfian_indices
samples Zipfian-distributed alloc_index values via rand_distr::Zipf;
deterministic in (seed, count, prepop_count, theta). Two tests cover
determinism and the heavy-skew distribution shape at theta=0.99.
EOF
)"
```

---

## Task 3: `workload.rs` — `lognormal_sizes` + 2 tests

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module (after the zipfian tests from Task 2):

```rust
    #[test]
    fn lognormal_sizes_determinism() {
        let a = lognormal_sizes(42, 1000, 4096, 1_048_576);
        let b = lognormal_sizes(42, 1000, 4096, 1_048_576);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1000);
    }

    #[test]
    fn lognormal_sizes_clamps_outliers() {
        // Generate 10K samples; assert all fall inside [16, 4_194_304].
        // The lognormal tail can produce both ~0-byte and gigabyte
        // values; the clamp protects bench timing from outliers.
        let samples = lognormal_sizes(7, 10_000, 4096, 1_048_576);
        for &s in &samples {
            assert!(s >= 16, "lognormal_sizes should clamp at >= 16, got {s}");
            assert!(
                s <= 4_194_304,
                "lognormal_sizes should clamp at <= 4_194_304, got {s}"
            );
        }
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::lognormal_sizes`
Expected: compile error — `cannot find function lognormal_sizes`.

- [ ] **Step 3: Add the `lognormal_sizes` function**

Edit the `use` line for `rand_distr` to also import `LogNormal`:

```rust
use rand_distr::{Distribution, LogNormal, Zipf};
```

Add the function immediately after `zipfian_indices`:

```rust
/// Log-normal-distributed sizes in bytes. `median_bytes` and
/// `p99_bytes` parameterize the distribution: log-normal has shape
/// (mu, sigma); we solve for mu/sigma such that exp(mu) = median
/// and exp(mu + 2.326 * sigma) ≈ p99 (z-score 2.326 for the 99th
/// percentile of a standard normal).
///
/// Returns Vec<usize> of length `n`. Sizes clamped to [16, 4_194_304]
/// (16B floor, 4MB ceiling) to avoid pathological outliers from the
/// log-normal tail. ~0.001% of unbounded log-normal samples land at
/// multi-GB sizes; clamping accepts a tiny bias in exchange for stable
/// wall-clock measurements.
///
/// Deterministic: same (seed, n, median_bytes, p99_bytes) → same Vec.
pub fn lognormal_sizes(
    seed: u64,
    n: usize,
    median_bytes: usize,
    p99_bytes: usize,
) -> Vec<usize> {
    assert!(median_bytes > 0, "lognormal_sizes: median_bytes must be > 0");
    assert!(
        p99_bytes >= median_bytes,
        "lognormal_sizes: p99_bytes must be >= median_bytes"
    );
    let mu = (median_bytes as f64).ln();
    // z-score 2.326 for the 99th percentile of a standard normal.
    let sigma = ((p99_bytes as f64).ln() - mu) / 2.326;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let dist = LogNormal::new(mu, sigma).unwrap();
    (0..n)
        .map(|_| {
            let raw = dist.sample(&mut rng);
            // Clamp to [16, 4_194_304]. NaN/inf → clamped via direct comparison.
            let clamped = if raw.is_finite() {
                raw.max(16.0).min(4_194_304.0) as usize
            } else {
                16
            };
            clamped
        })
        .collect()
}
```

- [ ] **Step 4: Run tests, expect both passing**

Run: `cd bench && cargo test workload::tests::lognormal_sizes`
Expected: 2 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: add lognormal_sizes to workload primitives

Solves for (mu, sigma) from (median, p99) via the standard normal
2.326 z-score for p99. Samples clamped to [16, 4_194_304] to avoid
the long log-normal tail producing pathological multi-GB outliers
that would dominate bench wall-clock. Two tests cover determinism
and the clamp invariant.
EOF
)"
```

---

## Task 4: `workload.rs` — `mix_operations` + 2 tests

**Files:**
- Modify: `bench/src/workload.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn mix_operations_op_proportions() {
        // 50/50 read/update mix over 10_000 ops should produce ~5000
        // of each (binomial slack: ±200 covers > 99.99% of cases).
        let access: Vec<usize> = (0..10_000).map(|i| i % 100).collect();
        let sizes: Vec<usize> = (0..10_000).map(|_| 1024).collect();
        let ops = mix_operations(
            42,
            10_000,
            &[(OpKind::Read, 0.5), (OpKind::Update, 0.5)],
            &access,
            &sizes,
        );
        assert_eq!(ops.len(), 10_000);
        let reads = ops.iter().filter(|o| matches!(o, Operation::Read { .. })).count();
        let updates = ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        assert!(
            (4800..=5200).contains(&reads),
            "expected ~5000 reads ±200, got {reads}"
        );
        assert!(
            (4800..=5200).contains(&updates),
            "expected ~5000 updates ±200, got {updates}"
        );
        assert_eq!(reads + updates, 10_000);
    }

    #[test]
    fn mix_operations_uses_provided_inputs() {
        // Allocate ops should use sizes; Read ops should use access_pattern.
        let access: Vec<usize> = vec![7, 13, 21, 35];
        let sizes: Vec<usize> = vec![64, 128, 256, 512];
        let ops = mix_operations(
            42,
            4,
            &[(OpKind::Allocate, 0.5), (OpKind::Read, 0.5)],
            &access,
            &sizes,
        );
        assert_eq!(ops.len(), 4);
        // Each emitted op's size (Allocate) or alloc_index (Read) must
        // come from the provided slices. We don't enforce ordering — the
        // generator may consume entries in any deterministic order — but
        // the multiset of values for each op kind must match a prefix of
        // the corresponding input slice.
        for op in &ops {
            match op {
                Operation::Allocate { size } => {
                    assert!(sizes.contains(size), "allocate size {size} not in sizes slice");
                }
                Operation::Read { alloc_index } => {
                    assert!(
                        access.contains(alloc_index),
                        "read alloc_index {alloc_index} not in access slice"
                    );
                }
                _ => panic!("expected only Allocate or Read"),
            }
        }
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test workload::tests::mix_operations`
Expected: compile error — `cannot find function mix_operations`.

- [ ] **Step 3: Add the `mix_operations` function**

Edit the `use` for `rand_distr` to include `WeightedIndex`:

```rust
use rand_distr::{Distribution, LogNormal, WeightedIndex, Zipf};
```

Add the function after `lognormal_sizes`:

```rust
/// Compose a mixed-op sequence from per-op-kind probabilities.
/// Decouples op-kind sampling from access-pattern + size sampling —
/// each op gets a fresh sample from the appropriate input slice.
///
/// `op_specs` is a slice of (OpKind, weight) pairs; weights are
/// normalized internally by `WeightedIndex`. `access_pattern` provides
/// alloc_index values for Read/Update/Delete ops (consumed in order).
/// `sizes` provides byte counts for Allocate/Update ops (consumed in
/// order).
///
/// The function consumes one access_pattern entry and/or one sizes
/// entry per op based on op kind:
///   - Allocate → consumes one size
///   - Read → consumes one access_pattern entry
///   - Update → consumes one access_pattern entry AND one size
///   - Delete → consumes one access_pattern entry
///
/// Caller must provide enough entries; the function panics with a
/// clear message on under-provision. The scenario generators
/// precompute the right counts based on the op-mix probabilities.
///
/// Deterministic: same (seed, count, op_specs, access_pattern, sizes)
/// → same Vec.
pub fn mix_operations(
    seed: u64,
    count: usize,
    op_specs: &[(OpKind, f64)],
    access_pattern: &[usize],
    sizes: &[usize],
) -> Vec<Operation> {
    assert!(!op_specs.is_empty(), "mix_operations: op_specs must be non-empty");
    let weights: Vec<f64> = op_specs.iter().map(|(_, w)| *w).collect();
    let kinds: Vec<OpKind> = op_specs.iter().map(|(k, _)| *k).collect();
    let weighted = WeightedIndex::new(&weights).unwrap();

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut access_iter = access_pattern.iter();
    let mut sizes_iter = sizes.iter();
    let mut out: Vec<Operation> = Vec::with_capacity(count);

    for _ in 0..count {
        let kind = kinds[weighted.sample(&mut rng)];
        let op = match kind {
            OpKind::Allocate => {
                let size = *sizes_iter
                    .next()
                    .expect("mix_operations: not enough sizes for Allocate ops");
                Operation::Allocate { size }
            }
            OpKind::Read => {
                let alloc_index = *access_iter
                    .next()
                    .expect("mix_operations: not enough access entries for Read ops");
                Operation::Read { alloc_index }
            }
            OpKind::Update => {
                let alloc_index = *access_iter
                    .next()
                    .expect("mix_operations: not enough access entries for Update ops");
                let size = *sizes_iter
                    .next()
                    .expect("mix_operations: not enough sizes for Update ops");
                Operation::Update { alloc_index, size }
            }
            OpKind::Delete => {
                let alloc_index = *access_iter
                    .next()
                    .expect("mix_operations: not enough access entries for Delete ops");
                Operation::Delete { alloc_index }
            }
        };
        out.push(op);
    }
    out
}
```

- [ ] **Step 4: Run tests, expect both passing**

Run: `cd bench && cargo test workload::tests::mix_operations`
Expected: 2 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/workload.rs
git commit -m "$(cat <<'EOF'
bench: add mix_operations to workload primitives

Composes a mixed-op sequence from per-OpKind probabilities. Decouples
op-kind sampling (WeightedIndex) from access-pattern and size sampling
— callers pre-sample those independently and pass slices. The function
consumes from access_pattern / sizes per op-kind requirements
(Allocate→size, Read→access, Update→both, Delete→access). Two tests
cover proportions and input usage.
EOF
)"
```

---

## Task 5: `scenarios.rs` scaffold — `seed_for` + module wire-up

**Files:**
- Create: `bench/src/scenarios.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Create `bench/src/scenarios.rs` with module header + `seed_for`**

```rust
// Scenario tier — four end-to-end YCSB-style workloads from master
// spec §4. Each scenario is a thin composition of the workload-data
// primitives (zipfian_indices, lognormal_sizes, mix_operations) plus
// the spec's parameters.
//
// Each scenario has a paired `gen_<name>_prepopulate` generator that
// produces the pre-population workload (allocate ops at the right
// sizes); `run_scenario_cell` runs prepopulate untimed before
// running the scenario workload timed.
//
// Hardcoded per-scenario seeds (see `seed_for`) — DefaultHasher
// randomizes per-process, so derived seeds wouldn't reproduce.

use crate::workload::{
    lognormal_sizes, mix_operations, zipfian_indices, OpKind, Operation, Workload,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Per-scenario seeds. Hardcoded rather than hashed — Rust's
/// DefaultHasher randomizes per-process state so derived seeds
/// would not reproduce across runs.
pub fn seed_for(scenario: &str) -> u64 {
    match scenario {
        "ycsb-a" => 0x6001,
        "ycsb-b" => 0x6002,
        "mutation-log" => 0x6003,
        "document-store" => 0x6004,
        _ => panic!("unknown scenario: {scenario}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_for_returns_distinct_seeds_per_scenario() {
        let names = ["ycsb-a", "ycsb-b", "mutation-log", "document-store"];
        let seeds: Vec<u64> = names.iter().map(|n| seed_for(n)).collect();
        let unique: std::collections::HashSet<u64> = seeds.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "scenario seeds must be distinct");
    }

    #[test]
    #[should_panic(expected = "unknown scenario")]
    fn seed_for_panics_on_unknown_scenario() {
        let _ = seed_for("not-a-real-scenario");
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs`**

The current `bench/src/lib.rs` (after PR 5) has:

```rust
pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod runner;
pub mod sqlite_engine;
pub mod summary;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use runner::EngineMode;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
```

Add `pub mod scenarios;` (alphabetically between `runner` and `sqlite_engine`):

```rust
pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod runner;
pub mod scenarios;
pub mod sqlite_engine;
pub mod summary;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use runner::EngineMode;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
```

(Top-level re-exports of `OpKind` and the scenario generators land in Task 9 alongside `ScenarioResult`.)

- [ ] **Step 3: Run tests, expect both passing**

Run: `cd bench && cargo test scenarios::tests`
Expected: 2 passed.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/scenarios.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: scaffold scenarios module + seed_for

New top-level module bench/src/scenarios.rs for the four scenario
generators landing in tasks 6-8. seed_for returns hardcoded per-scenario
u64 seeds; DefaultHasher randomizes per-process so derived seeds would
not reproduce across runs. Two tests cover seed distinctness and the
panic on unknown-scenario name.
EOF
)"
```

---

## Task 6: `scenarios.rs` — YCSB-A and YCSB-B + 1 test

**Files:**
- Modify: `bench/src/scenarios.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `scenarios.rs`:

```rust
    #[test]
    fn gen_ycsb_a_shape() {
        let seed = seed_for("ycsb-a");
        let prepop = gen_ycsb_a_prepopulate(seed);
        let workload = gen_ycsb_a(seed);

        // Pre-population: 100K Allocate ops of 1KB each
        assert_eq!(prepop.name, "ycsb-a-prepopulate");
        assert_eq!(prepop.ops.len(), 100_000);
        assert_eq!(prepop.prepop_count, 0);
        for op in &prepop.ops {
            assert!(matches!(op, Operation::Allocate { size: 1024 }));
        }

        // Main workload: 100K ops, 50/50 read/update over 100K records
        assert_eq!(workload.name, "ycsb-a");
        assert_eq!(workload.ops.len(), 100_000);
        assert_eq!(workload.prepop_count, 100_000);
        let reads = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let updates = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        // 50/50 mix over 100K → expect ~50K of each (binomial ±500)
        assert!((49_500..=50_500).contains(&reads));
        assert!((49_500..=50_500).contains(&updates));
        assert_eq!(reads + updates, 100_000);
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test scenarios::tests::gen_ycsb_a_shape`
Expected: compile error — `cannot find function gen_ycsb_a` and `gen_ycsb_a_prepopulate`.

- [ ] **Step 3: Implement YCSB-A and YCSB-B generators**

Add to `bench/src/scenarios.rs`, between `seed_for` and the `#[cfg(test)] mod tests` block:

```rust
/// S1: YCSB-A — 100K records × 1KB pre-pop, 100K ops 50/50 read/update,
/// Zipfian θ=0.99 (heavy skew, ~75% of accesses to ~10% of records).
/// Master spec §4.1.
pub fn gen_ycsb_a(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let op_count = 100_000;
    let theta = 0.99;
    // Each Read or Update consumes one access entry; total = op_count.
    let access = zipfian_indices(seed, op_count, prepop_count, theta);
    // Each Update consumes one size; ~50K of them. To stay simple,
    // pre-allocate op_count sizes (extras are unused). All 1KB fixed.
    let sizes = vec![1024usize; op_count];
    let ops = mix_operations(
        seed,
        op_count,
        &[(OpKind::Read, 0.5), (OpKind::Update, 0.5)],
        &access,
        &sizes,
    );
    Workload {
        name: "ycsb-a".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for YCSB-A: 100K Allocate ops of 1KB each.
pub fn gen_ycsb_a_prepopulate(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let ops: Vec<Operation> = (0..prepop_count)
        .map(|_| Operation::Allocate { size: 1024 })
        .collect();
    Workload {
        name: "ycsb-a-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}

/// S2: YCSB-B — same setup as S1, mix is 95% read / 5% update.
/// Master spec §4.2.
pub fn gen_ycsb_b(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let op_count = 100_000;
    let theta = 0.99;
    let access = zipfian_indices(seed, op_count, prepop_count, theta);
    let sizes = vec![1024usize; op_count];
    let ops = mix_operations(
        seed,
        op_count,
        &[(OpKind::Read, 0.95), (OpKind::Update, 0.05)],
        &access,
        &sizes,
    );
    Workload {
        name: "ycsb-b".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for YCSB-B: identical to YCSB-A (100K × 1KB).
pub fn gen_ycsb_b_prepopulate(seed: u64) -> Workload {
    let prepop_count = 100_000;
    let ops: Vec<Operation> = (0..prepop_count)
        .map(|_| Operation::Allocate { size: 1024 })
        .collect();
    Workload {
        name: "ycsb-b-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}
```

Note on the `sizes` vector for read/update workloads: `mix_operations` consumes one size per Allocate or Update op. A 50/50 read/update mix over 100K ops produces ~50K updates, so we need ~50K sizes. We over-provision by allocating 100K sizes (1KB each); the extras are simply not consumed. This is simpler than computing the exact expected count from the binomial distribution.

- [ ] **Step 4: Run, expect 1 passed**

Run: `cd bench && cargo test scenarios::tests::gen_ycsb_a_shape`
Expected: 1 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/scenarios.rs
git commit -m "$(cat <<'EOF'
bench: add YCSB-A and YCSB-B scenario generators

YCSB-A: 100K records × 1KB pre-pop, 100K ops 50/50 read/update,
Zipfian θ=0.99. YCSB-B: same setup, 95/5 read/update. Each scenario
has a paired _prepopulate generator that emits 100K Allocate ops of
1KB. Test asserts the YCSB-A shape — op_count, prepop_count, op-mix
proportions; YCSB-B is structurally identical and passes by
construction.
EOF
)"
```

---

## Task 7: `scenarios.rs` — Mutation Log + 1 test

**Files:**
- Modify: `bench/src/scenarios.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `scenarios.rs`:

```rust
    #[test]
    fn gen_mutation_log_shape() {
        let seed = seed_for("mutation-log");
        let prepop = gen_mutation_log_prepopulate(seed);
        let workload = gen_mutation_log(seed);

        // Pre-population: 10K Allocate ops, sizes uniform [64, 4096]
        assert_eq!(prepop.name, "mutation-log-prepopulate");
        assert_eq!(prepop.ops.len(), 10_000);
        assert_eq!(prepop.prepop_count, 0);
        for op in &prepop.ops {
            match op {
                Operation::Allocate { size } => {
                    assert!(
                        (64..=4096).contains(size),
                        "prepop size {size} out of [64, 4096]"
                    );
                }
                _ => panic!("expected only Allocate ops in prepop"),
            }
        }

        // Main workload: 100K ops, 25/25/25/25 alloc/read/update/delete
        assert_eq!(workload.name, "mutation-log");
        assert_eq!(workload.ops.len(), 100_000);
        assert_eq!(workload.prepop_count, 10_000);
        let alloc = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Allocate { .. }))
            .count();
        let read = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let update = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        let delete = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Delete { .. }))
            .count();
        // 25/25/25/25 over 100K → expect ~25K each (multinomial ±500)
        assert!((24_500..=25_500).contains(&alloc));
        assert!((24_500..=25_500).contains(&read));
        assert!((24_500..=25_500).contains(&update));
        assert!((24_500..=25_500).contains(&delete));
        assert_eq!(alloc + read + update + delete, 100_000);
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test scenarios::tests::gen_mutation_log_shape`
Expected: compile error — `cannot find function gen_mutation_log`.

- [ ] **Step 3: Implement Mutation Log generators**

Add to `bench/src/scenarios.rs`, after the YCSB-B functions:

```rust
/// S3: Mutation Log — 10K records, sizes uniform [64B, 4KB], 100K ops
/// 25%/25%/25%/25% allocate/read/update/delete, uniform random access.
/// Master spec §4.3.
pub fn gen_mutation_log(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let op_count = 100_000;
    // Uniform random access; a fresh Vec each call (deterministic via seed).
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let access: Vec<usize> = (0..op_count)
        .map(|_| rng.gen_range(0..prepop_count))
        .collect();
    // Sizes uniform [64, 4096] inclusive.
    let mut size_rng = ChaCha8Rng::seed_from_u64(seed.wrapping_add(1));
    let sizes: Vec<usize> = (0..op_count)
        .map(|_| size_rng.gen_range(64..=4096))
        .collect();
    let ops = mix_operations(
        seed,
        op_count,
        &[
            (OpKind::Allocate, 0.25),
            (OpKind::Read, 0.25),
            (OpKind::Update, 0.25),
            (OpKind::Delete, 0.25),
        ],
        &access,
        &sizes,
    );
    Workload {
        name: "mutation-log".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for Mutation Log: 10K Allocate ops, sizes uniform
/// [64B, 4KB] inclusive.
pub fn gen_mutation_log_prepopulate(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let mut rng = ChaCha8Rng::seed_from_u64(seed.wrapping_add(2));
    let ops: Vec<Operation> = (0..prepop_count)
        .map(|_| Operation::Allocate {
            size: rng.gen_range(64..=4096),
        })
        .collect();
    Workload {
        name: "mutation-log-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}
```

Note on seed derivation: we use `seed.wrapping_add(1)` and `seed.wrapping_add(2)` for the auxiliary RNGs (sizes and prepop sizes) so each gets an independent stream. ChaCha8Rng with offset seeds produces uncorrelated outputs.

- [ ] **Step 4: Run, expect 1 passed**

Run: `cd bench && cargo test scenarios::tests::gen_mutation_log_shape`
Expected: 1 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/scenarios.rs
git commit -m "$(cat <<'EOF'
bench: add Mutation Log scenario generator (S3)

10K records × uniform [64, 4096] byte sizes; 100K ops in equal mix
(allocate/read/update/delete each at 25%); uniform random access.
Per master spec §4.3 the access pattern is uniform (not Zipfian) — the
COW-stress workload aims to exercise every page roughly equally.
Auxiliary RNGs use seed.wrapping_add(N) for sizes and prepop sizes
to keep streams independent.
EOF
)"
```

---

## Task 8: `scenarios.rs` — Document Store + 1 test

**Files:**
- Modify: `bench/src/scenarios.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    #[test]
    fn gen_document_store_shape() {
        let seed = seed_for("document-store");
        let prepop = gen_document_store_prepopulate(seed);
        let workload = gen_document_store(seed);

        // Pre-population: 10K Allocate ops with log-normal sizes
        assert_eq!(prepop.name, "document-store-prepopulate");
        assert_eq!(prepop.ops.len(), 10_000);
        assert_eq!(prepop.prepop_count, 0);
        for op in &prepop.ops {
            match op {
                Operation::Allocate { size } => {
                    // Sizes clamped to [16, 4_194_304] by lognormal_sizes
                    assert!(
                        (16..=4_194_304).contains(size),
                        "prepop size {size} out of clamp range"
                    );
                }
                _ => panic!("expected only Allocate ops in prepop"),
            }
        }

        // Main workload: 50K ops, 70/20/10 read/alloc/update
        assert_eq!(workload.name, "document-store");
        assert_eq!(workload.ops.len(), 50_000);
        assert_eq!(workload.prepop_count, 10_000);
        let read = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let alloc = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Allocate { .. }))
            .count();
        let update = workload
            .ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        // 70/20/10 over 50K → expect 35K/10K/5K (multinomial ±500)
        assert!((34_500..=35_500).contains(&read));
        assert!((9_500..=10_500).contains(&alloc));
        assert!((4_500..=5_500).contains(&update));
        assert_eq!(read + alloc + update, 50_000);
    }
```

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test scenarios::tests::gen_document_store_shape`
Expected: compile error — `cannot find function gen_document_store`.

- [ ] **Step 3: Implement Document Store generators**

Add to `bench/src/scenarios.rs`, after the Mutation Log functions:

```rust
/// S4: Document Store — 10K records, log-normal sizes (median 4KB,
/// p99 ≈ 1MB), 50K ops 70%/20%/10% read/allocate/update, Zipfian
/// θ=0.7 (moderate skew, more spread than YCSB-A's 0.99).
/// Master spec §4.4.
pub fn gen_document_store(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let op_count = 50_000;
    let theta = 0.7;
    let access = zipfian_indices(seed, op_count, prepop_count, theta);
    // Sizes for Allocate (~10K of them) and Update (~5K of them):
    // lognormal with median 4KB, p99 1MB.
    let sizes = lognormal_sizes(seed.wrapping_add(1), op_count, 4096, 1_048_576);
    let ops = mix_operations(
        seed,
        op_count,
        &[
            (OpKind::Read, 0.70),
            (OpKind::Allocate, 0.20),
            (OpKind::Update, 0.10),
        ],
        &access,
        &sizes,
    );
    Workload {
        name: "document-store".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Pre-population for Document Store: 10K Allocate ops with log-normal
/// sizes (median 4KB, p99 ≈ 1MB).
pub fn gen_document_store_prepopulate(seed: u64) -> Workload {
    let prepop_count = 10_000;
    let sizes = lognormal_sizes(seed.wrapping_add(2), prepop_count, 4096, 1_048_576);
    let ops: Vec<Operation> = sizes
        .iter()
        .map(|&size| Operation::Allocate { size })
        .collect();
    Workload {
        name: "document-store-prepopulate".to_string(),
        seed,
        prepop_count: 0,
        ops,
    }
}
```

- [ ] **Step 4: Run, expect 1 passed**

Run: `cd bench && cargo test scenarios::tests::gen_document_store_shape`
Expected: 1 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/src/scenarios.rs
git commit -m "$(cat <<'EOF'
bench: add Document Store scenario generator (S4)

10K records × log-normal sizes (median 4KB, p99 ≈ 1MB); 50K ops
70/20/10 read/alloc/update; Zipfian θ=0.7 (moderate skew, more
spread than YCSB-A's 0.99). Per master spec §4.4 — catches
regressions in Chisel's overflow path under realistic mixed-size
workloads. Auxiliary RNGs use seed.wrapping_add(N) for size
sampling to keep streams independent.
EOF
)"
```

---

## Task 9: `runner.rs` — `ScenarioResult` + `run_scenario_cell` + 2 tests

**Files:**
- Modify: `bench/src/runner.rs`
- Modify: `bench/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Add to `bench/src/runner.rs`'s `tests` module:

```rust
    #[test]
    fn run_scenario_cell_chisel_smoke() {
        // Small synthetic scenario: 50 prepop, 100 timed ops.
        let prepop = Workload {
            name: "test-prepop".to_string(),
            seed: 0x9001,
            prepop_count: 0,
            ops: (0..50).map(|_| Operation::Allocate { size: 256 }).collect(),
        };
        let workload = Workload {
            name: "test-scenario".to_string(),
            seed: 0x9001,
            prepop_count: 50,
            ops: (0..100).map(|i| Operation::Read { alloc_index: i % 50 }).collect(),
        };
        let result = run_scenario_cell(EngineMode::ChiselStrict, "smoke", &prepop, &workload);
        assert_eq!(result.scenario, "smoke");
        assert_eq!(result.mode, "chisel-strict");
        assert_eq!(result.op_count, 100);
        assert!(result.total_wall_clock_ns > 0);
        assert!(result.throughput_ops_per_sec > 0.0);
        // p50 <= p95 <= p99 (sanity, percentile ordering)
        assert!(result.p50_ns <= result.p95_ns);
        assert!(result.p95_ns <= result.p99_ns);
        // ChiselStrict produces non-null counters
        assert!(result.counters.is_some());
    }

    #[test]
    fn run_scenario_cell_returns_counters_only_for_chisel() {
        let prepop = Workload {
            name: "test-prepop".to_string(),
            seed: 0x9002,
            prepop_count: 0,
            ops: (0..50).map(|_| Operation::Allocate { size: 256 }).collect(),
        };
        let workload = Workload {
            name: "test-scenario".to_string(),
            seed: 0x9002,
            prepop_count: 50,
            ops: (0..100).map(|i| Operation::Read { alloc_index: i % 50 }).collect(),
        };
        let result = run_scenario_cell(EngineMode::RedbStrict, "smoke", &prepop, &workload);
        assert!(result.counters.is_none(), "redb cells produce counters: None");
    }
```

These tests need `Workload` in scope. The `runner.rs` file already imports it via `use crate::workload::{Operation, Workload};` from earlier PRs. The test module's `use super::*;` brings it into the test scope.

- [ ] **Step 2: Run, expect compile error**

Run: `cd bench && cargo test runner::tests::run_scenario_cell`
Expected: compile error — `cannot find function run_scenario_cell` and `cannot find type ScenarioResult`.

- [ ] **Step 3: Add `ScenarioResult` and `run_scenario_cell`**

Edit `bench/src/runner.rs`. Find the existing `capture_aux_metrics_warm_read` function and add `ScenarioResult` + `run_scenario_cell` immediately after it (before the `#[cfg(test)]` block).

Imports needed at top of file (extend the existing `use` block):

```rust
use crate::summary::format::percentile_linear_interp;
use std::time::Instant;
```

Then the type and function:

```rust
/// Result of running one (scenario, mode) cell. Captures everything
/// the post-processor will surface in the markdown + JSON.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub mode: String,
    pub total_wall_clock_ns: u64,
    pub op_count: usize,
    pub throughput_ops_per_sec: f64,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub final_file_size_bytes: u64,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

/// Helper: returns true iff applying this op to an engine requires
/// a transaction (Allocate / Update / Delete / DeleteMany). Reads
/// happen outside any explicit tx.
fn op_is_mutating(op: &Operation) -> bool {
    matches!(
        op,
        Operation::Allocate { .. }
            | Operation::Update { .. }
            | Operation::Delete { .. }
            | Operation::DeleteMany { .. }
    )
}

/// Run one scenario cell end-to-end:
///   1. Open a fresh engine in `mode` at a tempfile path
///   2. Run `prepopulate_workload` untimed (each Allocate in its own tx
///      for simplicity; pre-pop time is excluded from the measurement)
///   3. Snapshot file size + counters (the "before" state for deltas)
///   4. Start wall-clock timer
///   5. Iterate `scenario_workload.ops`, calling apply_op on each;
///      mutating ops get wrapped in a single-op tx (begin/commit);
///      reads happen bare. Per-op `Instant::now()` captures latency.
///   6. Stop wall-clock timer
///   7. Snapshot file size + counters (the "after" state)
///   8. Compute percentiles from per-op timings via percentile_linear_interp
///
/// Engine drops at end of function; tempfile is auto-deleted.
pub fn run_scenario_cell(
    mode: EngineMode,
    scenario_name: &str,
    prepopulate_workload: &Workload,
    scenario_workload: &Workload,
) -> ScenarioResult {
    let working = tempfile::NamedTempFile::new().expect("create tempfile");
    let mut engine = mode
        .open(working.path(), CACHE_SIZE_PAGES)
        .expect("open engine");

    // Pre-population phase (untimed). Each allocate in its own tx
    // for engine compatibility; we don't care about pre-pop throughput.
    let mut snapshot_ids: Vec<u64> = Vec::with_capacity(prepopulate_workload.ops.len());
    let mut new_ids_during_prepop: Vec<Identifier> = Vec::new();
    for op in &prepopulate_workload.ops {
        engine.begin().expect("begin prepop tx");
        apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids_during_prepop);
        engine.commit().expect("commit prepop tx");
    }
    // Move new_ids_during_prepop into snapshot_ids for the timed phase.
    snapshot_ids.extend(new_ids_during_prepop.iter().map(|id| id.0));

    // Capture "before" state right after prepopulate.
    let counters_before = engine.internal_counters().expect("counters before");
    let size_after_prepop = engine.file_size_bytes().expect("file size before");

    // Timed phase: run scenario_workload with per-op Instant::now().
    let mut per_op_ns: Vec<u64> = Vec::with_capacity(scenario_workload.ops.len());
    let mut new_ids: Vec<Identifier> = Vec::new();
    let total_start = Instant::now();
    for op in &scenario_workload.ops {
        let op_start = Instant::now();
        if op_is_mutating(op) {
            engine.begin().expect("begin tx");
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids);
            engine.commit().expect("commit tx");
        } else {
            apply_op(&mut *engine, op, &snapshot_ids, &mut new_ids);
        }
        per_op_ns.push(op_start.elapsed().as_nanos() as u64);
    }
    let total_wall_clock = total_start.elapsed();

    // Capture "after" state.
    let counters_after = engine.internal_counters().expect("counters after");
    let size_after = engine.file_size_bytes().expect("file size after");

    // Compute percentiles from per-op distribution.
    let mut sorted: Vec<f64> = per_op_ns.iter().map(|&n| n as f64).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile_linear_interp(&sorted, 0.50).unwrap_or(0.0);
    let p95 = percentile_linear_interp(&sorted, 0.95).unwrap_or(0.0);
    let p99 = percentile_linear_interp(&sorted, 0.99).unwrap_or(0.0);

    let total_ns = total_wall_clock.as_nanos() as u64;
    let op_count = scenario_workload.ops.len();
    let throughput = if total_ns == 0 {
        0.0
    } else {
        op_count as f64 / (total_ns as f64 / 1e9)
    };

    ScenarioResult {
        scenario: scenario_name.to_string(),
        mode: mode.label().to_string(),
        total_wall_clock_ns: total_ns,
        op_count,
        throughput_ops_per_sec: throughput,
        p50_ns: p50,
        p95_ns: p95,
        p99_ns: p99,
        final_file_size_bytes: size_after,
        file_size_delta_bytes: size_after as i64 - size_after_prepop as i64,
        counters: counter_delta(counters_before, counters_after),
    }
}
```

The function uses several existing helpers from earlier PRs: `apply_op` (PR 4b), `counter_delta` (PR 4b), `EngineMode::open` (PR 4b), `CACHE_SIZE_PAGES` (PR 4b). All are already in scope at the top of `runner.rs`.

- [ ] **Step 4: Add re-exports to `lib.rs`**

The current `bench/src/lib.rs` re-exports:

```rust
pub use runner::EngineMode;
```

Update to also re-export `ScenarioResult`, `run_scenario_cell`, and the scenario-tier types:

```rust
pub use runner::{run_scenario_cell, EngineMode, ScenarioResult};
pub use scenarios::{
    gen_document_store, gen_document_store_prepopulate, gen_mutation_log,
    gen_mutation_log_prepopulate, gen_ycsb_a, gen_ycsb_a_prepopulate, gen_ycsb_b,
    gen_ycsb_b_prepopulate, seed_for,
};
pub use workload::{OpKind, Operation, Workload};
```

The full updated re-export section:

```rust
pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use runner::{run_scenario_cell, EngineMode, ScenarioResult};
pub use scenarios::{
    gen_document_store, gen_document_store_prepopulate, gen_mutation_log,
    gen_mutation_log_prepopulate, gen_ycsb_a, gen_ycsb_a_prepopulate, gen_ycsb_b,
    gen_ycsb_b_prepopulate, seed_for,
};
pub use sqlite_engine::SqliteEngine;
pub use workload::{OpKind, Operation, Workload};
```

- [ ] **Step 5: Run tests, expect both passing**

Run: `cd bench && cargo test runner::tests::run_scenario_cell`
Expected: 2 passed.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/runner.rs bench/src/lib.rs
git commit -m "$(cat <<'EOF'
bench: add ScenarioResult + run_scenario_cell to runner

run_scenario_cell opens a fresh engine, runs prepopulate_workload
untimed (each allocate in its own tx for compatibility), captures
"before" state (file size + counters), runs scenario_workload timed
with per-op Instant::now() to collect per-op latencies, captures
"after" state, computes percentiles via the existing
summary::format::percentile_linear_interp, returns a ScenarioResult.

Mutating ops get wrapped in single-op transactions; reads happen
bare. The per-op timings include the per-tx commit cost — the right
thing for a YCSB-style "client-observed latency" measurement.

Two tests cover the chisel-strict happy path and the non-Chisel
counters: None invariant.
EOF
)"
```

---

## Task 10: `bench/benches/scenarios.rs` + `[[bench]]` declaration

**Files:**
- Modify: `bench/Cargo.toml`
- Create: `bench/benches/scenarios.rs`

- [ ] **Step 1: Add the `[[bench]]` declaration to `Cargo.toml`**

The current `bench/Cargo.toml` has `[[bench]] micro_grid` and `[[bin]] summarize`. Add a new `[[bench]] scenarios` block immediately after the `[[bench]] micro_grid` block:

```toml
[[bench]]
name = "micro_grid"
harness = false

[[bench]]
name = "scenarios"
harness = false

[[bin]]
name = "summarize"
path = "src/bin/summarize.rs"
```

- [ ] **Step 2: Create `bench/benches/scenarios.rs`**

```rust
// Bench-binary entry for the scenario tier. Iterates the 12 cells
// (4 scenarios × 3 strict modes), calls run_scenario_cell for each,
// streams ScenarioResult rows to bench/results/scenarios_metrics.jsonl
// (one JSON object per line, flushed after each cell for crash
// resilience).
//
// We do NOT use Criterion here — the master-spec runtime budget of
// 1-6 minutes per full tier rules out Criterion's many-samples-per-bench
// model. Inline Instant::now() timing inside run_scenario_cell gives
// per-op latency distribution from a single 100K-op run, which is
// already self-averaging.
//
// Cargo wiring: this file is the source for [[bench]] name = "scenarios"
// with harness = false in bench/Cargo.toml. Run via:
//
//   cargo bench --bench scenarios

use chisel_bench::runner::{run_scenario_cell, EngineMode};
use chisel_bench::scenarios::{
    gen_document_store, gen_document_store_prepopulate, gen_mutation_log,
    gen_mutation_log_prepopulate, gen_ycsb_a, gen_ycsb_a_prepopulate, gen_ycsb_b,
    gen_ycsb_b_prepopulate, seed_for,
};
use chisel_bench::workload::Workload;
use std::io::Write;

const STRICT_MODES: &[EngineMode] = &[
    EngineMode::ChiselStrict,
    EngineMode::RedbStrict,
    EngineMode::SqliteStrict,
];

fn main() -> std::io::Result<()> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = format!("{manifest_dir}/results/scenarios_metrics.jsonl");
    if let Some(p) = std::path::Path::new(&out_path).parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut writer = std::fs::File::create(&out_path)?;

    for (scenario_name, prepop, workload) in build_scenarios() {
        for &mode in STRICT_MODES {
            eprintln!("running {scenario_name} on {} ...", mode.label());
            let result = run_scenario_cell(mode, scenario_name, &prepop, &workload);
            serde_json::to_writer(&mut writer, &result)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            eprintln!(
                "  total {:.2}s, p50 {:.0} ns, p99 {:.0} ns, throughput {:.1} ops/s",
                result.total_wall_clock_ns as f64 / 1e9,
                result.p50_ns,
                result.p99_ns,
                result.throughput_ops_per_sec,
            );
        }
    }
    eprintln!("Wrote 12 cells to {out_path}");
    Ok(())
}

fn build_scenarios() -> Vec<(&'static str, Workload, Workload)> {
    let s1 = seed_for("ycsb-a");
    let s2 = seed_for("ycsb-b");
    let s3 = seed_for("mutation-log");
    let s4 = seed_for("document-store");
    vec![
        ("ycsb-a", gen_ycsb_a_prepopulate(s1), gen_ycsb_a(s1)),
        ("ycsb-b", gen_ycsb_b_prepopulate(s2), gen_ycsb_b(s2)),
        (
            "mutation-log",
            gen_mutation_log_prepopulate(s3),
            gen_mutation_log(s3),
        ),
        (
            "document-store",
            gen_document_store_prepopulate(s4),
            gen_document_store(s4),
        ),
    ]
}
```

- [ ] **Step 3: Verify the bench target compiles**

Run: `cd bench && cargo bench --bench scenarios --no-run 2>&1 | tail -5`
Expected: clean compile, "Finished" line.

- [ ] **Step 4: Run a quick smoke (the actual scenarios are slow — defer the full run to acceptance)**

The full bench takes 1-6 minutes. We don't run it here in Task 10; Task 16 (acceptance) will. For now, just verify the target builds and the binary's `--help` if it had one. Since this binary doesn't take args, there's nothing to spot-check beyond the build.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add bench/Cargo.toml bench/benches/scenarios.rs
git commit -m "$(cat <<'EOF'
bench: add scenarios bench binary + [[bench]] declaration

bench/benches/scenarios.rs is the harness=false entry point for the
scenario tier. Iterates 12 cells (4 scenarios × 3 strict modes),
calls run_scenario_cell for each, streams ScenarioResult rows to
bench/results/scenarios_metrics.jsonl. Flushes after each cell for
crash resilience (Ctrl-C mid-run leaves N completed cells parseable).
[[bench]] declaration added to Cargo.toml — was deferred from task 1
because Cargo validates target sources at parse time.
EOF
)"
```

---

## Task 11: `summary/discover.rs` — `ScenarioMetrics` + `load_scenarios_jsonl`

**Files:**
- Modify: `bench/src/summary/discover.rs`
- Modify: `bench/src/summary/mod.rs`

- [ ] **Step 1: Add `ScenarioMetrics` type + `load_scenarios_jsonl` function**

Add to `bench/src/summary/discover.rs`, after the existing `copy_raw_archive` function and before the `#[cfg(test)] mod tests` block:

```rust
/// Per-scenario metrics, one per (scenario, mode) cell. Mirrors the
/// scenarios_metrics.jsonl schema produced by bench/benches/scenarios.rs.
/// Renderers (render_json, render_md) consume Vec<ScenarioMetrics>
/// alongside Vec<Cell> to produce the full PR 5 + PR 6 output.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScenarioMetrics {
    pub scenario: String,
    pub mode: String,
    pub total_wall_clock_ns: u64,
    pub op_count: usize,
    pub throughput_ops_per_sec: f64,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub final_file_size_bytes: u64,
    pub file_size_delta_bytes: i64,
    pub counters: Option<ChiselCountersDelta>,
}

/// Load the scenarios_metrics.jsonl file produced by the scenario
/// bench binary. Returns Vec<ScenarioMetrics> sorted by (scenario,
/// mode) for deterministic output. Missing file → empty Vec (matches
/// PR 5's "warn-and-continue for missing aux" pattern). Malformed
/// lines logged to stderr but don't abort.
pub fn load_scenarios_jsonl(path: &Path) -> Vec<ScenarioMetrics> {
    if !path.exists() {
        eprintln!(
            "warning: scenarios-metrics file '{}' missing; markdown will omit the scenario section",
            path.display()
        );
        return Vec::new();
    }
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: could not read scenarios-metrics '{}': {}",
                path.display(),
                e
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (lineno, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ScenarioMetrics>(line) {
            Ok(m) => out.push(m),
            Err(e) => {
                eprintln!(
                    "warning: skipping malformed scenarios-metrics line {}: {}",
                    lineno + 1,
                    e
                );
            }
        }
    }
    out.sort_by(|a, b| {
        a.scenario
            .cmp(&b.scenario)
            .then_with(|| a.mode.cmp(&b.mode))
    });
    out
}
```

- [ ] **Step 2: Update `mod.rs` re-exports**

The current `bench/src/summary/mod.rs` (after PR 5) has:

```rust
pub use discover::{copy_raw_archive, discover_cells, AuxMetrics, Cell, TimingStats};
```

Add `load_scenarios_jsonl` and `ScenarioMetrics` to the re-export:

```rust
pub use discover::{
    copy_raw_archive, discover_cells, load_scenarios_jsonl, AuxMetrics, Cell,
    ScenarioMetrics, TimingStats,
};
```

- [ ] **Step 3: Verify build**

Run: `cd bench && cargo build && cargo test`
Expected: clean compile, all 54 + 11 (new from tasks 2-9) = 65 existing-and-new tests still pass.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/src/summary/discover.rs bench/src/summary/mod.rs
git commit -m "$(cat <<'EOF'
bench: add ScenarioMetrics type + load_scenarios_jsonl

ScenarioMetrics mirrors the scenarios_metrics.jsonl schema that
bench/benches/scenarios.rs streams. load_scenarios_jsonl parses the
file with the same warn-and-continue discipline as the existing
aux-metrics loader: missing file → empty Vec + warning, malformed
lines → log + skip. Output sorted by (scenario, mode) for
determinism.
EOF
)"
```

---

## Task 12: `summary/render_json.rs` — extend with `scenarios` key + 1 test

**Files:**
- Modify: `bench/src/summary/render_json.rs`

- [ ] **Step 1: Update the test fixture and add a new test**

The existing `render_json_schema_round_trips` test calls `render_json(&cells, &metadata)` (2 args). The new signature has 3 args. Update the existing test to pass `&[]` for scenarios, and add a new test for the scenarios case.

Replace the `render_json_schema_round_trips` test in `bench/src/summary/render_json.rs` with the updated 3-arg form:

```rust
    #[test]
    fn render_json_schema_round_trips() {
        let cells = vec![
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "chisel-strict".to_string(),
                size: "32B".to_string(),
                timing: Some(TimingStats {
                    p50_ns: 1234.5,
                    p95_ns: 1567.8,
                    p99_ns: 1890.2,
                }),
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 8192,
                    counters: Some(ChiselCountersDelta {
                        cache_hits: 12,
                        cache_misses: 3,
                        fsync_calls: 2,
                        pages_allocated: 4,
                    }),
                }),
            },
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "redb-strict".to_string(),
                size: "32B".to_string(),
                timing: None,
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 4096,
                    counters: None,
                }),
            },
        ];

        let value = render_json(&cells, &[], &fixture_metadata());

        assert_eq!(value["metadata"]["cell_count"], 2);
        assert!(value["cells"].is_object());
        assert!(value["scenarios"].is_object());
        assert_eq!(value["scenarios"].as_object().unwrap().len(), 0);

        let c1 = &value["cells"]["allocate-1pertx/chisel-strict/32B"];
        assert_eq!(c1["p50_ns"], 1234.5);
        assert_eq!(c1["counters"]["cache_hits"], 12);

        let c2 = &value["cells"]["allocate-1pertx/redb-strict/32B"];
        assert!(c2["p50_ns"].is_null());
        assert_eq!(c2["file_size_delta_bytes"], 4096);
        assert!(c2["counters"].is_null());
    }
```

Add a new test for scenarios:

```rust
    #[test]
    fn render_json_includes_scenarios_top_level() {
        use crate::summary::discover::ScenarioMetrics;

        let scenarios = vec![
            ScenarioMetrics {
                scenario: "ycsb-a".to_string(),
                mode: "chisel-strict".to_string(),
                total_wall_clock_ns: 15_000_000_000,
                op_count: 100_000,
                throughput_ops_per_sec: 6666.7,
                p50_ns: 120_000.0,
                p95_ns: 180_000.0,
                p99_ns: 250_000.0,
                final_file_size_bytes: 104_857_600,
                file_size_delta_bytes: 4_194_304,
                counters: Some(ChiselCountersDelta {
                    cache_hits: 99_000,
                    cache_misses: 1_000,
                    fsync_calls: 100_000,
                    pages_allocated: 12_500,
                }),
            },
            ScenarioMetrics {
                scenario: "ycsb-a".to_string(),
                mode: "redb-strict".to_string(),
                total_wall_clock_ns: 19_000_000_000,
                op_count: 100_000,
                throughput_ops_per_sec: 5263.2,
                p50_ns: 145_000.0,
                p95_ns: 200_000.0,
                p99_ns: 320_000.0,
                final_file_size_bytes: 110_000_000,
                file_size_delta_bytes: 5_000_000,
                counters: None,
            },
        ];

        let value = render_json(&[], &scenarios, &fixture_metadata());

        assert!(value["scenarios"].is_object());
        let scenarios_obj = value["scenarios"].as_object().unwrap();
        assert_eq!(scenarios_obj.len(), 2);

        let s1 = &value["scenarios"]["ycsb-a/chisel-strict"];
        assert_eq!(s1["op_count"], 100_000);
        assert_eq!(s1["p50_ns"], 120_000.0);
        assert_eq!(s1["counters"]["cache_hits"], 99_000);

        let s2 = &value["scenarios"]["ycsb-a/redb-strict"];
        assert!(s2["counters"].is_null());
        assert_eq!(s2["file_size_delta_bytes"], 5_000_000);
    }
```

- [ ] **Step 2: Run, expect compile errors**

Run: `cd bench && cargo test summary::render_json::tests`
Expected: compile errors — `render_json` is called with 3 args, but the function takes 2; `scenarios` key access in tests fails.

- [ ] **Step 3: Update `render_json` signature + implementation**

Replace the `render_json` function in `bench/src/summary/render_json.rs` with the 3-arg version:

```rust
use crate::summary::discover::{Cell, ScenarioMetrics};
use crate::summary::metadata::Metadata;
use serde_json::{json, Map, Value};

/// Render a Vec<Cell> + Vec<ScenarioMetrics> + Metadata into the
/// results.json document. Output schema:
///
///   {
///     "metadata": { ... metadata fields ... },
///     "cells": {
///       "<row>/<mode>/<size>": { ... timing + aux fields ... },
///       ...
///     },
///     "scenarios": {
///       "<scenario>/<mode>": { ... scenario fields ... },
///       ...
///     }
///   }
///
/// Missing data is explicit `null`, not omitted — keeps the schema
/// rectangular for diff tooling. Empty `scenarios` slice produces
/// `"scenarios": {}` (still present, just empty).
pub fn render_json(cells: &[Cell], scenarios: &[ScenarioMetrics], metadata: &Metadata) -> Value {
    let mut cells_map = Map::new();
    for cell in cells {
        let key = format!("{}/{}/{}", cell.row, cell.mode, cell.size);
        cells_map.insert(key, render_cell_json(cell));
    }
    let mut scenarios_map = Map::new();
    for s in scenarios {
        let key = format!("{}/{}", s.scenario, s.mode);
        scenarios_map.insert(key, render_scenario_json(s));
    }
    json!({
        "metadata": metadata,
        "cells": cells_map,
        "scenarios": scenarios_map,
    })
}

fn render_cell_json(cell: &Cell) -> Value {
    json!({
        "p50_ns": cell.timing.map(|t| t.p50_ns),
        "p95_ns": cell.timing.map(|t| t.p95_ns),
        "p99_ns": cell.timing.map(|t| t.p99_ns),
        "file_size_delta_bytes": cell.aux.map(|a| a.file_size_delta_bytes),
        "counters": cell.aux.and_then(|a| a.counters),
    })
}

fn render_scenario_json(s: &ScenarioMetrics) -> Value {
    json!({
        "total_wall_clock_ns": s.total_wall_clock_ns,
        "op_count": s.op_count,
        "throughput_ops_per_sec": s.throughput_ops_per_sec,
        "p50_ns": s.p50_ns,
        "p95_ns": s.p95_ns,
        "p99_ns": s.p99_ns,
        "final_file_size_bytes": s.final_file_size_bytes,
        "file_size_delta_bytes": s.file_size_delta_bytes,
        "counters": s.counters,
    })
}
```

- [ ] **Step 4: Run tests, expect both passing**

Run: `cd bench && cargo test summary::render_json::tests`
Expected: 2 passed.

- [ ] **Step 5: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff. Note: the binary `bench/src/bin/summarize.rs` calls `render_json(&cells, &metadata)` (2 args) and will fail to compile until Task 14 updates it. Clippy will surface this as an error. To keep this task's commit green, ALSO update the binary's call site at this point — see Task 14 for the full binary changes; the minimal change here is changing one call line.

Update `bench/src/bin/summarize.rs` line that calls `render_json`:

```rust
    let json = render_json(&cells, &metadata);
```

becomes:

```rust
    let json = render_json(&cells, &[], &metadata);
```

(The same applies to `render_markdown` after Task 13. For now, only `render_json` was updated.)

- [ ] **Step 6: Re-verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/summary/render_json.rs bench/src/bin/summarize.rs
git commit -m "$(cat <<'EOF'
bench: extend render_json with scenarios top-level key

Adds a third top-level key `scenarios` to results.json, mirroring the
existing `cells` shape but keyed by `<scenario>/<mode>`. Empty
scenarios slice produces `"scenarios": {}` — schema is rectangular
either way for PR 7's CI diff. Existing test updated to pass &[] for
scenarios; new test asserts the scenarios-populated case. Binary
call site also updated to the 3-arg form so the build stays green.
EOF
)"
```

---

## Task 13: `summary/render_md.rs` — extend with scenario table + 2 tests

**Files:**
- Modify: `bench/src/summary/render_md.rs`

- [ ] **Step 1: Update the existing test and add a new one**

The existing tests in `render_md.rs` call `render_markdown(&cells, &metadata)` (2 args). Update them to the 3-arg form, and add a new test for the scenario-table case.

Replace the existing `render_markdown_includes_required_sections` and `render_markdown_skipped_cells_render_as_dash` tests with the 3-arg form:

```rust
    #[test]
    fn render_markdown_includes_required_sections() {
        let md = render_markdown(&fixture_cells(), &[], &fixture_metadata());
        assert!(md.contains("# Chisel Benchmark Summary"), "missing H1");
        assert!(md.contains("## Durability mode legend"), "missing legend");
        assert!(md.contains("## Method"), "missing method/disclaimer section");
        assert!(md.contains("## Micro grid"), "missing micro grid header");
        assert!(
            md.contains("### `allocate-1pertx`"),
            "missing allocate-1pertx subsection"
        );
        assert!(md.contains("## File-size delta"), "missing file-size delta header");
        assert!(
            md.contains("## Chisel internals appendix"),
            "missing appendix header"
        );
    }

    #[test]
    fn render_markdown_skipped_cells_render_as_dash() {
        let md = render_markdown(&fixture_cells(), &[], &fixture_metadata());
        assert!(md.contains("—"), "missing em-dash for skipped cell");
        let redb_line = md
            .lines()
            .find(|l| l.starts_with("| redb-strict |"))
            .expect("redb-strict line should exist in micro grid");
        assert!(
            redb_line.contains("—"),
            "redb-strict row should contain em-dash for missing timing"
        );
    }
```

Add a new test for scenarios:

```rust
    #[test]
    fn render_markdown_includes_scenario_table() {
        use crate::summary::discover::ScenarioMetrics;

        let scenarios = vec![
            ScenarioMetrics {
                scenario: "ycsb-a".to_string(),
                mode: "chisel-strict".to_string(),
                total_wall_clock_ns: 15_000_000_000,
                op_count: 100_000,
                throughput_ops_per_sec: 6666.7,
                p50_ns: 120_000.0,
                p95_ns: 180_000.0,
                p99_ns: 250_000.0,
                final_file_size_bytes: 104_857_600,
                file_size_delta_bytes: 4_194_304,
                counters: Some(ChiselCountersDelta {
                    cache_hits: 99_000,
                    cache_misses: 1_000,
                    fsync_calls: 100_000,
                    pages_allocated: 12_500,
                }),
            },
            ScenarioMetrics {
                scenario: "ycsb-a".to_string(),
                mode: "redb-strict".to_string(),
                total_wall_clock_ns: 19_000_000_000,
                op_count: 100_000,
                throughput_ops_per_sec: 5263.2,
                p50_ns: 145_000.0,
                p95_ns: 200_000.0,
                p99_ns: 320_000.0,
                final_file_size_bytes: 110_000_000,
                file_size_delta_bytes: 5_000_000,
                counters: None,
            },
        ];

        let md = render_markdown(&[], &scenarios, &fixture_metadata());
        assert!(md.contains("## Scenario tier"), "missing scenario tier header");
        // Both scenario rows present
        assert!(md.contains("| ycsb-a | chisel-strict |"));
        assert!(md.contains("| ycsb-a | redb-strict |"));
        // Throughput formatted as integer ops/s
        assert!(md.contains("6666 ops/s") || md.contains("6667 ops/s"));
    }
```

- [ ] **Step 2: Run, expect compile errors**

Run: `cd bench && cargo test summary::render_md::tests`
Expected: compile errors — `render_markdown` is called with 3 args.

- [ ] **Step 3: Update `render_markdown` signature + add scenario rendering**

Edit `bench/src/summary/render_md.rs`. Update the public function signature and add a new private helper.

The existing `render_markdown` function header:

```rust
pub fn render_markdown(cells: &[Cell], metadata: &Metadata) -> String {
```

becomes:

```rust
pub fn render_markdown(
    cells: &[Cell],
    scenarios: &[ScenarioMetrics],
    metadata: &Metadata,
) -> String {
```

The body inserts a new `render_scenario_table(...)` call between the disclaimer and the micro-grid section:

```rust
pub fn render_markdown(
    cells: &[Cell],
    scenarios: &[ScenarioMetrics],
    metadata: &Metadata,
) -> String {
    let mut out = String::new();
    render_header(&mut out, metadata);
    render_durability_legend(&mut out);
    render_disclaimer(&mut out);
    if !scenarios.is_empty() {
        render_scenario_table(&mut out, scenarios);
    }
    render_micro_grid(&mut out, cells);
    render_file_size_table(&mut out, cells);
    render_chisel_internals_appendix(&mut out, cells, scenarios);
    render_footer(&mut out, metadata);
    out
}
```

Add the import for `ScenarioMetrics` at the top of the file:

```rust
use crate::summary::discover::{Cell, ScenarioMetrics};
```

Add the new `render_scenario_table` function. Place it before `render_micro_grid`:

```rust
fn render_scenario_table(out: &mut String, scenarios: &[ScenarioMetrics]) {
    let _ = writeln!(out, "## Scenario tier\n");
    let _ = writeln!(
        out,
        "End-to-end YCSB-style workloads. Each scenario runs once per strict durability mode (chisel-strict, redb-strict, sqlite-strict). Per-op timings collected inline via `Instant::now()` before/after each op; percentiles computed from the full distribution (no Criterion sampling — see PR 6 spec §3.4)."
    );
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "| scenario | mode | throughput | p50 | p95 | p99 | total | final size |"
    );
    let _ = writeln!(
        out,
        "|----------|------|-----------:|----:|----:|----:|------:|-----------:|"
    );
    for s in scenarios {
        let throughput = s.throughput_ops_per_sec.round() as u64;
        let p50 = format_duration_ns(s.p50_ns);
        let p95 = format_duration_ns(s.p95_ns);
        let p99 = format_duration_ns(s.p99_ns);
        let total = format_duration_ns(s.total_wall_clock_ns as f64);
        let final_size = format_bytes(s.final_file_size_bytes as i64);
        let _ = writeln!(
            out,
            "| {} | {} | {} ops/s | {} | {} | {} | {} | {} |",
            s.scenario, s.mode, throughput, p50, p95, p99, total, final_size
        );
    }
    let _ = writeln!(out);
}
```

Update `render_chisel_internals_appendix` to accept the new `scenarios` parameter and add a scenario subsection:

```rust
fn render_chisel_internals_appendix(
    out: &mut String,
    cells: &[Cell],
    scenarios: &[ScenarioMetrics],
) {
    let _ = writeln!(out, "## Chisel internals appendix\n");
    let _ = writeln!(
        out,
        "Counter deltas for cells where `engine_mode = chisel-strict`. One row per cell (row × size); columns are the four counters from `Chisel::counters()`.\n"
    );
    let _ = writeln!(
        out,
        "| row | size | cache_hits | cache_misses | fsync_calls | pages_allocated |"
    );
    let _ = writeln!(
        out,
        "|-----|------|------------|--------------|-------------|-----------------|"
    );

    let mut chisel_cells: Vec<&Cell> = cells.iter().filter(|c| c.mode == "chisel-strict").collect();
    chisel_cells.sort_by(|a, b| {
        a.row.cmp(&b.row).then_with(|| {
            let asize = parse_size_to_bytes(&a.size).unwrap_or(u64::MAX);
            let bsize = parse_size_to_bytes(&b.size).unwrap_or(u64::MAX);
            asize.cmp(&bsize)
        })
    });

    for cell in chisel_cells {
        let counters = cell.aux.and_then(|a| a.counters);
        let (h, m, f, p) = match counters {
            Some(c) => (
                c.cache_hits.to_string(),
                c.cache_misses.to_string(),
                c.fsync_calls.to_string(),
                c.pages_allocated.to_string(),
            ),
            None => (
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
            ),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            cell.row, cell.size, h, m, f, p
        );
    }
    let _ = writeln!(out);

    // Scenario subsection — chisel-strict scenarios only.
    let chisel_scenarios: Vec<&ScenarioMetrics> = scenarios
        .iter()
        .filter(|s| s.mode == "chisel-strict")
        .collect();
    if !chisel_scenarios.is_empty() {
        let _ = writeln!(
            out,
            "Counter deltas for chisel-strict scenarios (parallel section):\n"
        );
        let _ = writeln!(
            out,
            "| scenario | cache_hits | cache_misses | fsync_calls | pages_allocated |"
        );
        let _ = writeln!(
            out,
            "|----------|------------|--------------|-------------|-----------------|"
        );
        for s in chisel_scenarios {
            let (h, m, f, p) = match s.counters {
                Some(c) => (
                    c.cache_hits.to_string(),
                    c.cache_misses.to_string(),
                    c.fsync_calls.to_string(),
                    c.pages_allocated.to_string(),
                ),
                None => (
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                ),
            };
            let _ = writeln!(out, "| {} | {} | {} | {} | {} |", s.scenario, h, m, f, p);
        }
        let _ = writeln!(out);
    }
}
```

- [ ] **Step 4: Update the binary's `render_markdown` call site**

Edit `bench/src/bin/summarize.rs`. The existing call:

```rust
    let md = render_markdown(&cells, &metadata);
```

becomes:

```rust
    let md = render_markdown(&cells, &[], &metadata);
```

(Task 14 will update this fully to read scenarios from disk and pass them in.)

- [ ] **Step 5: Run tests, expect 3 passing**

Run: `cd bench && cargo test summary::render_md::tests`
Expected: 3 passed.

- [ ] **Step 6: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 7: Commit**

```bash
git add bench/src/summary/render_md.rs bench/src/bin/summarize.rs
git commit -m "$(cat <<'EOF'
bench: extend render_markdown with scenario tier section

Adds a "Scenario tier" H2 section between the Method disclaimer and
the Micro grid. One row per (scenario, mode) — 12 rows in production.
Columns: scenario, mode, throughput (rounded ops/s), p50, p95, p99
(magnitude-adaptive units via summary::format), total wall-clock,
final file size. Section is omitted entirely when no scenarios are
loaded.

The Chisel internals appendix gets a parallel scenario subsection —
chisel-strict scenarios only, columns are the four counters. Existing
tests updated to the 3-arg form; new test asserts the scenario rows.
EOF
)"
```

---

## Task 14: `bin/summarize.rs` — wire scenarios into the pipeline

**Files:**
- Modify: `bench/src/bin/summarize.rs`

- [ ] **Step 1: Add a CLI flag for scenarios path + wire scenarios through**

The current `bench/src/bin/summarize.rs` (after PR 5 + tasks 12-13's call-site updates) has the `Cli` struct with three flags (`--out`, `--criterion`, `--aux`). Add `--scenarios` and pass through to the renderers.

The full updated file:

```rust
// CLI entry point for the chisel-bench-summarize post-processor.
// Reads PR 4b's bench output (Criterion sample.json + aux_metrics.jsonl)
// PLUS PR 6's scenarios_metrics.jsonl, and emits summary.md +
// results.json + raw/ under bench/results/<UTC>/.
//
// All logic lives in the chisel_bench::summary library module; this
// file is just argv parsing, error printing, and exit codes.

use chisel_bench::summary::{
    copy_raw_archive, discover_cells, gather_metadata, load_scenarios_jsonl, render_json,
    render_markdown,
};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chisel-bench-summarize", version)]
#[command(about = "Post-process Criterion + aux-metrics + scenarios output")]
struct Cli {
    /// Output directory (default: bench/results/<UTC-ISO8601>/)
    #[arg(long)]
    out: Option<PathBuf>,

    /// Criterion output directory.
    #[arg(long, default_value = "target/criterion")]
    criterion: PathBuf,

    /// Aux-metrics JSONL produced by the micro-grid bench.
    #[arg(long, default_value = "bench/results/aux_metrics.jsonl")]
    aux: PathBuf,

    /// Scenarios-metrics JSONL produced by the scenario bench.
    #[arg(long, default_value = "bench/results/scenarios_metrics.jsonl")]
    scenarios: PathBuf,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Discover cells (micro grid) and load scenarios (PR 6).
    let cells = discover_cells(&cli.criterion, &cli.aux)?;
    let scenarios = load_scenarios_jsonl(&cli.scenarios);
    if cells.is_empty() && scenarios.is_empty() {
        return Err(
            "no cells or scenarios discovered — did you run cargo bench?".into(),
        );
    }

    // 2. Resolve output directory.
    let out_dir = cli.out.unwrap_or_else(|| {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
        PathBuf::from(format!("bench/results/{ts}"))
    });
    std::fs::create_dir_all(&out_dir)?;

    // 3. Gather metadata.
    let metadata = gather_metadata(&cli.criterion, &cli.aux, cells.len())?;

    // 4. Render markdown + JSON.
    let md = render_markdown(&cells, &scenarios, &metadata);
    let json = render_json(&cells, &scenarios, &metadata);

    // 5. Write output artifacts.
    std::fs::write(out_dir.join("summary.md"), &md)?;
    std::fs::write(
        out_dir.join("results.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    copy_raw_archive(&cli.criterion, &out_dir.join("raw"))?;

    // 6. Tell user where to find it.
    println!(
        "Wrote {} cells + {} scenarios to {}",
        cells.len(),
        scenarios.len(),
        out_dir.display()
    );
    println!(
        "  - summary.md  ({} bytes)",
        std::fs::metadata(out_dir.join("summary.md"))?.len()
    );
    println!(
        "  - results.json ({} bytes)",
        std::fs::metadata(out_dir.join("results.json"))?.len()
    );
    println!("  - raw/ (Criterion estimates.json + sample.json archive)");

    Ok(())
}
```

- [ ] **Step 2: Verify the binary compiles + `--help` works**

Run: `cd bench && cargo build --bin summarize 2>&1 | tail -3`
Expected: clean compile.

Run: `cd bench && cargo run --bin summarize -- --help 2>&1 | tail -20`
Expected: clap-formatted help text with the four flags (`--out`, `--criterion`, `--aux`, `--scenarios`).

- [ ] **Step 3: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 4: Commit**

```bash
git add bench/src/bin/summarize.rs
git commit -m "$(cat <<'EOF'
bench: wire scenarios into the summarize binary pipeline

Add --scenarios <FILE> flag (default bench/results/scenarios_metrics.jsonl).
Pipeline now: discover_cells + load_scenarios_jsonl → gather_metadata →
render_markdown(cells, scenarios, metadata) → render_json(cells,
scenarios, metadata) → write artifacts. Exit-with-error condition is
"both cells and scenarios empty" rather than just "cells empty" — a
scenarios-only run is still a valid summary to produce.
EOF
)"
```

---

## Task 15: Test fixture + integration smoke test extension

**Files:**
- Create: `bench/tests/fixtures/scenarios_metrics.jsonl`
- Modify: `bench/tests/summarize_smoke.rs`

- [ ] **Step 1: Create the scenarios fixture**

Create `bench/tests/fixtures/scenarios_metrics.jsonl` with two lines (1 chisel-strict, 1 redb-strict). The values are hand-crafted for assertions:

```jsonl
{"scenario":"ycsb-a","mode":"chisel-strict","total_wall_clock_ns":15234567890,"op_count":100000,"throughput_ops_per_sec":6566.4,"p50_ns":120000.0,"p95_ns":180000.0,"p99_ns":250000.0,"final_file_size_bytes":104857600,"file_size_delta_bytes":4194304,"counters":{"cache_hits":99000,"cache_misses":1000,"fsync_calls":100000,"pages_allocated":12500}}
{"scenario":"ycsb-a","mode":"redb-strict","total_wall_clock_ns":19100000000,"op_count":100000,"throughput_ops_per_sec":5235.6,"p50_ns":145000.0,"p95_ns":200000.0,"p99_ns":320000.0,"final_file_size_bytes":110100000,"file_size_delta_bytes":5242880,"counters":null}
```

- [ ] **Step 2: Update the integration smoke test**

The existing `bench/tests/summarize_smoke.rs` invokes the binary against the existing fixtures (criterion + aux_metrics.jsonl). Update it to also exercise the scenarios path.

Replace the test body to add scenarios:

```rust
// Integration smoke test: invoke the summarize binary against the
// committed fixtures and verify the three output artifacts are produced
// with sensible sizes + structure. PR 6 extension: also exercise the
// scenarios-metrics file path.

use assert_cmd::Command;
use std::path::PathBuf;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn summarize_smoke_runs_against_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("out");

    let mut cmd = Command::cargo_bin("summarize").unwrap();
    cmd.arg("--out").arg(&out_dir);
    cmd.arg("--criterion").arg(fixtures_root().join("criterion"));
    cmd.arg("--aux").arg(fixtures_root().join("aux_metrics.jsonl"));
    cmd.arg("--scenarios")
        .arg(fixtures_root().join("scenarios_metrics.jsonl"));

    let output = cmd.output().expect("failed to run binary");
    assert!(
        output.status.success(),
        "summarize exited non-zero. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let md_path = out_dir.join("summary.md");
    let json_path = out_dir.join("results.json");
    let raw_dir = out_dir.join("raw");
    assert!(md_path.exists(), "summary.md missing");
    assert!(json_path.exists(), "results.json missing");
    assert!(raw_dir.is_dir(), "raw/ directory missing");

    // PR 5 cells assertions (unchanged).
    let json_content = std::fs::read_to_string(&json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
    assert!(parsed["metadata"].is_object());
    assert!(parsed["cells"].is_object());
    let cells_obj = parsed["cells"].as_object().unwrap();
    assert!(
        cells_obj.len() >= 2,
        "expected at least 2 cells in fixture output, got {}",
        cells_obj.len()
    );

    // PR 6 scenarios assertions.
    assert!(
        parsed["scenarios"].is_object(),
        "results.json missing scenarios top-level key"
    );
    let scenarios_obj = parsed["scenarios"].as_object().unwrap();
    assert_eq!(
        scenarios_obj.len(),
        2,
        "expected 2 scenarios from fixture, got {}",
        scenarios_obj.len()
    );
    let s1 = &parsed["scenarios"]["ycsb-a/chisel-strict"];
    assert_eq!(s1["op_count"], 100000);
    assert_eq!(s1["counters"]["cache_hits"], 99000);
    let s2 = &parsed["scenarios"]["ycsb-a/redb-strict"];
    assert!(s2["counters"].is_null());

    // Markdown should include the Scenario tier section.
    let md_content = std::fs::read_to_string(&md_path).unwrap();
    assert!(
        md_content.contains("## Scenario tier"),
        "summary.md missing Scenario tier section"
    );
    assert!(
        md_content.contains("ycsb-a"),
        "summary.md missing ycsb-a row"
    );

    let chisel_raw = raw_dir
        .join("allocate-1pertx")
        .join("chisel-strict")
        .join("32B");
    assert!(
        chisel_raw.join("sample.json").exists(),
        "raw chisel-strict sample.json missing"
    );
    assert!(
        chisel_raw.join("estimates.json").exists(),
        "raw chisel-strict estimates.json missing"
    );
}
```

- [ ] **Step 3: Run the test**

Run: `cd bench && cargo test --test summarize_smoke 2>&1 | tail -10`
Expected: 1 passed.

- [ ] **Step 4: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: no warnings, no diff.

- [ ] **Step 5: Commit**

```bash
git add bench/tests/fixtures/scenarios_metrics.jsonl bench/tests/summarize_smoke.rs
git commit -m "$(cat <<'EOF'
bench: extend summarize smoke test for scenarios path

New fixture file scenarios_metrics.jsonl with 2 lines (1 chisel-strict,
1 redb-strict). The existing integration smoke test now passes
--scenarios pointing at the fixture and asserts: results.json has the
scenarios top-level key with 2 entries, counters non-null for chisel
and null for redb, summary.md includes "## Scenario tier" with a
ycsb-a row.
EOF
)"
```

---

## Task 16: Final acceptance verification

**Files:**
- Read-only checks across the bench subcrate.

- [ ] **Step 1: Run the full unit + integration test suite**

Run: `cd bench && cargo test 2>&1 | grep "test result" | tail -10`
Expected counts:
- 36 (PR 5 lib) + 6 (workload primitives × 2) + 5 (scenarios) + 2 (run_scenario_cell) + 1 (render_json scenarios) + 1 (render_md scenarios) = **51 lib tests**
- 15 equivalence tests
- 1 lib smoke (PR 4b)
- 1 runner smoke (PR 4b)
- 1 summarize smoke (PR 5+6)
- = **69 tests total**, all passing

Adjust expectations if exact count differs ±2 due to test bookkeeping.

- [ ] **Step 2: Verify clippy and fmt clean**

Run: `cd bench && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

Run: `cargo fmt -- --check` (from `/Users/xof/Documents/Dev/chisel/.worktrees/bench-scenario-tier`)
Expected: no diff.

- [ ] **Step 3: Verify the engine-agnostic invariant for `scenarios.rs`**

Run: `grep -nE "^use (chisel|redb|rusqlite)" bench/src/scenarios.rs`
Expected: no output. Scenarios are pure data-layer code; engines are reached through the trait abstraction in `runner.rs`.

- [ ] **Step 4: Verify the `--help` text**

Run: `cd bench && cargo run --bin summarize -- --help 2>&1 | tail -15`
Expected: clap-formatted help showing four flags: `--out`, `--criterion`, `--aux`, `--scenarios`, plus `--help` and `--version`.

- [ ] **Step 5: Run the scenarios bench end-to-end**

Run: `cd bench && time cargo bench --bench scenarios 2>&1 | tail -20`
Expected: completes in under 10 minutes (target 1-6 min per spec; 10 min ceiling for hardware variance). Stderr shows 12 cells (4 scenarios × 3 modes) with per-cell summary lines.

If the run exceeds 10 minutes:
- Identify the slow scenario from the per-cell timing logs
- Possible causes: SQLite WAL flake (apply the fix from PR 4b's `flush_for_snapshot`); very-slow `gen_document_store` (lognormal sizes producing pathological 1MB allocations on a slow filesystem)
- Document and either tune the scenario parameters in this PR or defer

- [ ] **Step 6: Verify scenarios_metrics.jsonl was produced correctly**

Run: `wc -l bench/results/scenarios_metrics.jsonl`
Expected: exactly 12 lines (4 scenarios × 3 modes).

Run: `awk -F'"scenario":"' '{print $2}' bench/results/scenarios_metrics.jsonl | awk -F'"' '{print $1}' | sort | uniq -c`
Expected: each of the 4 scenarios appears 3 times.

- [ ] **Step 7: Run the post-processor and verify the full output**

Run: `cd bench && cargo run --bin summarize 2>&1 | tail -5`
Expected: "Wrote 0 cells + 12 scenarios to bench/results/<timestamp>/" (assuming no micro-grid bench has been run; the cells count is 0 if `target/criterion/` is empty, but scenarios count is 12).

If `target/criterion/` has data from earlier micro-grid runs, the cells count will be non-zero too.

Verify the summary.md content:

```bash
TS=$(ls -td bench/results/2* | head -1)
head -40 "$TS/summary.md"
```

Expected output: header + durability legend + method + Scenario tier section with 12 rows.

- [ ] **Step 8: Verify results.json has the scenarios key**

Run: `python3 -c "import json; r=json.load(open('$TS/results.json')); print(list(r['scenarios'].keys()))"`
Expected: a list of 12 keys, each `<scenario>/<mode>`.

- [ ] **Step 9: Cross-check spec acceptance criteria**

Spec §7.6 acceptance criteria 1-7:
1. ✓ cargo build / cargo test pass — verified in Step 1
2. ✓ cargo clippy --all-targets -- -D warnings clean — verified in Step 2
3. ✓ cargo fmt -- --check clean — verified in Step 2
4. ✓ The 14 new tests pass — verified in Step 1
5. ✓ cargo bench --bench scenarios completes under 10 minutes — verified in Step 5
6. ✓ summary.md has Scenario tier section with 12 rows; results.json has scenarios with 12 entries; scenarios_metrics.jsonl has 12 lines — verified in Steps 6-8
7. Project commenting standards held — verified by visual inspection of the modified files

- [ ] **Step 10: No commit needed if all checks pass**

If steps 1-9 all pass, do nothing — the plan is complete.

If clippy/fmt produced any warnings or fmt diffs, address them as a small cleanup commit titled `bench: cleanup`.

---

## Final state after all tasks

- `bench/Cargo.toml` has `rand_distr = "0.4"` dep + `[[bench]] name = "scenarios"` target.
- `bench/src/workload.rs` has 4 new pub items: `OpKind`, `zipfian_indices`, `lognormal_sizes`, `mix_operations`. ~200 LOC added.
- `bench/src/scenarios.rs` has `seed_for` + 8 generators (4 scenarios × 2 [main + prepopulate]). ~140 LOC.
- `bench/src/runner.rs` has `ScenarioResult` + `run_scenario_cell`. ~110 LOC added.
- `bench/benches/scenarios.rs` is the bench-binary entry. ~80 LOC.
- `bench/src/summary/{discover,render_json,render_md}.rs` extended with scenario types + renderer paths. ~165 LOC added.
- `bench/src/bin/summarize.rs` extended with `--scenarios` flag and pipeline wiring. ~20 LOC added.
- `bench/tests/fixtures/scenarios_metrics.jsonl` is the new 2-line fixture.
- `bench/tests/summarize_smoke.rs` extended with scenarios assertions.
- 14 new tests + 1 fixture file added. Total bench tests: ~69.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check` all clean.
- `cargo bench --bench scenarios` runs the 12 cells in 1-10 minutes.
- 16 commits (15 code-bearing + 1 acceptance-only if cleanup needed).

PR 7 (CI workflow) can now begin: it runs `cargo bench --bench scenarios` on each PR, captures `results.json`, diffs against `main`'s baseline, and posts a regression report as a PR comment.
