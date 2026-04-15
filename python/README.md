# chisel (Python binding)

Python binding for [Chisel](..), a transactional slot-based storage engine
written in Rust with shadow-paging durability.

## Install

```bash
pip install chisel
```

Wheels are provided for CPython 3.11–3.13 on Linux (x86_64, aarch64) and
macOS (x86_64, arm64). Windows is not supported (the engine uses `flock`).

## Quick start

```python
import chisel

with chisel.open("db.chisel") as db:
    with db.transaction() as tx:
        h = tx.allocate(b"hello")
    print(db.read(h))  # -> b'hello'
```

In-memory mode (for tests, benchmarks, and ephemeral work):

```python
with chisel.open(None) as db:
    ...
```

## Values

Values are raw bytes. Writes accept anything implementing the buffer
protocol (`bytes`, `bytearray`, `memoryview`, `array.array`); reads
return `bytes`. `str` is rejected — encode explicitly:

```python
tx.allocate(s.encode("utf-8"))
```

## Errors

All database errors inherit from `chisel.ChiselError`, split into two tiers:

- `chisel.OperationalError` — the database is healthy; the caller made a
  mistake (invalid handle, no active transaction, etc.). Recoverable.
- `chisel.FatalError` — storage integrity is in question. Drop the handle
  and reopen. Includes `PoisonedError`, which is raised on every call after
  a fatal error.

```python
try:
    with db.transaction() as tx:
        ...
except chisel.FatalError:
    db.close()
    db = chisel.open("db.chisel")  # reopen; shadow-paging restores last commit
```

## Thread safety

A `Chisel` instance is **not** safe for concurrent use from multiple threads.
A `Chisel` *can* be handed from one thread to another (it is `Send`), but two
threads must never call into the same `Chisel` at the same time. Use one
instance per thread, or serialize access externally.

## Design

See the [design spec](../docs/superpowers/specs/2026-04-14-chisel-python-interface-design.md)
for the full API surface and rationale.
