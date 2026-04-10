# Chisel: Transactional Slot-Based Storage Engine

**Date:** 2026-04-09
**Status:** Approved
**Language:** Rust

## Overview

Chisel is a single-user, transactional key-value storage engine that uses shadow paging (copy-on-write) for crash durability. It stores opaque byte values and returns u64 handles for later retrieval. The engine is designed to back a database, with indexes built on top using handles as record locators.

Durability is prioritized over performance. The file is always in a consistent state on disk — no recovery procedure is needed after a crash.

## Core Characteristics

- **Single-user:** Exclusive file access enforced by `flock`. No concurrent readers or writers.
- **Handle-based:** The engine assigns u64 handles. Callers never choose keys.
- **Shadow paging:** Writes go to new pages. Old pages remain intact until commit completes. The file is always consistent.
- **Savepoints:** PostgreSQL-style named savepoints with rollback-to and release semantics.
- **Checksummed:** Every page has a trailing XXH3 checksum for torn-page detection and bit-rot detection.
- **Little-endian:** All on-disk integers are little-endian. Documented, not negotiable at runtime.

## Architecture: Hybrid Handle Table + Slotted Data Pages

The design uses two levels of indirection:

1. A **handle table** (radix tree of pages) maps each u64 handle to a (page_id, slot_index) pair.
2. **Slotted data pages** pack multiple values per page using a slot directory.

This indirection means handles are stable across updates, defragmentation, and value size changes. The cost is one extra page read per lookup, which is nearly always a cache hit.

### Why This Over Alternatives

- **Page-per-handle** wastes space for small values (a 200-byte value fills an 8KB page).
- **Slotted pages without indirection** embeds the physical location in the handle, requiring forwarding pointers when values grow beyond page capacity.
- **Hybrid** gives stable handles, space-efficient packing, and clean defragmentation at the cost of bounded indirection.

## File Layout

All pages are 8KB (8192 bytes). The file size is always a multiple of 8KB.

### Superblock (Pages 0 and 1)

Two copies of the superblock alternate on each commit. The active superblock is the one with the higher `txn_counter` AND a valid checksum.

```
bytes  0..4    magic: u32 (0x4348534C = "CHSL")
bytes  4..8    format_version: u32
bytes  8..16   txn_counter: u64
bytes 16..24   root_handle_table_page: u64
bytes 24..32   root_freemap_page: u64
bytes 32..40   total_pages: u64
bytes 40..48   next_handle: u64
bytes 48..52   page_size: u32 (8192)
bytes 52..8184  reserved: zeroed
bytes 8184..8192  checksum: u64 (XXH3 over bytes 0..8184)
```

The checksum is positioned at the end of the page so that a torn write that doesn't reach the checksum is automatically detected.

**Future extension:** The superblock count can be increased beyond 2 to trade commit performance for additional durability. The selection algorithm ("highest valid txn_counter") generalizes to any number of copies. The superblock count is specified in `Options` and passed to `Chisel::open`.

### File Initialization

When `Chisel::open` encounters a nonexistent or empty file (with `create_if_missing: true`):

1. Write **Page 0** (Superblock A): magic, format version, `txn_counter: 1`, root pointers set to `u64::MAX` (sentinel for "not yet allocated"), `total_pages: 2`, `next_handle: 0`, valid checksum.
2. Write **Page 1** (Superblock B): zeroed (invalid checksum, so Superblock A wins selection).
3. `fsync`.

The resulting file is 16KB — two superblocks, no data. The handle table and freemap pages are allocated lazily on the first `allocate()` inside a transaction. This avoids pre-allocating structure for an empty database.

When `Chisel::open` encounters an existing file:

1. Acquire exclusive `flock`.
2. Read all superblocks, validate checksums, select the one with the highest valid `txn_counter`.
3. Verify file size is consistent with `total_pages`.
4. Load root pointers into memory. Ready for operations.

### Common Page Header (All Non-Superblock Pages)

```
bytes  0..1    page_type: u8 (0x01=HandleTable, 0x02=Data, 0x03=Overflow, 0x04=FreeMap)
bytes  1..2    flags: u8
bytes  2..4    reserved: u16
bytes  4..12   txn_counter: u64
               ...page-type specific...
bytes 8184..8192  checksum: u64 (XXH3)
```

## Handle Table

A radix tree of pages. Each leaf page holds 510 entries (16 bytes each, fitting in the 8168-byte usable body).

### Handle Table Entry (16 bytes)

```
bytes 0..8    page_id: u64
bytes 8..10   slot_index: u16
bytes 10..11  flags: u8 (LIVE, DELETED, OVERFLOW)
bytes 11..16  reserved: [u8; 5]
```

### Capacity

- One level (root is leaf): 510 handles
- Two levels: 510 × 510 = 260,100 handles
- Three levels: 510³ ≈ 132 million handles

### Lookup

Handle N is found at: tree leaf `N / 510`, entry index `N % 510`. All lookups are O(1) arithmetic plus O(depth) page reads.

### COW Cost

Updating one handle entry COWs the leaf page plus each interior node up to the root: 2-3 page writes regardless of total handle count.

## Slotted Data Pages

Each data page packs multiple values using a slot directory that grows forward and value data that grows backward.

```
┌──────┬──────────────┬─────────────────────┬──────────┬────────┐
│ Hdr  │  Slot Dir    │    Free Space       │   Data   │Checksum│
│ 16B  │  grows →     │                     │  ← grows │  8B    │
└──────┴──────────────┴─────────────────────┴──────────┴────────┘
```

### Data Page Header (16 bytes)

```
bytes  0..1   page_type: u8 (0x02)
bytes  1..2   flags: u8
bytes  2..4   slot_count: u16
bytes  4..6   free_start: u16 (end of slot directory)
bytes  6..8   free_end: u16 (start of data region)
bytes  8..16  txn_counter: u64
```

### Slot Directory Entry (6 bytes)

```
bytes 0..2  offset: u16 (from page start)
bytes 2..4  length: u16
bytes 4..6  flags: u16 (live/dead/overflow)
```

### Capacity

- Usable body: 8168 bytes
- Maximum single value: 8162 bytes (one slot entry + value)
- Typical small values (100B): ~70 per page
- Typical medium values (2KB): ~3-4 per page

### In-Page Compaction

When a page has enough total free space but it's fragmented (holes between values from deletions), the engine compacts the page by packing surviving data contiguously and updating slot offsets. This happens opportunistically during COW — since the page is already being copied, compaction is free.

## Overflow Pages

Values exceeding 8162 bytes bypass the slotted page mechanism. The handle table entry is flagged `OVERFLOW` and points directly to the first overflow page.

Each overflow page (type 0x03) contains:
- `total_length: u64` — full value size
- `next_page: u64` — next overflow page (0 if last)
- Up to 8152 bytes of payload

## Free Space Management

### Page-Level: Free Page Map (Bitmap)

A bitmap where each bit represents one page. 1 = free, 0 = in use.

- 8168 usable bytes × 8 bits = 65,344 pages per freemap page
- 65,344 × 8KB ≈ 512 MB of data space per freemap page
- For larger files, freemap pages form a tree (like the handle table). Initial implementation targets a single freemap page (512MB), with tree support added when needed.
- The freemap is COW'd; the superblock holds `root_freemap_page`

**Allocation strategy:**
1. Scan bitmap for a free page near the target location (locality-preserving)
2. Fall back to first free page
3. If no free pages, extend the file

### Slot-Level: In-Page

Tracked by the page header's `free_start` / `free_end` fields. Dead slots create holes that are compacted opportunistically during COW.

## Defragmentation

An explicit, caller-invoked maintenance operation that runs as a normal transaction.

1. Identify sparse data pages (below a configurable occupancy threshold, default 25%)
2. Move values from sparse pages into fuller pages
3. Update handle table entries to reflect new locations
4. Free emptied pages in the freemap
5. Truncate trailing free pages to shrink the file
6. Commit

Can be run incrementally (limited number of pages per pass) to bound transaction size. Crash-safe by design — if interrupted, the old pages are still intact.

## Transactions

### Write Path (Copy-on-Write)

Every modification:
1. COWs the affected data page (or overflow chain)
2. COWs the handle table leaf and path to root
3. COWs the freemap if allocation/deallocation occurred

Old pages are never modified. The superblock still points to them until commit.

### Commit Protocol (Two-Phase)

**Phase 1 — Flush:**
1. Write all dirty pages to disk
2. Compute checksums for each page
3. `fsync` — all new pages are durable

**Phase 2 — Swap:**
4. Increment `txn_counter`
5. Write new root pointers to the *inactive* superblock
6. Compute superblock checksum
7. `fsync` — commit is now durable

**Phase 3 — Reclaim:**
8. Add replaced pages to the freemap

### Rollback

Discard all dirty pages. Restore roots from the superblock. No I/O required — the on-disk state was never modified.

## Savepoints (PostgreSQL Semantics)

Named savepoints with a stack-based model.

### Operations

- **`savepoint(name)`** — Capture current root pointers, push onto stack. Records which pages are dirtied and freed from this point forward.
- **`rollback_to(name)`** — Find the named savepoint. Free all pages dirtied since it. Un-free all pages freed since it. Restore roots to the savepoint's snapshot. Discard all savepoints above it. The named savepoint itself is preserved (can roll back to it again).
- **`release(name)`** — Merge the savepoint's dirty/freed page tracking into the level below. Remove it and all savepoints above it. Changes become part of the parent level.
- **`commit()`** — Implicitly releases all savepoints.

### In-Memory Structure

```rust
struct Savepoint {
    name: String,
    root_handle_table_page: u64,
    root_freemap_page: u64,
    next_handle: u64,
    dirty_pages: Vec<u64>,
    freed_pages: Vec<u64>,
}

struct Transaction {
    base_roots: Roots,
    current_roots: Roots,
    savepoints: Vec<Savepoint>,
    dirty_pages: Vec<u64>,
    freed_pages: Vec<u64>,
}
```

## Crash Recovery

On open:
1. Read both superblocks
2. Validate checksums
3. Pick the one with the higher `txn_counter` and valid checksum
4. That is the database state

No WAL replay. No undo/redo passes.

### Crash Scenarios

| Crash Point | Result | Notes |
|---|---|---|
| During normal operations | Last committed state | Dirty pages only in memory |
| During commit Phase 1 (flushing) | Last committed state | Superblock not updated; new pages are orphans |
| During commit Phase 2 (superblock swap) | Last committed state | Torn superblock has invalid checksum |
| During commit Phase 3 (reclaim) | New committed state | Old pages leak as orphans |

### Orphan Recovery

Crashes can leave orphaned pages — allocated on disk but unreachable. These waste space but don't affect correctness. Defrag reclaims them by walking all reachable pages and marking everything else free. An optional eager mode can do this on open.

### Limitations

- **Both superblocks corrupt:** Unrecoverable. Requires two crashes at exactly the wrong moments. Mitigated by increasing superblock count.
- **Bit rot:** Detected by checksums but not repairable. No redundant copy exists. Users needing repair should use a checksumming filesystem (ZFS, btrfs).
- **File truncation:** Detected on open by comparing file size to `total_pages`. Refused with a clear error.

## Torn-Page Protection

Shadow paging is inherently torn-page safe for data pages: writes go to new locations, so a torn write leaves the old page intact. Checksums on every page serve as detection for corruption outside crash scenarios (bit rot, hardware errors). The only critical write is the superblock swap, protected by the dual-superblock mechanism.

## Page Cache

A straightforward LRU cache. All page I/O flows through it.

- Configurable size (default: 1024 pages = 8MB)
- Checksum validation on every page read from disk
- Dirty pages are never evicted (flushed during commit)
- No locking needed (single-user)

## Public API

```rust
// Opening & Closing
Chisel::open(path: &Path, options: Options) -> Result<Chisel>
Chisel::close(self) -> Result<()>

// Data Operations (mutating operations require active transaction)
allocate(&mut self, value: &[u8]) -> Result<u64>
read(&self, handle: u64) -> Result<Vec<u8>>   // Outside txn: last committed state. Inside txn: sees uncommitted changes.
update(&mut self, handle: u64, value: &[u8]) -> Result<()>
delete(&mut self, handle: u64) -> Result<()>

// Transactions
begin(&mut self) -> Result<()>
commit(&mut self) -> Result<()>
rollback(&mut self) -> Result<()>

// Savepoints (require active transaction)
savepoint(&mut self, name: &str) -> Result<()>
rollback_to(&mut self, name: &str) -> Result<()>
release(&mut self, name: &str) -> Result<()>

// Maintenance
defrag(&mut self, options: DefragOptions) -> Result<DefragStats>
stats(&self) -> Result<Stats>
handles(&self) -> Result<HandleIterator>
```

### Configuration

```rust
struct Options {
    cache_size: usize,         // Max cached pages (default: 1024)
    create_if_missing: bool,   // Create file if absent (default: true)
    read_only: bool,           // No transactions allowed
    superblock_count: usize,   // Number of superblock copies (default: 2, future extension)
}

struct DefragOptions {
    sparse_threshold: f64,     // Min occupancy to be "sparse" (default: 0.25)
    max_pages: usize,          // Max pages per pass (0 = unlimited)
}
```

### Error Categories

- **Operational:** Invalid handle, no active transaction, duplicate savepoint name. Database is fine.
- **Fatal:** I/O error, checksum mismatch. Database may be corrupt. Caller must close and reopen.

## Crate Structure

```
chisel/
├── Cargo.toml
├── src/
│   ├── lib.rs              // Public API: Chisel struct, Options, re-exports
│   ├── error.rs            // Error types (operational + fatal)
│   ├── page.rs             // Page type definitions, common header, constants
│   ├── superblock.rs       // Superblock layout, dual-superblock selection
│   ├── page_io.rs          // Raw file I/O, aligned reads/writes, flock
│   ├── page_cache.rs       // LRU cache, checksum validation on read
│   ├── freemap.rs          // Bitmap free page tracking, allocation hints
│   ├── data_page.rs        // Slotted page: insert/read/update/delete/compact
│   ├── overflow.rs         // Overflow page chains for large values
│   ├── handle_table.rs     // Radix tree of handle entries, COW logic
│   ├── transaction.rs      // Begin/commit/rollback, savepoint stack
│   ├── defrag.rs           // Page consolidation, file truncation
│   └── stats.rs            // Database statistics
└── tests/
    ├── basic_ops.rs        // allocate, read, update, delete
    ├── transactions.rs     // commit, rollback, savepoints
    ├── crash_recovery.rs   // simulated crash at each phase
    ├── overflow.rs         // large values spanning multiple pages
    ├── defrag.rs           // fragmentation + compaction
    └── stress.rs           // many operations, many savepoints
```

### Module Dependencies

Strictly bottom-up:

1. `page`, `superblock`, `error` — pure types, no I/O
2. `page_io` — raw file operations, flock
3. `page_cache` — LRU cache, checksum validation
4. `data_page`, `overflow`, `freemap` — page-type-specific logic
5. `handle_table` — radix tree, uses page_cache + data_page + freemap
6. `transaction`, `defrag`, `stats` — orchestration
7. `lib.rs` — thin public API

COW is implemented per-module rather than centralized, because each tree structure (handle table, freemap) has format-specific COW logic that benefits from colocation with the page format code.
