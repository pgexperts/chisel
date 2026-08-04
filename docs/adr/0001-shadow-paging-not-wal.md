---
id: 0001
title: Shadow paging, not WAL
date: unknown
status: Accepted
---

# 0001. Shadow paging, not WAL

**Context:** Two dominant approaches to ACID durability exist for embedded engines. Write-ahead log (WAL) writes intent records to a sequential journal first, then applies changes to the data file in place; recovery replays unfinished log entries. Shadow paging writes new versions of mutated pages to fresh page slots, leaves old pages intact, and atomically swaps a root pointer to make the new state visible; recovery picks the most recent valid root.

**Decision:** Shadow paging. Every mutation allocates a fresh page via `PageCache::new_page`; the previously-committed page stays intact at its original position. Commit writes the new pages' bytes, fsyncs, then writes a new superblock to a different slot than the currently-active one and fsyncs again. Recovery on open is `Superblock::select` over the N candidate slots, picking the one with the highest valid `txn_counter`.

**Alternatives considered:**

- *WAL with in-place updates.* Standard for production-grade DBs (PostgreSQL, SQLite). Rejected for v1: WAL recovery is a substantial subsystem (replay state machine, checkpoint handling, log truncation) that adds risk surface comparable to the entire rest of Chisel. Shadow paging trades disk space (live + previous version of every mutated page until commit) for code simplicity.
- *Hybrid (WAL for small writes, shadow for large).* Considered briefly, rejected as combining the worst of both — recovery code paths multiply and the boundary between modes becomes another correctness obligation.

**Consequences:**

- *Positive:* No log replay; recovery is one read of N superblock slots plus checksum validation. The "is this database open" check is the same code path as crash recovery. Crash safety is provable by inspection: any state where the previous superblock is intact remains recoverable, and `fsync` ordering ensures the new superblock isn't durable until its referenced data pages are.
- *Positive:* COW is a natural fit. Every mutation produces a new page; transactions are simply "the set of new pages plus a candidate new superblock." Rollback is "discard the new pages and the new superblock."
- *Negative:* Disk space cost. Updating a single byte of a page costs an entire new page (8 KB) until the next commit, when the old page becomes freeable. Workloads that write small deltas to many pages have high write amplification.
- *Negative:* Defragmentation becomes necessary over time. `defrag.rs` exists for this.
- *Locked-in:* Reverting to WAL would require rewriting `transaction.rs`, `page_cache.rs` (no more "fresh page per mutation"), and the recovery path in `lib.rs`.

---
