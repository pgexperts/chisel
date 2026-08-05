# Chisel

[![CI](https://github.com/pgexperts/chisel/actions/workflows/ci.yml/badge.svg)](https://github.com/pgexperts/chisel/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/chisel-storage.svg)](https://crates.io/crates/chisel-storage)
[![docs.rs](https://docs.rs/chisel-storage/badge.svg)](https://docs.rs/chisel-storage)
[![PyPI](https://img.shields.io/pypi/v/chisel-storage.svg)](https://pypi.org/project/chisel-storage/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A transactional, crash-durable key-value storage engine written in Rust. Chisel uses **shadow paging** (copy-on-write) to guarantee that the database file is always in a consistent state. There is no write-ahead log and no recovery procedure — after a crash, you just open the file and it's correct.

Chisel is designed for single-writer embedded use: one process holds the file via `flock`, all mutations go through `&mut self`, and the API is synchronous. A PyO3 binding ships alongside the Rust crate; see [`python/README.md`](python/README.md).

## Status

1.0. Current release: `1.0.0`. Both the API and the on-disk format are now frozen per the compatibility promise in [On-disk format compatibility](#on-disk-format-compatibility).

## Features

- **Crash durability** — N configurable superblocks (2–16) with round-robin writes ensure committed data survives crashes. Every page carries an XXH3 checksum for torn-write and bit-rot detection.
- **Transactions** — begin / commit / rollback with shadow-paging durability (all data pages fsync'd before the superblock fsync; three fsyncs per commit).
- **Savepoints** — PostgreSQL-style named savepoints with `rollback_to` (savepoint preserved for retry) and `release` (merges into the enclosing scope).
- **Handles** — store a value, get back a `Handle` (a `u64` newtype). Read, update, or delete by handle. Handles are stable across updates, defrag, and reopens.
- **Chunk tags** — attach an immutable `Tag` (a non-zero `u32` newtype) to a chunk at allocation time; a reverse membership index maps each tag back to its handles. Enumerate a tag's members, delete with a tag-match assertion, or bulk-drop a whole tagged relation in bounded passes. Untagged is the absence of a tag, not a zero value: `tag()` returns `None`.
- **Value packing** — slotted data pages pack multiple small values per 8 KB page; values over ~8 KB transparently overflow into chained pages.
- **Named roots** — a small fixed table in the superblock mapping string names to handles. Survives commit / rollback transactionally.
- **Defragmentation** — explicit `defrag()` consolidates sparse pages and returns a count-based stats record.
- **In-memory mode** — same engine, `Vec<u8>`-backed I/O, no file and no lock. For tests, benchmarks, and ephemeral work.
- **At-rest encryption** — optional, off by default. Every page is sealed with XChaCha20-Poly1305 AEAD under a client-supplied key (raw 32-byte or Argon2id passphrase). An 8-slot envelope table wraps the data-encryption key, so credential rotation is O(1) — no bulk re-encryption.
- **Poison model** — any fatal error (I/O failure, checksum mismatch, commit-protocol failure) poisons the handle; recovery is drop-and-reopen. Mirrors `std::sync::Mutex` poisoning.
- **Single-writer** — exclusive `flock` at the filesystem level; `&mut self` on every mutating method.

## Install

Add to your `Cargo.toml`:

```toml
[dependencies]
chisel-storage = "1.0"
```

The published crate is named `chisel-storage` (plain `chisel` was already taken); the library itself is still `chisel`, so your code imports it as `use chisel::{Chisel, Options};` regardless.

(Not yet published to crates.io; use a path or git dependency in the meantime: `chisel-storage = { path = "path/to/chisel" }`.)

## Quick Start

```rust,no_run
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
cargo test                                # engine + bench (default-members); do NOT use --lib
cargo clippy --workspace -- -D warnings   # --workspace also lints the python/ binding
cargo fmt -- --check
```

`cargo test` from the workspace root covers the engine and the bench crate (`default-members = [".", "bench"]`). Do **not** use `cargo test --lib` — it skips the `tests/` integration suite. MSRV is verified with a library-only build under the pinned toolchain: `cargo +1.82 build -p chisel`.

CI runs the Rust checks above plus a Python matrix (CPython 3.11 and 3.13 × Linux/macOS) that builds the PyO3 binding via `maturin develop --release` and runs `pytest -v` in `python/tests`. A separate workflow builds abi3 wheels on tagged releases.

### Python binding

The `python/` subcrate is a PyO3 wrapper (`chisel-py` → `_chisel.abi3.so`) with its own `Cargo.toml` and `pyproject.toml`:

```bash
cd python
python -m venv .venv && source .venv/bin/activate
pip install maturin pytest hypothesis   # the `test` extra requires pytest>=8 and hypothesis>=6
maturin develop --release
pytest -v
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

A handle is a stable `u64` returned by `allocate()`. It maps through a radix-tree **handle table** rooted in the superblock to a `(page, slot)` location in a slotted data page. This indirection means values can move internally — during `update()` to a larger value, or during `defrag()` — without changing the handle. A handle that reached a commit is retired on delete and never reused. One minted by a transaction that rolls back (or is lost to crash recovery) *is* re-minted, because the counter rewinds with the rest of the roots snapshot — so commit before recording a handle outside the database.

### Transactions

All mutations require an active transaction. `begin()` opens one, `commit()` makes it durable, `rollback()` discards it. Only one transaction is active at a time — Chisel has savepoints, not nested transactions.

```rust
# fn main() -> chisel::Result<()> {
# use chisel::Chisel;
# let mut db = Chisel::open_in_memory()?;
db.begin()?;
let h1 = db.allocate(b"a")?;
let h2 = db.allocate(b"b")?;
db.commit()?;  // both h1 and h2 become durable atomically
# Ok(())
# }
```

Rollback is effectively free: pages written during the transaction were never linked from a superblock, so they are simply abandoned. There is no undo log to replay.

### Savepoints

Savepoints are named marks within a transaction.

- `rollback_to(name)` undoes changes back to the savepoint but keeps the savepoint on the stack so you can try again.
- `release(name)` flattens the savepoint into the enclosing scope; any savepoints layered on top of it are also released.
- `rollback()` (full rollback) and `commit()` both clear the entire savepoint stack.

```rust
# fn main() -> chisel::Result<()> {
# use chisel::Chisel;
# let mut db = Chisel::open_in_memory()?;
db.begin()?;
let keep = db.allocate(b"keep")?;
db.savepoint("experiment")?;
let _ = db.allocate(b"maybe discard")?;
db.rollback_to("experiment")?;  // discards the _ handle; keep remains; sp still open
db.release("experiment")?;
db.commit()?;
# Ok(())
# }
```

### Named roots

A small fixed-size table in the superblock mapping short string names to handles, intended for long-lived entry points such as a meta-B-tree root. Changes are transactional: `set_root_name` takes effect on commit and reverts on rollback.

```rust
# fn main() -> chisel::Result<()> {
# use chisel::Chisel;
# let mut db = Chisel::open_in_memory()?;
db.begin()?;
let meta = db.allocate(b"meta-root-payload")?;
db.set_root_name("meta", meta)?;
db.commit()?;

// Later, possibly after reopen:
let meta = db.get_root_name("meta")?.expect("meta root should be set");
# Ok(())
# }
```

Names are bounded in length and must be valid UTF-8 without embedded NUL; the table has a small fixed capacity. See `TransactionManager::set_root_name` for exact limits.

### Defragmentation

`defrag()` consolidates sparse data pages: it re-inserts values from pages whose live-slot count falls below a threshold so those pages become fully free and can be reclaimed. It runs inside an active transaction so it composes with other work and commits atomically.

```rust
# fn main() -> chisel::Result<()> {
# use chisel::Chisel;
# let mut db = Chisel::open_in_memory()?;
use chisel::DefragOptions;

db.begin()?;
// DefragOptions is #[non_exhaustive]; build it with the chained setters
// rather than a struct literal.
let stats = db.defrag(
    DefragOptions::default()
        .sparse_threshold(0.25)  // relocate pages under 25% full, per page
        .max_values(0),          // 0 = no cap on values relocated
)?;
db.commit()?;
# Ok(())
# }
```

### Chunk tags

`allocate_tagged(value, tag)` attaches an immutable tag to a chunk and registers it in a reverse membership index. The tag is fixed at allocation: `update` preserves it, and retagging means delete + re-allocate. Plain `delete` is self-maintaining (it drops the handle from the index automatically); `delete_tagged` is the verified variant that errors with `TagMismatch` if the stored tag differs.

A tag is a `Tag`, not a bare `u32` — `Tag` wraps a `NonZeroU32`, so zero is unrepresentable rather than being an "untagged" sentinel you could pass by accident. Untagged means *no tag at all*: allocate with plain `allocate()`, and `tag()` returns `None` for such a handle.

```rust
# fn main() -> chisel::Result<()> {
# use chisel::Chisel;
# let mut db = Chisel::open_in_memory()?;
use chisel::Tag;

let tag = Tag::new(42).expect("42 is non-zero");   // or Tag::try_from(42u32)?

db.begin()?;
let a = db.allocate_tagged(b"row-a", tag)?;
let _b = db.allocate_tagged(b"row-b", tag)?;
db.commit()?;

assert_eq!(db.tag(a)?, Some(tag));      // None for a handle allocated untagged
let members = db.handles_with_tag(tag)?; // both handles; order unspecified, but repeatable within a session
assert_eq!(members.len(), 2);

// Bounded relation-drop: loop until the tag is fully drained.
loop {
    db.begin()?;
    let progress = db.delete_with_tag(tag, 256)?; // up to 256 chunks per pass
    db.commit()?;
    if progress.complete {
        break;
    }
}
# Ok(())
# }
```

### In-memory mode

`Chisel::open_in_memory()` creates a memory-backed database using a `Vec<u8>`-backed `PageIo`. Same code path, same guarantees except durability — no filesystem, no `flock`, and all data is lost on drop.

```rust
# fn main() -> chisel::Result<()> {
# use chisel::Chisel;
let mut db = Chisel::open_in_memory()?;
// ... same API as a file-backed Chisel ...
# Ok(())
# }
```

For tuned options (cache size, superblock count), use `Chisel::open_in_memory_with_options(options)`.

### Encryption

Encryption is opt-in and driven entirely through `Options::encryption_key`. A key is supplied as a `chisel::Key`:

- `Key::Raw(bytes)` — high-entropy key material (32 bytes), stretched to a key-encryption key via HKDF-SHA256.
- `Key::Passphrase(string)` — a human secret, stretched via Argon2id (memory-hard). Cost comes from `Options::argon2_params` on create, or the stored slot's params on reopen.

Both variants zeroize their material on drop.

```rust,no_run
# fn main() -> chisel::Result<()> {
use chisel::{Chisel, Key, Options};
use std::path::Path;
use zeroize::Zeroizing;

// Create (or reopen) an encrypted database.
let key = Key::Passphrase(Zeroizing::new("correct horse battery staple".into()));
let mut db = Chisel::open(
    Path::new("secret.db"),
    Options::default().encryption_key(key),
)?;
# Ok(())
# }
```

On create, Chisel generates a random data-encryption key (DEK), encrypts every page under it with XChaCha20-Poly1305, and wraps the DEK under a key-encryption key derived from your supplied key. On reopen, the supplied key must unwrap one of the on-disk key slots or `open` fails with `InvalidEncryptionKey`. Supplying a key to a plaintext DB returns `EncryptionNotSupported`; omitting it on an encrypted DB returns `NoEncryptionKey`.

The wrapped DEK lives in an **8-slot key table**. Because the DEK itself never changes, credential rotation only re-wraps the DEK in a slot — it is O(1), independent of database size. `add_key` stages a second credential (both open the DB), `rotate_key` replaces one credential in place, and `remove_key` retires one (refusing the last remaining slot with `LastKeySlot`). A full table returns `NoFreeKeySlot`.

See [ARCHITECTURE.md#on-disk-encryption](ARCHITECTURE.md#on-disk-encryption) for the on-disk layout (crypto header, key slots, per-page nonce stride) and [THEORY.md](THEORY.md) for the rationale behind the envelope scheme and the shadow-paging nonce discipline (with [`docs/adr/`](docs/adr/) as the dated decision log).

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
| `allocate(value)` | Store a value; returns a `Handle` |
| `allocate_tagged(value, tag)` | Store a value with an immutable `Tag` (non-zero); returns a `Handle` |
| `read(handle)` | Retrieve a value (takes `&self`) |
| `update(handle, value)` | Replace a value (handle preserved) |
| `delete(handle)` | Remove a handle |
| `delete_many(handles)` | Batch-delete in the current transaction |
| `tag(handle)` | Read a handle's tag as `Option<Tag>`; `None` if untagged (takes `&self`) |
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
| `file_size_bytes()` | Physical size of the database file in bytes (takes `&self`) |
| `defrag(options)` | Consolidate sparse pages |
| `add_key(existing, new)` | Stage a second credential; both `existing` and `new` then open the DB. `Result<()>` |
| `rotate_key(old, new)` | Replace credential `old` with `new` in place. `Result<()>` |
| `remove_key(key)` | Retire credential `key`; `LastKeySlot` if it is the only one. `Result<()>` |

## Options

`Options` is `#[non_exhaustive]`, so a downstream crate cannot build one with a struct literal. Start from `Options::default()` and use the chained setters — every field has one:

```rust
# fn main() {
use chisel::{DrainInsertion, Options};

let options = Options::default()
    .cache_max_bytes(8 * 1024 * 1024)             // in-memory LRU cap, default 8 MiB
    .spillway_max_bytes(1024 * 8 * 1024 * 1024)   // sidecar overflow file cap, default 8 GiB
    .drain_insertion(DrainInsertion::LruTail)     // default; use Mru to drain at insertion
    .create_if_missing(true)
    .read_only(false)
    .superblock_count(2);                         // 2..=16; only consulted on create
// .encryption_key(key) to create/open an encrypted DB;
// .argon2_params(params) to override the OWASP defaults (create only).
# }
```

`cache_max_bytes` is a strict cap on the in-memory LRU cache. When the cache is full and a dirty page cannot be evicted, overflow dirty pages spill to a sidecar `Spillway` file rather than returning an error. The spillway file is bounded by `spillway_max_bytes` (default 8 GiB). Setting `spillway_max_bytes = 0` disables the spillway entirely, restoring the pre-spillway `CacheFull` semantics at the strict cache cap: the operational error `CacheFull` fires when the cache is full and no eviction is possible. With the spillway enabled, exhausting both the cache and the spillway returns `SpillwayFull { limit_bytes }` (also operational; caller recovers by committing or rolling back).

`read_only = true` still acquires an exclusive `flock` — it only suppresses writes at the application layer. Two read-only opens cannot coexist on the same file. This is a deliberate choice: even a reader must block concurrent writers to keep the shadow-paging invariants intact.

`superblock_count` is set at create time and stored on disk; reopening discovers it from the winning superblock. Higher N increases durability against consecutive torn writes at the cost of N × 8 KB of file space: N = 3 survives one torn commit plus a torn retry, N = 4 survives two retries.

`encryption_key` defaults to `None` (plaintext). `Some(key)` creates a new encrypted database (sealing a fresh random data-encryption key under the supplied key) or reopens one (unwrapping the stored key). `argon2_params` defaults to `None`, which uses the OWASP-recommended Argon2id cost (19 MiB / t=2 / p=1) when deriving a key from a `Key::Passphrase` on create; it is ignored for raw keys and on reopen (the stored slot carries its own params). See the [Encryption](#encryption) concept below.

## Error handling

`ChiselError` splits into two conceptual tiers.

**Operational errors** — the database is healthy; the caller made a mistake. Catch and continue.

`InvalidHandle`, `TagMismatch`, `NoActiveTransaction`, `TransactionAlreadyActive`, `TransactionInProgress`, `SavepointNotFound`, `DuplicateSavepoint`, `ReadOnlyMode`, `FileNotFound`, `InvalidRootName`, `RootNameTableFull`, `InvalidSuperblockCount`, `CacheFull`, `SpillwayFull`, `NoEncryptionKey`, `InvalidEncryptionKey`, `EncryptionNotSupported`, `NoFreeKeySlot`, `LastKeySlot`.

**Fatal errors** — storage integrity is in question. Drop the handle and reopen.

`IoError`, `ChecksumMismatch`, `CorruptSuperblock`, `FileSizeMismatch`, `LockFailed`, `UnsupportedFormatVersion`, `UnsupportedPageSize`, `InvalidFreemapDepth`, `CorruptPage`, `InvalidPageId`, `DecryptionFailed`.

`DecryptionFailed { page_id }` is fatal: an AEAD authentication failure while decrypting an already-read page means the ciphertext or session key can no longer be trusted, so it poisons the handle exactly like `ChecksumMismatch` (see the poison model below). It is distinct from the operational `InvalidEncryptionKey`, which fires at open time when the supplied key unwraps no key slot — before any data page is served.

Use `ChiselError::is_fatal()` to classify at runtime.

**`Poisoned` is in neither tier**, and `is_fatal()` returns `false` for it. That is deliberate: `is_fatal()` answers "should this error poison the handle?", and by the time you see `Poisoned` the handle already is — re-poisoning is meaningless. But it also means `Poisoned` is *not* recoverable-and-continue either: it is terminal, and the only response is the same drop-and-reopen a fatal error calls for. Match it explicitly alongside `is_fatal()`, as the snippet below does; routing it into an "operational, keep going" arm produces a loop where every call returns `Poisoned` forever.

### Poison model

On any fatal error — including a failed commit-protocol fsync — the `Chisel` handle becomes **poisoned**. Every subsequent call returns `ChiselError::Poisoned`, regardless of whether it is a read or a write. The only legal recovery is to drop the handle and call `Chisel::open` again; the shadow-paging recovery path then restores the database to the last durable state.

`commit` is the one method where the operational/fatal split above does not apply. Its only non-poisoning error is `NoActiveTransaction`, checked before the protocol starts. Once `cache.flush()` has run the manager is in a partial-commit state, so *every* error it can then return poisons the handle — including `CacheFull` and `SpillwayFull`, which are operational anywhere else. Do not catch those from `commit` and call `rollback` to recover: the handle is already poisoned and `rollback` will tell you so.

```rust,no_run
# fn main() -> chisel::Result<()> {
# use chisel::{Chisel, ChiselError, Options};
# let path = std::path::Path::new("db.chisel");
# let mut db = Chisel::open(path, Options::default())?;
match db.commit() {
    Ok(()) => (),
    // `Poisoned` is matched explicitly: is_fatal() is false for it, so it
    // would otherwise fall into the operational arm and loop forever.
    Err(e) if e.is_fatal() || matches!(e, ChiselError::Poisoned) => {
        drop(db);
        db = Chisel::open(path, Options::default())?;
        // Chisel is now at its last-committed state; retry the work if needed.
    }
    Err(e) => return Err(e),  // operational — handle per your caller's policy
}
# Ok(())
# }
```

The poison model is mandatory because Linux `fsync` semantics (post-2018 "fsyncgate") do not permit safely retrying a failed fsync: the kernel may have discarded the dirty pages before reporting the error. macOS `F_FULLFSYNC` has similar semantics. PostgreSQL `PANIC`s on fsync failure for exactly this reason.

## On-disk format compatibility

**Within a given major version, the on-disk format is sacred.** Any file written by any release with major version *N* will be readable by any other release with major version *N*, regardless of minor or patch level. A file written by a future, incompatible release fails fast with `UnsupportedFormatVersion` rather than being silently misinterpreted.

### How it's encoded

Versioning is two-tiered.

**File level** — each superblock carries a packed `format_version` u32: upper 16 bits = MAJOR, lower 16 bits = MINOR. The open-time gate compares MAJOR only. A 1.3 binary opens a 1.7 file cleanly, but a 1.3 binary rejects a 2.0 file. Minor bumps within a major are reserved for additive changes, so older binaries can safely *read* newer-minor files. The chunk-tags feature is the first such additive minor bump (MINOR 0 → 1): it adds a per-chunk tag and a membership index, and pre-tag files open cleanly with every chunk untagged.

Encryption introduces the second major: an encrypted database is stamped MAJOR = 2 (`FORMAT_MAJOR_VERSION_ENCRYPTED`), because its on-disk layout carries a crypto header and every page is ciphertext — an encryption-unaware or older binary cannot make sense of it. The MAJOR-only open gate therefore hard-rejects a MAJOR = 2 file with `UnsupportedFormatVersion` rather than misreading it, exactly as it rejects any future incompatible major. Plaintext databases stay at MAJOR = 1.

**Page level** — each non-superblock page carries a one-byte `page_format_version` in its header, letting individual page layouts evolve within a major without a file-wide format bump. The post-1.0 upgrade story is lazy migration: on read, the page-type module dispatches on its page's declared version; on write, it always produces the current version; cold pages stay in the old layout until an opt-in `db.upgrade()` sweep rewrites them. An additional 8 bytes are reserved in every non-superblock page header for future common-header fields.

Write safety across minors is a narrower guarantee: a binary at MINOR = *m* opening a file at MINOR = *m' > m* cannot safely commit without risking overwriting fields it doesn't know about. The open gate is MAJOR-only by design, so minor variants coexist — same-major files of any minor open successfully, and the chunk-tags MINOR = 1 variant is the first such case. The write-refusal arm (refuse writes when file MINOR > binary MINOR) is implemented (I29): opening a newer-minor file forces the handle read-only, so any mutation returns `ReadOnlyMode` rather than risking a write that clobbers fields the binary doesn't know about. The post-1.0 cross-minor read-compatibility guarantee is absolute; write-compatibility requires binary MINOR ≥ file MINOR.

### Format history

Before 1.0, the on-disk format changed between pre-release builds without a major-version bump; such changes were called out in release notes. The 1.0 release freezes the plaintext format at MAJOR = 1 for the entire 1.x line; encrypted databases carry MAJOR = 2 (see above), and each major's on-disk format is sacred within that major going forward.

Files written by prior pre-1.0 development builds (flat `format_version`, which decodes as MAJOR = 0) are rejected at open time — recreate the database. No production-grade migration is provided for pre-1.0 files.

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

Encryption is exposed through the `open()` `encryption_key` keyword (default `None`): pass `bytes` for a raw 32-byte key or `str` for a passphrase. The three key-management methods take the same `bytes | str` credential vocabulary:

```python
import chisel

db = chisel.open("secret.db", encryption_key="correct horse battery staple")

db.add_key(existing="correct horse battery staple", new=b"\x00" * 32)  # stage a 2nd credential
db.rotate_key(old=b"\x00" * 32, new="new passphrase")                   # replace in place
db.remove_key("correct horse battery staple")                          # retire one
```

`add_key` / `rotate_key` raise `NoFreeKeySlotError` when the 8-slot table is full; `remove_key` raises `LastKeySlotError` rather than leaving the DB with no usable credential.

## Design documents

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — living architecture overview: layer model, commit protocol, recovery, full on-disk format byte-by-byte, and cross-cutting concepts. Start here if you're reading the codebase to *act* on it.
- [`THEORY.md`](THEORY.md) — theory of operation: *why* the design is what it is — the load-bearing decisions, the rejected alternatives, and the implementation history. Read this to build a durable model before changing the engine.
- [`docs/adr/`](docs/adr/) — architecture decision records: one file per decision, with the context and the alternatives that were rejected.
- [GitHub issues](https://github.com/pgexperts/chisel/issues) — the running issue log. This replaced a tracked `ISSUES.md` on 2026-08-03; `I<number>` markers in code comments refer to entries in that retired file, which remains readable in git history.

## License

MIT — see [LICENSE](LICENSE) for the full text. The same license applies to the `python/` PyO3 binding and the `bench/` benchmark subcrate.
