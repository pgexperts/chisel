# Chisel

A transactional, crash-durable key-value storage engine written in Rust. Chisel uses **shadow paging** (copy-on-write) to guarantee that the database file is always in a consistent state. There is no write-ahead log and no recovery procedure — after a crash, you just open the file and it's correct.

Chisel is designed for single-writer embedded use: one process holds the file via `flock`, all mutations go through `&mut self`, and the API is synchronous. A PyO3 binding ships alongside the Rust crate; see [`python/README.md`](python/README.md).

## Status

Pre-1.0. Current release: `0.1.0`. The API is stable-by-intent but subject to revision until 1.0 ships. The on-disk format is likewise pre-stable; see [On-disk format compatibility](#on-disk-format-compatibility) for the 1.0-and-onward promise.

## Features

- **Crash durability** — N configurable superblocks (2–16) with round-robin writes ensure committed data survives crashes. Every page carries an XXH3 checksum for torn-write and bit-rot detection.
- **Transactions** — begin / commit / rollback with two-phase durability (fsync data pages, then fsync superblock).
- **Savepoints** — PostgreSQL-style named savepoints with `rollback_to` (savepoint preserved for retry) and `release` (merges into the enclosing scope).
- **Handles** — store a value, get back a `u64` handle. Read, update, or delete by handle. Handles are stable across updates, defrag, and reopens.
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
| `read(handle)` | Retrieve a value (takes `&self`) |
| `update(handle, value)` | Replace a value (handle preserved) |
| `delete(handle)` | Remove a handle |
| `delete_many(handles)` | Batch-delete in the current transaction |
| `set_root_name(name, handle)` | Bind a name to a handle in the named-root table |
| `get_root_name(name)` | Look up a named root (takes `&self`) |
| `clear_root_name(name)` | Remove a named root |
| `handles()` | Enumerate all live handles (takes `&self`) |
| `stats()` | Handle count, page count, file size |
| `defrag(options)` | Consolidate sparse pages |

## Options

```rust
use chisel::Options;

let options = Options {
    cache_size: 1024,        // pages in the LRU, 8 KB each → 8 MB default
    create_if_missing: true,
    read_only: false,
    superblock_count: 2,     // 2..=16; only consulted on create
};
```

`cache_size` is a count of pages, not bytes. The cache is a soft limit: a single transaction can grow past it while dirty pages pin eviction. A hard ceiling of `cache_size × 8` protects against runaway growth by returning `CacheFull` (operational; caller recovers via commit or rollback).

`read_only = true` still acquires an exclusive `flock` — it only suppresses writes at the application layer. Two read-only opens cannot coexist on the same file. This is a deliberate choice: even a reader must block concurrent writers to keep the shadow-paging invariants intact.

`superblock_count` is set at create time and stored on disk; reopening discovers it from the winning superblock. Higher N increases durability against consecutive torn writes at the cost of N × 8 KB of file space: N = 3 survives one torn commit plus a torn retry, N = 4 survives two retries.

## Error handling

`ChiselError` splits into two conceptual tiers.

**Operational errors** — the database is healthy; the caller made a mistake. Catch and continue.

`InvalidHandle`, `NoActiveTransaction`, `TransactionAlreadyActive`, `SavepointNotFound`, `DuplicateSavepoint`, `ReadOnlyMode`, `FileNotFound`, `InvalidRootName`, `RootNameTableFull`, `InvalidSuperblockCount`, `CacheFull`.

**Fatal errors** — storage integrity is in question. Drop the handle and reopen.

`IoError`, `ChecksumMismatch`, `CorruptSuperblock`, `FileSizeMismatch`, `InvalidMagic`, `LockFailed`, `UnsupportedFormatVersion`, `CorruptPage`, `InvalidPageId`, `Poisoned`.

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

**File level** — each superblock carries a packed `format_version` u32: upper 16 bits = MAJOR, lower 16 bits = MINOR. The open-time gate compares MAJOR only. A 1.3 binary opens a 1.7 file cleanly, but a 1.3 binary rejects a 2.0 file. Minor bumps within a major are reserved for additive changes, so older binaries can safely *read* newer-minor files.

**Page level** — each non-superblock page carries a one-byte `page_format_version` in its header, letting individual page layouts evolve within a major without a file-wide format bump. The post-1.0 upgrade story is lazy migration: on read, the page-type module dispatches on its page's declared version; on write, it always produces the current version; cold pages stay in the old layout until an opt-in `db.upgrade()` sweep rewrites them. An additional 8 bytes are reserved in every non-superblock page header for future common-header fields.

Write safety across minors is a narrower guarantee: a binary at MINOR = *m* opening a file at MINOR = *m' > m* cannot safely commit without risking overwriting fields it doesn't know about. Starting with the first minor bump after 1.0, the open path will refuse writes in that direction (read-only on the newer-minor file); until then the check is a no-op because no minor variants exist. The post-1.0 cross-minor read-compatibility guarantee is absolute; write-compatibility requires binary MINOR ≥ file MINOR.

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

Rust stable, edition 2021.

## Python binding

A PyO3 wrapper lives in the `python/` subdirectory and is published to PyPI as `chisel`:

```bash
pip install chisel
```

The Python API mirrors the Rust one but adds context managers for transactions and savepoints. See [`python/README.md`](python/README.md).

## Design documents

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — living architecture overview: layer model, commit protocol, recovery, full on-disk format byte-by-byte, and cross-cutting concepts. Start here if you're reading the codebase for the first time.

Deeper notes frozen at decision time in [`docs/superpowers/specs/`](docs/superpowers/specs/):

- [`2026-04-09-chisel-storage-engine-design.md`](docs/superpowers/specs/2026-04-09-chisel-storage-engine-design.md) — shadow paging, handle table, page layouts.
- [`2026-04-13-chisel-in-memory-mode-design.md`](docs/superpowers/specs/2026-04-13-chisel-in-memory-mode-design.md) — the `Vec<u8>`-backed `PageIo`.
- [`2026-04-14-chisel-python-interface-design.md`](docs/superpowers/specs/2026-04-14-chisel-python-interface-design.md) — the PyO3 API surface and error hierarchy.

Open issues, closed issues, and the decision log live in [`ISSUES.md`](ISSUES.md).

## License

TBD
