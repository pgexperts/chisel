# Chisel

Transactional slot-based storage engine in Rust using shadow paging (copy-on-write) for crash durability.

## Build & Test

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

CI runs the Rust checks above plus a Python matrix (CPython 3.11 and 3.13 ×
Linux/macOS) that builds the PyO3 binding via `maturin develop` and runs
`pytest` in `python/tests`. A separate `wheels.yml` workflow builds abi3
wheels on tagged releases.

### Python binding

The `python/` subcrate is a PyO3 wrapper (`chisel-py` → `_chisel.abi3.so`)
with its own `Cargo.toml` and `pyproject.toml`. Build locally with:

```bash
cd python
python -m venv .venv && source .venv/bin/activate
pip install maturin pytest
maturin develop
pytest
```

Public Python API lives in `python/chisel/__init__.py`; type stubs in
`python/chisel/chisel.pyi`. See `python/README.md` for usage.

### Bench harness

The `bench/` subcrate is the foundation of an in-progress benchmark
suite — independent crate, sibling to `python/`, path-deps on the
root `chisel` crate. Currently provides the `Engine` trait (uniform
façade over Chisel, redb, and SQLite), working `ChiselEngine`,
`RedbEngine`, and `SqliteEngine` impls, and cross-engine equivalence
tests (5 scenarios × 3 engines = 15 tests). Build / run locally with:

```bash
cd bench && cargo test
```

PR 4a (workload data layer — `Operation`/`Workload` types + six
seeded generators in `bench/src/workload.rs`, ChaCha8Rng-pinned for
cross-version reproducibility) landed on `main` as of 2026-04-30.
PR 4b (Runner machinery + 6-row Criterion micro grid in
`bench/src/runner.rs` + `bench/benches/micro_grid.rs`, producing
165 cells of wall-clock + file-size + Chisel-internal-counter
metrics into `target/criterion/...` and `bench/results/aux_metrics.jsonl`)
landed on `main` as of 2026-05-01. The original PR 4 from the master
spec was split into 4a + 4b once it became clear ~600 LOC in one PR
was less reviewable than two smaller PRs.

PR 5 (markdown summary post-processor: a binary `chisel-bench-summarize`
in `bench/src/bin/summarize.rs` plus a library module under
`bench/src/summary/`) landed on `main` as of 2026-05-03. Reads
`target/criterion/<row>/<mode>/<size>/sample.json` plus `bench/results/aux_metrics.jsonl`
and emits three artifacts under `bench/results/<UTC-ISO8601>/`:
`summary.md` (per-row markdown tables with magnitude-adaptive units),
`results.json` (flat composite-key schema for PR 7's CI diff), and
`raw/` (archival copy of estimates.json + sample.json per cell).
Percentiles are computed directly from `sample.json` per-iteration
times via numpy-style linear interpolation (consistent p50/p95/p99
semantics rather than mixing Criterion's bootstrap median with a CI
proxy). Run with `cd bench && cargo run --bin summarize`.

PR 6 (scenario tier — four YCSB-style end-to-end workloads in
`bench/src/scenarios.rs` + `bench/benches/scenarios.rs`, driven by
new `run_scenario_cell` in `bench/src/runner.rs`, post-processed
into the same `summary.md` / `results.json` artifacts) landed on
`main` as of 2026-05-03. Scenarios are YCSB-A (50/50 read/update,
Zipfian θ=0.99), YCSB-B (95/5), Mutation Log (25/25/25/25
allocate/read/update/delete uniform), Document Store (70/20/10
read/allocate/update with lognormal sizes, Zipfian θ=0.7). Each
runs once per strict durability mode → 12 cells. Inline
`Instant::now()` timing rather than Criterion (the master-spec
budget of 1–6 minutes per full tier rules out Criterion's
many-samples-per-bench model).

Three latent bugs surfaced at PR 6's end-to-end acceptance gate
that no per-task unit test caught — none of the unit tests run
the scenarios bench against a real engine: (1) `run_scenario_cell`
originally did one-allocate-per-tx during prepop (100K fsyncs on
chisel-strict ≈ 12 min/cell on macOS APFS); fixed by mirroring
PR 4b's `populate_snapshot` byte-accumulator chunking, generalized
for heterogeneous op sizes. (2) `gen_mutation_log` generated
Read/Update/Delete on indices without tracking which had been
deleted; replaced with a state-aware walk maintaining a live-set
`Vec<usize>` (Allocate extends with `next_alloc_index`,
Read/Update sample without removal, Delete swap-removes), plus a
`mutation_log_op_sequence_is_engine_applicable` test that
simulates `apply_op`'s resolve view to catch the bug class.
(3) `discover_cells` errored `NoCellsFound` when criterion dir
was empty even with scenarios present; `summarize.rs` now catches
that and `CriterionDirNotFound` and lets the unified
`cells.is_empty() && scenarios.is_empty()` gate decide.

PR 6's runtime: spec said 1–6 minutes target / 10 minutes ceiling.
On macOS that ceiling is unreachable — chisel uses Rust's
`sync_all` which calls `fcntl(F_FULLFSYNC)` (durable through the
disk cache), while SQLite by default uses plain `fsync()` (which
on macOS only flushes to the disk's write cache without
F_FULLFSYNC). Result: chisel-strict cells are fsync-bound at
~5–10 ms per commit while sqlite-strict cells run ~3 orders of
magnitude faster. Full 12-cell grid takes ~70–90 minutes on
macOS APFS at the spec's workload sizes; Linux CI runners (no
F_FULLFSYNC overhead) will be much faster. The cross-engine
fairness gap is pre-existing from PR 3's `SqliteEngine` wrapper
and is deferred to PR 8 (cross-engine relative-performance
tests), which will need `PRAGMA fullfsync=ON` to be apples-to-
apples on macOS.

The 4b grid is 6 rows, not the 9 the master spec called for: three
1000-per-tx variants (update, delete, delete_many) were dropped during
implementation because 1000 random ops over the prepopulated DB pin a
working set of dirty pages exceeding Chisel's 2048-page cache ceiling.
The dropped row functions remain in `micro_grid.rs` (with `#[allow(dead_code)]`)
so they can be re-enabled in a future PR with a configurable larger cache.
SQLite snapshot-restore needed a special `Engine::flush_for_snapshot()`
hook (default no-op; SQLite override does `journal_mode=DELETE`) because
WAL mode leaves committed data in the `-wal` sibling between explicit
checkpoints — `std::fs::copy` of the main `.db` alone otherwise yields
"database disk image is malformed" on reopen.

PR 7 (CI integration — `chisel-bench-diff` binary at
`bench/src/bin/diff.rs` plus `.github/workflows/bench.yml` workflow
that runs the scenario tier on each PR, diffs against `main`'s
baseline, posts a sticky regression-report comment) landed on
`main` as of 2026-05-04. Workflow does a two-checkout strategy:
build + run scenarios on `main`, build + run on PR HEAD, summarize
both, run the diff binary, post the result via
`peter-evans/find-comment` + `create-or-update-comment` keyed on
the marker `<!-- chisel-bench-diff -->` (subsequent pushes update
the same comment instead of stacking). Thresholds: throughput +
p50 at 5%, p95 + p99 at 10%, worse-direction only, no absolute
time floor in v1. Pinned to `ubuntu-latest` per the PR 6 macOS
fsync caveat — Linux makes chisel-strict and sqlite-strict
fsync costs comparable. Workflow is signal-only — never blocks
merge. Run cost on Linux: ~10 min for the full two-side
comparison.

PR 7's spec/plan at
`docs/superpowers/specs/2026-05-04-chisel-bench-ci-design.md` +
`docs/superpowers/plans/2026-05-04-chisel-bench-ci.md`. The
subagent-driven review caught three substantive issues that all
got fixed: missing test for `MalformedScenarioEntry`,
`(None, None)` arm silently returning `Unchanged` instead of
`unreachable!`, and a `-0.0%` display bug for zero-delta
throughput cells (found at two sites in `render.rs`). Worth
remembering as evidence that the two-stage review pattern (spec
compliance + code quality) finds things even when the plan
provides verbatim source.

PR 7's first acceptance gate (the workflow runs against PR 7
itself per spec §7.1) caught a real environmental issue:
`origin/main` was 76 commits behind local `main` because PRs
4-6 were merged locally but never pushed to GitHub. The
workflow's `Checkout main` step pulled the PR-3-state bench
subcrate which lacked the `summarize` binary, and the build
failed at the `Build bench (main)` step. Fix was a single
`git push origin main` after which the re-run went green.
The pattern to remember: any workflow that does `Checkout main`
+ build assumes `origin/main` is current; pre-push should
include `git log origin/main..main` as a sanity check when
the work depends on prior PRs being on GitHub.

PR 8 (cross-engine relative-performance tests, Chisel vs redb
vs SQLite, with the macOS-fsync fairness fix noted above) is
the only remaining bench-suite item. PR 8 is an addendum to
the original design and will get its own spec/plan pair when
brainstormed. The master design spec at
`docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`
covers PRs 1–7. Per-PR plans alongside in
`docs/superpowers/plans/`.

## Architecture

The module dependency graph is strictly bottom-up — no circular dependencies:

1. `page.rs`, `superblock.rs`, `error.rs` — Pure types, constants, checksums. No I/O.
2. `page_io.rs` — Raw file I/O with `flock`. The only module that touches the filesystem.
3. `page_cache.rs` — LRU cache over `PageIo` with a strict `cache_max_bytes` cap; dirty overflow spills to `spillway.rs` (a sidecar file or in-memory buffer). All other modules access pages through this; the spill/rehydrate path is invisible above the cache.
3a. `spillway.rs` — sidecar overflow file (`<db_path>.spillway`) that absorbs LRU-tail dirty pages when the cache is full of dirty pages. Per-slot XXH3 checksums; truncated at open and at every commit/rollback; never fsynced. Owned by `PageCache`.
4. `freemap.rs`, `data_page.rs`, `overflow.rs` — Page-type-specific logic. Each operates on raw `[u8; PAGE_SIZE]` buffers.
5. `handle_table.rs` — Radix tree mapping `u64` handles to `(page_id, slot_index)`. Implements its own COW.
6. `transaction.rs` — Orchestrates handle table, data pages, overflow, and superblock into transactional operations.
7. `defrag.rs`, `stats.rs` — Maintenance utilities.
8. `lib.rs` — Thin `Chisel` public API wrapping `TransactionManager`.

### Key design decisions

- **Shadow paging, not WAL.** Writes go to new pages; old pages stay intact. Commit = fsync new pages + swap superblock. Crash recovery = pick the valid superblock. No log replay.
- **COW is per-module, not centralized.** The handle table and freemap each implement their own copy-on-write using `PageCache::new_page()`. This avoids a monolithic COW abstraction.
- **Handle table indirection.** Handles are stable u64 IDs that map through a radix tree to physical `(page, slot)` locations. Values can move freely on update or defrag.
- **N superblocks** (default 2, configurable 2..=16 at create time via `Options::superblock_count`) rotate by `txn_counter % N`. The slot with the highest `txn_counter` and a valid checksum wins. This is the atomic commit mechanism; higher N survives consecutive torn writes.
- **Durability over performance.** Every commit does fsyncs at well-defined points (I28 pre-drain + main-pages flush + superblock). The spillway is never fsynced — its content does not need to survive a crash.
- **Spillway over hard ceiling.** The cache is a strict bound; dirty overflow spills to a `<db_path>.spillway` sidecar file (default cap `1024 × cache_max_bytes` = 8 GiB). This replaced the pre-existing 8× `HARD_CEILING_MULTIPLIER` elasticity. Setting `Options::spillway_max_bytes = 0` disables the spillway and restores `CacheFull`-at-cap semantics. New operational error `SpillwayFull { limit_bytes }` fires when both cache and spillway are exhausted. Spec: `docs/superpowers/specs/2026-05-03-chisel-spillway-design.md`.
- **Checksums on every page.** Both main-file pages (the existing XXH3 stamp) and spillway slots (an additional per-slot XXH3 over `page_id || page_bytes`) are checksum-verified on read; mismatch is fatal and poisons the transaction.
- **Poison on fatal error.** On any commit-path I/O failure, checksum mismatch, or corrupt superblock, the `TransactionManager` becomes poisoned (matches `std::sync::Mutex` semantics). Every subsequent call returns `ChiselError::Poisoned`; the only legal recovery is `close()` + reopen, which picks the last-durable superblock. Driven by Linux fsyncgate semantics — a failed `fsync()` cannot be safely retried.
- **In-memory mode.** `Chisel::open_in_memory` (also `chisel.open(None)` from Python) runs the full engine against a `Vec<u8>`-backed `PageIo` with no filesystem and no `flock`. Same code path, same guarantees except durability; used for tests, benchmarks, and ephemeral work.
- **Format-version compatibility.** The on-disk `format_version` is a packed `u32` (upper 16 bits MAJOR, lower 16 bits MINOR; see I29). Same-MAJOR files are mutually readable across any minor; a different MAJOR is rejected at open. Each non-superblock page also carries a one-byte `page_format_version` (I31) so individual page layouts can evolve within a major without a file-wide bump. Both schemes leave reserved bytes for forward-compatible extension. Touching either constant or any byte that participates in the on-disk format is a public-stability decision, not a refactor.

### Backlog and decision log

`ISSUES.md` is the canonical list of open work, latent bugs, and completed
fixes (each entry marked `✅ IMPLEMENTED <date>` or still open). Consult it
before proposing changes — many obvious-looking simplifications have already
been addressed. As of the 2026-04-22 status note, every roadmap item (R1–R5)
and every entry from the three commenting passes (2026-04-10, 2026-04-17,
2026-04-22) has landed; the only deferred work is two named Phase-2 followups
(I29 minor-write enforcement; I31 eager upgrader) that are gated on triggers
in a future minor release rather than calendar time.

Concurrent in-progress work (tracked outside `ISSUES.md` via spec+plan
docs): the benchmark-suite series. PRs 1 (counter instrumentation
exposing `Chisel::counters()`), 2 (`bench/` subcrate + `Engine` trait
+ `ChiselEngine`), and 3 (`RedbEngine` + `SqliteEngine` + cross-engine
equivalence tests) have landed on `main` as of 2026-04-30; PRs 4a, 4b,
5, and 6 have landed as of 2026-05-03; PR 7 (CI integration) has landed
as of 2026-05-04. PR 8 (cross-engine relative-performance tests, Chisel
vs redb vs SQLite, with the macOS-fsync fairness fix `PRAGMA fullfsync=ON`
on `SqliteEngine`) is the only remaining bench-suite item. PR 8 is an
addendum to the original design — it will get its own spec/plan pair
when brainstormed. See
`docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`.

The **spillway feature** (out-of-band from the bench-suite series) landed
on `main` as of 2026-05-04. Adds `src/spillway.rs` plus integration
across `PageCache` (spill on dirty overflow, rehydrate on miss, drain
under the existing fsync, truncate on rollback) and the public API
(`Chisel::set_cache_max_bytes` / `set_spillway_max_bytes` /
`set_drain_insertion`). Breaking change: `Options::cache_size: usize`
(page count) → `Options::cache_max_bytes: u64` (bytes); default
unchanged at 8 MiB. Plus new `Options::spillway_max_bytes` (default
1024× = 8 GiB; 0 disables spillway and restores legacy `CacheFull`-at-cap
semantics) and `Options::drain_insertion` (`LruTail` default | `Mru`).
The pre-existing 8× `HARD_CEILING_MULTIPLIER` elasticity is removed.
The bench engine (`bench/src/chisel_engine.rs`) was updated mid-PR to
enable the spillway by default — the original "spillway disabled for
cross-engine fairness" reasoning was backwards (SQLite uses a temp file
for transaction overflow, redb uses on-disk btrees; disabling Chisel's
spillway makes Chisel the only engine that fails on big transactions,
which is the unfair config). Spec/plan at
`docs/superpowers/specs/2026-05-03-chisel-spillway-design.md` +
`docs/superpowers/plans/2026-05-04-chisel-spillway.md`.

Lessons captured during the spillway rollout, worth remembering:
1. **Per-task `cargo test` from the repo root does NOT run the bench
   subcrate's tests.** Bench is a sibling crate, not a workspace member.
   `cd bench && cargo test` is documented in CLAUDE.md as a separate
   step but per-task gates skipped it. The final whole-PR review caught
   the missed bench test failures, but follow-up will add bench tests
   to `ci.yml`.
2. **A breaking change in cache discipline ripples to every consumer
   that papered over a different limitation.** The bench engine had
   been quietly relying on the 8× elasticity as a substitute for proper
   transaction-overflow handling. Removing the elasticity exposed the
   missing config; the right fix was to give Chisel the spillway
   (production parity), not to keep it disabled and lower other budgets.
3. **No-spill commit cost is 3 fsyncs, not 2.** I28 pre-drain flush +
   main-pages flush + superblock. The spec called it "two-fsync" because
   the spec author was thinking only of the spillway's contribution
   (zero); the actual baseline was already 3. The
   `no_spill_workload_preserves_two_fsync_commit` test now pins to
   `== 3` with documentation of the protocol so a future reader knows
   what each fsync covers.

## Conventions

- Platform: macOS/Linux (uses `flock` via `libc`).
- On-disk format: little-endian, 8KB pages, XXH3 checksums.
- All page reads go through `PageCache`, which validates checksums on load.
- Tests use `tempfile::NamedTempFile` for isolated database files.
- Error types: `ChiselError::InvalidHandle` etc. are operational (database is fine). `ChiselError::IoError`, `ChecksumMismatch`, `CorruptSuperblock` are fatal.

## Commenting standards

Comments should explain choices, tradeoffs, higher-level algorithms,
constraints, and invariants — not restate what the code does. Each file
should have a brief header noting its role in the overall system.
Emphasize non-obvious side effects, ordering dependencies, and
intentional design decisions. The audience is a reader (human or AI)
encountering this code for the first time.

## Design Spec

Living architecture overview in `ARCHITECTURE.md` at the repo root — layer
model, commit protocol, recovery, full on-disk format byte-by-byte. Start
there if you need a current map of the system before changing code.

Decision-time specs (frozen at the time the work was approved) in
`docs/superpowers/specs/`:
- `2026-04-09-chisel-storage-engine-design.md` — storage engine design
- `2026-04-13-chisel-in-memory-mode-design.md` — in-memory mode
- `2026-04-14-chisel-python-interface-design.md` — Python binding API surface
