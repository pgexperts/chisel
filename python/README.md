# chisel (Python binding)

[![PyPI](https://img.shields.io/pypi/v/chisel-storage.svg)](https://pypi.org/project/chisel-storage/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../LICENSE)

Python binding for [Chisel](..), a transactional slot-based storage engine written in Rust with shadow-paging durability. The engine is embedded, single-writer, and fully synchronous.

## Status

1.0. Current release: `1.0.0`. Both the API and the on-disk format are now frozen per the compatibility promise in the [root README](../README.md#on-disk-format-compatibility).

## Install

```bash
pip install chisel-storage
```

The distribution is named `chisel-storage` (plain `chisel` was already taken on PyPI); the importable module is still `chisel`, so your code reads `import chisel` regardless.

Not yet published to PyPI; build locally with [maturin](https://www.maturin.rs/) in the meantime:

```bash
pip install maturin
cd python && maturin develop
```

Wheels for CPython 3.11–3.13 on Linux (x86_64, aarch64) and macOS (x86_64, arm64) will be provided on PyPI when published. Windows is not supported — the engine uses `flock`.

## Quick start

```python
import chisel

with chisel.open("db.chisel") as db:
    with db.transaction() as tx:
        h = tx.allocate(b"hello")
    print(db.read(h))  # -> b'hello'
```

In-memory mode (no filesystem, no lock, lost on close):

```python
with chisel.open(None) as db:
    ...
```

## Transactions

`db.transaction()` returns a `Transaction` context manager that commits on clean exit and rolls back on exception. Inside the `with` block, call the data methods on the `tx` object; outside, use `db` directly for reads.

```python
with chisel.open("db.chisel") as db:
    with db.transaction() as tx:
        h1 = tx.allocate(b"a")
        h2 = tx.allocate(b"b")
    # both h1 and h2 are durably committed here
    assert db.read(h1) == b"a"
    assert db.read(h2) == b"b"
```

Only one transaction is active at a time — nesting `with db.transaction()` inside another raises `TransactionAlreadyActiveError`. Use savepoints for nested scopes (see below).

### Explicit commit / rollback

`Transaction` also exposes `.commit()` and `.rollback()` for explicit drive inside a `with` block:

```python
with db.transaction() as tx:
    h = tx.allocate(b"...")
    if something_went_wrong:
        tx.rollback()   # the __exit__ will not re-commit
    else:
        tx.commit()     # also sets the `finished` guard
```

A second explicit drive (after a previous `.commit()`, `.rollback()`, or implicit `__exit__`) raises `AlreadyFinishedError`. Context-manager exits stay idempotent: if you call `.commit()` inside the block, the `__exit__` silently short-circuits.

### Low-level form

For code that needs finer control, `db.begin() / db.commit() / db.rollback()` are available directly on the `Chisel` object. Mixing them with the `with db.transaction()` form in the same block is not supported.

Savepoints are not available in the low-level form: there is no `db.savepoint()`. If you need savepoints, use the `with db.transaction() as tx:` form — savepoints are exposed as `tx.savepoint(name)`, deliberately tied to a `Transaction` object so its lifetime bounds the savepoint stack.

## Savepoints

Named marks within a transaction. `Savepoint` is itself a context manager: on clean exit it calls `release()`; on exception it calls `rollback_to()`.

```python
with db.transaction() as tx:
    h_keep = tx.allocate(b"keep")
    with tx.savepoint("experiment") as sp:
        h_discard = tx.allocate(b"maybe discard")
        if experiment_failed:
            sp.rollback_to()  # h_discard dropped; h_keep stays; sp still open
    # sp is released by __exit__ on normal exit, or rolled_back_to on exception
```

- `sp.release()` flattens the savepoint into the enclosing scope; any savepoints layered on top are also released.
- `sp.rollback_to()` undoes changes back to the savepoint and leaves it on the stack so you can try again.
- A second explicit `.release()` or `.rollback_to()` raises `AlreadyFinishedError`.
- `sp.name` is a read-only attribute returning the savepoint's name — useful for logging or debugging mid-transaction.

## Values (buffer protocol)

Writes accept any buffer-protocol object: `bytes`, `bytearray`, `memoryview`, `array.array`, NumPy arrays, and so on. Reads return `bytes`. `str` is rejected — encode explicitly:

```python
tx.allocate(s.encode("utf-8"))
```

Values up to ~8 KB are packed into slotted data pages; larger values transparently overflow into chained pages. The caller cannot tell which path was taken except via `stats()`.

## Handles

A handle is a stable `int` returned by `allocate()`. It survives `update()`, `defrag()`, and reopens. `delete()` retires the handle — it is never reused within a database's lifetime.

```python
with db.transaction() as tx:
    h = tx.allocate(b"original")
    tx.update(h, b"replacement")      # same h
    tx.delete(h)                       # h is now invalid
# tx.read(h) would raise InvalidHandleError post-commit
```

For bulk deletes, `delete_many` takes a sequence of handles in one call:

```python
with db.transaction() as tx:
    tx.delete_many([h1, h2, h3])
```

To drop chunks by tag instead of by explicit handle list, see [Tags](#tags) -- `delete_with_tag` handles large tagged sets in bounded batches.

`db.handles()` enumerates every live handle (order unspecified).

## Tags

Each chunk can carry an immutable `u32` *tag* assigned at allocation. The engine
keeps a reverse membership index (tag -> handles), so you can enumerate or
bulk-drop every chunk sharing a tag without scanning.

Tags are **non-zero**: every tagged method raises `ValueError: tag must be
non-zero` when given `0`, including `handles_with_tag(0)`. "Untagged" is not a
tag value -- it is the absence of one. Allocate with plain `allocate()` to leave
a chunk untagged, and `tag()` returns `None` (not `0`) for such a handle.

Tags are **immutable**. They are fixed at `allocate_tagged()` time; `update()`
preserves the tag while replacing the value. There is no "set tag" operation.

The five tag methods live on both the `Chisel` object and the `Transaction`
context manager:

```python
with db.transaction() as tx:
    h = tx.allocate_tagged(b"row-payload", tag=42)
    assert tx.tag(h) == 42                     # None if untagged
    assert tx.handles_with_tag(42) == [h]      # reverse-index lookup

    tx.update(h, b"new-payload")               # tag stays 42
    assert tx.tag(h) == 42
```

`delete_tagged(handle, tag)` deletes only if the stored tag matches, raising
`TagMismatchError` otherwise (the chunk is left intact):

```python
with db.transaction() as tx:
    tx.delete_tagged(h, 42)        # raises TagMismatchError if h's tag != 42
```

`delete_with_tag(tag, max)` is a **bounded** relation-drop. It deletes up to
`max` chunks carrying `tag` and returns `(deleted_handles, complete)`. Loop
`transaction -> delete_with_tag -> commit` until `complete` is `True` to drop a
large tagged set in fixed-size, separately-committed batches (`max == 0` is a
no-op returning `complete == False`):

```python
while True:
    with db.transaction() as tx:
        deleted, complete = tx.delete_with_tag(42, max=1000)
    if complete:
        break
```

All five methods are also available on the bare `Chisel` object between
`db.begin()` and `db.commit()`.

## Client byte

Each chunk can carry a mutable opaque `int` (0–255) "client byte". Chisel stores
it but never interprets it — no search, no filter, no index. It is independent of
the tag: the tag is immutable and carries membership semantics; the client byte is
freely mutable and carries whatever meaning the caller assigns.

- `client_byte(handle) -> int` — read the byte; `0` if never set. Valid inside or
  outside a transaction.
- `set_client_byte(handle, byte)` — write the byte. Requires an active transaction;
  raises `NoActiveTransactionError` if called outside one. Transactional: reverts on
  rollback. `update()` preserves the client byte (same carry-forward as the tag).

```python
with db.transaction() as tx:
    h = tx.allocate(b"payload")
    assert tx.client_byte(h) == 0          # default
    tx.set_client_byte(h, 7)
    assert tx.client_byte(h) == 7

# client_byte persists across transactions
assert db.client_byte(h) == 7

with db.transaction() as tx:
    tx.update(h, b"new-payload")           # byte stays 7
    assert tx.client_byte(h) == 7
```

Both methods raise `InvalidHandleError` for a deleted or unknown handle.
`set_client_byte` also raises `NoActiveTransactionError` if called outside a transaction,
and `OverflowError` if `byte` is outside the valid range `0–255`.

Both methods are also available on the bare `Chisel` object between `db.begin()`
and `db.commit()`.

## Named roots

A small fixed-size table mapping short names to handles, stored in the superblock. Intended for long-lived entry points such as a meta-B-tree root. Changes are transactional.

```python
with db.transaction() as tx:
    h = tx.allocate(b"meta-root-payload")
    tx.set_root_name("meta", h)

# Later, possibly after reopen:
meta = db.get_root_name("meta")  # -> int, or None if unbound
```

Names must be non-empty, UTF-8, bounded in length, and contain no NUL bytes. The table has a small fixed capacity; `RootNameTableFullError` fires on overflow.

`tx.clear_root_name(name)` removes a binding. Like `set_root_name`, it's transactional — the unbinding takes effect on commit and reverts on rollback:

```python
with db.transaction() as tx:
    tx.clear_root_name("meta")
```

## Stats and defrag

```python
s = db.stats()
# Stats(handle_count=1234, total_pages=567, file_size_bytes=4644864)

with db.transaction() as tx:
    # defrag is available on both objects; `tx.defrag(...)` is the same call.
    # Either way it runs against the currently active transaction.
    result = db.defrag(chisel.DefragOptions(sparse_threshold=0.25, max_values=0))
# DefragStats(pages_examined=..., pages_freed=..., values_moved=...)
```

`defrag()` requires an active transaction so it composes with other work and is atomic on commit. `max_values = 0` means "no cap"; otherwise it bounds how many values get relocated in one pass, which is the knob for keeping a single pass's cost predictable.

## Engine counters

`db.counters()` returns a `Counters` snapshot of four cumulative engine-activity counters since the database was opened. Useful for debugging hot reads, characterising commit overhead, or driving an external benchmark harness.

```python
c = db.counters()
# Counters(cache_hits=1234, cache_misses=89, pages_allocated=42, fsync_calls=14)
```

The four fields:

- `cache_hits` — `PageCache.get` returned a cached page without disk I/O.
- `cache_misses` — `PageCache.get` had to load from disk (and validate the page's XXH3 checksum).
- `pages_allocated` — `PageCache.new_page` invocations; counts attempted allocations even when the cache subsequently rejects with `CacheFullError` at the strict `cache_max_bytes` cap.
- `fsync_calls` — successful `PageIo.fsync` invocations; three per commit (pre-drain flush + main pages flush + superblock).

The counters are point-in-time snapshots; they do not update after the call. Read them again to observe new totals. They reset implicitly on `close()` + reopen because the underlying engine state is rebuilt; nothing is persisted to disk.

The typical pattern is read-subtract-read for per-operation deltas:

```python
before = db.counters()
with db.transaction() as tx:
    tx.allocate(b"...")
after = db.counters()
print(after.fsync_calls - before.fsync_calls)   # commit cost: 3 (pre-drain + data + superblock)
print(after.pages_allocated - before.pages_allocated)
```

## Opening a database

```python
chisel.open(
    path,                                        # str, os.PathLike, or None for in-memory
    cache_max_bytes=8_388_608,                   # bytes; default 8 MiB (= 1024 × 8 KiB pages)
    spillway_max_bytes=None,                     # None → 1024 × cache_max_bytes (8 GiB default); 0 disables
    drain_insertion=chisel.DrainInsertion.LruTail,
    create_if_missing=True,
    read_only=False,
    superblock_count=2,                          # 2..=16, only consulted on create
    encryption_key=None,                         # bytes = raw key, str = passphrase; see Encryption
)
```

`read_only=True` still takes an exclusive `flock` — it only suppresses writes at the application layer. Two read-only opens cannot coexist on the same file.

`superblock_count` is stored at create time and discovered on reopen; it controls how many superblock slots the engine rotates through on commit. Higher N trades disk space (N × 8 KB) for durability against consecutive torn writes.

`chisel.open(None)` produces an in-memory database (same engine, `Vec<u8>`-backed I/O, no file, no lock, lost on close).

### Cache and spillway

The page cache is bounded strictly by `cache_max_bytes`. When the cache is full of dirty pages (nothing evictable), overflow spills into a sidecar file `<path>.spillway` rather than failing. The spillway is bounded by `spillway_max_bytes` and is truncated at every commit and rollback. It is never `fsync`ed — its contents are uncommitted by definition, so a crash with a non-empty spillway is recovered by discarding it.

- `spillway_max_bytes=None` (default) scales the spillway cap to `1024 × cache_max_bytes` — 8 GiB at the 8 MiB cache default. This matches the Rust API's default and is the recommended setting for normal workloads.
- `spillway_max_bytes=0` disables the spillway entirely and restores `CacheFullError`-at-cap semantics. Useful for tests that want deterministic "cache is full" failures, or for memory-constrained deployments where you'd rather error than spill to disk.
- Any positive integer caps the spillway in bytes. When both the cache and the spillway are exhausted, the engine raises `SpillwayFullError`; commit or roll back to drain.

`drain_insertion` controls where rehydrated spillway pages re-enter the cache when the spillway drains at commit:

- `chisel.DrainInsertion.LruTail` (default) places them at the LRU end, so they're the next pages evicted under normal use. Right for workloads that don't re-read pages they just wrote.
- `chisel.DrainInsertion.Mru` places them at the MRU end. Right for workloads that re-read recently-written pages — the rehydrated cache entries stay warm.

In-memory mode (`chisel.open(None)`) uses an in-memory spillway buffer instead of a sidecar file; everything else behaves the same.

### Runtime configuration

All three options above can also be changed *between* transactions on a live `Chisel` handle:

```python
with chisel.open("db.chisel") as db:
    db.set_cache_max_bytes(4 * 1024 * 1024)              # halve to 4 MiB
    db.set_spillway_max_bytes(0)                          # disable spillway
    db.set_drain_insertion(chisel.DrainInsertion.Mru)
    with db.transaction() as tx:
        ...  # uses the new config
```

Each setter operates ONLY on between-transactions state: calling any of them while a transaction is active raises `TransactionInProgressError`. The engine guards this because shrinking the cache or spillway mid-transaction would either need to reject pinned dirty pages or silently overflow them, neither of which is a clean story — commit or roll back first.

The setters take effect immediately after they return. A subsequent `db.transaction()` uses the new caps and policy; the previous transaction (already committed or rolled back) was unaffected.

## Encryption

Pass `encryption_key` to `chisel.open()` to create or open an encrypted database. Every page is encrypted at rest; the key never touches the file, only a wrapped copy of the data key does.

The key argument accepts exactly two Python types, and the type *is* the meaning:

- **`bytes`** — a raw key: the bytes are used as HKDF input keying material. Any non-empty length is accepted (32 bytes is the conventional choice); empty is rejected.
- **`str`** — a passphrase, run through Argon2id to derive the key. Slow by design (that is the point of a passphrase); expect a noticeable delay on both create and open.

Anything else raises `TypeError`. The choice is made when the slot is written: the slot records which KDF produced it, and a later open uses that record. Key material is zeroized when dropped and is never logged or included in a repr.

```python
# Create encrypted. The FIRST open of a new path fixes it as encrypted.
with chisel.open("secret.chisel", encryption_key=b"\x00" * 32) as db:
    with db.transaction() as tx:
        h = tx.allocate(b"private")

# Reopen with the same key.
with chisel.open("secret.chisel", encryption_key=b"\x00" * 32) as db:
    assert db.read(h) == b"private"
```

Mismatches between the key you pass and the file you open are all distinct errors, so they can be told apart:

| Situation | Raises |
|---|---|
| Encrypted file, no `encryption_key` given | `NoEncryptionKeyError` |
| Encrypted file, key unlocks no slot | `InvalidEncryptionKeyError` |
| Plaintext file, `encryption_key` given | `EncryptionNotSupportedError` |

There is no way to encrypt a database after the fact, or to decrypt one: whether a file is encrypted is decided when it is created. To convert, create a new database and copy the values across.

### Managing keys

A database carries a key-slot table with **8 slots**. Each slot holds the same data key wrapped under a different user key, so several keys can open the same database and a key can be replaced without re-encrypting a single page.

```python
db.add_key(existing, new)   # wrap the data key under `new` as well; needs a key that already works
db.rotate_key(old, new)     # replace `old`'s slot with `new` in one step
db.remove_key(key)          # free the slot `key` unlocks
```

All three are **between-transaction** operations: calling one while a transaction is active raises `TransactionInProgressError`. Each argument is a key in the same `bytes`-or-`str` vocabulary as `encryption_key`.

The failure modes worth planning for:

- `add_key` / `rotate_key` raise `NoFreeKeySlotError` when all 8 slots are occupied — `remove_key` a stale one first.
- `remove_key` raises `LastKeySlotError` rather than removing the only remaining key, which would leave the database permanently unopenable.
- All three raise `InvalidEncryptionKeyError` if the `existing` / `old` / `key` argument unlocks no slot.

## Errors

All Chisel errors inherit from `chisel.ChiselError`, which splits into two tiers.

### Operational — the database is healthy; the caller made a mistake

Catch and continue.

| Class | When it fires |
|---|---|
| `InvalidHandleError` | Unknown or deleted handle passed to `read` / `update` / `delete` |
| `NoActiveTransactionError` | Mutation attempted outside a transaction |
| `TransactionAlreadyActiveError` | `begin()` called while one is already running |
| `TransactionInProgressError` | `set_cache_max_bytes` / `set_spillway_max_bytes` / `set_drain_insertion` called while a transaction is active; commit or roll back first |
| `SavepointNotFoundError` | `rollback_to` / `release` on an unknown savepoint name |
| `DuplicateSavepointError` | `savepoint(name)` reused an active name |
| `ReadOnlyModeError` | Write attempted on a read-only handle |
| `DatabaseFileNotFoundError` | `create_if_missing=False` and file absent |
| `InvalidRootNameError` | Named-root name is empty, too long, or not valid UTF-8 |
| `RootNameTableFullError` | All named-root slots are in use |
| `InvalidSuperblockCountError` | `superblock_count` outside `2..=16` |
| `InvalidArgon2ParamsError` | `argon2_params` carries cost values the KDF cannot use (checked before the file is touched, so it is never a wrong-key problem) |
| `CacheFullError` | Page cache hit its strict `cache_max_bytes` cap with every cached page dirty (no clean page available for eviction) AND the spillway is disabled (`spillway_max_bytes=0`); commit or roll back to drain. When the spillway is enabled (default), the cache overflows into it instead and you'll see `SpillwayFullError` only if the spillway also fills. |
| `SpillwayFullError` | Spillway sidecar's `spillway_max_bytes` cap was reached during a transaction; commit or roll back to drain the spillway. Database is intact. |
| `NoEncryptionKeyError` | Opened an encrypted database without supplying `encryption_key` |
| `InvalidEncryptionKeyError` | `encryption_key` was supplied but unwraps no key slot (wrong passphrase or wrong raw bytes) |
| `EncryptionNotSupportedError` | `encryption_key` was supplied but the database is plaintext |
| `NoFreeKeySlotError` | `add_key` / `rotate_key` attempted but all 8 key-slot table entries are in use |
| `LastKeySlotError` | `remove_key` would clear the last active key slot, leaving the database permanently unopenable |
| `TagMismatchError` | `delete_tagged(handle, tag)` was passed a `tag` that doesn't match the handle's stored tag; the chunk and membership index are left untouched |
| `ClosedError` | Any call on a `Chisel`, `Transaction`, or `Savepoint` after `db.close()` |
| `AlreadyFinishedError` | Second explicit drive on a transaction or savepoint |

### Fatal — storage integrity is in question

Drop the handle and reopen.

| Class | When it fires |
|---|---|
| `IoError` | Underlying filesystem I/O error. Also subclasses the builtin `OSError`, so it is catchable as `except OSError` and `.errno` is `OSError`'s native attribute (with `.strerror` set when an errno exists). Carries `.errno` (the raw OS error code, or `None` if unavailable) and `.kind` (the Rust `io::ErrorKind` name, e.g. `"PermissionDenied"`), so callers can branch on the cause without parsing the message. |
| `DecryptionFailedError` | A page or the superblock body failed AEAD authentication (wrong key, or the ciphertext was tampered with) |
| `ChecksumMismatchError` | A page's XXH3 checksum did not validate on load |
| `CorruptSuperblockError` | No readable superblock slot found |
| `FileSizeMismatchError` | File size inconsistent with the superblock's claim |
| `LockFailedError` | Could not acquire `flock` — another process holds the file |
| `UnsupportedFormatVersionError` | File's `format_version` beyond this binary's support |
| `CorruptPageError` | Page structure violates its invariants (e.g., overflow-chain cycle) |
| `InvalidPageIdError` | Request for a page beyond the physical file length |
| `PoisonedError` | Raised on every call after a prior fatal error |

### Recovery

```python
try:
    with db.transaction() as tx:
        ...
except chisel.FatalError:
    db.close()
    db = chisel.open("db.chisel")
    # The reopen picks up the last durable superblock. The failed
    # transaction was never linearized, so it simply does not exist.
```

The shadow-paging recovery path guarantees the reopened database is at a consistent, committed state — there is no log replay and no partial-recovery window.

You can also check the poisoned state explicitly via the `is_poisoned` read-only property (useful for periodic health checks or reading state after catching a `PoisonedError`):

```python
if db.is_poisoned:
    db.close()
    db = chisel.open("db.chisel")
```

## Thread safety

Calls into a `Chisel` instance are **serialized**, not concurrent. Every per-operation method holds the GIL for its whole duration and takes an internal `Mutex`, so two threads calling the same instance at once cannot corrupt memory or interleave partway through an operation — a concurrent `read()` from several threads is safe and is covered by the test suite.

What is *not* safe is sharing **transaction state** across threads. There is one active transaction per instance, so two threads must not interleave transactions on the same `Chisel`: thread A's `commit()` will commit thread B's writes, and a `Savepoint` or `Transaction` object is not meaningful outside the thread that is driving it. Either confine a transaction to one thread, or use one instance per thread (the underlying Rust `Chisel` is `Send`, so an instance can also be handed from one thread to another).

The consequence to plan for is **latency, not safety**: only `chisel.open()` releases the GIL. Every other call holds it, so a long-running engine operation — a large commit's three fsyncs, a big `defrag()` — blocks *all* other Python threads in the process for its duration, not just threads touching this database. If that matters, keep such operations off threads that are servicing latency-sensitive work.

## In-memory mode

`chisel.open(None)` creates a memory-backed database with no filesystem access and no lock. Same engine, same API, same guarantees except durability — all data is lost when the `Chisel` object is closed or garbage-collected.

Useful for:

- Unit tests (faster than tempfiles, no cleanup)
- Benchmarking against `sqlite :memory:`-style comparators
- Ephemeral caches and scratch storage

## On-disk format compatibility

**Within a given major version, the on-disk format is sacred.** Any file written by any release with major version *N* will be readable by any other release with major version *N*, regardless of minor or patch level. Opening a file written by an incompatible future version raises `UnsupportedFormatVersionError` rather than silently misinterpreting the bytes.

### How it's encoded

Versioning is two-tiered. **File level**: each superblock carries a packed `format_version` u32 — upper 16 bits = MAJOR, lower 16 bits = MINOR. The open-time gate compares MAJOR only: a 1.3 binary opens a 1.7 file cleanly, a 1.3 binary rejects a 2.0 file. **Page level**: each non-superblock page carries a one-byte format version in its header, letting individual page layouts evolve within a major without a file-wide bump — the basis for lazy per-page upgrade on future versions.

Cross-minor *read* compatibility is absolute within a major. *Write* compatibility is narrower: starting with the first post-1.0 minor bump, a binary at MINOR = *m* opening a file at MINOR = *m' > m* will be restricted to read-only to avoid clobbering fields the binary doesn't know about. Until 1.1 ships this check is a no-op because no minor variants exist.

### Pre-1.0 caveat

Until Chisel reaches 1.0, the on-disk format may change between pre-release builds without a major-version bump. Any such pre-1.0 change will be called out in release notes. The first 1.0 release freezes MAJOR at 1 for the entire 1.x line.

Files written by prior development builds (pre-1.0 flat `format_version`, which decodes as MAJOR = 0) are rejected at open time — recreate the database. No migration is provided for pre-release files.

## Design

The underlying Rust engine is documented in [`../README.md`](../README.md) and [`../ARCHITECTURE.md`](../ARCHITECTURE.md). The Python binding mirrors the Rust API but adds context managers for transactions and savepoints; the error hierarchy mirrors `ChiselError` directly with operational vs fatal tiers.
