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

PRs 5–8 of the bench-suite series will add output post-processing (5),
scenarios (6), CI integration (7), and cross-engine relative-performance
tests (8 — Chisel vs redb vs SQLite). Design spec at
`docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`
covers PRs 1–7; PR 8 is an addendum tracked here pending its own
spec/plan. Per-PR plans alongside in `docs/superpowers/plans/`.

## Architecture

The module dependency graph is strictly bottom-up — no circular dependencies:

1. `page.rs`, `superblock.rs`, `error.rs` — Pure types, constants, checksums. No I/O.
2. `page_io.rs` — Raw file I/O with `flock`. The only module that touches the filesystem.
3. `page_cache.rs` — LRU cache over `PageIo`. All other modules access pages through this.
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
- **Durability over performance.** Every commit does two fsyncs (data pages, then superblock). Checksums on every page.
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
equivalence tests) have landed on `main` as of 2026-04-30; PRs 4–8
are pending. PR 8 (cross-engine relative-performance tests, Chisel vs
redb vs SQLite) is an addendum to the original design — it will get
its own spec/plan pair when brainstormed. See
`docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md`.

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
