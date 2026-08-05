---
id: 0008
title: In-memory mode
date: unknown
status: Accepted
---

# 0008. In-memory mode

**Context:** Tests, benchmarks, and ephemeral workloads (e.g., a script that builds a transient database) shouldn't pay the cost of disk I/O or interact with the filesystem at all. A separate in-memory implementation would duplicate substantial code and risk drifting from the disk-backed engine.

**Decision:** `Chisel::open_in_memory` runs the full engine against a `Vec<u8>`-backed `PageIo` with no filesystem and no `flock`. Same code path, same guarantees except durability. Used for tests, benchmarks, and ephemeral work. Also exposed as `chisel.open(None)` in the Python binding.

**Alternatives considered:**

- *Mock filesystem at the OS level (e.g., tmpfs).* Tests would still incur kernel-call overhead and filesystem semantics. Rejected — `Vec<u8>` is faster and removes the OS from the test loop entirely.
- *Separate `MemChisel` type.* Would duplicate every method and risk drift. Rejected.
- *Backend trait with `FileBackend` and `MemBackend` impls.* This is essentially what `PageIo` already is internally — the `Backing` enum has `File(...)` and `Memory(Vec<u8>)` variants. The decision was to expose this via a constructor (`open_in_memory`) rather than via a public trait surface, keeping the public API simple.

**Consequences:**

- *Positive:* Tests are fast and hermetic.
- *Positive:* Same code path means in-memory tests catch bugs that would also affect on-disk operation.
- *Positive:* Python users can experiment with `chisel.open(None)` without managing temp files.
- *Negative:* No `flock` means the in-memory mode cannot detect "two `open_in_memory` calls share a state" — but this is impossible by construction since each call creates its own `Vec<u8>`.
- *Negative:* Counters reset on close+reopen because the `Vec<u8>` doesn't persist; in-memory mode loses counter history that would have survived for a disk-backed reopen of the same file.

---
