---
id: 0002
title: Single-writer enforced by `&mut self`
date: unknown
status: Accepted
---

# 0002. Single-writer enforced by `&mut self`

**Context:** Embedded databases face a choice: single-writer (one mutator at a time, often with multiple concurrent readers) vs. multi-writer (transactions interleave, requiring locking, MVCC, or both). The choice affects the API surface, the storage format (MVCC needs version chains), the recovery model, and the testing burden.

**Decision:** Single-writer, single-process. Enforced at three levels: (a) the OS via exclusive `flock` in `page_io.rs`, (b) the type system via `&mut self` on every mutating Chisel API, (c) explicit project-memory note that this is *philosophical*, not a v1 simplification.

**Alternatives considered:**

- *Multi-writer with internal locking.* Would require RwLock or Mutex around `PageCache`, transaction-conflict detection, deadlock handling. Roughly doubles the engine's complexity.
- *MVCC.* Adds version chains to every page, garbage-collection responsibilities, snapshot-isolation semantics. Out of scope for an embedded single-process engine.
- *Single-writer at v1, multi-writer at v2.* Rejected because the `&mut self` API is load-bearing — relaxing it later would be a breaking change for every consumer, and the type system encodes the invariant in a way internal locking cannot.

**Consequences:**

- *Positive:* No internal locking. `RefCell<PageCache>` (not `Mutex`) inside `TransactionManager` lets `read()` / `handles()` / `stats()` take `&self` without external wrapping; the borrow checker handles the rest.
- *Positive:* The type system makes "two concurrent transactions" impossible to express. There is no test for it because there is no API for it.
- *Negative:* Workloads that need concurrent writers must serialize at a higher layer (e.g., Exilis does this via its own `RefCell<Chisel>` inside the storage backend).
- *Locked-in:* See above. Multi-writer would be a v2.0 breaking change, not a minor.

---
