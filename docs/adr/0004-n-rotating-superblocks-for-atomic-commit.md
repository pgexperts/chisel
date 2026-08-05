---
id: 0004
title: N rotating superblocks for atomic commit
date: unknown
status: Accepted
---

# 0004. N rotating superblocks for atomic commit

**Context:** The commit protocol's atomicity hinges on swapping a root pointer in a single durable write. The simplest implementation is a single superblock at offset 0, overwritten on every commit. But a single superblock is vulnerable: a torn write (kernel buffered the new bytes but crashed before all of them reached disk) leaves the file unrecoverable.

**Decision:** N superblocks (configurable at create time via `Options::superblock_count`, range 2..=16, default 2) occupy file offsets 0..N. Commit writes to slot `txn_counter % N` — always the slot with the lowest `txn_counter` among the surviving N. Recovery (`Superblock::select`) reads all N slots, validates each (magic + checksum + `superblock_count` in range), and picks the highest valid `txn_counter`.

**Alternatives considered:**

- *Single superblock with double-write buffer.* PostgreSQL-style approach (every page is written twice, once to a buffer area and once in place). Rejected because shadow paging already provides the same guarantee for data pages — the only page that needs the double-write is the superblock itself, and N rotating slots is conceptually simpler than maintaining a separate buffer area.
- *Write-side journal for the superblock.* Mini-WAL just for the superblock. Rejected: same complexity argument as ADR-1.

**Consequences:**

- *Positive:* Trivial torn-write recovery. A torn slot fails the checksum; `Superblock::select` ignores it and picks the previous slot. Higher N (3..16) survives consecutive torn writes.
- *Positive:* No separate journal. The superblock's own slots ARE the journal.
- *Positive:* Configurable space/durability tradeoff. The user picks N at create time based on their crash tolerance.
- *Negative:* N pages of overhead at the start of every file. With default N=2 and 8 KB pages, that's 16 KB minimum file size before any data.
- *Locked-in:* The on-disk layout reserves the first N pages for superblocks; changing N for an existing file would require migration.

---
