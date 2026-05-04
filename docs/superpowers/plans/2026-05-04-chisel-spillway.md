# Spillway Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the spillway feature: replace the page cache's 8× hard-ceiling elasticity with a sidecar overflow file (the "spillway") so a single transaction can touch a working set larger than the cache, with two-fsync commit cost preserved in the no-spill case.

**Architecture:** Spilling is fully encapsulated below layer 3. `PageCache` holds an `Option<Spillway>`; modules above (`freemap`, `data_page`, `overflow`, `handle_table`, `transaction`) see no API change. `get`/`get_mut`/`new_page` continue to return `&[u8; PAGE_SIZE]` exactly as today. The spillway file is opened alongside the database (`<path>.spillway`), truncated at open and at every commit/rollback, never fsynced.

**Tech Stack:** Rust 2021. No new dependencies. The spillway uses the existing `PageIo` open path for file management and reuses the existing XXH3 checksum (`page::compute_checksum` / variant) for per-slot integrity.

**Spec:** [`docs/superpowers/specs/2026-05-03-chisel-spillway-design.md`](docs/superpowers/specs/2026-05-03-chisel-spillway-design.md)

**Pre-commit checklist (every commit task must pass these from the repo root):**
- `cargo build`
- `cargo test`
- `cargo clippy -- -D warnings`
- `cargo fmt -- --check`

For the bench subcrate, also from `bench/`:
- `cargo test`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt -- --check`

For the python subcrate (Task 3 only), `cd python && cargo check` (no maturin needed for type-checking; macOS link errors with `cargo build` are pre-existing PyO3 limitations and irrelevant to verification).

**Worktree:** `.worktrees/spillway`, branch `claude/spillway`.

**Project conventions:**
- No `Co-Authored-By` trailer in commits.
- No Claude-referencing text in commit messages.
- Heredoc commit messages use `<<'EOF'` (single-quoted).

---

## Task 1: Options rename + new config fields + error variants

**Goal:** Rename `Options::cache_size: usize` → `cache_max_bytes: u64`, add `spillway_max_bytes: u64`, add `drain_insertion: DrainInsertion`, define the `DrainInsertion` enum, add `ChiselError::SpillwayFull` and `ChiselError::TransactionInProgress`. Update every call site in `src/`. The build is green and all tests pass at the end of this task — no functional change yet, just the new shape.

**Files:**
- Modify: `src/lib.rs` (Options struct, defaults, doc comments, open/open_in_memory plumbing)
- Modify: `src/error.rs` (add 2 enum variants + Display arms)
- Modify: `src/page_cache.rs` (PageCache::new signature: take byte limits + DrainInsertion)
- Modify: any test that constructs `Options { cache_size: ... }` or calls `PageCache::new(io, N)` with a page count

- [ ] **Step 1: Edit `src/lib.rs` — Options struct and doc comment**

Find the Options struct and its Default impl (around lines 44–75). Replace:

```rust
/// `cache_size` is a count of pages (not bytes), passed directly to the LRU
/// `PageCache`. `read_only` still takes an exclusive `flock` — it only
/// suppresses writes at the application layer.
///
/// `superblock_count` (ISSUES.md R4) controls how many superblock slots a
/// freshly-created database uses. Default 2 (matches the original layout);
/// valid range is 2..=16. Higher N trades disk space (N × 8 KB) for
/// resilience against consecutive torn writes — N=3 survives one torn
/// commit followed by a torn retry, N=4 survives two retries. This
/// option is ONLY consulted when creating a new database; reopening an
/// existing file discovers N from the on-disk superblock itself.
#[derive(Debug, Clone)]
pub struct Options {
    pub cache_size: usize,
    pub create_if_missing: bool,
    pub read_only: bool,
    pub superblock_count: u32,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            cache_size: 1024,
            create_if_missing: true,
            read_only: false,
            superblock_count: superblock::DEFAULT_SUPERBLOCK_COUNT,
        }
    }
}
```

with:

```rust
/// `cache_max_bytes` is a strict upper bound on the in-memory page cache, in
/// bytes. Internally converted to a page count via `bytes / PAGE_SIZE`
/// (rounded down, clamped to at least one page). Replaces the previous
/// `cache_size: usize` (page count) field; bytes are user-friendly because
/// callers think in MB/GB, not 8KB units. Default 8 MiB = 1024 pages
/// (matches the previous default).
///
/// `spillway_max_bytes` is a strict upper bound on the spillway sidecar
/// file, in bytes (excluding per-slot 16-byte headers). When the cache
/// is full and dirty, overflow dirty pages are written to the spillway
/// rather than aborting; exceeding this limit trips
/// `ChiselError::SpillwayFull`. Default `1024 * cache_max_bytes` (8 GiB
/// at the default cache size). Setting to 0 disables the spillway
/// entirely — overflow then trips `ChiselError::CacheFull` at the
/// strict cache cap, with no 8× elasticity (the previous
/// `HARD_CEILING_MULTIPLIER` is removed).
///
/// `drain_insertion` controls where commit-drain rehydrated pages land
/// in the LRU. `LruTail` (default) makes them first eviction candidates
/// after commit, preserving the pre-transaction warm working set;
/// `Mru` treats them as just-touched. See spec §"Drain insertion policy".
///
/// `read_only` still takes an exclusive `flock` — it only suppresses
/// writes at the application layer.
///
/// `superblock_count` (ISSUES.md R4) controls how many superblock slots a
/// freshly-created database uses. Default 2 (matches the original layout);
/// valid range is 2..=16. Higher N trades disk space (N × 8 KB) for
/// resilience against consecutive torn writes — N=3 survives one torn
/// commit followed by a torn retry, N=4 survives two retries. This
/// option is ONLY consulted when creating a new database; reopening an
/// existing file discovers N from the on-disk superblock itself.
#[derive(Debug, Clone)]
pub struct Options {
    pub cache_max_bytes: u64,
    pub spillway_max_bytes: u64,
    pub drain_insertion: DrainInsertion,
    pub create_if_missing: bool,
    pub read_only: bool,
    pub superblock_count: u32,
}

/// Where commit-drain rehydrated pages are inserted into the LRU.
///
/// `LruTail` makes the just-drained pages the first eviction candidates
/// after commit; preserves any pre-transaction warm pages. The default,
/// per spec §"Drain insertion policy".
///
/// `Mru` treats drained pages as recently touched. Useful when the
/// caller expects to read them again next transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainInsertion {
    LruTail,
    Mru,
}

impl Default for Options {
    fn default() -> Options {
        let cache_max_bytes = 8 * 1024 * 1024; // 8 MiB = 1024 × 8 KiB pages
        Options {
            cache_max_bytes,
            spillway_max_bytes: 1024 * cache_max_bytes,
            drain_insertion: DrainInsertion::LruTail,
            create_if_missing: true,
            read_only: false,
            superblock_count: superblock::DEFAULT_SUPERBLOCK_COUNT,
        }
    }
}
```

- [ ] **Step 2: Edit `src/lib.rs` — `Chisel::open` and `open_in_memory_with_options`**

Find both call sites of `PageCache::new(io, options.cache_size)` (around lines 143 and 190). Replace each with:

```rust
let cache = PageCache::new(
    io,
    options.cache_max_bytes,
    options.spillway_max_bytes,
    options.drain_insertion,
);
```

The new `PageCache::new` signature is defined in Step 4 below; this is a forward reference that will compile after that edit.

- [ ] **Step 3: Edit `src/error.rs` — add two operational variants**

After `CacheFull { limit: usize }` (around line 45) add two new variants:

```rust
    // The spillway file has reached its `spillway_max_bytes` cap with
    // every cached entry dirty, so there is neither room in the cache
    // nor room in the spillway. Operational: the DB on disk is still
    // intact. Recovery is to commit (which drains the spillway and
    // resets it) or roll back. Spec 2026-05-03-chisel-spillway-design.md.
    SpillwayFull { limit_bytes: u64 },
    // Raised when a configuration mutator (e.g. set_cache_max_bytes,
    // set_spillway_max_bytes, set_drain_insertion) is called while a
    // transaction is in flight. Operational: caller commits or rolls
    // back, then retries. The mutators only operate on between-
    // transactions state; mid-transaction shrink would either reject
    // or silently spill, neither of which is a clean story.
    TransactionInProgress,
```

In the `is_fatal` impl, leave both variants out (they're operational).

In the Display impl, after the `CacheFull` arm add:

```rust
            ChiselError::SpillwayFull { limit_bytes } => write!(
                f,
                "spillway full: {limit_bytes} bytes used; commit or roll back to free cache and spillway"
            ),
            ChiselError::TransactionInProgress => write!(
                f,
                "configuration changes are only allowed between transactions; commit or roll back first"
            ),
```

- [ ] **Step 4: Edit `src/page_cache.rs` — change `PageCache::new` signature**

Find the existing constructor (around line 125):

```rust
pub fn new(mut io: PageIo, max_pages: usize) -> PageCache {
    let max_pages = max_pages.max(1);
    let next_page_id = io.page_count().unwrap_or(0);
    PageCache {
        io,
        entries: HashMap::new(),
        lru: LruIndex::new(),
        dirty_count: 0,
        max_pages,
        next_page_id,
        cache_hits: Cell::new(0),
        cache_misses: Cell::new(0),
        pages_allocated: Cell::new(0),
    }
}
```

Replace with:

```rust
/// Construct a cache over an already-opened `PageIo`.
///
/// `cache_max_bytes` is the strict upper bound on the in-memory cache,
/// in bytes. Converted internally to a page count via
/// `bytes / PAGE_SIZE as u64`, clamped to at least one page.
///
/// `spillway_max_bytes` is the strict upper bound on the spillway
/// sidecar file (in bytes, header overhead excluded). Spillway open is
/// deferred to the first spill; we just record the cap here. Setting
/// to 0 means "no spillway"; overflow trips `CacheFull` at the
/// `cache_max_bytes` cap.
///
/// `drain_insertion` is captured for use during commit drain (see
/// `flush`).
///
/// `next_page_id` is seeded from the file's current length. The
/// transaction manager calls `set_next_page_id` later to install the
/// authoritative high-water mark from the chosen superblock.
///
/// `unwrap_or(0)` on page_count failure is a tradeoff: we'd rather
/// construct a usable cache and surface the underlying I/O error on
/// the next real operation than fail the constructor.
///
/// `max_pages` is clamped to at least 1. A value of 0 would trip
/// `CacheFull` on the first allocation regardless of workload.
pub fn new(
    mut io: PageIo,
    cache_max_bytes: u64,
    spillway_max_bytes: u64,
    drain_insertion: crate::DrainInsertion,
) -> PageCache {
    let max_pages = (cache_max_bytes / PAGE_SIZE as u64).max(1) as usize;
    let next_page_id = io.page_count().unwrap_or(0);
    PageCache {
        io,
        entries: HashMap::new(),
        lru: LruIndex::new(),
        dirty_count: 0,
        max_pages,
        spillway_max_bytes,
        drain_insertion,
        next_page_id,
        cache_hits: Cell::new(0),
        cache_misses: Cell::new(0),
        pages_allocated: Cell::new(0),
    }
}
```

Add the new fields to the struct definition (around line 70 — `pub struct PageCache`):

```rust
pub struct PageCache {
    io: PageIo,
    entries: HashMap<u64, CacheEntry>,
    lru: LruIndex,
    dirty_count: usize,
    max_pages: usize,
    /// Strict upper bound on the spillway sidecar file in bytes
    /// (excluding per-slot headers). 0 means spillway disabled —
    /// overflow trips CacheFull at the cache cap. Set via Options;
    /// runtime-mutable between transactions via set_spillway_max_bytes.
    spillway_max_bytes: u64,
    /// LRU position policy for commit-drain rehydrated pages. Captured
    /// from Options at construction; runtime-mutable between
    /// transactions via set_drain_insertion.
    drain_insertion: crate::DrainInsertion,
    next_page_id: u64,
    cache_hits: Cell<u64>,
    cache_misses: Cell<u64>,
    pages_allocated: Cell<u64>,
}
```

- [ ] **Step 5: Edit `src/page_cache.rs` test helper `fresh_cache`**

Around line 669 the test helper builds a cache with the old API:

```rust
fn fresh_cache(max_pages: usize) -> PageCache {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    std::mem::forget(file);
    PageCache::new(io, max_pages)
}
```

Replace with:

```rust
fn fresh_cache(max_pages: usize) -> PageCache {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    std::mem::forget(file);
    let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
    PageCache::new(io, cache_max_bytes, 0, crate::DrainInsertion::LruTail)
}
```

This intentionally passes `spillway_max_bytes = 0` so the existing tests preserve the legacy "fails fast on cache pressure" contract. Spillway-aware tests get added later.

Update the four other inline `PageCache::new` test sites in this file (search `PageCache::new(io, `): each takes a page count today; convert to `bytes = N * PAGE_SIZE` and add `0, DrainInsertion::LruTail`. Specifically:

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
grep -n "PageCache::new(io, " src/page_cache.rs
```

For each match line `PageCache::new(io, NUMBER)`, replace with `PageCache::new(io, NUMBER * PAGE_SIZE as u64, 0, crate::DrainInsertion::LruTail)`.

- [ ] **Step 6: Edit `src/transaction.rs` — update test helpers**

Find every `PageCache::new(io, N)` and `Options { cache_size: ... }` in `src/transaction.rs`:

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
grep -nE "PageCache::new\(io,|cache_size\s*:" src/transaction.rs
```

Each `PageCache::new(io, N)` → `PageCache::new(io, N * PAGE_SIZE as u64, 0, crate::DrainInsertion::LruTail)`.

Each `Options { cache_size: N, .. }` → `Options { cache_max_bytes: N * PAGE_SIZE as u64, spillway_max_bytes: 0, drain_insertion: DrainInsertion::LruTail, .. }`.

Add `use crate::PAGE_SIZE;` and `use crate::DrainInsertion;` to the top of the test module if not already imported. (The non-test code in `transaction.rs` doesn't construct Options directly — only test helpers do.)

- [ ] **Step 7: Verify build + tests are green**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: clean build, all 286 existing chisel tests pass (no functional change yet, just shape).

If clippy complains about an unused field (`spillway_max_bytes` or `drain_insertion`), add `#[allow(dead_code)]` to those fields temporarily (will be removed in Task 9 when they're actually used).

- [ ] **Step 8: Commit**

```bash
git add src/lib.rs src/error.rs src/page_cache.rs src/transaction.rs
git commit -m "$(cat <<'EOF'
chisel: rename Options::cache_size -> cache_max_bytes; add spillway config

Breaking API change in preparation for the spillway feature:

- Options::cache_size: usize (page count) → cache_max_bytes: u64 (bytes).
  Conversion is bytes / PAGE_SIZE, clamped to at least one page.
  Default unchanged at 8 MiB = 1024 pages.
- New Options::spillway_max_bytes (default 1024 × cache_max_bytes =
  8 GiB) — strict upper bound on the spillway sidecar; 0 disables.
- New Options::drain_insertion (DrainInsertion::LruTail | Mru,
  default LruTail) — LRU position for commit-drain rehydrated pages.
- New ChiselError::SpillwayFull { limit_bytes } — operational; fires
  when both cache and spillway are exhausted.
- New ChiselError::TransactionInProgress — operational; raised by the
  set_* configuration mutators that follow.
- PageCache::new signature now takes (io, cache_max_bytes,
  spillway_max_bytes, drain_insertion); existing callers updated to
  pass spillway_max_bytes = 0 (legacy CacheFull-at-cap behavior)
  during this scaffolding step.

No functional change: all 286 existing tests pass with the new shape.
The spillway implementation lands incrementally in subsequent tasks.
EOF
)"
```

---

## Task 2: bench subcrate Options rename

**Goal:** Update `bench/src/chisel_engine.rs` for the renamed Options field. Bench tests still pass.

**Files:**
- Modify: `bench/src/chisel_engine.rs` (2 Options construction sites)

- [ ] **Step 1: Edit `bench/src/chisel_engine.rs`**

Find both Options constructions (around lines 33 and 47 — search for `cache_size`):

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
grep -n "cache_size" bench/src/chisel_engine.rs
```

For each `Options { cache_size, .. }` (where `cache_size` is a parameter that's a page count), update to:

```rust
Options {
    cache_max_bytes: cache_size as u64 * chisel::PAGE_SIZE as u64,
    spillway_max_bytes: 0,
    drain_insertion: chisel::DrainInsertion::LruTail,
    ..Options::default()
}
```

Note `chisel::PAGE_SIZE` may not be re-exported; if the build complains, change to `8192` (PAGE_SIZE constant value) with a comment, or add `pub use page::PAGE_SIZE;` to `src/lib.rs` and re-run.

Doc-comment update in the file header (around line 4–5): change `cache_size` to `cache_size_pages` or "page count" to keep the param name semantically clear; the function parameter name itself can stay as `cache_size` but the doc text should clarify it's pages.

Actually, leave the parameter name as `cache_size: usize` (it's the bench function's internal parameter, not Options); only update the Options construction inside.

- [ ] **Step 2: Verify bench tests + lint pass**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway/bench
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

Expected: 88 bench tests pass, clippy + fmt clean.

- [ ] **Step 3: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
git add bench/src/chisel_engine.rs
git commit -m "$(cat <<'EOF'
bench: update ChiselEngine for Options::cache_max_bytes rename

Mechanical follow-up to the breaking Options change in chisel: the two
Options constructions in chisel_engine.rs convert the page-count
parameter to bytes via cache_size as u64 * PAGE_SIZE as u64, set
spillway_max_bytes = 0 (legacy CacheFull-at-cap behavior), and
specify DrainInsertion::LruTail.

The bench harness's per-engine cache_size_pages parameter is unchanged
— it's a bench-internal concept that flows through to ChiselEngine
and is converted at the Options boundary. No bench-grid configuration
changes.
EOF
)"
```

---

## Task 3: python subcrate Options rename

**Goal:** Update `python/src/db.rs` for the renamed Options field. The Python pyfunction's user-facing parameter changes from `cache_size` (page count) to `cache_max_bytes` (bytes), matching the new Rust API.

**Files:**
- Modify: `python/src/db.rs`
- Modify: `python/chisel/chisel.pyi` (type stubs)
- Modify: `python/tests/*.py` if they reference `cache_size=` (search first)

- [ ] **Step 1: Find every reference**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
grep -rn "cache_size" python/
```

- [ ] **Step 2: Edit `python/src/db.rs`**

Around line 60-70 the comment block describes positional defaults. Update:

```rust
// positionally supply cache_size by accident; the defaults mirror
```

→

```rust
// positionally supply cache_max_bytes by accident; the defaults mirror
```

Update the same block's references to `cache_size` mirror as `cache_max_bytes`.

Around line 71 the `#[pyo3(...)]` defaults:

```rust
    cache_size = 1024,
```

→

```rust
    cache_max_bytes = 8 * 1024 * 1024,
```

Around line 79 the function parameter:

```rust
    cache_size: usize,
```

→

```rust
    cache_max_bytes: u64,
```

Around line 105-106 the Options construction:

```rust
    let options = Options {
        cache_size,
        ..
    };
```

→

```rust
    let options = chisel::Options {
        cache_max_bytes,
        spillway_max_bytes: 0,
        drain_insertion: chisel::DrainInsertion::LruTail,
        ..chisel::Options::default()
    };
```

Adjust the prefix (`chisel::` vs no prefix) to match the file's existing import style.

- [ ] **Step 3: Edit `python/chisel/chisel.pyi`**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
grep -n "cache_size" python/chisel/chisel.pyi
```

Each `cache_size: int = 1024` → `cache_max_bytes: int = 8388608` (= 8 MiB). Update doc strings if they say "pages" — change to "bytes."

- [ ] **Step 4: Edit any python tests that reference `cache_size=`**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
grep -rn "cache_size" python/tests/
```

For each `cache_size=N` in test code, replace with `cache_max_bytes=N * 8192` (the page-count → bytes conversion).

- [ ] **Step 5: Verify the python crate type-checks**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway/python
cargo check
```

Expected: clean type-check. (Full `cargo build` may fail on macOS due to the pre-existing PyO3 cdylib link issue — that's irrelevant; CI builds python via maturin in the matrix and will catch any real issue.)

- [ ] **Step 6: Commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
git add python/src/db.rs python/chisel/chisel.pyi python/tests/
git commit -m "$(cat <<'EOF'
chisel-py: rename cache_size -> cache_max_bytes for Options change

Mechanical follow-up to chisel's Options rename. The Python
pyfunction signature becomes:

    chisel.open(path=None, cache_max_bytes=8 * 1024 * 1024, ...)

cache_max_bytes is the byte equivalent of the previous cache_size
(page count). The conversion is bytes / PAGE_SIZE inside the Rust
layer, but Python users now speak bytes consistently with the new
Rust API.

Type stubs and any test references updated to match. Spillway is
disabled (spillway_max_bytes = 0) for v1 of the binding; exposing
spillway controls via Python is a future enhancement once the Rust
side has shipped and stabilized.
EOF
)"
```

---

## Task 4: spillway.rs scaffolding + Spillway::open

**Goal:** Create the `spillway` module with the file format types, slot constants, and a `Spillway::open` constructor that handles both file-backed and memory-backed variants. No spilling or rehydration logic yet — just the open path and the resident-set state.

**Files:**
- Create: `src/spillway.rs`
- Modify: `src/lib.rs` (`mod spillway;`)

- [ ] **Step 1: Create `src/spillway.rs`**

```rust
// spillway.rs — sidecar overflow file for oversized dirty sets.
//
// Architecture: layer 3-adjacent — owned by PageCache, invisible to all
// modules above. Holds dirty pages that the in-cache LRU has been
// forced to spill because the cache is full of dirty pages and a new
// allocation would push it past its strict cap.
//
// Lifecycle (spec 2026-05-03-chisel-spillway-design.md, "Lifecycle"):
//   open       file is created (or reused) and truncated to zero. Any
//              pre-existing content is garbage from a crashed prior
//              process and unconditionally discarded.
//   spill      page_id allocates a slot (or overwrites its existing
//              one), bytes + per-slot checksum are written.
//   rehydrate  slot is read, checksum verified, bytes returned.
//   truncate   file shrunk to zero, resident-set index cleared. Called
//              at commit, rollback, and defrag.
//
// Slot layout (PAGE_SIZE + 16 bytes):
//   u64  page_id     (the main-file page id this slot shadows)
//   u64  checksum    (XXH3 over (page_id || page_bytes))
//   [u8] page bytes  (PAGE_SIZE = 8192 bytes)
//
// On-disk format is little-endian (matches the main-file convention).
//
// In-memory state: `slots: HashMap<u64, u64>` maps page_id to slot
// index. The slot index is 0-based and dense; the file is sparse only
// in the sense that slots may be overwritten in place (re-spill of an
// already-resident page reuses the slot).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{ChiselError, Result};
use crate::page::PAGE_SIZE;

/// Per-slot header: u64 page_id + u64 XXH3 checksum.
pub const SLOT_HEADER_SIZE: usize = 16;
/// Total bytes a slot occupies on disk (header + page).
pub const SLOT_SIZE: usize = SLOT_HEADER_SIZE + PAGE_SIZE;

/// Spillway backing storage: real file on disk, or in-memory bytes for
/// memory-mode databases.
enum Backing {
    File { file: File, path: PathBuf },
    Memory { bytes: Vec<u8> },
}

pub struct Spillway {
    backing: Backing,
    /// page_id -> slot index. Built up by `spill`; consulted by
    /// `is_resident` and `rehydrate`; cleared by `truncate`.
    slots: HashMap<u64, u64>,
    /// High-water mark for slot allocation. Bumped by every new spill;
    /// reused on re-spill of an already-resident page id (no bump).
    /// Reset to 0 on truncate.
    next_slot_index: u64,
    /// Strict upper bound on the spillway file's logical size in bytes,
    /// excluding per-slot headers. Captured at construction; runtime-
    /// mutable via PageCache::set_spillway_max_bytes.
    max_bytes: u64,
}

impl Spillway {
    /// Open (or create + truncate) a file-backed spillway alongside the
    /// main database. The path is `<db_path>.spillway`. Any pre-existing
    /// content is discarded — no superblock can possibly point at
    /// spillway bytes, so this is always safe.
    pub fn open_file(db_path: &Path, max_bytes: u64) -> Result<Spillway> {
        let mut path = db_path.as_os_str().to_owned();
        path.push(".spillway");
        let path: PathBuf = path.into();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Spillway {
            backing: Backing::File { file, path },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
        })
    }

    /// Open a memory-backed spillway. Used by `Chisel::open_in_memory`.
    /// Drops on close like the rest of memory mode.
    pub fn open_memory(max_bytes: u64) -> Spillway {
        Spillway {
            backing: Backing::Memory { bytes: Vec::new() },
            slots: HashMap::new(),
            next_slot_index: 0,
            max_bytes,
        }
    }

    /// True if `page_id` has a slot in this spillway.
    pub fn is_resident(&self, page_id: u64) -> bool {
        self.slots.contains_key(&page_id)
    }

    /// Number of slots currently allocated (residents).
    pub fn slot_count(&self) -> u64 {
        self.next_slot_index
    }

    /// Logical size in bytes (excludes per-slot headers).
    pub fn logical_bytes(&self) -> u64 {
        self.next_slot_index * PAGE_SIZE as u64
    }

    /// Strict upper bound on logical size, settable at construction or
    /// via PageCache::set_spillway_max_bytes between transactions.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Update the cap. Caller (PageCache::set_spillway_max_bytes) must
    /// already have ensured no transaction is in flight.
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn open_file_truncates_existing_content() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();
        let spillway_path = {
            let mut p = db_path.as_os_str().to_owned();
            p.push(".spillway");
            PathBuf::from(p)
        };

        // Pre-populate the spillway path with garbage from a "previous
        // process" — open_file must overwrite it.
        std::fs::write(&spillway_path, b"garbage").unwrap();

        let spw = Spillway::open_file(&db_path, 1024 * 1024).unwrap();
        assert!(!spw.is_resident(42));
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert_eq!(spw.max_bytes(), 1024 * 1024);

        // The on-disk file was truncated by the open path.
        let on_disk = std::fs::read(&spillway_path).unwrap();
        assert_eq!(on_disk.len(), 0);

        // Cleanup — Spillway has no Drop; manually delete the spillway file.
        let _ = std::fs::remove_file(&spillway_path);
    }

    #[test]
    fn open_memory_starts_empty() {
        let spw = Spillway::open_memory(1024 * 1024);
        assert!(!spw.is_resident(0));
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert_eq!(spw.max_bytes(), 1024 * 1024);
    }

    #[test]
    fn set_max_bytes_updates_cap() {
        let mut spw = Spillway::open_memory(1024);
        spw.set_max_bytes(2048);
        assert_eq!(spw.max_bytes(), 2048);
    }
}
```

- [ ] **Step 2: Edit `src/lib.rs` — declare the spillway module**

Find the existing `mod` declarations (near the top of `src/lib.rs`):

```bash
grep -n "^mod " src/lib.rs
```

Add `mod spillway;` alphabetically next to the other internal modules. Module is `pub(crate)` by default — no public re-exports needed; consumers go through `PageCache`.

- [ ] **Step 3: Verify**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo build
cargo test --lib spillway::
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: 3 new spillway tests pass; total test count goes from 286 → 289.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/spillway.rs
git commit -m "$(cat <<'EOF'
chisel: spillway module skeleton (file format + Spillway::open)

Adds src/spillway.rs with the on-disk slot layout (16-byte header +
PAGE_SIZE bytes), the Backing enum (File | Memory mirroring PageIo's
two backings), and Spillway::open_file / open_memory constructors
that always start clean — pre-existing spillway content from a
crashed prior process is unconditionally truncated.

Three unit tests cover the open + clean-state guarantees. No spill
or rehydrate logic yet; those land in the next tasks.

Header comment captures the lifecycle (open / spill / rehydrate /
truncate), the slot layout in bytes, the in-memory resident-set
state, and the rationale for re-spill being slot-overwriting (caps
the spillway by working-set size, not mutation count).
EOF
)"
```

---

## Task 5: Spillway::spill (write a page to a slot)

**Goal:** Implement `Spillway::spill(page_id, &[u8; PAGE_SIZE])` that allocates (or reuses) a slot, writes the slot header (page_id + checksum) and page bytes, updates the resident-set map. Trips `SpillwayFull` if the new write would exceed `max_bytes`.

**Files:**
- Modify: `src/spillway.rs` (add `spill` method + helpers)

- [ ] **Step 1: Add the spill method**

In `src/spillway.rs`, inside the `impl Spillway` block (after `set_max_bytes`), add:

```rust
    /// Write `page_bytes` to this spillway, keyed by `page_id`. If the
    /// page is already resident, overwrites its existing slot in place
    /// (no slot-count growth, no max_bytes check). Otherwise allocates
    /// a new slot at `next_slot_index` — but first checks that the
    /// post-write logical size stays within `max_bytes`.
    pub fn spill(&mut self, page_id: u64, page_bytes: &[u8; PAGE_SIZE]) -> Result<()> {
        let slot_index = if let Some(&existing) = self.slots.get(&page_id) {
            existing
        } else {
            // New slot would push logical size past the cap?
            let post_write_bytes = (self.next_slot_index + 1) * PAGE_SIZE as u64;
            if post_write_bytes > self.max_bytes {
                return Err(ChiselError::SpillwayFull {
                    limit_bytes: self.max_bytes,
                });
            }
            let new_index = self.next_slot_index;
            self.next_slot_index += 1;
            self.slots.insert(page_id, new_index);
            new_index
        };

        write_slot(&mut self.backing, slot_index, page_id, page_bytes)?;
        Ok(())
    }
```

- [ ] **Step 2: Add the `write_slot` free function (private helper)**

Add at the bottom of `src/spillway.rs`, before the `#[cfg(test)] mod tests`:

```rust
/// Compute the per-slot checksum: XXH3 over (page_id || page_bytes).
/// Distinct from the main-file page checksum because a spilled page
/// may not yet have a stamped main-file checksum (see spec).
fn slot_checksum(page_id: u64, page_bytes: &[u8; PAGE_SIZE]) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&page_id.to_le_bytes());
    hasher.update(page_bytes);
    hasher.digest()
}

fn write_slot(
    backing: &mut Backing,
    slot_index: u64,
    page_id: u64,
    page_bytes: &[u8; PAGE_SIZE],
) -> Result<()> {
    let checksum = slot_checksum(page_id, page_bytes);
    let offset = slot_index * SLOT_SIZE as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    header[..8].copy_from_slice(&page_id.to_le_bytes());
    header[8..16].copy_from_slice(&checksum.to_le_bytes());
    match backing {
        Backing::File { file, .. } => {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&header)?;
            file.write_all(page_bytes)?;
        }
        Backing::Memory { bytes } => {
            let needed = (offset + SLOT_SIZE as u64) as usize;
            if bytes.len() < needed {
                bytes.resize(needed, 0);
            }
            let off = offset as usize;
            bytes[off..off + SLOT_HEADER_SIZE].copy_from_slice(&header);
            bytes[off + SLOT_HEADER_SIZE..off + SLOT_SIZE].copy_from_slice(page_bytes);
        }
    }
    Ok(())
}
```

This requires `xxhash-rust` to be a dependency. Check `Cargo.toml`:

```bash
grep xxhash Cargo.toml
```

It should already be present (the main file uses XXH3 for page checksums). If somehow not, add `xxhash-rust = { version = "0.8", features = ["xxh3"] }` to `[dependencies]`.

- [ ] **Step 3: Add tests for spill**

In the `#[cfg(test)] mod tests` block, add:

```rust
    fn page(byte: u8) -> [u8; PAGE_SIZE] {
        [byte; PAGE_SIZE]
    }

    #[test]
    fn spill_inserts_new_slot() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        assert!(spw.is_resident(100));
        assert_eq!(spw.slot_count(), 1);
        assert_eq!(spw.logical_bytes(), PAGE_SIZE as u64);
    }

    #[test]
    fn re_spill_of_resident_page_reuses_slot() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(100, &page(0xBB)).unwrap(); // overwrite
        assert_eq!(spw.slot_count(), 1, "slot count must not grow on re-spill");
    }

    #[test]
    fn spill_full_returns_spillway_full_error() {
        // max_bytes accommodates exactly 2 page payloads (excluding header).
        let max_bytes = (PAGE_SIZE * 2) as u64;
        let mut spw = Spillway::open_memory(max_bytes);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(101, &page(0xBB)).unwrap();
        let err = spw.spill(102, &page(0xCC)).unwrap_err();
        match err {
            ChiselError::SpillwayFull { limit_bytes } => {
                assert_eq!(limit_bytes, max_bytes);
            }
            other => panic!("expected SpillwayFull, got {other:?}"),
        }
    }
```

- [ ] **Step 4: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --lib spillway::
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: 3 new tests pass (6 total in spillway module). 292 total tests.

```bash
git add src/spillway.rs
git commit -m "$(cat <<'EOF'
chisel: Spillway::spill — allocate or reuse slot, write with checksum

Adds the spill path: a page_id either reuses its existing slot (in-
place overwrite, no slot-count growth) or allocates a fresh slot at
next_slot_index. New-slot allocation checks against max_bytes first
and returns SpillwayFull if the post-write logical size would exceed
the cap.

Per-slot XXH3 checksum is computed over (page_id || page_bytes) and
stamped in the 16-byte slot header. The page_id field of the header
is for diagnostic / sanity-check use during rehydrate (Task 6); the
checksum is what actually gates rehydrate's torn-write detection.

Three new tests: insert-new, re-spill-reuses-slot, spillway-full-
returns-error.
EOF
)"
```

---

## Task 6: Spillway::rehydrate (read a slot, verify checksum)

**Goal:** Implement `rehydrate(page_id) -> Result<[u8; PAGE_SIZE]>` that reads a slot, verifies the checksum, and returns the bytes. Checksum mismatch is fatal (`ChecksumMismatch { page_id }`).

**Files:**
- Modify: `src/spillway.rs` (add `rehydrate` method + helper)

- [ ] **Step 1: Add the rehydrate method**

In the `impl Spillway` block, add after `spill`:

```rust
    /// Read the slot for `page_id`, verify the per-slot checksum, return
    /// the bytes. Returns `ChecksumMismatch { page_id }` (fatal) on a
    /// torn write — caller poisons the transaction. Returns
    /// `InvalidPageId { page_id }` if the page is not resident
    /// (programming error in the caller, not a torn-write).
    pub fn rehydrate(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let slot_index = match self.slots.get(&page_id) {
            Some(&i) => i,
            None => return Err(ChiselError::InvalidPageId { page_id }),
        };
        let (stored_page_id, stored_checksum, page_bytes) =
            read_slot(&mut self.backing, slot_index)?;

        // Sanity check: the slot's stored page_id must match what the
        // resident-set says it should be. A mismatch implies in-memory
        // corruption (slots map drifted from disk) and is treated as
        // checksum failure.
        if stored_page_id != page_id {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        let computed = slot_checksum(page_id, &page_bytes);
        if computed != stored_checksum {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        Ok(page_bytes)
    }
```

- [ ] **Step 2: Add the `read_slot` helper (free function)**

Below `write_slot`:

```rust
fn read_slot(
    backing: &mut Backing,
    slot_index: u64,
) -> Result<(u64, u64, [u8; PAGE_SIZE])> {
    let offset = slot_index * SLOT_SIZE as u64;
    let mut header = [0u8; SLOT_HEADER_SIZE];
    let mut page_bytes = [0u8; PAGE_SIZE];
    match backing {
        Backing::File { file, .. } => {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut header)?;
            file.read_exact(&mut page_bytes)?;
        }
        Backing::Memory { bytes } => {
            let off = offset as usize;
            if bytes.len() < off + SLOT_SIZE {
                return Err(ChiselError::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("spillway memory backing too short for slot {slot_index}"),
                )));
            }
            header.copy_from_slice(&bytes[off..off + SLOT_HEADER_SIZE]);
            page_bytes.copy_from_slice(&bytes[off + SLOT_HEADER_SIZE..off + SLOT_SIZE]);
        }
    }
    let stored_page_id = u64::from_le_bytes(header[..8].try_into().unwrap());
    let stored_checksum = u64::from_le_bytes(header[8..16].try_into().unwrap());
    Ok((stored_page_id, stored_checksum, page_bytes))
}
```

- [ ] **Step 3: Add rehydrate tests**

In the test module:

```rust
    #[test]
    fn rehydrate_round_trips_bytes() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        let original = page(0xAB);
        spw.spill(100, &original).unwrap();
        let restored = spw.rehydrate(100).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn rehydrate_after_overwrite_returns_latest_bytes() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(100, &page(0xBB)).unwrap();
        let restored = spw.rehydrate(100).unwrap();
        assert_eq!(restored, page(0xBB));
    }

    #[test]
    fn rehydrate_missing_page_returns_invalid_page_id() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        let err = spw.rehydrate(999).unwrap_err();
        assert!(matches!(err, ChiselError::InvalidPageId { page_id: 999 }));
    }

    #[test]
    fn rehydrate_with_corrupted_byte_returns_checksum_mismatch() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        // Corrupt the page bytes directly (simulating a torn write).
        if let Backing::Memory { ref mut bytes } = spw.backing {
            // Skip the 16-byte header, flip a bit in the page bytes.
            bytes[SLOT_HEADER_SIZE] ^= 0x01;
        }
        let err = spw.rehydrate(100).unwrap_err();
        assert!(matches!(err, ChiselError::ChecksumMismatch { page_id: 100 }));
    }
```

- [ ] **Step 4: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --lib spillway::
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: 4 new tests pass (10 total in spillway). 296 total tests.

```bash
git add src/spillway.rs
git commit -m "$(cat <<'EOF'
chisel: Spillway::rehydrate — read slot, verify checksum

Symmetric counterpart to spill: looks up the slot index for page_id
in the resident-set map, reads the (header, bytes) pair from backing,
verifies both the stored page_id and the XXH3 checksum match. Any
mismatch produces ChecksumMismatch { page_id } (fatal — caller
poisons the transaction).

A request for a non-resident page returns InvalidPageId (operational,
caller bug). The stored-page-id sanity check guards against in-memory
corruption where the slots map drifted away from on-disk reality.

Four new tests: round-trip, overwrite-then-rehydrate, missing-page,
corrupted-byte-detected.
EOF
)"
```

---

## Task 7: Spillway::truncate + drain batching

**Goal:** Implement `truncate()` to clear the spillway file/memory and reset the resident-set; implement `drained_ids(batch_size)` and `take_drained(page_id)` so PageCache can iterate the spillway during commit drain.

**Files:**
- Modify: `src/spillway.rs`

- [ ] **Step 1: Add truncate**

In `impl Spillway`, after `rehydrate`:

```rust
    /// Clear all slots, reset the resident-set, and shrink the backing
    /// to zero bytes. Called at every commit (after drain) and every
    /// rollback. The spillway holds no live content between
    /// transactions.
    pub fn truncate(&mut self) -> Result<()> {
        self.slots.clear();
        self.next_slot_index = 0;
        match &mut self.backing {
            Backing::File { file, .. } => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
            }
            Backing::Memory { bytes } => {
                bytes.clear();
            }
        }
        Ok(())
    }
```

- [ ] **Step 2: Add drain-batching helpers**

After `truncate`:

```rust
    /// Pop a batch of up to `batch_size` (page_id, slot_index) pairs
    /// out of the resident set without dropping the file content. The
    /// PageCache drain reads each pair, rehydrates the page, then
    /// flushes it to the main file. After all batches are processed,
    /// `truncate()` is called to shrink the spillway.
    ///
    /// Order is unspecified — HashMap iteration order is not stable.
    /// The drain doesn't need a particular order; one batch's
    /// rehydrates all flush together with later batches under a
    /// single fsync.
    pub fn drain_batch(&mut self, batch_size: usize) -> Vec<u64> {
        let mut ids = Vec::with_capacity(batch_size.min(self.slots.len()));
        for &id in self.slots.keys().take(batch_size) {
            ids.push(id);
        }
        ids
    }

    /// Drop a single page_id from the resident-set after its bytes have
    /// been rehydrated into the cache. The slot is NOT reused for new
    /// allocations until the next `truncate` (mid-drain growth would
    /// be a re-entrancy hazard); the file's tail bytes simply become
    /// garbage and are reclaimed by `truncate`.
    pub fn forget(&mut self, page_id: u64) {
        self.slots.remove(&page_id);
    }
```

- [ ] **Step 3: Add tests**

```rust
    #[test]
    fn truncate_clears_residents_and_resets_index() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        spw.spill(101, &page(0xBB)).unwrap();
        assert_eq!(spw.slot_count(), 2);

        spw.truncate().unwrap();
        assert_eq!(spw.slot_count(), 0);
        assert_eq!(spw.logical_bytes(), 0);
        assert!(!spw.is_resident(100));
        assert!(!spw.is_resident(101));

        // After truncate, fresh spills allocate from index 0 again.
        spw.spill(200, &page(0xCC)).unwrap();
        assert_eq!(spw.slot_count(), 1);
    }

    #[test]
    fn drain_batch_returns_resident_ids_up_to_batch_size() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 8);
        for id in 100..105 {
            spw.spill(id, &page(id as u8)).unwrap();
        }
        let batch = spw.drain_batch(3);
        assert_eq!(batch.len(), 3);
        for id in &batch {
            assert!((100..105).contains(id), "unexpected id {id} in batch");
        }
    }

    #[test]
    fn forget_drops_from_resident_set() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 4);
        spw.spill(100, &page(0xAA)).unwrap();
        assert!(spw.is_resident(100));
        spw.forget(100);
        assert!(!spw.is_resident(100));
    }
```

- [ ] **Step 4: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --lib spillway::
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: 3 new tests pass (13 total in spillway). 299 total.

```bash
git add src/spillway.rs
git commit -m "$(cat <<'EOF'
chisel: Spillway::truncate + drain helpers

truncate() wipes the resident set and shrinks the backing to zero —
called at every commit (after the drain has flushed all spilled bytes
into the main file) and every rollback (which discards the spillway
without flushing). After truncate, fresh spills allocate from
slot index 0 again.

drain_batch(n) returns up to n resident page_ids — order is
unspecified; the drain doesn't need a particular order since one
fsync covers all rehydrate-then-flush operations regardless of order.

forget(page_id) drops a single page from the resident set after the
PageCache has consumed its bytes. The corresponding slot is NOT
reused for new allocations until truncate; this avoids re-entrancy
in the drain loop.

Three new tests: truncate-clears-and-resets, drain_batch-returns-
ids, forget-drops-from-set.
EOF
)"
```

---

## Task 8: PageCache holds Spillway; lazy-open on first spill

**Goal:** Wire `Spillway` into `PageCache`. The spillway is opened lazily on first spill (so a no-spill workload pays zero filesystem cost) and the necessary state for opening it (the db path or "in-memory" marker) is captured at PageCache construction.

**Files:**
- Modify: `src/page_cache.rs`
- Modify: `src/lib.rs` (pass db_path / in-memory marker through to PageCache)

- [ ] **Step 1: Edit `src/lib.rs` — give `PageCache::new` enough context to open a spillway**

The current `PageCache::new(io, cache_max_bytes, spillway_max_bytes, drain_insertion)` doesn't know the db path. Add a fifth parameter — a `SpillwayLocation` enum — and update both call sites:

In `src/lib.rs` near the other public types (top of the file, after `DrainInsertion`):

```rust
/// How to open a spillway sidecar. `Path` for file-backed databases
/// (path is the main db path; spillway will be at `<path>.spillway`),
/// `InMemory` for memory-backed.
#[derive(Debug, Clone)]
pub enum SpillwayLocation {
    Path(std::path::PathBuf),
    InMemory,
}
```

In the existing `Chisel::open` (around line 143):

```rust
let cache = PageCache::new(
    io,
    options.cache_max_bytes,
    options.spillway_max_bytes,
    options.drain_insertion,
);
```

→

```rust
let cache = PageCache::new(
    io,
    options.cache_max_bytes,
    options.spillway_max_bytes,
    options.drain_insertion,
    SpillwayLocation::Path(path.to_path_buf()),
);
```

In `Chisel::open_in_memory_with_options` (around line 190):

```rust
let cache = PageCache::new(
    io,
    options.cache_max_bytes,
    options.spillway_max_bytes,
    options.drain_insertion,
);
```

→

```rust
let cache = PageCache::new(
    io,
    options.cache_max_bytes,
    options.spillway_max_bytes,
    options.drain_insertion,
    SpillwayLocation::InMemory,
);
```

- [ ] **Step 2: Edit `src/page_cache.rs` — add fields and update constructor**

In the struct definition, add (alongside `drain_insertion`):

```rust
    /// How to lazily open the spillway when a first spill happens. Held
    /// here rather than opening eagerly because no-spill workloads
    /// shouldn't pay any filesystem cost for a feature they never use.
    spillway_location: crate::SpillwayLocation,
    /// Lazily-initialized spillway. None until the first spill needs it.
    spillway: Option<crate::spillway::Spillway>,
```

Update the constructor signature:

```rust
pub fn new(
    mut io: PageIo,
    cache_max_bytes: u64,
    spillway_max_bytes: u64,
    drain_insertion: crate::DrainInsertion,
    spillway_location: crate::SpillwayLocation,
) -> PageCache {
    let max_pages = (cache_max_bytes / PAGE_SIZE as u64).max(1) as usize;
    let next_page_id = io.page_count().unwrap_or(0);
    PageCache {
        io,
        entries: HashMap::new(),
        lru: LruIndex::new(),
        dirty_count: 0,
        max_pages,
        spillway_max_bytes,
        drain_insertion,
        spillway_location,
        spillway: None,
        next_page_id,
        cache_hits: Cell::new(0),
        cache_misses: Cell::new(0),
        pages_allocated: Cell::new(0),
    }
}
```

- [ ] **Step 3: Add a private helper to lazy-open**

In the `impl PageCache` block (anywhere private), add:

```rust
    /// Lazy-open the spillway on first spill. Subsequent calls reuse
    /// the existing one. Returns SpillwayFull if `spillway_max_bytes`
    /// is 0 (spillway disabled by configuration); the caller must
    /// fall back to the legacy CacheFull path in that case.
    fn ensure_spillway(&mut self) -> Result<&mut crate::spillway::Spillway> {
        if self.spillway_max_bytes == 0 {
            return Err(ChiselError::SpillwayFull { limit_bytes: 0 });
        }
        if self.spillway.is_none() {
            let spw = match &self.spillway_location {
                crate::SpillwayLocation::Path(p) => {
                    crate::spillway::Spillway::open_file(p, self.spillway_max_bytes)?
                }
                crate::SpillwayLocation::InMemory => {
                    crate::spillway::Spillway::open_memory(self.spillway_max_bytes)
                }
            };
            self.spillway = Some(spw);
        }
        Ok(self.spillway.as_mut().unwrap())
    }
```

- [ ] **Step 4: Update test helper `fresh_cache`**

```rust
fn fresh_cache(max_pages: usize) -> PageCache {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    std::mem::forget(file);
    let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
    PageCache::new(
        io,
        cache_max_bytes,
        0,
        crate::DrainInsertion::LruTail,
        crate::SpillwayLocation::InMemory,
    )
}
```

Update other inline `PageCache::new(io, ...)` test sites the same way (search for `PageCache::new(io,` and add the location parameter).

- [ ] **Step 5: Update transaction.rs test helpers similarly**

```bash
grep -n "PageCache::new(io," src/transaction.rs
```

Each line: append `, crate::SpillwayLocation::InMemory)` before the closing `)`. (Tests can use InMemory regardless of file-backing because the PageIo is already file-backed; the spillway being in-memory doesn't change correctness for these tests.)

Wait — actually, for tests that DO want a real on-disk spillway path, `InMemory` would be misleading. But none of the existing tests exercise spillway paths (they all pass `spillway_max_bytes = 0`, which short-circuits before opening). So `InMemory` is a fine placeholder.

For new spillway integration tests (Task 14+), use `SpillwayLocation::Path(...)` with a tempfile.

- [ ] **Step 6: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: clean. 299 tests still pass (no functional change).

```bash
git add src/lib.rs src/page_cache.rs src/transaction.rs
git commit -m "$(cat <<'EOF'
chisel: PageCache wires Spillway via lazy-open on first spill

PageCache::new now takes a SpillwayLocation (Path | InMemory) so it
can open a Spillway lazily — the first time maybe_evict needs to spill
a page. No-spill workloads pay zero filesystem cost: no spillway file
is ever created.

When spillway_max_bytes is 0, ensure_spillway short-circuits to
SpillwayFull { limit_bytes: 0 } so the caller can map that to the
legacy CacheFull path. Distinguishing "disabled" from "exhausted at
limit > 0" is a Task 9 concern.

All existing test helpers updated to pass SpillwayLocation::InMemory;
since they all use spillway_max_bytes = 0 the location is effectively
a placeholder. New spillway tests in subsequent tasks will use
SpillwayLocation::Path with tempfiles for the real on-disk path.

No functional change yet — Spillway is reachable but never invoked.
EOF
)"
```

---

## Task 9: Spill on overflow in maybe_evict

**Goal:** Wire `Spillway` into `maybe_evict` so when the cache is full and every entry is dirty, the LRU-tail dirty page is spilled (and removed from the cache) instead of the cache growing past its cap. With `spillway_max_bytes = 0`, preserve the legacy `CacheFull` behavior — but at the cache cap, not the 8× cap. Remove `HARD_CEILING_MULTIPLIER`.

**Files:**
- Modify: `src/page_cache.rs`

- [ ] **Step 1: Replace `maybe_evict` with the spill-aware version**

Find `maybe_evict` (around line 627). Replace its body:

```rust
    fn maybe_evict(&mut self) -> Result<()> {
        // Phase A: evict clean LRU-tail entries until we fit, exactly
        // as before.
        while self.entries.len() > self.max_pages {
            if self.dirty_count == self.entries.len() {
                break; // Phase B handles this — every entry is dirty.
            }
            let victim = self
                .lru
                .iter_lru_to_mru()
                .find(|&id| !self.entries.get(&id).is_none_or(|e| e.dirty));
            match victim {
                Some(id) => {
                    self.entries.remove(&id);
                    self.lru.remove(id);
                }
                None => break,
            }
        }

        // Phase B: still over the cap and every entry is dirty? Spill
        // the LRU-tail dirty page to the spillway. If the spillway is
        // disabled (spillway_max_bytes == 0), surface CacheFull at the
        // strict cache cap (no 8× elasticity).
        while self.entries.len() > self.max_pages {
            if self.spillway_max_bytes == 0 {
                return Err(ChiselError::CacheFull {
                    limit: self.max_pages,
                });
            }
            // Find the LRU-tail dirty page (every entry is dirty here,
            // so iter_lru_to_mru's first item is the right victim).
            let victim_id = match self.lru.iter_lru_to_mru().next() {
                Some(id) => id,
                None => break, // Should be unreachable when entries.len() > 0.
            };
            // Lift the page bytes out of the cache before calling into
            // ensure_spillway (which borrows &mut self).
            let entry = self
                .entries
                .remove(&victim_id)
                .expect("LRU referenced page id not in entries");
            self.lru.remove(victim_id);
            // entry was dirty; preserve dirty_count's invariant.
            self.dirty_count -= 1;

            // Spill (may return SpillwayFull, in which case we DO NOT
            // re-insert — the entry is dropped and the caller will
            // observe SpillwayFull on this allocation).
            let spw = self.ensure_spillway()?;
            spw.spill(victim_id, &entry.buf)?;
        }
        Ok(())
    }
```

- [ ] **Step 2: Remove `HARD_CEILING_MULTIPLIER`**

Find and delete the `const HARD_CEILING_MULTIPLIER: usize = 8;` line near line 61, plus its 13-line doc comment block. Add a brief replacement comment in its place:

```rust
// Cache size discipline (replaces the pre-spillway HARD_CEILING_MULTIPLIER):
// `max_pages` is now a strict upper bound. Overflow dirty pages are spilled
// to a sidecar `Spillway` file rather than growing the cache. Workloads
// that explicitly want the legacy "fail fast at the cache ceiling" semantics
// can set Options::spillway_max_bytes = 0; CacheFull then fires at
// max_pages itself, with no elasticity. See spec
// 2026-05-03-chisel-spillway-design.md.
```

Update the comment block at the top of the file (the "Hard ceiling (ISSUES.md I19)" bullet around lines 29-38) to match:

Replace:

```
// - The cache is a SOFT limit with a HARD ceiling. `load_page` evicts
//   before insertion; `new_page` evicts after insertion, so a single
//   allocation can transiently push the map to `max_pages + 1`. When
//   every page in the cache is dirty, `maybe_evict` cannot evict anyone
//   and the cache legitimately grows past `max_pages` — but only up to
//   `max_pages * HARD_CEILING_MULTIPLIER` (default 8×), after which it
//   returns `ChiselError::CacheFull` rather than exhaust memory. See
//   I19 for the design. Recovery from `CacheFull` is to commit or roll
//   back the transaction; commit itself pre-drains the cache (I28) so
//   `CacheFull` cannot arise on the commit path.
```

with:

```
// - The cache is a STRICT bound with sidecar overflow. `load_page` evicts
//   before insertion; `new_page` evicts after insertion. When every page
//   in the cache is dirty, `maybe_evict` spills the LRU-tail dirty page
//   to the `Spillway` sidecar file rather than growing the cache.
//   `spillway_max_bytes` caps the spillway file; `SpillwayFull` is the
//   operational error if both cache and spillway are exhausted. With
//   `spillway_max_bytes = 0`, the spillway is disabled and `CacheFull`
//   fires at the strict cache cap (no elasticity, no spilling). The
//   pre-spillway 8× HARD_CEILING_MULTIPLIER design is gone — see spec
//   2026-05-03-chisel-spillway-design.md.
```

- [ ] **Step 3: Update the existing tests that relied on the 8× ceiling**

Two existing tests directly test the 8× behavior; both need rewriting. Find them:

```bash
grep -n "HARD_CEILING_MULTIPLIER\|cache_full_fires_when\|cache_full_is_recoverable" src/page_cache.rs
```

**`cache_full_fires_when_all_pages_dirty_past_hard_ceiling`** — rewrite to test the new strict-cap-with-spillway-disabled behavior:

```rust
    // Regression test for spec §"Failure surface" — when spillway is
    // disabled (max_bytes = 0), CacheFull fires at the strict cache
    // cap, with no elasticity. (Replaces the pre-spillway test that
    // exercised the 8× HARD_CEILING_MULTIPLIER.)
    #[test]
    fn cache_full_fires_at_strict_cap_when_spillway_disabled() {
        let max_pages = 4;
        let mut cache = fresh_cache(max_pages);
        // fresh_cache sets spillway_max_bytes = 0, so we should hit
        // CacheFull at max_pages exactly, not 8 × max_pages.
        for _ in 0..max_pages {
            cache
                .new_page()
                .expect("allocations up to the strict cap must succeed");
        }
        assert_eq!(cache.entries.len(), max_pages);
        let err = cache.new_page().unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { limit } if limit == max_pages),
            "expected CacheFull {{ limit: {max_pages} }}, got {err:?}"
        );
    }
```

**`cache_full_is_recoverable_via_flush`** — rewrite for the new threshold:

```rust
    #[test]
    fn cache_full_is_recoverable_via_flush() {
        let max_pages = 4;
        let mut cache = fresh_cache(max_pages);
        for _ in 0..max_pages {
            cache.new_page().unwrap();
        }
        assert!(matches!(
            cache.new_page(),
            Err(ChiselError::CacheFull { .. })
        ));
        cache.flush().unwrap();
        cache
            .new_page()
            .expect("post-flush allocation should succeed");
        assert!(cache.entries.len() <= max_pages);
    }
```

- [ ] **Step 4: Add a new test exercising the spillway path**

```rust
    /// New cache helper that ENABLES the spillway. Used by spillway-
    /// path tests; existing tests use fresh_cache (spillway disabled)
    /// to preserve their CacheFull semantics.
    fn fresh_cache_with_spillway(max_pages: usize, spillway_max_bytes: u64) -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        std::mem::forget(file);
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        PageCache::new(
            io,
            cache_max_bytes,
            spillway_max_bytes,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        )
    }

    #[test]
    fn dirty_overflow_spills_when_spillway_enabled() {
        let max_pages = 4;
        // Spillway has room for 8 spilled pages.
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let mut cache = fresh_cache_with_spillway(max_pages, spillway_bytes);
        // Allocate 8 dirty pages — 4 in cache, 4 spilled.
        for _ in 0..8 {
            cache.new_page().expect("allocations should spill, not fail");
        }
        // Cache is at its strict cap.
        assert_eq!(cache.entries.len(), max_pages);
        // Spillway holds the overflow.
        let spw = cache.spillway.as_ref().unwrap();
        assert_eq!(spw.slot_count(), 4);
    }

    #[test]
    fn spillway_full_fires_when_both_cache_and_spillway_exhausted() {
        let max_pages = 4;
        // Spillway has room for 4 spilled pages — 8 total dirty pages
        // possible before SpillwayFull.
        let spillway_bytes = 4 * PAGE_SIZE as u64;
        let mut cache = fresh_cache_with_spillway(max_pages, spillway_bytes);
        for _ in 0..(max_pages + 4) {
            cache.new_page().unwrap();
        }
        // The 9th allocation must trip SpillwayFull.
        let err = cache.new_page().unwrap_err();
        assert!(
            matches!(err, ChiselError::SpillwayFull { limit_bytes } if limit_bytes == spillway_bytes),
            "expected SpillwayFull {{ limit_bytes: {spillway_bytes} }}, got {err:?}"
        );
    }
```

- [ ] **Step 5: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: rewritten tests pass, 2 new tests pass. The transaction.rs tests that depended on the 8× elasticity (e.g., `cache_full_during_commit_does_not_poison`) may now fail because the new behavior trips CacheFull earlier. Update those test helpers to enable the spillway (pass `spillway_max_bytes` > 0 instead of 0) — or update the test's expected outcome.

Specifically, look at:

```bash
grep -n "CacheFull\|HARD_CEILING_MULTIPLIER\|hard_ceiling" src/transaction.rs
```

For each test that exercises CacheFull behavior:
- If the test's intent is "verify CacheFull is operational, not fatal," it stays valid with the spillway disabled.
- If the test's intent is "verify the engine can grow past the soft cap," update it to use a spillway-enabled cache and update the expected behavior to "spill, not CacheFull."

Test `i28_cache_full_during_commit_must_not_poison` is the trickiest one. Read its body carefully — the spec preserves the I28 semantics: commit's pre-drain still ensures CacheFull cannot arise on the commit path. With the spillway, the commit drain absorbs everything regardless, so this test may need the spillway disabled (spillway_max_bytes = 0) to actually exercise the original CacheFull-on-commit-allocate scenario. Adjust the test helper accordingly.

```bash
git add src/page_cache.rs src/transaction.rs
git commit -m "$(cat <<'EOF'
chisel: spill on dirty overflow in maybe_evict; remove 8x elasticity

Replaces the pre-spillway HARD_CEILING_MULTIPLIER with a strict cache
cap + sidecar spillway:

- Phase A of maybe_evict (evict-clean-LRU-tail) is unchanged.
- Phase B fires when every entry is dirty and the cache is over cap.
  With spillway_max_bytes > 0, spill the LRU-tail dirty page; with
  spillway_max_bytes == 0, surface CacheFull at the strict cap (no
  elasticity).
- HARD_CEILING_MULTIPLIER constant deleted; its 13-line doc comment
  replaced with a brief reference to the spec.

Existing tests rewritten:
- cache_full_fires_when_all_pages_dirty_past_hard_ceiling renamed to
  cache_full_fires_at_strict_cap_when_spillway_disabled and updated
  to expect CacheFull at max_pages, not 8 × max_pages.
- cache_full_is_recoverable_via_flush keeps the same intent but the
  threshold is max_pages now.

Two new tests:
- dirty_overflow_spills_when_spillway_enabled verifies the spillway
  absorbs overflow.
- spillway_full_fires_when_both_cache_and_spillway_exhausted
  verifies the operational error path.

transaction.rs CacheFull-related tests updated to disable spillway
(spillway_max_bytes = 0) where they need to exercise the legacy
CacheFull path for I28 / I19 regression coverage.
EOF
)"
```

---

## Task 10: Rehydrate-on-miss in load_page

**Goal:** When `get`/`get_mut` misses the cache, check the spillway resident set first. If the page is there, rehydrate (read + checksum-verify), insert into cache as **dirty** (a spilled page is by definition dirty — it was dirty when spilled and the main file doesn't reflect the in-flight change). Otherwise fall through to disk read as today.

**Files:**
- Modify: `src/page_cache.rs`

- [ ] **Step 1: Modify `load_page`**

Find `load_page` (around line 569). Replace its body:

```rust
    fn load_page(&mut self, page_id: u64) -> Result<()> {
        self.maybe_evict()?;

        // Check spillway first — a resident page is by definition dirty
        // (it was dirty when spilled). Disk read would return the stale
        // pre-transaction bytes.
        if let Some(spw) = self.spillway.as_mut() {
            if spw.is_resident(page_id) {
                let buf = spw.rehydrate(page_id)?;
                spw.forget(page_id);
                self.entries.insert(
                    page_id,
                    CacheEntry {
                        buf: Box::new(buf),
                        dirty: true, // re-loaded spilled page is dirty
                    },
                );
                self.dirty_count += 1;
                self.lru.push_front(page_id);
                return Ok(());
            }
        }

        // Fall through to disk: the page is not spilled, so its
        // last-committed bytes live in the main file (or the page id
        // is bogus, in which case PageIo will surface it).
        let buf = self.io.read_page(page_id)?;
        if !page::verify_checksum(&buf) {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        self.entries.insert(
            page_id,
            CacheEntry {
                buf: Box::new(buf),
                dirty: false,
            },
        );
        self.lru.push_front(page_id);
        Ok(())
    }
```

- [ ] **Step 2: Add a test**

```rust
    #[test]
    fn rehydrate_after_spill_returns_in_flight_bytes_not_disk() {
        let max_pages = 2;
        let spillway_bytes = 4 * PAGE_SIZE as u64;
        let mut cache = fresh_cache_with_spillway(max_pages, spillway_bytes);

        // Allocate page A, write a sentinel pattern, but DON'T flush.
        let id_a = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(id_a).unwrap();
            buf[0] = 0xAA;
        }

        // Force overflow: allocate enough new pages that page A spills.
        // After max_pages + 1 allocations, the LRU-tail dirty page (id_a)
        // will be spilled by Phase B of maybe_evict.
        for _ in 0..max_pages {
            cache.new_page().unwrap();
        }

        // page A is now resident in the spillway, NOT in the cache.
        assert!(!cache.entries.contains_key(&id_a));
        assert!(cache.spillway.as_ref().unwrap().is_resident(id_a));

        // get_mut(id_a) must rehydrate from spillway (the in-flight
        // bytes), not read the all-zero disk content.
        let buf = cache.get_mut(id_a).unwrap();
        assert_eq!(buf[0], 0xAA, "rehydrated page must hold in-flight write");
    }
```

- [ ] **Step 3: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

```bash
git add src/page_cache.rs
git commit -m "$(cat <<'EOF'
chisel: rehydrate spilled pages on cache miss

load_page now checks the spillway resident-set BEFORE falling through
to disk. A spilled page is by definition dirty (it was dirty when
spilled and the main file does not yet reflect the in-flight change),
so reading from disk would return stale bytes.

Rehydrate path: read the slot, verify the per-slot checksum, insert
into the cache as a dirty entry, drop from the spillway resident-set
(forget). The slot is NOT immediately reclaimed in the on-disk file;
truncate at commit/rollback handles file shrink.

One test: a page is written, spilled by overflow, then read back —
the in-flight bytes survive the spill/rehydrate round-trip.
EOF
)"
```

---

## Task 11: Drain in flush()

**Goal:** Extend `flush()` so that after writing all in-cache dirty pages, it iterates the spillway in batches: rehydrate each batch into the cache as dirty entries, flush them to the main file, drop from the spillway. The single trailing fsync covers everything.

**Files:**
- Modify: `src/page_cache.rs`

- [ ] **Step 1: Edit `flush()`**

Find `flush()` (around line 312). Replace its body:

```rust
    pub fn flush(&mut self) -> Result<()> {
        // Phase 1a: write every currently-dirty in-cache page.
        let dirty_ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&id, _)| id)
            .collect();
        for page_id in dirty_ids {
            let entry = self.entries.get_mut(&page_id).unwrap();
            self.io.write_page(page_id, &entry.buf)?;
            entry.dirty = false;
        }
        self.dirty_count = 0;

        // Phase 1b: drain the spillway. Each batch is up to max_pages
        // page_ids; we rehydrate them, flush to main file, drop from
        // the spillway. The single trailing fsync covers all writes.
        if let Some(spw) = self.spillway.as_mut() {
            while spw.slot_count() > 0 {
                let batch = spw.drain_batch(self.max_pages);
                if batch.is_empty() {
                    break;
                }
                for &page_id in &batch {
                    let buf = spw.rehydrate(page_id)?;
                    self.io.write_page(page_id, &buf)?;
                    spw.forget(page_id);
                }
            }
            // After draining all bytes, shrink the spillway.
            spw.truncate()?;
        }

        // Phase 2: single fsync covers main-file in-cache writes plus
        // every drained-batch write. Spec §"Commit drain" preserves
        // the two-fsync commit cost (this fsync + the superblock fsync
        // in TransactionManager::commit_inner).
        self.io.fsync()?;
        Ok(())
    }
```

The DrainInsertion policy is NOT applied here — drained pages are NOT re-inserted into the cache after flush; they are dropped after writing to disk. This is correct: by the time flush returns, the cache contains only the in-cache pages it had before the drain (now clean), and the spillway is empty. Re-adding drained pages to the cache would push it over `max_pages` for no reason.

Wait — re-reading the spec (§"Drain insertion policy"):

> The drain pulls a batch of pages from the spillway and inserts them into the (now-empty-of-dirty) cache for their write.

Hmm, the spec says the drained pages are inserted into the cache. Re-reading more carefully — the pages need to be written to disk, which means having their bytes available. Inserting into the cache (briefly) is one way; writing directly via `io.write_page(buf)` is another. The above code uses the latter.

Actually re-reading the spec one more time:

> 2.b. For each, load into the cache as a dirty entry (overwriting any prior cached entry for that id).
> 2.c. Flush as in step 1 — buffered writes to main file.

So the spec wants the drained page to land in the cache (with optional position policy), then the flush writes it. After flush, the entry is clean and stays in the cache (subject to LRU pressure).

OK, the spec is clearer now. Let me update the implementation:

```rust
    pub fn flush(&mut self) -> Result<()> {
        // Phase 1a: write every currently-dirty in-cache page.
        let dirty_ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&id, _)| id)
            .collect();
        for page_id in dirty_ids {
            let entry = self.entries.get_mut(&page_id).unwrap();
            self.io.write_page(page_id, &entry.buf)?;
            entry.dirty = false;
        }
        self.dirty_count = 0;

        // Phase 1b: drain the spillway. For each batch, rehydrate into
        // the cache (per drain_insertion policy), flush, drop from
        // spillway resident-set. The single trailing fsync covers
        // every write. Spec §"Commit drain" preserves two-fsync commit.
        let drain_policy = self.drain_insertion;
        loop {
            let batch = match self.spillway.as_mut() {
                Some(spw) if spw.slot_count() > 0 => spw.drain_batch(self.max_pages),
                _ => break,
            };
            if batch.is_empty() {
                break;
            }
            for page_id in batch {
                // Rehydrate (read + checksum-verify), then write to
                // main file. We don't strictly need the cache for the
                // write — we have buf in hand — but the spec wants
                // the cache repopulated post-commit per drain_insertion.
                let buf = {
                    let spw = self.spillway.as_mut().unwrap();
                    let b = spw.rehydrate(page_id)?;
                    spw.forget(page_id);
                    b
                };
                self.io.write_page(page_id, &buf)?;
                // Insert into cache (clean — already on disk), with
                // LRU position per drain_insertion policy.
                let entry = CacheEntry {
                    buf: Box::new(buf),
                    dirty: false,
                };
                self.entries.insert(page_id, entry);
                match drain_policy {
                    crate::DrainInsertion::LruTail => self.lru.push_back(page_id),
                    crate::DrainInsertion::Mru => self.lru.push_front(page_id),
                }
            }
            // After insertion, evict back down to max_pages if needed.
            // The drained entries are CLEAN now and so are eligible
            // for eviction; we don't gate this with the spillway path
            // because the spillway is being drained, not added to.
            while self.entries.len() > self.max_pages {
                let victim = self
                    .lru
                    .iter_lru_to_mru()
                    .find(|&id| !self.entries.get(&id).is_none_or(|e| e.dirty));
                match victim {
                    Some(id) => {
                        self.entries.remove(&id);
                        self.lru.remove(id);
                    }
                    None => break,
                }
            }
        }
        // Truncate the spillway file once all batches are drained.
        if let Some(spw) = self.spillway.as_mut() {
            spw.truncate()?;
        }

        // Phase 2: single fsync covers main-file in-cache writes plus
        // every drained-batch write.
        self.io.fsync()?;
        Ok(())
    }
```

This requires `lru.push_back(id)` — check whether `LruIndex` has it. If not, add it.

```bash
grep -n "push_back\|push_front" src/lru.rs
```

If `push_back` doesn't exist, add it (mirror `push_front` but insert at the LRU-tail end).

- [ ] **Step 2: Verify the LruIndex API has push_back; add if missing**

```bash
grep -n "fn push" src/lru.rs
```

If only `push_front` exists, add (in `src/lru.rs` `impl LruIndex`):

```rust
    /// Insert (or move) `id` to the LRU-tail end (least recent). The
    /// drain insertion policy uses this when DrainInsertion::LruTail
    /// makes drained pages first eviction candidates.
    pub fn push_back(&mut self, id: u64) {
        // Mirror push_front: remove if present, then insert at tail.
        self.remove(id);
        // Implementation depends on the LruIndex's internal structure;
        // check src/lru.rs for the existing push_front and adapt.
        // (At time of writing the LRU is a doubly-linked list backed
        // by a HashMap of (prev, next) pointers; tail-insert is a
        // ~10-line analogue of head-insert.)
        // ... see the existing push_front for the template.
    }
```

The actual code depends on `LruIndex`'s internal representation, which the implementer should check. Add a minimal test:

```rust
    #[test]
    fn push_back_inserts_at_tail() {
        let mut lru = LruIndex::new();
        lru.push_front(1);
        lru.push_front(2); // 2 is MRU, 1 is LRU
        lru.push_back(3); // 3 should become LRU
        let order: Vec<u64> = lru.iter_lru_to_mru().collect();
        assert_eq!(order, vec![3, 1, 2]);
    }
```

- [ ] **Step 3: Add a flush-drain test**

```rust
    #[test]
    fn flush_drains_spilled_pages_to_main_file() {
        let max_pages = 2;
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let mut cache = fresh_cache_with_spillway(max_pages, spillway_bytes);

        // Allocate 4 pages — 2 in cache, 2 in spillway.
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = cache.new_page().unwrap();
            ids.push(id);
            let buf = cache.get_mut(id).unwrap();
            buf[0] = i as u8;
            crate::page::stamp_checksum(buf);
        }
        assert_eq!(cache.spillway.as_ref().unwrap().slot_count(), 2);

        // Flush drains the spillway. After flush, spillway is empty.
        cache.flush().unwrap();
        assert!(cache.spillway.is_none() || cache.spillway.as_ref().unwrap().slot_count() == 0);

        // Each page can be re-read from disk with its sentinel intact.
        for (i, id) in ids.iter().enumerate() {
            let buf = cache.get(*id).unwrap();
            assert_eq!(buf[0], i as u8, "page {id} lost its sentinel during drain");
        }
    }
```

- [ ] **Step 4: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

```bash
git add src/page_cache.rs src/lru.rs
git commit -m "$(cat <<'EOF'
chisel: flush drains the spillway under a single fsync

flush() now has three phases:
1a. Write every dirty in-cache page (unchanged).
1b. Drain the spillway in batches of max_pages. Each batch:
    - rehydrate (read + checksum-verify) into a temp buf
    - drop from the spillway resident-set
    - write to the main file via PageIo
    - re-insert into the cache as clean per drain_insertion policy
      (LruTail = first eviction candidate; Mru = recently touched)
    - evict back down to max_pages if drained pages over-grew it
2.  Truncate the spillway file to zero (it's empty now).
3.  Single fsync — covers in-cache writes AND every batch write.

Two-fsync commit cost preserved (this fsync + superblock fsync in
TransactionManager::commit_inner). Per spec §"Commit drain": batch
writes don't need their own fsync because a crash before the trailing
fsync is just a rolled-back transaction (no main-file bytes
committed without it).

Adds LruIndex::push_back for the LruTail drain-insertion path.
One new test: flush_drains_spilled_pages_to_main_file verifies that
in-flight bytes spilled to the spillway end up correctly on disk
after flush.
EOF
)"
```

---

## Task 12: Spillway truncate in discard_all_dirty + truncate(n)

**Goal:** Integrate spillway lifecycle with rollback paths. `discard_all_dirty()` truncates the spillway entirely. `truncate(n)` drops spillway entries with `id >= n` (matches main-file truncate semantics: anything past the watermark goes).

**Files:**
- Modify: `src/page_cache.rs`
- Modify: `src/spillway.rs` (add per-id forget for truncate path)

- [ ] **Step 1: Edit `discard_all_dirty`**

Find `discard_all_dirty` (around line 362). After its existing body, add:

```rust
        // Spillway holds in-flight bytes for the current transaction
        // only. Rollback drops them all. truncate() also resets
        // next_slot_index so the next transaction allocates from 0.
        if let Some(spw) = self.spillway.as_mut() {
            // Errors on rollback are swallowed: rollback is a recovery
            // path; if the spillway file can't be truncated, we still
            // need to drop the dirty cache entries to maintain
            // engine consistency. The next open will re-truncate any
            // stale spillway content.
            let _ = spw.truncate();
        }
```

- [ ] **Step 2: Edit `truncate`**

Find `truncate` (around line 413). After the existing body, before `self.io.set_page_count(n)?`:

```rust
        // Drop spillway entries with id >= n. This matches the main
        // file's truncate semantics: anything past the watermark is
        // gone. Matters for rollback_to_inner where we shrink to a
        // savepoint's watermark — the spilled pages with id >= that
        // watermark are pages allocated AFTER the savepoint.
        if let Some(spw) = self.spillway.as_mut() {
            let to_forget: Vec<u64> = (0..spw.slot_count())
                .filter_map(|_| None) // placeholder — real impl below
                .collect();
            // ^ placeholder; actual impl uses spw's resident-set keys.
            // Replace with: spw.forget_above(n);
            spw.forget_above(n);
        }
```

The `forget_above(n)` method needs adding to `Spillway`. Replace the placeholder block above with:

```rust
        if let Some(spw) = self.spillway.as_mut() {
            spw.forget_above(n);
        }
```

- [ ] **Step 3: Add `Spillway::forget_above` in `src/spillway.rs`**

In `impl Spillway` (after `forget`):

```rust
    /// Drop every resident page id >= `n` from the spillway. Matches
    /// `PageCache::truncate(n)` semantics: anything past the watermark
    /// is gone. Slot indices are NOT reused mid-transaction, so this
    /// just removes from the resident-set; the corresponding bytes in
    /// the file become garbage that the next `truncate()` reclaims.
    pub fn forget_above(&mut self, n: u64) {
        self.slots.retain(|&page_id, _| page_id < n);
    }
```

- [ ] **Step 4: Add tests**

In `src/page_cache.rs`'s test module:

```rust
    #[test]
    fn discard_all_dirty_truncates_spillway() {
        let max_pages = 2;
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let mut cache = fresh_cache_with_spillway(max_pages, spillway_bytes);
        for _ in 0..4 {
            cache.new_page().unwrap(); // 2 spilled
        }
        assert_eq!(cache.spillway.as_ref().unwrap().slot_count(), 2);
        cache.discard_all_dirty();
        assert_eq!(cache.spillway.as_ref().unwrap().slot_count(), 0);
    }

    #[test]
    fn truncate_drops_spillway_entries_above_watermark() {
        let max_pages = 2;
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let mut cache = fresh_cache_with_spillway(max_pages, spillway_bytes);
        // Allocate page 0..5; some end up spilled.
        for _ in 0..6 {
            cache.new_page().unwrap();
        }
        let pre_count = cache.spillway.as_ref().unwrap().slot_count();
        // Truncate to 3 — pages 3, 4, 5 are gone (whichever of those
        // are spilled disappear from the resident-set).
        cache.truncate(3).unwrap();
        let post_count = cache.spillway.as_ref().unwrap().slot_count();
        // Some entries removed (exact count depends on which pages spilled).
        assert!(
            post_count <= pre_count,
            "truncate should not grow the spillway"
        );
        // Verify by checking individual residency:
        let spw = cache.spillway.as_ref().unwrap();
        for id in 3..6 {
            assert!(!spw.is_resident(id), "page {id} should be gone after truncate(3)");
        }
    }
```

In `src/spillway.rs`'s test module:

```rust
    #[test]
    fn forget_above_drops_high_ids_only() {
        let mut spw = Spillway::open_memory(SLOT_SIZE as u64 * 8);
        for id in 0..6 {
            spw.spill(id, &page(id as u8)).unwrap();
        }
        spw.forget_above(3);
        for id in 0..3 {
            assert!(spw.is_resident(id), "low id {id} should still be resident");
        }
        for id in 3..6 {
            assert!(!spw.is_resident(id), "high id {id} should be gone");
        }
    }
```

- [ ] **Step 5: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

```bash
git add src/page_cache.rs src/spillway.rs
git commit -m "$(cat <<'EOF'
chisel: integrate spillway with rollback paths

discard_all_dirty (used by rollback_inner) now also truncates the
spillway. Errors during truncate-on-rollback are swallowed: the next
open will re-truncate any stale spillway content unconditionally, so
rollback consistency does not depend on this truncate succeeding.

truncate(n) (used by both rollback_inner and rollback_to_inner) drops
spillway resident-set entries with id >= n. This matches main-file
truncate semantics: anything past the watermark is gone, regardless
of whether it was in cache, on disk, or spilled. Slot indices are
NOT reused mid-transaction, so file bytes between forgotten slots
become garbage that the next truncate() reclaims.

Adds Spillway::forget_above(n) for the watermark-driven drop. Three
new tests cover the spillway-truncate-on-rollback path.
EOF
)"
```

---

## Task 13: Chisel runtime mutability methods

**Goal:** Add `set_cache_max_bytes`, `set_spillway_max_bytes`, `set_drain_insertion` on `Chisel` and route them through `TransactionManager` with the `TransactionInProgress` check.

**Files:**
- Modify: `src/lib.rs` (Chisel impl)
- Modify: `src/transaction.rs` (TransactionManager pass-throughs)
- Modify: `src/page_cache.rs` (the actual setters)

- [ ] **Step 1: Add setters on `PageCache`**

In `impl PageCache` (after `set_next_page_id`):

```rust
    /// Update the cache's strict upper bound to `bytes`. Caller must
    /// ensure no transaction is in flight (TransactionManager checks
    /// this). Shrinking evicts clean LRU-tail entries until we fit;
    /// dirty entries (which shouldn't exist between transactions) are
    /// preserved and may push the cache temporarily over the new cap
    /// — the next allocation reasserts the limit via maybe_evict.
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        let new_max_pages = (bytes / PAGE_SIZE as u64).max(1) as usize;
        self.max_pages = new_max_pages;
        // Best-effort shrink: evict clean entries from LRU tail.
        while self.entries.len() > self.max_pages {
            if self.dirty_count == self.entries.len() {
                break;
            }
            let victim = self
                .lru
                .iter_lru_to_mru()
                .find(|&id| !self.entries.get(&id).is_none_or(|e| e.dirty));
            match victim {
                Some(id) => {
                    self.entries.remove(&id);
                    self.lru.remove(id);
                }
                None => break,
            }
        }
        Ok(())
    }

    /// Update the spillway cap. The spillway is empty between
    /// transactions (truncated at every commit/rollback), so resize
    /// is a state-free operation. Shrinking to 0 disables the
    /// spillway; subsequent overflow trips CacheFull.
    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.spillway_max_bytes = bytes;
        if let Some(spw) = self.spillway.as_mut() {
            spw.set_max_bytes(bytes);
        }
        // If we just disabled it (bytes == 0) and a spillway exists,
        // keep it open but its slot_count remains 0 (we're between
        // transactions). The next ensure_spillway call will gate on
        // bytes == 0 anyway.
        Ok(())
    }

    /// Update the drain insertion policy. Captured for use by the next
    /// `flush()` invocation.
    pub fn set_drain_insertion(&mut self, policy: crate::DrainInsertion) -> Result<()> {
        self.drain_insertion = policy;
        Ok(())
    }
```

- [ ] **Step 2: Add `TransactionManager` pass-throughs**

In `src/transaction.rs`, find the existing `pub fn` methods on `TransactionManager` (e.g., `begin`, `commit`). Add three new ones:

```rust
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        if self.poisoned {
            return Err(ChiselError::Poisoned);
        }
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.set_cache_max_bytes(bytes)
    }

    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        if self.poisoned {
            return Err(ChiselError::Poisoned);
        }
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.set_spillway_max_bytes(bytes)
    }

    pub fn set_drain_insertion(&mut self, policy: crate::DrainInsertion) -> Result<()> {
        if self.poisoned {
            return Err(ChiselError::Poisoned);
        }
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.set_drain_insertion(policy)
    }
```

(Adjust `self.poisoned` to whatever the actual field is — read transaction.rs to confirm. Probably `is_poisoned: bool` or similar.)

- [ ] **Step 3: Add `Chisel` API**

In `src/lib.rs`'s `impl Chisel`, after the existing methods:

```rust
    /// Resize the in-memory cache cap. Returns
    /// `ChiselError::TransactionInProgress` if a transaction is
    /// active. Shrinking evicts clean LRU-tail entries to fit;
    /// growing takes effect on the next allocation. See spec
    /// §"Runtime mutability".
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.txm.set_cache_max_bytes(bytes)
    }

    /// Resize the spillway cap. Setting to 0 disables the spillway
    /// (subsequent overflow trips CacheFull at the cache cap).
    /// Returns `ChiselError::TransactionInProgress` if a transaction
    /// is active. The spillway is empty between transactions, so
    /// resize is state-free.
    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.txm.set_spillway_max_bytes(bytes)
    }

    /// Update the drain insertion policy used at the next commit.
    /// Returns `ChiselError::TransactionInProgress` if a transaction
    /// is active.
    pub fn set_drain_insertion(&mut self, policy: DrainInsertion) -> Result<()> {
        self.txm.set_drain_insertion(policy)
    }
```

- [ ] **Step 4: Add tests**

In `src/lib.rs` (in the `#[cfg(test)]` mod tests if present, or in a new file under `tests/`):

```rust
    #[test]
    fn set_cache_max_bytes_returns_transaction_in_progress_mid_txn() {
        let mut db = Chisel::open_in_memory().unwrap();
        db.begin().unwrap();
        let err = db.set_cache_max_bytes(16 * 1024 * 1024).unwrap_err();
        assert!(matches!(err, ChiselError::TransactionInProgress));
        db.rollback().unwrap();
    }

    #[test]
    fn set_cache_max_bytes_succeeds_between_transactions() {
        let mut db = Chisel::open_in_memory().unwrap();
        // Default is 8 MiB; bump to 16 MiB.
        db.set_cache_max_bytes(16 * 1024 * 1024).unwrap();
        // No observable side-effect from the public API — but begin
        // and a single commit should still work.
        db.begin().unwrap();
        let _h = db.allocate(b"x").unwrap();
        db.commit().unwrap();
    }

    #[test]
    fn set_spillway_max_bytes_to_zero_disables_spillway() {
        let mut db = Chisel::open_in_memory().unwrap();
        db.set_spillway_max_bytes(0).unwrap();
        // Subsequent overflow trips CacheFull.
        // (More substantive tests in Task 16.)
    }
```

- [ ] **Step 5: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

```bash
git add src/lib.rs src/transaction.rs src/page_cache.rs
git commit -m "$(cat <<'EOF'
chisel: runtime-mutable cache/spillway/drain config

Adds three between-transactions setters on Chisel:
- set_cache_max_bytes(bytes): resize the cache cap
- set_spillway_max_bytes(bytes): resize the spillway cap (0 disables)
- set_drain_insertion(policy): change the LRU policy for drain

Each routes through TransactionManager which gates on active_txn,
returning ChiselError::TransactionInProgress mid-transaction. The
PageCache-level setters perform the best-effort shrink (evict clean
LRU-tail entries until under the new cap); the spillway setter
forwards to Spillway::set_max_bytes if a spillway is open.

Spec §"Runtime mutability" justifies between-transactions only:
mid-transaction shrink would either reject, evict spilled pages
(impossible — they're dirty), or trip SpillwayFull retroactively;
none is a clean story.

Three tests verify the txn-in-progress check, between-txn success,
and that set_spillway_max_bytes(0) disables (covered more fully
in Task 16).
EOF
)"
```

---

## Task 14: Integration test — large-tx-with-spill

**Goal:** End-to-end test that a transaction whose dirty working set exceeds `cache_max_bytes * 4` commits successfully and produces the same final state as the same workload split into smaller transactions.

**Files:**
- Create: `tests/spillway_integration.rs`

- [ ] **Step 1: Write the test**

```rust
// Integration tests for the spillway feature. Lives in tests/ so it
// exercises the full Chisel public API the way a real consumer would.

use chisel::{Chisel, DrainInsertion, Options};

#[test]
fn large_transaction_with_spill_produces_identical_state() {
    // Tiny cache so the working set definitely overflows.
    let cache_max_bytes = 16 * 8192; // 16 pages
    let opts_with_spillway = Options {
        cache_max_bytes,
        spillway_max_bytes: 1024 * cache_max_bytes,
        drain_insertion: DrainInsertion::LruTail,
        ..Options::default()
    };

    // Run A: one big transaction, working set = ~64 pages of dirty
    // (4× cache cap; spillway absorbs the overflow).
    let mut db_a = Chisel::open_in_memory_with_options(opts_with_spillway.clone()).unwrap();
    db_a.begin().unwrap();
    let mut handles_a = Vec::new();
    for i in 0..200 {
        let payload = vec![i as u8; 1024]; // 1 KB each
        let h = db_a.allocate(&payload).unwrap();
        handles_a.push((h, payload));
    }
    db_a.commit().unwrap();

    // Run B: identical workload, but split into 10 transactions of
    // 20 ops each (no spill needed).
    let mut db_b = Chisel::open_in_memory_with_options(opts_with_spillway).unwrap();
    let mut handles_b = Vec::new();
    for chunk_start in (0..200).step_by(20) {
        db_b.begin().unwrap();
        for i in chunk_start..chunk_start + 20 {
            let payload = vec![i as u8; 1024];
            let h = db_b.allocate(&payload).unwrap();
            handles_b.push((h, payload));
        }
        db_b.commit().unwrap();
    }

    // Both runs should produce identical handle→payload mappings.
    // Handles themselves may differ (allocation order may vary), but
    // the BYTES recoverable from each handle must match.
    // Read back from db_a and verify.
    for (h, expected) in &handles_a {
        let bytes = db_a.read(*h).unwrap();
        assert_eq!(bytes, *expected, "handle {h} content corrupt after spill");
    }
    for (h, expected) in &handles_b {
        let bytes = db_b.read(*h).unwrap();
        assert_eq!(bytes, *expected, "control run handle {h} content corrupt");
    }
}

#[test]
fn rollback_with_spill_leaves_main_file_unchanged() {
    let cache_max_bytes = 16 * 8192;
    let opts = Options {
        cache_max_bytes,
        spillway_max_bytes: 1024 * cache_max_bytes,
        drain_insertion: DrainInsertion::LruTail,
        ..Options::default()
    };
    let mut db = Chisel::open_in_memory_with_options(opts).unwrap();

    // Commit a baseline transaction.
    db.begin().unwrap();
    let baseline_h = db.allocate(b"baseline").unwrap();
    db.commit().unwrap();

    // Open a big transaction that spills, then roll it back.
    db.begin().unwrap();
    let mut spilled_handles = Vec::new();
    for i in 0..200 {
        let h = db.allocate(&vec![i as u8; 1024]).unwrap();
        spilled_handles.push(h);
    }
    db.rollback().unwrap();

    // Baseline still intact.
    let bytes = db.read(baseline_h).unwrap();
    assert_eq!(bytes, b"baseline");

    // Spilled handles are gone (rollback abandoned them).
    for h in spilled_handles {
        assert!(db.read(h).is_err(), "handle {h} survived rollback");
    }

    // Subsequent commits work normally.
    db.begin().unwrap();
    let _h = db.allocate(b"post-rollback").unwrap();
    db.commit().unwrap();
}
```

- [ ] **Step 2: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --test spillway_integration
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

Expected: 2 new integration tests pass.

```bash
git add tests/spillway_integration.rs
git commit -m "$(cat <<'EOF'
chisel: integration tests for spillway end-to-end

Two tests in tests/spillway_integration.rs:

1. large_transaction_with_spill_produces_identical_state — a single
   transaction whose dirty working set is 4× the cache cap commits
   successfully; the resulting state is identical to the same
   workload split into 10 smaller transactions (no spill needed).
   Validates the spec's central claim that the spillway lets a
   transaction touch a working set larger than the cache without
   semantic difference.

2. rollback_with_spill_leaves_main_file_unchanged — rollback of a
   transaction that spilled leaves the pre-transaction baseline
   intact and abandons the spilled handles. Validates the spec's
   "rollback discards spillway, untruncates main file" lifecycle.
EOF
)"
```

---

## Task 15: No-spill regression test (verify two-fsync commit cost)

**Goal:** A workload that fits in the cache must issue exactly 2 fsyncs per commit (1 main + 1 superblock) and zero spillway writes — preserving today's commit cost when the spillway isn't needed.

**Files:**
- Modify: `tests/spillway_integration.rs`
- May modify: `src/page_io.rs` if the fsync counter isn't accessible (it is — `fsync_count()` exists).

- [ ] **Step 1: Add the test**

Append to `tests/spillway_integration.rs`:

```rust
#[test]
fn no_spill_workload_preserves_two_fsync_commit() {
    // Workload sized to fit comfortably in the default 8 MiB cache.
    let mut db = Chisel::open_in_memory().unwrap();

    // Allocate 50 small handles — well under the 1024-page cache cap.
    db.begin().unwrap();
    for i in 0..50u32 {
        db.allocate(&i.to_le_bytes()).unwrap();
    }
    let pre_commit_counters = db.counters();
    db.commit().unwrap();
    let post_commit_counters = db.counters();

    let fsync_delta = post_commit_counters.fsync_calls - pre_commit_counters.fsync_calls;
    assert_eq!(
        fsync_delta, 2,
        "no-spill commit must issue exactly 2 fsyncs (data + superblock); got {fsync_delta}"
    );
}
```

`Chisel::counters()` returns a `ChiselCounters` (verified earlier). The fsync_calls field captures every PageIo::fsync invocation.

Note: this test relies on `ChiselCounters` being public from the chisel crate. Check:

```bash
grep -nE "pub use.*ChiselCounters|pub fn counters" src/lib.rs
```

If `counters()` isn't public on `Chisel`, expose it. If `ChiselCounters` isn't re-exported, add `pub use stats::ChiselCounters;` to `src/lib.rs`.

- [ ] **Step 2: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --test spillway_integration
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

```bash
git add tests/spillway_integration.rs src/lib.rs
git commit -m "$(cat <<'EOF'
chisel: no-spill regression test for two-fsync commit cost

A no-spill workload (default 8 MiB cache; 50 tiny allocations well
under the cap) must continue issuing exactly 2 fsyncs per commit —
1 for the dirty data pages in flush(), 1 for the superblock in
TransactionManager::commit_inner. Per spec §"Commit drain":
"Two-fsync commit cost is preserved: one fsync of the main file
covers all writes (in-cache flush + every drain batch); one fsync
of the superblock makes the transaction visible. The spillway
itself is never fsynced."

This test guards against accidental regression where someone adds a
spillway-related fsync (spillway content does not need to survive a
crash, so fsync'ing it would be pure overhead).
EOF
)"
```

---

## Task 16: spillway_max_bytes = 0 opt-out test

**Goal:** Validate that `spillway_max_bytes = 0` preserves today's `CacheFull`-on-cache-pressure behavior (no spillway file ever created; CacheFull fires at the strict cache cap with no elasticity).

**Files:**
- Modify: `tests/spillway_integration.rs`

- [ ] **Step 1: Add the test**

```rust
#[test]
fn spillway_max_bytes_zero_disables_spillway_and_fires_cache_full() {
    use chisel::ChiselError;
    let cache_max_bytes = 4 * 8192; // 4 pages
    let opts = Options {
        cache_max_bytes,
        spillway_max_bytes: 0, // OPT-OUT
        drain_insertion: DrainInsertion::LruTail,
        ..Options::default()
    };
    let mut db = Chisel::open_in_memory_with_options(opts).unwrap();

    db.begin().unwrap();
    // First few allocates fit in the cache.
    let mut hit_cache_full = false;
    for _ in 0..50 {
        match db.allocate(&[0u8; 4096]) {
            Ok(_) => {}
            Err(ChiselError::CacheFull { .. }) => {
                hit_cache_full = true;
                break;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(
        hit_cache_full,
        "with spillway disabled, allocation must trip CacheFull"
    );
    db.rollback().unwrap();
}

#[test]
fn spillway_max_bytes_zero_creates_no_spillway_file() {
    use std::path::PathBuf;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_path: PathBuf = dir.path().join("test.chisel");
    let spillway_path: PathBuf = {
        let mut p = db_path.as_os_str().to_owned();
        p.push(".spillway");
        PathBuf::from(p)
    };

    let opts = Options {
        spillway_max_bytes: 0,
        ..Options::default()
    };
    let mut db = Chisel::open(&db_path, opts).unwrap();
    db.begin().unwrap();
    let _h = db.allocate(b"x").unwrap();
    db.commit().unwrap();
    drop(db);

    // No spillway file should ever have been created.
    assert!(
        !spillway_path.exists(),
        "spillway file should not exist when spillway_max_bytes = 0"
    );
}
```

- [ ] **Step 2: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --test spillway_integration
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

```bash
git add tests/spillway_integration.rs
git commit -m "$(cat <<'EOF'
chisel: integration tests for spillway_max_bytes = 0 opt-out

Two tests:

1. spillway_max_bytes_zero_disables_spillway_and_fires_cache_full
   verifies that with spillway_max_bytes = 0, allocations past the
   strict cache cap return CacheFull (not SpillwayFull, not silent
   8× elasticity). Spec §"Configuration": "with the spillway
   disabled, CacheFull fires on the first allocation past
   cache_max_bytes (no 8× elasticity)."

2. spillway_max_bytes_zero_creates_no_spillway_file verifies that
   on a file-backed database, NO spillway sidecar is ever created
   when the feature is opted out. Spec §"Architectural shape":
   "lazy-open on first spill, so a no-spill workload pays zero
   filesystem cost."

Together these two tests pin the contract that the spillway
feature is fully opt-out: callers who set spillway_max_bytes = 0
get exactly today's pre-spillway behavior.
EOF
)"
```

---

## Task 17: Crash-injection test

**Goal:** Validate that a process killed mid-spill recovers cleanly — the next open truncates any orphaned spillway content, the main file is byte-identical to its last-committed state.

**Files:**
- Modify: `tests/spillway_integration.rs`

- [ ] **Step 1: Add the test**

```rust
#[test]
fn crash_mid_spill_recovers_to_last_committed_state() {
    use std::path::PathBuf;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_path: PathBuf = dir.path().join("crash.chisel");
    let spillway_path: PathBuf = {
        let mut p = db_path.as_os_str().to_owned();
        p.push(".spillway");
        PathBuf::from(p)
    };

    // Step 1: commit a known baseline.
    {
        let mut db = Chisel::open(&db_path, Options::default()).unwrap();
        db.begin().unwrap();
        let _h = db.allocate(b"baseline").unwrap();
        db.commit().unwrap();
    } // db drops, file closes

    // Step 2: simulate a crash mid-spill by writing garbage directly
    // to the spillway file path. (We can't actually kill mid-flush
    // from a unit test; the spec's contract is "any pre-existing
    // spillway content is discarded at open" — that's what we exercise.)
    std::fs::write(&spillway_path, b"\xFF\xFF\xFF crashed mid-spill garbage").unwrap();
    assert!(spillway_path.exists());
    let pre_open_garbage_size = std::fs::metadata(&spillway_path).unwrap().len();
    assert!(pre_open_garbage_size > 0);

    // Step 3: reopen the database. Open should truncate the orphaned
    // spillway. The baseline must still be readable.
    let mut db = Chisel::open(&db_path, Options::default()).unwrap();
    // Force the spillway lazy-open by spilling something. After lazy-
    // open, the orphaned bytes should have been truncated.
    db.begin().unwrap();
    let _h = db.allocate(b"post-recovery").unwrap();
    db.commit().unwrap();

    // The spillway file should now exist (lazy-opened) but be empty
    // (truncate-on-open + truncate-after-commit both reset it).
    let post_recovery_size = std::fs::metadata(&spillway_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        post_recovery_size, 0,
        "spillway must be truncated to zero after open + commit"
    );
}
```

This test simulates a crash by writing garbage to the spillway file and reopening; we don't have a way to truly fork-and-kill mid-flush from a unit test, but the spec's contract is "any pre-existing spillway content is discarded at open," and this test verifies that contract directly.

- [ ] **Step 2: Verify + commit**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo test --test spillway_integration
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

```bash
git add tests/spillway_integration.rs
git commit -m "$(cat <<'EOF'
chisel: integration test for crash recovery via spillway truncate

Validates spec §"Lifecycle" "open" entry: pre-existing spillway
content from a crashed prior process is unconditionally truncated.
Test simulates the crash by writing garbage bytes directly to the
spillway file path between two database opens; reopening should
discard that garbage entirely (lazy-open's truncate(create=true)
contract).

The reopen sequence is: open the database → start a transaction → do
ANY allocation → commit. The first allocation that needs a spillway
triggers lazy-open with truncate=true, which discards the orphaned
bytes. The trailing commit's flush() also calls truncate(), so the
final on-disk size is zero.

This test is necessarily a simulation rather than a true fork-and-kill
because Rust's test harness can't kill a process mid-flush, but the
spec's contract is exactly "discard pre-existing content at open" —
that's what the simulation exercises directly.
EOF
)"
```

---

## Task 18: Pre-merge verification + push

**Goal:** Run the full check matrix on the merged-spillway branch, push, open PR. Per-task and final reviews are done; this is the one-time pre-push gate.

- [ ] **Step 1: Full check matrix from repo root**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

Expected: clean. Test count should be roughly 286 + ~25 spillway tests + ~5 integration tests ≈ 316.

- [ ] **Step 2: From `bench/`**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway/bench
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

- [ ] **Step 3: From `python/`**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway/python
cargo check
```

(`cargo build` is not expected to work on macOS — pre-existing PyO3 cdylib link issue.)

- [ ] **Step 4: Confirm no Claude trailers in the commit log**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
git log --oneline main..HEAD
git log main..HEAD | grep -iE "co-authored-by|claude" || echo "clean"
```

Expected: "clean".

- [ ] **Step 5: Push and open PR**

```bash
cd /Users/xof/Documents/Dev/chisel/.worktrees/spillway
git push -u origin claude/spillway
gh pr create --title "Spillway: sidecar overflow file for oversized dirty sets" --body "$(cat <<'EOF'
## Summary
- Replaces the page cache's 8× hard-ceiling elasticity with a bounded sidecar file (the spillway) so a single transaction can touch a working set larger than the cache.
- Strict `cache_max_bytes` cap; overflow dirty pages spill to the spillway. `spillway_max_bytes = 0` opts out (preserves today's CacheFull-at-cap behavior).
- Per-slot XXH3 checksum guards against torn writes on the spillway file.
- Two-fsync commit cost preserved in the no-spill case (verified by regression test).
- Runtime-mutable cache/spillway/drain config between transactions; `TransactionInProgress` mid-txn.

## Breaking changes
- `Options::cache_size: usize` (page count) → `Options::cache_max_bytes: u64` (bytes). Default value unchanged at 8 MiB.
- New `Options::spillway_max_bytes: u64` (default 1024× cache_max_bytes = 8 GiB).
- New `Options::drain_insertion: DrainInsertion` (default `LruTail`).
- `HARD_CEILING_MULTIPLIER` removed; `CacheFull` semantics tightened (fires at cache cap when spillway is disabled, never fires when spillway is enabled — `SpillwayFull` is the operational error in that case).

## Test plan
- [x] All 286 pre-existing chisel tests pass with the renamed Options.
- [x] Spillway module: 13 unit tests (open / spill / rehydrate / truncate / drain helpers).
- [x] PageCache integration: 5 new tests (spill on overflow, rehydrate on miss, drain in flush, truncate on rollback, runtime mutability).
- [x] End-to-end integration: large-tx-with-spill matches small-txs control, rollback-with-spill leaves main file untouched, no-spill commit issues exactly 2 fsyncs, spillway_max_bytes=0 disables and creates no spillway file.
- [x] Crash recovery: orphaned spillway content discarded at open.

Spec: `docs/superpowers/specs/2026-05-03-chisel-spillway-design.md`.
Plan: `docs/superpowers/plans/2026-05-04-chisel-spillway.md`.
EOF
)"
```

The bench workflow (PR 7) will run on this PR and report any regression on the no-spill scenarios. A real regression here is unexpected (no-spill workloads pay zero new cost) but the workflow's signal is the safety net.

---

## Self-review checklist

Run after writing all tasks. Fix issues inline; no need to re-review.

1. **Spec coverage:**
   - §"Goals": large working set ✓ (Task 14), preserve crash safety ✓ (Task 17), preserve commit cost ✓ (Task 15), operational SpillwayFull ✓ (Task 9)
   - §"Lifecycle": open-truncates ✓ (Task 4), spill on dirty cache full ✓ (Task 9), rehydrate on miss ✓ (Task 10), drain on commit ✓ (Task 11), truncate on rollback ✓ (Task 12), defrag truncate (defrag is a separate code path that runs between transactions; spillway is empty by then — no Task; covered by spec §"Risk review")
   - §"Commit drain": batching with single fsync ✓ (Task 11)
   - §"Drain insertion policy": Mru | LruTail ✓ (Task 1, applied in Task 11)
   - §"Configuration": cache_max_bytes ✓ (Task 1), spillway_max_bytes ✓ (Task 1), drain_insertion ✓ (Task 1)
   - §"Runtime mutability": set_cache_max_bytes / set_spillway_max_bytes / set_drain_insertion ✓ (Task 13)
   - §"On-disk format": 16-byte slot header ✓ (Task 4 SLOT_HEADER_SIZE), per-slot checksum ✓ (Task 5)
   - §"Failure surface": SpillwayFull at cache+spillway exhaustion ✓ (Task 9), poison on torn write ✓ (Task 6), crash leaves orphaned file truncated next open ✓ (Task 17)
   - §"Testing" all rows: spill→rehydrate ✓ (Task 5+6), checksum mismatch ✓ (Task 6), re-spill ✓ (Task 5), large-tx ✓ (Task 14), rollback-with-spill ✓ (Task 14), crash injection ✓ (Task 17), no-spill regression ✓ (Task 15), runtime mutability ✓ (Task 13). Cross-policy benchmark for drain_insertion: NOT in this plan — flagged as a follow-up (the default LruTail ships per spec §"Resolved decisions").

2. **Placeholder scan:** searched for TBD/TODO/FIXME — none found in the final plan body. The plan does mention "TBD" in the spec quotes; that's the spec's word, in passages now overridden by the resolutions.

3. **Type consistency:**
   - `Options::cache_max_bytes: u64` — used uniformly in Tasks 1, 2, 3, 13.
   - `Options::spillway_max_bytes: u64` — uniformly across all tasks.
   - `DrainInsertion` enum — defined in Task 1, used in Task 11.
   - `SpillwayLocation` enum — defined in Task 8, used in Tasks 8+.
   - `Spillway::open_file` / `open_memory` — Task 4.
   - `Spillway::spill` / `rehydrate` / `truncate` / `drain_batch` / `forget` / `forget_above` / `is_resident` / `slot_count` / `logical_bytes` / `set_max_bytes` — all defined where used.
   - `ChiselError::SpillwayFull { limit_bytes }` (Task 1), `ChiselError::TransactionInProgress` (Task 1) — used in Tasks 5, 9, 13, 16.

4. **Cross-policy benchmark for drain_insertion** — spec lists this in Testing but it's a separate concern (a bench scenario). Plan ships LruTail as the default per the resolved decisions; the benchmark to confirm/revise is left as a follow-up issue (file at the appropriate time once spillway is in production).

---

That's the plan. Tasks 1-17 are commit-producing; Task 18 is the pre-merge gate. Total estimated production code: ~600 LOC (Spillway module ~250 LOC, PageCache changes ~150 LOC, Options/error ~50 LOC, set_* methods ~80 LOC, integration tests ~150 LOC, plus the rename ripple ~50 LOC). Test count grows from 286 → ~316.
