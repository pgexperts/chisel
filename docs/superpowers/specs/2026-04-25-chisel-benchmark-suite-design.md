# Chisel Benchmark Suite — Design

**Date:** 2026-04-25
**Status:** Design approved; implementation plan pending.
**Scope:** Performance benchmarking harness for Chisel, comparing against redb and SQLite (disk-backed) across a diagnostic micro grid and a small set of realistic scenarios.

## 1. Goals and Non-Goals

### Goals

- Surface where Chisel is fast or slow relative to redb and SQLite, with enough diagnostic detail (per-operation, per-value-size, per-transaction-granularity) that an optimization session can read directly from the output and pick a target. (Optimization-target finder.)
- Detect performance regressions in Chisel commit-to-commit, with stable enough signal that a 10–20% regression on a realistic scenario is a clear positive in CI rather than noise. (Regression detector.)
- Produce headline cross-engine numbers as a byproduct of the above, suitable for the README and 1.0 release notes.

### Non-Goals (v1)

- Not a YCSB-cred competitive evaluation. We adopt YCSB workload names where they fit, but we are not running the YCSB Java harness or optimizing for direct numerical comparison with published YCSB results.
- Not a comprehensive cross-engine survey. Three engines, one machine class, one OS class. Platform variance is not chased.
- Not a multi-threaded benchmark. Chisel is deliberately single-writer; benchmarking otherwise would misrepresent it.
- Not a memory-resident comparison. SQLite has an in-memory mode and Chisel has `open_in_memory`, but the request is explicitly for disk mode. Memory-backed variants are out of scope.

## 2. Architecture

The harness is a layered library inside a new `bench/` subcrate, sibling to the existing `python/` subcrate. Four layers, each independently testable:

```
┌──────────────────────────────────────────────────────────────┐
│  Reporter      Criterion HTML + Markdown summary + JSON      │
├──────────────────────────────────────────────────────────────┤
│  Runner        Pre-populates dataset, controls cache state,  │
│                drives Workload over Engine, collects metrics │
├──────────────────────────────────────────────────────────────┤
│  Workload      Parameterized op sequence generator           │
│                (seeded → reproducible)                       │
├──────────────────────────────────────────────────────────────┤
│  Engine trait  Uniform façade: allocate/read/update/         │
│                delete/delete_many + tx control + introspect  │
└──────────────────────────────────────────────────────────────┘

  ChiselEngine ──► chisel = { path = ".." }     (instrumented)
  RedbEngine   ──► redb (latest stable)
  SqliteEngine ──► rusqlite (latest stable; PRAGMA-configured per durability mode)
```

### 2.1 Crate Layout

`bench/` is a sibling subcrate of `python/`, with its own `Cargo.toml`. It path-depends on `chisel`. redb, rusqlite, Criterion, and other bench dependencies are local to `bench/` and never enter the main crate's dependency graph — embedded users of `chisel` are unaffected.

### 2.2 The Engine Trait

A single trait abstracts over the three engines:

- Four mutating methods: `allocate`, `update`, `delete`, `delete_many`.
- One read method.
- Three transaction methods: `begin`, `commit`, `rollback`.
- Introspection: `file_size_bytes`, `internal_counters() -> Option<Counters>` — only `ChiselEngine` returns `Some`.

Read takes `&self`; mutating methods take `&mut self`. This matches Chisel's post-F3 shape and fits redb and SQLite naturally.

The trait is parameterized over an opaque `Identifier` newtype wrapping `u64`. Each engine maps it to its native form (Chisel handle, redb monotonic-counter key, SQLite `rowid`).

### 2.3 API Mapping: Handle-as-Natural-Identifier

Each engine returns its own native identifier on insert; subsequent reads/updates/deletes use that identifier. SQLite uses `INSERT … RETURNING rowid`; redb uses a caller-generated monotonic `u64` key; Chisel uses the returned `Handle`. The harness is measuring the workload Chisel is designed for: blob-store-by-its-own-identifier.

We do not measure external-key-lookup workloads. That would force Chisel to maintain a key→handle side-table, mixing two engines' work into one benchmark. A reader who needs external-key lookup can mentally add the side-table cost; a reader using blob-by-handle gets a directly applicable number.

### 2.4 Workloads as Data

A `Workload` is a struct describing a sequence of `Operation`s, generated from `(seed, size_distribution, op_mix, access_pattern, count)` parameters. The `Runner` applies it. Two consequences:

1. The same workload replays deterministically against any engine.
2. Workloads are inspectable and unit-testable without an engine present.

## 3. The Micro Grid

The diagnostic core. A `9 × 6 × 5` matrix per output metric.

### 3.1 Rows (the "what we measure" axis)

| Row | Operation | Tx granularity |
|-----|-----------|----------------|
| 1 | `allocate` | 1 per tx |
| 2 | `allocate` | 1000 per tx |
| 3 | `read` | warm (single op, no tx in Chisel/redb) |
| 4 | `read` | cold (first read after `open()`) |
| 5 | `update` | 1 per tx |
| 6 | `update` | 1000 per tx |
| 7 | `delete` | 1 per tx |
| 8 | `delete` | 1000 per tx |
| 9 | `delete_many(1000)` | single call |

Row 9 measures the bulk primitive against itself, not against `delete` × 1000. The diff between rows 8 and 9 tells you whether `delete_many` actually amortizes better than the loop.

### 3.2 Size Buckets

Six log-spaced sizes, one per Chisel internal regime:

| Size | Regime |
|------|--------|
| 32 B | Tiny — multiple values per page (R1 packing winner) |
| 256 B | Small — packed |
| 2 KB | Medium — packed |
| 16 KB | Just over inline boundary, single overflow page |
| 128 KB | Overflow chain, ~16 pages |
| 1 MB | Large overflow, ~128 pages |

### 3.3 Engine-Mode Columns

| Column | Engine | Durability |
|--------|--------|-----------|
| `chisel-strict` | Chisel | native (always fsync) |
| `redb-strict` | redb | `Durability::Immediate` |
| `sqlite-strict` | SQLite | `synchronous=FULL` + WAL |
| `redb-unsafe` | redb | `Durability::Eventual` |
| `sqlite-unsafe` | SQLite | `synchronous=OFF` |

Chisel has no unsafe column — there is no way to disable fsync, by design.

5 engine-modes × 6 sizes × 9 rows = **270 cells per output metric**. Output metrics: wall-clock median, p95, p99; file-size delta; Chisel-internal counters (Chisel rows only).

### 3.4 Pre-Population Calibration

Each `(operation, size)` cell is measured against a pre-populated dataset whose total raw payload is ≈25 MB:

| Size | Pre-populated count |
|------|---------------------|
| 32 B | ~800K |
| 256 B | ~100K |
| 2 KB | ~12.5K |
| 16 KB | ~1.5K |
| 128 KB | ~200 |
| 1 MB | ~25 |

The constant raw-payload target keeps file sizes roughly comparable across rows of the table. The actual-file-size column will diverge — and that divergence is itself one of the things we are measuring.

## 4. The Scenario Tier

Four scenarios, each modeling a realistic-but-distinct workload. Each runs only in **strict durability mode** across the three engines — the unsafe column is a diagnostic for the micro grid; for "real-world effects" only durable numbers belong.

### 4.1 S1: YCSB-A — Balanced Read/Update

- Pre-populate 100K records × 1 KB ≈ 100 MB dataset (deliberately exceeds default cache).
- 100K operations: 50% read, 50% update.
- Zipfian access (θ=0.99, the YCSB default — heavy skew, ~75% of accesses to ~10% of records).

### 4.2 S2: YCSB-B — Read-Heavy

- Same setup as S1; mix is 95% read / 5% update.
- The cache-effective workload: most accesses hit warm pages. A regression here means a cache or read-path regression.

### 4.3 S3: Mutation Log — COW Stress

- Pre-populate 10K records, sizes uniform in [64 B, 4 KB] (covers the entire R1-packed regime).
- 100K operations: 25% allocate, 25% read, 25% update, 25% delete. Uniform random access.
- Models OLTP-like row mutation: high churn, small values, page-cache and freemap worked hard.
- Catches regressions in COW efficiency and freemap reclamation.

### 4.4 S4: Document Store — Size-Distribution Realism

- Pre-populate 10K records, **log-normal sizes**: median 4 KB, p99 ≈ 1 MB. ~60 MB total dataset.
- 50K operations: 70% read, 20% allocate, 10% update. No delete.
- Zipfian access (θ=0.7 — moderate skew, more spread than YCSB-A's 0.99).
- Catches regressions where Chisel's overflow path stalls under realistic mixed-size workloads.

### 4.5 Per-Scenario Output

Total wall-clock duration; throughput (ops/sec); per-operation p50/p95/p99 latency; final file size.

### 4.6 Reproducibility

Every workload generator takes a seed; the seed is fixed per scenario by name (e.g., `seed = "ycsb-a"`). Same scenario, same seed, same operations, regardless of engine.

### 4.7 Run Cost

4 scenarios × 3 strict engines = 12 scenario runs. Target ~5–30 s per run, ~1–6 minutes total. CI runs only this tier; the 270-cell micro grid is local-only.

## 5. Methodology

### 5.1 Cache State

Each benchmark runs against a pre-populated dataset (so we measure operation cost, not initial-allocation cost). Default cache state is **warm**: the engine has been iterated through the dataset before measurement, so all relevant pages are in the engine's cache and the OS page cache.

### 5.2 Cold Reads

Read operations have a separate **cold** variant (row 4 of the micro grid). Cold means *Chisel-LRU cold*: the engine is freshly opened, no values touched, the first read of each measurement sample is the timed call.

We do **not** deliberately drop the OS page cache (would require root, doesn't run in CI). At small sizes the OS page cache likely holds the data warm; at 128 KB and 1 MB it's more often cold. The output table marks the metric as "cold (engine-cache cold; OS-cache best-effort)" so readers don't over-interpret.

Writes have no cold/warm split: writing to a fresh file pages in those pages and warms them as a side effect, so "cold write" isn't a distinct regime.

### 5.3 Sample Size and Stability

Criterion's defaults govern sampling: ~100 samples per benchmark, with adaptive iteration counts to keep total measurement time bounded. We keep these defaults unless variance turns out to be unacceptable for the regression-detection use case, at which point we tune `sample_size` and `measurement_time` per row.

## 6. Metrics and Instrumentation

### 6.1 Metrics Surfaced Per Cell

**Wall-clock time:** median, p95, p99 per operation, computed by Criterion from its sample distribution.

**File-size delta:** `stat()` the database file before and after, report the delta. Externally observable; comparable across engines.

**Chisel-internal counters** (Chisel rows only): four cumulative counters, snapshotted before and after each cell, reported as deltas:

| Counter | Site | Meaning |
|---------|------|---------|
| `cache_hits` | `PageCache::get` | Returned a cached page without I/O |
| `cache_misses` | `PageCache::get` | Had to read from disk |
| `fsync_calls` | `PageIo::sync*` | Total fsync invocations |
| `pages_allocated` | `PageCache::new_page` | New page allocations |

Hit rate is derived: `hits / (hits + misses)`. Together these attribute observed time to one of three buckets — fsync cost, cache-miss I/O, or allocation churn.

### 6.2 The Precursor Instrumentation PR

Lands first, in isolation, before the bench harness:

- Add four `Cell<u64>` counters to `PageCache` and `PageIo`. `Cell`, not `AtomicU64`: Chisel is single-writer by design and the harness reads counters on the same thread.
- Increment in the obvious places.
- Expose as `Chisel::counters() -> ChiselCounters`, a snapshot struct sibling to the existing `Chisel::stats()`. Counters are cumulative-from-open; the harness reads-subtract-reads to compute per-cell deltas.
- Three Rust unit tests: counters increment through a known operation sequence; counters reset on close/reopen; snapshot isolation.
- Python binding update: `chisel.counters()` returning a small dataclass mirror; `.pyi` updated; one Python test.
- Estimated cost: ~80–120 lines including tests. No format change, no API break.

### 6.3 Out of Scope for v1 Instrumentation

- Per-operation internal timers (e.g., "time in handle-table lookup vs data-page lookup vs overflow-chain walk"). Too invasive; instrument under a feature flag at investigation time if a specific microbench raises a question.
- redb / SQLite internals. Black boxes — apples-to-apples means measuring what the public API exposes.

## 7. Output and CI Integration

### 7.1 Output Artifacts

**Criterion HTML** (`target/criterion/...`). Built-in; per-cell detail pages with sample histograms and regression-vs-previous-run comparison. The diagnostic deep-dive surface.

**Markdown summary** (`bench/results/<timestamp>/summary.md`). Post-processed by a small Rust binary in `bench/` reading Criterion's `estimates.json`. Structure:

1. Header — date, machine info, Chisel commit, durability mode legend.
2. Micro grid — nine tables, one per row-of-the-9. Rows = engine-modes, columns = sizes, cells = `p50 (p99)`.
3. File-size delta table.
4. Scenario summary — one table for all four scenarios.
5. Chisel internals appendix — for cells where Chisel ran.

**JSON results** (`bench/results/<timestamp>/results.json`). Machine-readable equivalent. Schema: a single document indexed by `(scenario_or_cell_id, engine_mode)`. Used by CI for diff computation.

**Raw Criterion JSON snapshot** (`bench/results/<timestamp>/raw/`). Archival copy of per-cell `estimates.json`.

### 7.2 CI Workflow (Report-Only)

`.github/workflows/bench.yml`, triggers on `pull_request` against `main`. Pinned to `ubuntu-latest`. Runs only the scenario tier (12 runs).

The job runs benches twice in the same workflow:

1. Checkout `main`, build, run scenarios, save `results-main.json`.
2. Checkout PR head, build, run scenarios, save `results-pr.json`.
3. Diff and post a PR comment.

Doubling the runtime (~2–12 min per PR) is the cost of "no persistent state" in v1. A future optimization is to cache `main`'s results in a separate ref or artifact.

The PR comment flags regressions > 5% with an emoji warning but **never blocks merge**. The comment is signal, not gate. If a reviewer or PR author judges a flagged regression intentional or expected, they note it and merge.

### 7.3 What CI Does Not Run

- The micro grid (270 cells, ~30 minutes) — local-only.
- The unsafe durability columns — micro-grid diagnostic, not regression-relevant.
- Chisel-internal counter columns in the regression diff — used when investigating *why* a regression happened, not for detecting it.

## 8. Build Sequence

Seven PRs, in order. Each lands independently; each is meaningful on its own; the dependency graph is strictly forward.

| # | PR | Content | LOC | Lands when |
|---|----|---------|-----|------------|
| 1 | Instrumentation precursor | `Cell<u64>` counters in `PageCache` and `PageIo`; `Chisel::counters()`; Python binding mirror; tests | ~120 | Counters increment correctly; no API break |
| 2 | `bench/` subcrate + Engine trait + ChiselEngine | New subcrate alongside `python/`; `Engine` trait; `ChiselEngine` impl using PR 1 counters; smoke test | ~300 | Subcrate compiles; one engine works |
| 3 | RedbEngine + SqliteEngine | Both engines, both durability modes. Cross-engine equivalence test | ~400 | Three engines exercise identical workload semantics |
| 4 | Micro grid | `Workload`, `Operation`, generators; `Runner` with Criterion `iter_batched`, pre-population, warm/cold cache control; the 270-cell registration | ~600 | `cargo bench --bench micro_grid` produces Criterion HTML for all cells |
| 5 | Markdown summary post-processor | Small binary reading Criterion `estimates.json`; emits `summary.md` and `results.json` | ~250 | Markdown matches the schema in §7.1; JSON validates |
| 6 | Scenario tier | YCSB-A, YCSB-B, mutation log, document store. Each as its own bench group | ~400 | All four scenarios run end-to-end on the three strict engines |
| 7 | CI workflow | `.github/workflows/bench.yml`; two-checkout strategy; PR comment posting | ~80 yaml | Bench run on a representative PR posts a comment; flag fires for an injected 5% regression |

### 8.1 Minimal Viable Point

PR 4. With PRs 1–4 we have the diagnostic tier producing Criterion HTML for the optimization-finder use case. PR 5 makes the output legible to non-Criterion-fluent readers. PR 6 adds the regression-detection workload. PR 7 wires PR 6 into CI.

PRs 1–4 could ship as a private internal milestone if optimization work needs to start before the rest is built.

### 8.2 Rollback Paths

- PR 1 fails review: harmless — instrumentation drops, Chisel internals just aren't part of bench output later.
- PR 2 fails: subcrate doesn't merge; nothing depends on it yet.
- PR 3 fails: ChiselEngine still works; we have a Chisel-only diagnostic harness with no cross-engine comparison until 3 lands.
- PR 4 fails: foundation crates remain, no bench code. Easy revert.
- PRs 5–7 fail: harness still produces Criterion HTML; just not the prettier outputs.

### 8.3 Estimated Calendar Time

~2–3 weeks if pushed end-to-end, but PRs 1, 2, and 5 are independent enough to land in parallel with unrelated work. Other work should not be gated on this sequence.

## 9. Open Implementation-Phase Questions

These are deliberately deferred to the implementation plan:

- Workspace declaration vs standalone subcrate Cargo.toml shape.
- Specific Criterion configuration (`sample_size`, warm-up duration, `measurement_time`) per row.
- The exact `cargo bench` registration shape (one bench per cell vs grouped via `BenchmarkGroup`).
- Specific pinned versions of redb and rusqlite.
- GitHub Actions runner pinning (specific image, hardware class) if `ubuntu-latest` proves too variable.
- Bot-comment authentication (token, permissions, comment-update vs comment-append behavior).
- Vendored YCSB workload trace files vs in-process generation.

These are implementation detail that does not affect the design contract. The plan resolves them.
