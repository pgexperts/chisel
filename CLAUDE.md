# Chisel

Transactional slot-based storage engine in Rust using shadow paging (copy-on-write) for crash durability.

## Build & Test

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

CI runs all three checks on push to main and on PRs.

## Architecture

The module dependency graph is strictly bottom-up — no circular dependencies:

1. `page.rs`, `superblock.rs`, `error.rs` — Pure types, constants, checksums. No I/O.
2. `page_io.rs` — Raw file I/O with `flock`. The only module that touches the filesystem.
3. `page_cache.rs` — LRU cache over `PageIo`. All other modules access pages through this.
4. `freemap.rs`, `data_page.rs`, `overflow.rs` — Page-type-specific logic. Each operates on raw `[u8; PAGE_SIZE]` buffers.
5. `handle_table.rs` — Radix tree mapping `u64` handles to `(page_id, slot_index)`. Implements its own COW.
6. `transaction.rs` — Orchestrates handle table, data pages, overflow, and superblock into transactional operations.
7. `defrag.rs`, `stats.rs` — Maintenance utilities.
8. `lib.rs` — Thin `Chisel` public API wrapping `TransactionManager`.

### Key design decisions

- **Shadow paging, not WAL.** Writes go to new pages; old pages stay intact. Commit = fsync new pages + swap superblock. Crash recovery = pick the valid superblock. No log replay.
- **COW is per-module, not centralized.** The handle table and freemap each implement their own copy-on-write using `PageCache::new_page()`. This avoids a monolithic COW abstraction.
- **Handle table indirection.** Handles are stable u64 IDs that map through a radix tree to physical `(page, slot)` locations. Values can move freely on update or defrag.
- **Dual superblocks** alternate on commit. The one with the higher `txn_counter` and valid checksum wins. This is the atomic commit mechanism.
- **Durability over performance.** Every commit does two fsyncs (data pages, then superblock). Checksums on every page.

### Known v1 simplifications

These are intentional — correctness first, then optimize:

- `insert_into_data_page()` allocates a fresh page per value. Should search for pages with free space.
- The freemap bitmap is built but not wired into the page allocator. `PageCache::new_page()` extends the file.
- Defrag re-inserts all values. Should selectively consolidate sparse pages.

## Conventions

- Platform: macOS/Linux (uses `flock` via `libc`).
- On-disk format: little-endian, 8KB pages, XXH3 checksums.
- All page reads go through `PageCache`, which validates checksums on load.
- Tests use `tempfile::NamedTempFile` for isolated database files.
- Error types: `ChiselError::InvalidHandle` etc. are operational (database is fine). `ChiselError::IoError`, `ChecksumMismatch`, `CorruptSuperblock` are fatal.

## Design Spec

Full design rationale: `docs/superpowers/specs/2026-04-09-chisel-storage-engine-design.md`
