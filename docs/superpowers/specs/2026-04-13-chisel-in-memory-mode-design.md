# Chisel In-Memory Mode — Design

**Date:** 2026-04-13
**Status:** Draft (awaiting user review before implementation planning)

## Motivation

Chisel needs an in-memory mode primarily to enable apples-to-apples benchmark comparisons against SQLite's `:memory:` mode. The file-backed engine's performance is dominated by `fsync` latency on every commit; a like-for-like comparison requires removing the filesystem from both engines so that what is measured is the storage-engine overhead itself — slot management, the handle-table radix tree, shadow paging, COW, and page checksums — not disk I/O.

In-memory mode is **not** intended for durable workloads. It is lossy by construction: dropping the `Chisel` value (or process exit) discards all data.

## Goals

- Provide a mode whose semantics above the I/O layer are identical to file-backed Chisel: same API, same transaction behavior, same handle stability, same checksum discipline.
- Eliminate only the costs that are strictly filesystem-attributable: `fsync`, file I/O syscalls, and `flock`.
- Preserve all engine-level work that contributes to Chisel's real per-operation cost, so benchmark numbers remain honest.
- Keep the change localized — ideally contained within `page_io.rs` — so the rest of the module stack is unaffected.

## Non-goals

- Durability, persistence across process restart, crash recovery, or snapshotting to disk. A memory-backed Chisel is purely ephemeral.
- A pluggable storage backend abstraction. This is a single additional backing, not a framework.
- Performance tuning beyond what falls out of removing fsync. The in-memory path should not be faster than "the same engine minus fsync" — extra optimizations would compromise the benchmark premise.
- Multi-process / multi-client access. Memory-backed databases are trivially single-client by ownership of the `Chisel` value.

## What is switched off vs. kept

**Switched off (trivially — no filesystem):**
- `fsync` and `fsync` of the superblock page → no-op.
- `flock` acquisition / release → skipped; there is no file to lock.
- File I/O syscalls (`pread`, `pwrite`, `set_len`) → replaced by in-process memory copies.

**Kept (engine overhead that must appear in benchmarks):**
- XXH3 page checksums on every read and write.
- Shadow paging and per-module COW (the handle table's COW, the freemap's COW, data-page COW). These *are* the transaction mechanism; removing them would measure a different engine.
- Dual-superblock alternation on commit. Useless for recovery in memory mode, but removing it would alter the commit code path, so it stays for benchmark fidelity.
- `PageCache` with its LRU eviction, at the configured cache size.
- Handle-table radix tree traversal and COW.

## API surface

```rust
impl Chisel {
    // Existing:
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ChiselError>;
    pub fn open_with_options(path: impl AsRef<Path>, opts: ChiselOptions)
        -> Result<Self, ChiselError>;

    // New:
    pub fn open_in_memory() -> Result<Self, ChiselError>;
    pub fn open_in_memory_with_options(opts: ChiselOptions)
        -> Result<Self, ChiselError>;
}
```

- Distinct constructors, not a sentinel path or options flag. Rationale: distinct constructors match Rust convention for distinct states, avoid overloading `Path` arguments with magic strings, and keep the door open to differentiating the return type later if desired.
- Return type is `Result` for API symmetry with `open()`, even though the memory-mode construction has no fallible I/O step today. This leaves room to add fallible initialization later without an API break.
- All existing `ChiselOptions` apply unchanged: cache size, superblock count, any future option. No new fields on `ChiselOptions`.
- The returned `Chisel` is behaviorally indistinguishable from a file-backed one for every other public method. `commit()`, `rollback()`, reads, writes, handle allocation, defrag, and stats all work identically. The only differences are persistence (none) and latency (no fsync).

## Internal structure

All changes are contained in `page_io.rs`. No module above it is modified.

```rust
enum Backing {
    File {
        file: File,
        // lock-file handle etc., as today
    },
    Memory {
        pages: Vec<[u8; PAGE_SIZE]>,
    },
}

pub struct PageIo {
    backing: Backing,
    // Backing-agnostic fields stay at this level.
}
```

Per-method dispatch:

| `PageIo` method              | `File` variant                 | `Memory` variant                       |
|------------------------------|--------------------------------|----------------------------------------|
| `read_page(id)`              | `pread` into buffer            | copy from `pages[id]`                  |
| `write_page(id, buf)`        | `pwrite`                       | copy into `pages[id]`                  |
| allocate new page / `extend` | `set_len` grows file           | `pages.push([0; PAGE_SIZE])`           |
| `fsync()`                    | `fsync` the data file          | no-op                                  |
| superblock `fsync`           | `fsync` the data file          | no-op                                  |
| flock acquire / release      | as today                       | skipped                                |
| `len_pages()`                | file size / `PAGE_SIZE`        | `pages.len()`                          |

The per-method `match self.backing` is the full extent of the dispatch. A concrete enum branch (rather than `dyn PageIo` or a generic trait) is chosen deliberately: trait-object dispatch would add a vtable call per page operation, precisely the kind of overhead that would invalidate the benchmark premise. An enum branch is predictable and effectively free once the variant is hot.

`PageCache` and every module above it are untouched. They call the same `PageIo` methods and receive identical semantics — checksummed pages in, checksummed pages out.

## Bootstrap and lifecycle

`open_in_memory` starts with an empty `pages` vector, then performs the same fresh-database initialization path that `open()` uses for a new file: write superblock 0, write superblock 1, initialize freemap, etc. The recovery path (pick the higher-`txn_counter` valid superblock) is never reached in memory mode because there is no prior state.

On drop, the `Vec<[u8; PAGE_SIZE]>` is freed like any other Rust allocation. No explicit teardown is required beyond what `Drop` already does for file-backed mode.

## Testing

**Dual-backing parameterization of the integration suite.** A helper / macro generates two `#[test]` functions from each test body — one file-backed, one memory-backed:

```rust
enum Backing { File(NamedTempFile), Memory }

fn open_chisel(b: &Backing) -> Chisel {
    match b {
        Backing::File(f) => Chisel::open(f.path()).unwrap(),
        Backing::Memory => Chisel::open_in_memory().unwrap(),
    }
}

// Macro: dual_backing_test!(name, |b| { ... }) expands to
//   #[test] fn name_file()   { ... }
//   #[test] fn name_memory() { ... }
```

Exact macro shape is an implementation detail; the invariant is one test body → two `#[test]` functions.

**Scope:**
- All existing integration tests in `tests/` that exercise CRUD, transactions, handle stability across update/move, defrag, and stats → dual-backing.
- Tests that deliberately corrupt the on-disk file, simulate crashes, verify the recovery superblock selection, or exercise `flock` contention → file-only. These are meaningless in memory.
- Module-level unit tests in `src/**` → unchanged. They are already either backing-agnostic or legitimately backing-specific.

**New tests specific to memory mode:**
- `open_in_memory()` returns a usable instance and supports one full commit/rollback round trip.
- `open_in_memory_with_options()` respects a non-default cache size and a non-default superblock count (exercises options flow-through).
- A large-insert → drop → fresh-`open_in_memory()` smoke test to confirm that the backing memory is freed on drop and the new instance starts clean.

**Not tested:** persistence. By construction, there is none.

## Risk review

- **Benchmark dishonesty.** The core risk is that simplifying "durability" bleeds into simplifying "the engine," producing numbers that flatter Chisel. Mitigation: the "kept" list above is explicit, the enum-dispatch choice avoids vtable overhead, and the dual-backing test suite continuously verifies behavior parity above `page_io.rs`.
- **Divergence drift.** Over time, code changes to `page_io.rs` may treat the two variants asymmetrically in ways that aren't caught by tests. Mitigation: dual-backing parameterization catches observable divergence. Any behavior intentionally specific to one backing must be documented inline.
- **Memory growth.** An in-memory Chisel grows its `Vec<[u8; PAGE_SIZE]>` without bound as pages are allocated. This is acceptable for the benchmark use case (workloads are sized to fit RAM) and matches the file-backed behavior (the file grows without bound too). No separate cap is imposed.
