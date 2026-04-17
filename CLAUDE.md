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

### Backlog and decision log

`ISSUES.md` is the canonical list of open work, latent bugs, and completed
fixes (each entry marked `✅ IMPLEMENTED <date>` or still open). Consult it
before proposing changes — many obvious-looking simplifications have already
been addressed (R1 value packing, R2 freemap wiring, R3 selective defrag,
R4 configurable superblock count, F3 `read()` taking `&self`, and F2 named
roots all landed 2026-04-10/11; the Python binding for R5 is shipped even
though R5's entry is still formally open).

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

Specs in `docs/superpowers/specs/`:
- `2026-04-09-chisel-storage-engine-design.md` — storage engine design
- `2026-04-13-chisel-in-memory-mode-design.md` — in-memory mode
- `2026-04-14-chisel-python-interface-design.md` — Python binding API surface
