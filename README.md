# Chisel

A transactional, crash-durable key-value storage engine written in Rust.

Chisel uses **shadow paging** (copy-on-write) to guarantee that the database file is always in a consistent state. There is no write-ahead log and no recovery procedure — after a crash, you just open the file and it's correct.

## Features

- **Crash durability** — dual superblocks with alternating writes ensure committed data survives any single crash. Every page is checksummed (XXH3) for torn-write and bit-rot detection.
- **Transactions** — full begin/commit/rollback with two-phase commit (fsync data, then fsync superblock).
- **Savepoints** — PostgreSQL-style named savepoints with `rollback_to` (preserves the savepoint for retry) and `release` (merges work into the parent transaction).
- **Handles** — store a value, get back a `u64` handle. Read, update, or delete by handle. Handles are stable across updates and defragmentation.
- **Small value packing** — slotted data pages pack multiple values per 8KB page. Values over ~8KB transparently overflow into chained pages.
- **Single-user** — exclusive file locking via `flock`. Designed for embedded use where one process owns the file.
- **Defragmentation** — explicit `defrag()` consolidates sparse pages and can shrink the file.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
chisel = { path = "path/to/chisel" }
```

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
    let data = db.read(handle)?;
    assert_eq!(data, b"hello world");

    // Updates preserve the handle.
    db.begin()?;
    db.update(handle, b"updated value")?;
    db.commit()?;

    // Savepoints let you partially roll back.
    db.begin()?;
    let h1 = db.allocate(b"keep this")?;
    db.savepoint("before_experiment")?;
    let h2 = db.allocate(b"maybe discard")?;
    db.rollback_to("before_experiment")?;  // h2 is gone, h1 is kept
    db.commit()?;

    db.close()
}
```

## API

| Method | Description |
|--------|-------------|
| `Chisel::open(path, options)` | Open or create a database |
| `begin()` | Start a transaction |
| `commit()` | Durably commit the transaction |
| `rollback()` | Discard all changes since `begin()` |
| `savepoint(name)` | Create a named savepoint |
| `rollback_to(name)` | Undo to savepoint (savepoint preserved) |
| `release(name)` | Merge savepoint into parent |
| `allocate(value)` | Store a value, returns a `u64` handle |
| `read(handle)` | Retrieve a value |
| `update(handle, value)` | Replace a value (handle stays the same) |
| `delete(handle)` | Remove a value |
| `handles()` | Iterate all live handles |
| `stats()` | Page count, handle count, file size |
| `defrag(options)` | Consolidate sparse pages |

## How It Works

Chisel divides the database file into 8KB pages. A **handle table** (a radix tree) maps each `u64` handle to a physical location in a **slotted data page**. This indirection means values can move around internally without changing their handle.

Every write copies the affected pages to new locations (copy-on-write). The old pages remain untouched. When you call `commit()`, the engine:

1. Writes all new pages to disk and calls `fsync`
2. Writes a new superblock (with updated root pointers) and calls `fsync`

If the process crashes at any point, the old superblock still points to the old (valid) pages. On the next `open()`, the engine simply picks the superblock with the highest transaction counter and a valid checksum.

## Configuration

```rust
Options {
    cache_size: 1024,        // Pages to cache in memory (default: 8MB)
    create_if_missing: true, // Create the file if it doesn't exist
    read_only: false,        // Open in read-only mode (no transactions)
}
```

## Requirements

- Rust stable (edition 2021)
- macOS or Linux (uses `flock` for file locking)

## License

TBD
