# Chisel Python Interface — Design

**Date:** 2026-04-14
**Status:** Design approved; implementation plan pending.
**Scope:** First-class Python binding for the Chisel storage engine, targeted at application embedding (Python apps that use Chisel as their storage layer, analogous to how apps use `sqlite3`).

## 1. Goals and Non-Goals

### Goals

- A Pythonic, ergonomic API that lets embedded Python applications use Chisel as their storage engine.
- Preserve Chisel's correctness guarantees — transactional semantics, shadow-paging durability, poison-on-fatal-error — at the binding layer without weakening them.
- Zero-surprise idioms for Python users: context managers for resource scopes, bytes-like for values, standard exception hierarchies.
- `pip install chisel` works on Linux and macOS with prebuilt wheels; sdist available for source builds.
- The Python surface is versioned with the Rust crate in the same repository, so wheels always build against the exact in-tree engine.

### Non-Goals (v1)

- Windows support. Chisel's locking layer uses `flock` via `libc`; porting it is out of scope here. sdist will refuse to build on Windows with a clear error.
- Async/await interface. The engine is synchronous and single-threaded by design; wrapping it in asyncio is a downstream concern.
- Multi-threaded access to a single `Chisel` instance. Concurrent use from two threads is a programming error, not a supported mode.
- Streaming iteration over handles. Deferred until the Rust side exposes a walker; the v1 Python API types `handles()` as `Iterable[int]` so the future change is non-breaking.
- Wrapper `Handle` class. Handles are plain `int` in Python, matching the Rust `u64`.
- A higher-level data structure layer (B-tree, document store). That belongs to downstream clients.

## 2. Architecture

### 2.1 Binding Technology

PyO3 native extension, built with `maturin`. A `Py<Chisel>` owns a Rust `chisel::Chisel` directly; there is no intermediate C ABI layer, no cffi, and no RPC.

### 2.2 Repository Layout

```
chisel/
├── src/                    (existing Rust crate)
├── tests/                  (existing Rust integration tests)
├── python/                 (new)
│   ├── Cargo.toml          (cdylib crate, path dep on ../)
│   ├── pyproject.toml      (maturin-backed build)
│   ├── src/lib.rs          (PyO3 module, the binding layer)
│   ├── chisel/             (pure-Python package for stubs, __init__ re-exports)
│   │   ├── __init__.py
│   │   └── chisel.pyi      (hand-written type stubs)
│   └── tests/              (pytest suite)
└── docs/
```

The `python/` crate path-depends on the parent Chisel crate. Wheel builds therefore always compile against the in-tree Rust source, eliminating version-skew bugs between the engine and its binding.

### 2.3 Distribution

- **Wheels**: CPython 3.11, 3.12, 3.13 × {macOS x86_64, macOS arm64, Linux x86_64 manylinux, Linux aarch64 manylinux}. Built in CI with `cibuildwheel` using `abi3-py311`.
- **sdist**: published alongside wheels. On Windows, the build script raises a clear error pointing at the `flock` dependency.
- **Python version floor**: 3.11. Originally 3.10; raised during Task 4 implementation because pyo3 0.22's safe `PyBuffer` API is gated behind `Py_3_11` when `abi3` limited-API is active. Keeping 3.10 would require either dropping `abi3` (per-version wheels) or using raw FFI for the buffer protocol. The buffer protocol is central to the value API, so bumping the floor is the clean choice; 3.10 is near end-of-life.
- **Versioning**: Python package version matches the Rust crate version exactly.

### 2.4 CI Integration

The existing GitHub Actions workflow (which runs `cargo build` / `cargo test` / `cargo clippy` / `cargo fmt`) gets a new `python` job that:

1. Depends on the Rust jobs succeeding.
2. Runs `maturin develop` in `python/`.
3. Runs `pytest` on the platforms listed above (reduced matrix for PRs, full matrix on main).

## 3. Public API Surface

### 3.1 Opening a Database

```python
def open(
    path: str | os.PathLike | None,
    *,
    cache_size: int = 1024,
    create_if_missing: bool = True,
    read_only: bool = False,
    superblock_count: int = 2,
) -> Chisel: ...
```

- `path=None` opens an in-memory database (no sentinel `":memory:"` string).
- Options are kwargs; there is no `Options` class to import.
- `read_only=True` with `path=None` raises `ReadOnlyModeError`, matching the Rust behavior.
- `superblock_count` outside `[2, 16]` raises `InvalidSuperblockCountError`.

### 3.2 The `Chisel` Object

```python
class Chisel:
    def __enter__(self) -> Self: ...
    def __exit__(self, *exc_info) -> None: ...   # calls close()
    def close(self) -> None: ...

    # Transactions
    def transaction(self) -> Transaction: ...    # context manager factory
    def begin(self) -> None: ...
    def commit(self) -> None: ...
    def rollback(self) -> None: ...

    # Mutating ops (shortcut forms; require an active transaction)
    def allocate(self, value: collections.abc.Buffer) -> int: ...
    def update(self, handle: int, value: collections.abc.Buffer) -> None: ...
    def delete(self, handle: int) -> None: ...
    def delete_many(self, handles: collections.abc.Sequence[int]) -> None: ...

    # Reads (no active transaction required; mirrors Rust &self methods)
    def read(self, handle: int) -> bytes: ...
    def handles(self) -> collections.abc.Iterable[int]: ...   # v1: list[int]
    def stats(self) -> Stats: ...
    def get_root_name(self, name: str) -> int | None: ...

    # Named roots (mutating; require active transaction)
    def set_root_name(self, name: str, handle: int) -> None: ...
    def clear_root_name(self, name: str) -> None: ...

    # Maintenance
    def defrag(self, options: DefragOptions | None = None) -> DefragStats: ...

    # Health
    @property
    def is_poisoned(self) -> bool: ...
```

### 3.3 The `Transaction` Object

```python
class Transaction:
    def __enter__(self) -> Self: ...
    def __exit__(self, exc_type, exc, tb) -> None: ...
    # Clean exit -> commit(); exception -> rollback(); both propagate
    # fatal errors as poisoning.

    def allocate(self, value: collections.abc.Buffer) -> int: ...
    def read(self, handle: int) -> bytes: ...
    def update(self, handle: int, value: collections.abc.Buffer) -> None: ...
    def delete(self, handle: int) -> None: ...
    def delete_many(self, handles: collections.abc.Sequence[int]) -> None: ...

    def set_root_name(self, name: str, handle: int) -> None: ...
    def get_root_name(self, name: str) -> int | None: ...
    def clear_root_name(self, name: str) -> None: ...

    def savepoint(self, name: str) -> Savepoint: ...   # context manager factory
```

Both `Chisel` and `Transaction` expose the mutating methods. `tx.allocate(...)` reads more naturally inside a `with db.transaction() as tx:` block; `db.allocate(...)` is a convenience for callers driving `begin()`/`commit()` explicitly.

### 3.4 The `Savepoint` Object

```python
class Savepoint:
    name: str

    def __enter__(self) -> Self: ...
    def __exit__(self, exc_type, exc, tb) -> None: ...
    # Clean exit -> release(); exception -> rollback_to(); then propagate.

    def release(self) -> None: ...
    def rollback_to(self) -> None: ...
```

Savepoints nest:

```python
with db.transaction() as tx:
    with tx.savepoint("outer"):
        tx.allocate(b"a")
        with tx.savepoint("inner"):
            tx.allocate(b"b")
            # raising here rolls back to "inner"; "outer" state is preserved
```

The `Savepoint` object returned by `tx.savepoint(name)` can be used as a context manager (blessed path) or driven explicitly via `sp.release()` / `sp.rollback_to()` for code ported from the Rust API.

### 3.5 Structured Return Types

```python
@dataclass(frozen=True)
class Stats:
    handle_count: int
    total_pages: int
    file_size_bytes: int

@dataclass(frozen=True)
class DefragOptions:
    # fields mirror Rust's DefragOptions exactly
    ...

@dataclass(frozen=True)
class DefragStats:
    # fields mirror Rust's DefragStats exactly
    ...
```

Frozen dataclasses: attribute access, correct `__repr__`, type-checker friendly, hashable, extensible without breaking callers.

### 3.6 Value Types

- Writes accept anything implementing the buffer protocol (`bytes`, `bytearray`, `memoryview`, `array.array`, etc.).
- Reads return `bytes` (a fresh owned copy, as the Rust `read()` does).
- Passing `str` raises `TypeError("values must be bytes-like; got str — encode first, e.g. s.encode('utf-8')")`.

## 4. Error Hierarchy

```
chisel.ChiselError                    (base, inherits Exception)
├── chisel.OperationalError           (database healthy; caller misused it)
│   ├── InvalidHandleError
│   ├── NoActiveTransactionError
│   ├── TransactionAlreadyActiveError
│   ├── SavepointNotFoundError
│   ├── DuplicateSavepointError
│   ├── ReadOnlyModeError
│   ├── DatabaseFileNotFoundError     (named to avoid shadowing builtins.FileNotFoundError)
│   ├── InvalidRootNameError
│   ├── RootNameTableFullError
│   └── InvalidSuperblockCountError
└── chisel.FatalError                 (handle is poisoned; drop and reopen)
    ├── IoError                       (wraps OSError as .__cause__)
    ├── ChecksumMismatchError
    ├── CorruptSuperblockError
    ├── FileSizeMismatchError
    ├── InvalidMagicError
    ├── LockFailedError               (Rust is_fatal() classifies this fatal)
    ├── UnsupportedFormatVersionError
    ├── CorruptPageError
    ├── InvalidPageIdError
    └── PoisonedError                 (raised from every call after poisoning)
```

The Operational/Fatal split follows `ChiselError::is_fatal()` in `src/error.rs` — that method is authoritative for which variants trigger poisoning, so the Python hierarchy mirrors it exactly.

Every Rust `ChiselError` variant maps 1:1 to a Python exception class. The two-tier split (`OperationalError` vs `FatalError`) encodes Chisel's poison-on-fatal recovery protocol directly: catching `FatalError` is a caller's signal to drop the handle and reopen; catching `OperationalError` is recoverable without reopening.

The binding layer is responsible for:

- Translating Rust `ChiselError` into the right Python class.
- Attaching the underlying `std::io::Error` to `IoError` instances via `raise ... from OSError(...)` so `__cause__` chains work.
- Ensuring that once the Rust handle is poisoned, every subsequent method call raises `PoisonedError` regardless of its original category.

## 5. Threading and Concurrency

### 5.1 Thread Safety

A `Chisel` is `Send` but not `Sync`. The PyO3 layer respects this: the wrapper is `Send`, so a `Chisel` can be moved between threads, but concurrent use from multiple threads is a programming error. No runtime mutex is added — that would contradict Chisel's deliberate single-threaded design and add overhead to the common case.

Documentation will state this plainly: "A `Chisel` instance is not safe for concurrent use from multiple threads. Use one instance per thread, or serialize access externally."

### 5.2 GIL Release

The binding does NOT release the GIL during engine calls. The original intent was to wrap `commit()`, `read()`, and the CRUD methods in `py.allow_threads(...)`, but the engine's poison flag is implemented as an internal `Cell<bool>`, which makes `&Chisel` non-`Sync` (and therefore non-`Ungil`). PyO3's `allow_threads` requires the closure to be `Ungil`, so this does not compile.

Given Chisel's deliberate single-client design (one `Chisel` per process at the filesystem level; no cross-thread sharing in the public contract), this restriction costs nothing in practice: an embedded Python application that holds the GIL during a Chisel commit has no sibling thread that could make independent storage progress anyway. If a future engine change removes the `Cell<bool>` in favor of a `Sync`-compatible atomic, GIL-releasing wrappers can be added non-breakingly.

## 6. Testing Strategy

### 6.1 Layout

```
python/tests/
├── test_open.py              (path + in-memory, options validation)
├── test_transactions.py      (begin/commit/rollback, context manager, auto-rollback)
├── test_savepoints.py        (nested savepoints, release, rollback_to)
├── test_values.py            (bytes-like acceptance, str rejection, round-trip)
├── test_errors.py            (every error variant, poison behavior)
├── test_named_roots.py       (set/get/clear)
├── test_stats_defrag.py      (structured return types, defrag-inside-txn requirement)
├── test_threading.py         (GIL release observable; Send across threads works)
└── test_property.py          (hypothesis: round-trip invariant on bytes-like)
```

### 6.2 Scope

The Python tests verify that the binding surfaces Rust behavior correctly; they do not re-test engine internals. The Rust test suite remains the source of truth for storage-engine correctness.

Explicit coverage required:

- Auto-commit on clean `with` exit; auto-rollback on exception, including exceptions raised from Python callbacks.
- `PoisonedError` raised from every method after a fatal error; `is_poisoned` reflects the state.
- `str` values rejected with a helpful `TypeError` message.
- Context manager composition: `with chisel.open(...) as db: with db.transaction() as tx: with tx.savepoint(...): ...`.
- In-memory mode matches on-disk mode for all tested operations.

### 6.3 CI

Linux x86_64 and macOS arm64 on Python 3.11 (floor) and 3.13 (ceiling) for PRs; full matrix on main.

## 7. Out of Scope (Explicit Deferrals)

- **Streaming handles**: when the Rust side grows a walker, `handles()` swaps from `list[int]` to a true iterator without API change (already typed `Iterable[int]`).
- **Windows**: deferred until the Rust locking layer supports it.
- **Async binding**: can be built as a separate package layered on top; not part of v1.
- **Type-stub automation**: v1 ships hand-written stubs. Auto-generation from PyO3 can be revisited when it matures.
- **Richer handle type**: a `Handle` wrapper class could be added non-breakingly if instrumentation or cross-database safety becomes valuable.

## 8. Open Questions

None at design-approval time. Implementation-level details (exact PyO3 class layout, how to thread the `&mut Chisel` reference through Python's ownership model for nested context managers) will be resolved in the implementation plan.
