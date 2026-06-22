# Chisel

A transactional, crash-durable key-value storage engine written in Rust. Chisel uses **shadow paging** (copy-on-write) to guarantee that the database file is always in a consistent state. There is no write-ahead log and no recovery procedure — after a crash, you just open the file and it's correct.

Chisel is designed for single-writer embedded use: one process holds the file via `flock`, all mutations go through `&mut self`, and the API is synchronous. A PyO3 binding ships alongside the Rust crate; see [`python/README.md`](python/README.md).

## Status

Pre-1.0. Current release: `0.1.0`. The API is stable-by-intent but subject to revision until 1.0 ships. The on-disk format is likewise pre-stable; see [On-disk format compatibility](#on-disk-format-compatibility) for the 1.0-and-onward promise.

## Features

- **Crash durability** — N configurable superblocks (2–16) with round-robin writes ensure committed data survives crashes. Every page carries an XXH3 checksum for torn-write and bit-rot detection.
- **Transactions** — begin / commit / rollback with shadow-paging durability (all data pages fsync'd before the superblock fsync; three fsyncs per commit).
- **Savepoints** — PostgreSQL-style named savepoints with `rollback_to` (savepoint preserved for retry) and `release` (merges into the enclosing scope).
- **Handles** — store a value, get back a `u64` handle. Read, update, or delete by handle. Handles are stable across updates, defrag, and reopens.
- **Chunk tags** — attach an immutable `u32` tag to a chunk at allocation time; a reverse membership index maps each tag back to its handles. Enumerate a tag's members, delete with a tag-match assertion, or bulk-drop a whole tagged relation in bounded passes. Tag `0` means untagged.
- **Value packing** — slotted data pages pack multiple small values per 8 KB page; values over ~8 KB transparently overflow into chained pages.
- **Named roots** — a small fixed table in the superblock mapping string names to handles. Survives commit / rollback transactionally.
- **Defragmentation** — explicit `defrag()` consolidates sparse pages and returns a count-based stats record.
- **In-memory mode** — same engine, `Vec<u8>`-backed I/O, no file and no lock. For tests, benchmarks, and ephemeral work.
- **Poison model** — any fatal error (I/O failure, checksum mismatch, commit-protocol failure) poisons the handle; recovery is drop-and-reopen. Mirrors `std::sync::Mutex` poisoning.
- **Single-writer** — exclusive `flock` at the filesystem level; `&mut self` on every mutating method.

## Install

Add to your `Cargo.toml`:

```toml
[dependencies]
chisel = "0.1"
```

(While Chisel is pre-1.0 and not yet on crates.io, use a path or git dependency: `chisel = { path = "path/to/chisel" }`.)

## Quick Start

```rust
use chisel::{Chisel, Options};
use std::path::Path;

fn main() -> chisel::Result<()> {
    let mut db = Chisel::open(Path::new("my.db"), Options::default())?;

    // All mutations happen inside a transaction.
    db.begin()?;
    let handle = db.allocate(b"hello world")?;
    db.commit()?;

    // Reads work inside or outside a transaction.
    assert_eq!(db.read(handle)?, b"hello world");

    // Updates preserve the handle.
    db.begin()?;
    db.update(handle, b"updated value")?;
    db.commit()?;

    // Savepoints let you partially roll back within a transaction.
    db.begin()?;
    let keep = db.allocate(b"keep this")?;
    db.savepoint("before_experiment")?;
    let _discard = db.allocate(b"maybe discard")?;
    db.rollback_to("before_experiment")?;  // discard is gone; keep stays
    db.commit()?;

    db.close()
}
```

## Building from source

Cargo workspace with three members: the root `chisel` engine, the `python/` PyO3 binding, and the `bench/` benchmark suite. They share a single `Cargo.lock` and `target/` at the repo root. `default-members` is `[".", "bench"]`, so `cargo build` / `cargo test` from the workspace root exercise the engine and the bench harness together — the Python crate is workspace-resident (for Cargo.lock unification) but excluded from default builds because PyO3's `extension-module` feature needs maturin to set up linker flags. To build/install the Python binding, use `cd python && maturin develop --release`.

### Root crate

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

CI runs the Rust checks above plus a Python matrix (CPython 3.11 and 3.13 × Linux/macOS) that builds the PyO3 binding via `maturin develop` and runs `pytest` in `python/tests`. A separate workflow builds abi3 wheels on tagged releases.

### Python binding

The `python/` subcrate is a PyO3 wrapper (`chisel-py` → `_chisel.abi3.so`) with its own `Cargo.toml` and `pyproject.toml`:

```bash
cd python
python -m venv .venv && source .venv/bin/activate
pip install maturin pytest
maturin develop
pytest
```

See [`python/README.md`](python/README.md) for usage.

### Benchmark suite

The `bench/` subcrate provides Criterion micro-grid + YCSB-style scenario benchmarks comparing Chisel against redb and SQLite:

```bash
cd bench
cargo test                              # equivalence tests (5 scenarios × 3 engines)
cargo bench --bench micro_grid          # ~1-3 min on Linux, longer on macOS APFS
cargo bench --bench scenarios           # ~10-25 min on Linux, ~70-90 min on macOS APFS
cargo run --bin summarize               # produces summary.md, results.json, cross-engine.md
                                        # under bench/results/<UTC>/
```

The macOS APFS runtime gap is real: Chisel uses `fcntl(F_FULLFSYNC)` (durable through the disk write cache) and `SqliteEngine` matches it via `PRAGMA fullfsync=ON`. Linux `fsync()` already syncs through the cache, so the costs are comparable there.

## Concepts

### Handles

A handle is a stable `u64` returned by `allocate()`. It maps through a radix-tree **handle table** rooted in the superblock to a `(page, slot)` location in a slotted data page. This indirection means values can move internally — during `update()` to a larger value, or during `defrag()` — without changing the handle. Deleted handles are retired and never reused within a database's lifetime.

### Transactions

All mutations require an active transaction. `begin()` opens one, `commit()` makes it durable, `rollback()` discards it. Only one transaction is active at a time — Chisel has savepoints, not nested transactions.

```rust
db.begin()?;
let h1 = db.allocate(b"a")?;
let h2 = db.allocate(b"b")?;
db.commit()?;  // both h1 and h2 become durable atomically
```

Rollback is effectively free: pages written during the transaction were never linked from a superblock, so they are simply abandoned. There is no undo log to replay.

### Savepoints

Savepoints are named marks within a transaction.

- `rollback_to(name)` undoes changes back to the savepoint but keeps the savepoint on the stack so you can try again.
- `release(name)` flattens the savepoint into the enclosing scope; any savepoints layered on top of it are also released.
- `rollback()` (full rollback) and `commit()` both clear the entire savepoint stack.

```rust
db.begin()?;
let keep = db.allocate(b"keep")?;
db.savepoint("experiment")?;
let _ = db.allocate(b"maybe discard")?;
db.rollback_to("experiment")?;  // discards the _ handle; keep remains; sp still open
db.release("experiment")?;
db.commit()?;
```

### Named roots

A small fixed-size table in the superblock mapping short string names to handles, intended for long-lived entry points such as a meta-B-tree root. Changes are transactional: `set_root_name` takes effect on commit and reverts on rollback.

```rust
db.begin()?;
let meta = db.allocate(b"meta-root-payload")?;
db.set_root_name("meta", meta)?;
db.commit()?;

// Later, possibly after reopen:
let meta = db.get_root_name("meta")?.expect("meta root should be set");
```

Names are bounded in length and must be valid UTF-8 without embedded NUL; the table has a small fixed capacity. See `TransactionManager::set_root_name` for exact limits.

### Defragmentation

`defrag()` consolidates sparse data pages: it re-inserts values from pages whose live-slot count falls below a threshold so those pages become fully free and can be reclaimed. It runs inside an active transaction so it composes with other work and commits atomically.

```rust
use chisel::defrag::DefragOptions;

db.begin()?;
let stats = db.defrag(DefragOptions {
    sparse_threshold: 0.25,
    max_pages: 0,  // 0 = no cap on values relocated
})?;
db.commit()?;
```

### Chunk tags

`allocate_tagged(value, tag)` attaches an immutable `u32` tag to a chunk and registers it in a reverse membership index. The tag is fixed at allocation: `update` preserves it, and retagging means delete + re-allocate. Tag `0` is the untagged sentinel and is never indexed — use plain `allocate` for untagged values. Plain `delete` is self-maintaining (it drops the handle from the index automatically); `delete_tagged` is the verified variant that errors with `TagMismatch` if the stored tag differs.

```rust
db.begin()?;
let a = db.allocate_tagged(b"row-a", 42)?;
let _b = db.allocate_tagged(b"row-b", 42)?;
db.commit()?;

assert_eq!(db.tag(a)?, 42);
let members = db.handles_with_tag(42)?; // both handles; order unspecified, but repeatable within a session
assert_eq!(members.len(), 2);

// Bounded relation-drop: loop until the tag is fully drained.
loop {
    db.begin()?;
    let progress = db.delete_with_tag(42, 256)?; // up to 256 chunks per pass
    db.commit()?;
    if progress.complete {
        break;
    }
}
```

### In-memory mode

`Chisel::open_in_memory()` creates a memory-backed database using a `Vec<u8>`-backed `PageIo`. Same code path, same guarantees except durability — no filesystem, no `flock`, and all data is lost on drop.

```rust
let mut db = Chisel::open_in_memory()?;
// ... same API as a file-backed Chisel ...
```

For tuned options (cache size, superblock count), use `Chisel::open_in_memory_with_options(options)`.

## API reference

| Method | Purpose |
|---|---|
| `Chisel::open(path, options)` | Open or create a database file |
| `Chisel::open_in_memory()` | Open a memory-backed database with default options |
| `Chisel::open_in_memory_with_options(options)` | In-memory with explicit options |
| `close()` | Explicit close (returns `Result`); equivalent to drop |
| `is_poisoned()` | True if a fatal error has occurred |
| `begin()` | Start a transaction |
| `commit()` | Durably commit the transaction |
| `rollback()` | Discard all changes since `begin()` |
| `savepoint(name)` | Create a named savepoint |
| `rollback_to(name)` | Undo to savepoint (savepoint preserved) |
| `release(name)` | Merge savepoint into enclosing scope |
| `allocate(value)` | Store a value; returns a `u64` handle |
| `allocate_tagged(value, tag)` | Store a value with an immutable `u32` tag; returns a `u64` handle |
| `read(handle)` | Retrieve a value (takes `&self`) |
| `update(handle, value)` | Replace a value (handle preserved) |
| `delete(handle)` | Remove a handle |
| `delete_many(handles)` | Batch-delete in the current transaction |
| `tag(handle)` | Read a handle's tag, `0` if untagged (takes `&self`) |
| `handles_with_tag(tag)` | Enumerate live handles carrying `tag`; repeatable within a session, order unspecified (takes `&self`) |
| `client_byte(handle)` | Read the handle's opaque client byte, `0` if unset (takes `&self`) |
| `set_client_byte(handle, byte)` | Set the opaque client byte; mutable, transactional; `update()` preserves it |
| `delete_tagged(handle, tag)` | Delete only if the handle's tag matches; else `TagMismatch` |
| `delete_with_tag(tag, max)` | Bounded relation-drop: delete up to `max` chunks of `tag`; returns `TagDropProgress` |
| `set_root_name(name, handle)` | Bind a name to a handle in the named-root table |
| `get_root_name(name)` | Look up a named root (takes `&self`) |
| `clear_root_name(name)` | Remove a named root |
| `handles()` | Enumerate all live handles; repeatable within a session, order unspecified (takes `&self`) |
| `stats()` | Handle count, page count, file size (takes `&self`) |
| `counters()` | Engine-activity counters: cache hits/misses, fsync calls, pages allocated (takes `&self`) |
| `defrag(options)` | Consolidate sparse pages |

## Options

```rust
use chisel::Options;

let options = Options {
    cache_max_bytes: 8 * 1024 * 1024,           // in-memory LRU cap, default 8 MiB
    spillway_max_bytes: 1024 * 8 * 1024 * 1024, // sidecar overflow file cap, default 8 GiB
    drain_insertion: chisel::DrainInsertion::LruTail, // default; use Mru to drain at insertion
    create_if_missing: true,
    read_only: false,
    superblock_count: 2,                         // 2..=16; only consulted on create
};
```

`cache_max_bytes` is a strict cap on the in-memory LRU cache. When the cache is full and a dirty page cannot be evicted, overflow dirty pages spill to a sidecar `Spillway` file rather than returning an error. The spillway file is bounded by `spillway_max_bytes` (default 8 GiB). Setting `spillway_max_bytes = 0` disables the spillway entirely, restoring the pre-spillway `CacheFull` semantics at the strict cache cap: the operational error `CacheFull` fires when the cache is full and no eviction is possible. With the spillway enabled, exhausting both the cache and the spillway returns `SpillwayFull { limit_bytes }` (also operational; caller recovers by committing or rolling back).

`read_only = true` still acquires an exclusive `flock` — it only suppresses writes at the application layer. Two read-only opens cannot coexist on the same file. This is a deliberate choice: even a reader must block concurrent writers to keep the shadow-paging invariants intact.

`superblock_count` is set at create time and stored on disk; reopening discovers it from the winning superblock. Higher N increases durability against consecutive torn writes at the cost of N × 8 KB of file space: N = 3 survives one torn commit plus a torn retry, N = 4 survives two retries.

## Error handling

`ChiselError` splits into two conceptual tiers.

**Operational errors** — the database is healthy; the caller made a mistake. Catch and continue.

`InvalidHandle`, `TagMismatch`, `NoActiveTransaction`, `TransactionAlreadyActive`, `TransactionInProgress`, `SavepointNotFound`, `DuplicateSavepoint`, `ReadOnlyMode`, `FileNotFound`, `InvalidRootName`, `RootNameTableFull`, `InvalidSuperblockCount`, `CacheFull`, `SpillwayFull`.

**Fatal errors** — storage integrity is in question. Drop the handle and reopen.

`IoError`, `ChecksumMismatch`, `CorruptSuperblock`, `FileSizeMismatch`, `LockFailed`, `UnsupportedFormatVersion`, `CorruptPage`, `InvalidPageId`, `Poisoned`.

Use `ChiselError::is_fatal()` to classify at runtime.

### Poison model

On any fatal error — including a failed commit-protocol fsync — the `Chisel` handle becomes **poisoned**. Every subsequent call returns `ChiselError::Poisoned`, regardless of whether it is a read or a write. The only legal recovery is to drop the handle and call `Chisel::open` again; the shadow-paging recovery path then restores the database to the last durable state.

```rust
match db.commit() {
    Ok(()) => (),
    Err(e) if e.is_fatal() => {
        drop(db);
        db = Chisel::open(path, Options::default())?;
        // Chisel is now at its last-committed state; retry the work if needed.
    }
    Err(e) => return Err(e),  // operational — handle per your caller's policy
}
```

The poison model is mandatory because Linux `fsync` semantics (post-2018 "fsyncgate") do not permit safely retrying a failed fsync: the kernel may have discarded the dirty pages before reporting the error. macOS `F_FULLFSYNC` has similar semantics. PostgreSQL `PANIC`s on fsync failure for exactly this reason.

## On-disk format compatibility

**Within a given major version, the on-disk format is sacred.** Any file written by any release with major version *N* will be readable by any other release with major version *N*, regardless of minor or patch level. A file written by a future, incompatible release fails fast with `UnsupportedFormatVersion` rather than being silently misinterpreted.

### How it's encoded

Versioning is two-tiered.

**File level** — each superblock carries a packed `format_version` u32: upper 16 bits = MAJOR, lower 16 bits = MINOR. The open-time gate compares MAJOR only. A 1.3 binary opens a 1.7 file cleanly, but a 1.3 binary rejects a 2.0 file. Minor bumps within a major are reserved for additive changes, so older binaries can safely *read* newer-minor files. The chunk-tags feature is the first such additive minor bump (MINOR 0 → 1): it adds a per-chunk tag and a membership index, and pre-tag files open cleanly with every chunk untagged.

**Page level** — each non-superblock page carries a one-byte `page_format_version` in its header, letting individual page layouts evolve within a major without a file-wide format bump. The post-1.0 upgrade story is lazy migration: on read, the page-type module dispatches on its page's declared version; on write, it always produces the current version; cold pages stay in the old layout until an opt-in `db.upgrade()` sweep rewrites them. An additional 8 bytes are reserved in every non-superblock page header for future common-header fields.

Write safety across minors is a narrower guarantee: a binary at MINOR = *m* opening a file at MINOR = *m' > m* cannot safely commit without risking overwriting fields it doesn't know about. The open gate is MAJOR-only by design, so minor variants coexist — same-major files of any minor open successfully, and the chunk-tags MINOR = 1 variant is the first such case. The write-refusal arm (refuse writes when file MINOR > binary MINOR, leaving the newer-minor file read-only) is not yet wired up; it lands with the first post-1.0 minor bump that makes the direction observable. The post-1.0 cross-minor read-compatibility guarantee is absolute; write-compatibility requires binary MINOR ≥ file MINOR.

### Pre-1.0 caveat

Until Chisel reaches 1.0, the on-disk format may change between pre-release builds without a major-version bump. Any such pre-1.0 change will be called out in release notes. The first 1.0 release freezes MAJOR at 1 for the entire 1.x line.

Files written by prior development builds (pre-1.0 flat `format_version`, which decodes as MAJOR = 0) are rejected at open time — recreate the database. No production-grade migration is provided for pre-release files.

## How durability works

Chisel divides the database file into 8 KB pages. The superblock(s) at the file's head name the current handle-table root, freemap page, and other per-commit roots. Each commit:

1. Writes all dirty pages (handle-table COW copies, new data pages, new overflow pages, the updated freemap) and calls `fsync`. At this point every page the new superblock will reference is durable on the storage medium.
2. Writes the new superblock to the next slot in the round-robin (`txn_counter % superblock_count`) and calls `fsync`. This is the **linearization point** — before this returns, the transaction is not crash-safe; after it returns, the new state is observable on recovery.

If the process crashes at any point in the protocol, the previously-active superblock still points at a consistent set of pages. On the next `open()`, Chisel runs `Superblock::select` over all slots, picks the one with the highest transaction counter and a valid checksum, and ignores any torn or corrupt slots in favor of their siblings. No log replay, no partial recovery.

Every page carries an XXH3 checksum validated on load; cache hits skip revalidation, relying on the exclusive `flock` to prevent any other process from scribbling on the file.

## Platform support

Chisel runs on macOS and Linux. File locking uses `flock(2)` via `libc`. Windows is not currently supported and would require a different locking primitive.

`flock` is **advisory**, not mandatory. Cooperating processes (any other Chisel instance, or any tool that honours advisory locks) will respect the exclusive lock and block. A tool that bypasses advisory locking — `cp` while a transaction is in flight, some sync utilities, naive backup scripts that read the raw file — can still scribble on or read a torn view of the database. The shadow-paging invariants assume an exclusive owner; respect the lock from outside Chisel as well as inside it.

Rust stable, edition 2021. Minimum supported Rust version (MSRV): 1.82 — a conservative pin above the ~1.74 stdlib floor (the true floor comes from `io::Error::other`, stabilized in 1.74, used in `src/page_io.rs`). A future MSRV bump will appear in `Cargo.toml`'s `rust-version` field and be called out in release notes.

## Python binding

A PyO3 wrapper lives in the `python/` subdirectory and will be published to PyPI as `chisel`; until then, build locally with `maturin develop` (see [`python/README.md`](python/README.md)).

The Python API mirrors the Rust one but adds context managers for transactions and savepoints.

## Design documents

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — living architecture overview: layer model, commit protocol, recovery, full on-disk format byte-by-byte, and cross-cutting concepts. Start here if you're reading the codebase for the first time.
- [`ISSUES.md`](ISSUES.md) — running decision log: open issues, closed issues, and every design tradeoff with date-stamped rationale.

## License

MIT — see [LICENSE](LICENSE) for the full text. The same license applies to the `python/` PyO3 binding and the `bench/` benchmark subcrate.
