---
id: 0011
title: macOS-fsync fairness via `PRAGMA fullfsync=ON`
date: 2026-05-04
status: Accepted
---

# 0011. macOS-fsync fairness via `PRAGMA fullfsync=ON`

**Context:** On macOS APFS, Chisel's `sync_all` calls Rust's `File::sync_all` which translates to `fcntl(F_FULLFSYNC)` — a flush through the disk's write cache that is durable against power loss. SQLite's default `fsync()` on macOS only flushes to the disk's write cache (without F_FULLFSYNC). Result: bench measurements of Chisel-strict vs. SQLite-strict on macOS measured Apple-vs-Apple disk-cache semantics, not engine behavior — SQLite was ~3 orders of magnitude faster than Chisel on `Strict` durability for the wrong reason.

**Decision:** `SqliteEngine::open_file` issues `PRAGMA fullfsync=ON` for `DurabilityMode::Strict`. No `#[cfg(target_os)]` gate — Linux ignores the pragma; macOS uses `fcntl(F_FULLFSYNC)` matching Chisel's `sync_all`. Costs: one extra PRAGMA exec at SQLite open time.

**Alternatives considered:**

- *Disable F_FULLFSYNC on Chisel for macOS bench.* Would compromise Chisel's actual durability semantics in the bench, defeating the comparison's purpose.
- *Compare only on Linux.* Loses platform coverage; macOS is the platform where Chisel is most likely to be embedded (single-user developer machines).
- *Document the gap, don't fix it.* Rejected — silent unfairness in published numbers is worse than no numbers.

**Consequences:**

- *Positive:* macOS bench numbers now reflect engine behavior, not Apple's default `fsync` semantics.
- *Positive:* Linux numbers unchanged (`PRAGMA fullfsync=ON` is a no-op there).
- *Positive:* No `#[cfg(target_os)]` keeps the bench code platform-uniform.
- *Negative:* SQLite-strict on macOS is now slower than its previous bench numbers showed. README/release-note comparisons must be regenerated post-PR 8.

Confirmed empirically (PR 8 first-run bench-diff): SQLite-strict cells moved by ≤1.5% on Linux across all four scenarios after the change, validating the no-gate decision.

---
