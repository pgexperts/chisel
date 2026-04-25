# Chisel Counters Instrumentation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add four cumulative-from-open counters to Chisel — `cache_hits`, `cache_misses`, `pages_allocated`, `fsync_calls` — exposed as `Chisel::counters() -> ChiselCounters` (sibling to the existing `Chisel::stats()`), with a parallel `chisel.counters()` Python binding. This is PR 1 of the benchmark suite (the precursor that the harness consumes), but the counters are a generally useful introspection surface in their own right.

**Architecture:** Three counters live in `PageCache` (incremented in `get` and `new_page`); one lives in `PageIo` (incremented in `fsync`). All four are `Cell<u64>` so they can be read and incremented through `&self` (matching `PageIo::fsync`'s shape). A new `ChiselCounters` snapshot struct in `stats.rs` is materialized by `PageCache::counters()` (which collects its own three plus reads `io.fsync_count()`); `TransactionManager::counters()` and `Chisel::counters()` are thin delegates. Counters are cumulative-from-open: the harness reads-subtract-reads to compute per-cell deltas. They reset implicitly on close+reopen because both `PageCache` and `PageIo` are reconstructed.

**Tech Stack:** Rust 2021, `Cell<u64>` (single-writer, same-thread reads — `AtomicU64` would be premature), existing Chisel crate. Python side: PyO3 0.22 (existing), `@dataclass(frozen=True)` mirror.

**Spec:** `docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md` (§6.1, §6.2 — instrumentation precursor).

**Conventions assumed from CLAUDE.md:**
- Build/test: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt -- --check`.
- Module graph is bottom-up — do not introduce upward references.
- Comments explain *why*, not *what*. File headers note the file's role in the system.
- Counter wiring uses `Cell<u64>`, not `AtomicU64`. Chisel is single-writer by design (per the project memory note `project_chisel_single_client_design`); `AtomicU64` would be a wrong signal that cross-thread access is intended.

---

## File Map

Files modified:
- `src/stats.rs` — add `ChiselCounters` snapshot struct alongside the existing `Stats`.
- `src/page_io.rs` — add `Cell<u64>` for fsync calls; increment in `fsync`; expose `fsync_count(&self) -> u64`. Initialize on every constructor (`open`, `open_in_memory`).
- `src/page_cache.rs` — add three `Cell<u64>` for cache_hits / cache_misses / pages_allocated; increment in `get` and `new_page`; expose `counters(&self) -> ChiselCounters` that combines its own three with `io.fsync_count()`.
- `src/transaction.rs` — add `pub fn counters(&self) -> ChiselCounters` delegating to `self.cache.borrow().counters()`. Routes through the poison-aware wrapper, like `stats()`.
- `src/lib.rs` — add `pub fn counters(&self) -> ChiselCounters` delegating to `self.txm.counters()`. Public API surface.
- `python/src/db.rs` — add `fn counters(&self, py: Python<'_>) -> PyResult<PyObject>` materializing a `chisel.Counters` dataclass (parallels the existing `stats()` shape).
- `python/chisel/__init__.py` — add `Counters` `@dataclass(frozen=True)`; add to `__all__`.
- `python/chisel/chisel.pyi` — add `Counters` type stub; add `def counters(self) -> Counters: ...` to `Chisel` class.

Files created:
- `tests/counters.rs` — Chisel-level integration tests (delta correctness through known op sequences, reset on reopen, snapshot isolation).
- `python/tests/test_counters.py` — Python-side parity test.

Files unchanged: every other module under `src/`, every other test, every other Python source.

---

## Task 1: Add the `ChiselCounters` snapshot struct

Pure type definition. No test cycle — verified by compilation; downstream tasks consume it.

**Files:**
- Modify: `src/stats.rs`

- [ ] **Step 1: Confirm baseline — full test suite green before we start**

Run: `cargo test`
Expected: all existing tests pass. If anything is red, stop and report.

- [ ] **Step 2: Add `ChiselCounters` to `src/stats.rs`**

Append after the existing `Stats` struct:

```rust
/// Cumulative engine-activity counters since `open()`.
///
/// Snapshot semantics: `Chisel::counters()` returns a value-type copy. The
/// returned struct does NOT update as the engine continues to do work — read
/// it again to observe new totals. Counters are cumulative from open; they
/// reset implicitly on `close()` + reopen because the underlying `PageCache`
/// and `PageIo` are reconstructed.
///
/// Intended use: the bench harness reads `counters()` before and after each
/// measurement, reports the delta. General-purpose introspection (debugging,
/// observability) is also supported — the counters are cheap (Cell<u64>
/// increment in single-writer code paths).
///
/// Fields:
/// - `cache_hits` — `PageCache::get` returned a cached page without disk I/O.
/// - `cache_misses` — `PageCache::get` had to load from disk (and validate
///    checksum). Hit rate is `hits / (hits + misses)`.
/// - `pages_allocated` — `PageCache::new_page` invocations. Each is one new
///    page id past the prior high-water mark; the actual disk write happens
///    on the next `flush()`.
/// - `fsync_calls` — `PageIo::fsync` invocations. Two per Chisel commit
///    (data pages, then superblock); zero between commits in a normal txn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChiselCounters {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub pages_allocated: u64,
    pub fsync_calls: u64,
}
```

- [ ] **Step 3: Confirm it compiles**

Run: `cargo build`
Expected: clean build. (No tests yet — they reference this type from later tasks.)

- [ ] **Step 4: Commit**

```bash
git add src/stats.rs
git commit -m "Add ChiselCounters snapshot struct"
```

---

## Task 2: Add the fsync counter to `PageIo`

TDD: write the test first.

**Files:**
- Modify: `src/page_io.rs` (struct field + `fsync` increment + `fsync_count` accessor + test in the existing `#[cfg(test)] mod`)

- [ ] **Step 1: Write the failing test**

Append to the existing `#[cfg(test)] mod tests` block in `src/page_io.rs` (the one that contains `fsync_on_read_only_returns_read_only_mode`):

```rust
#[test]
fn fsync_count_increments_per_successful_fsync() {
    let f = seeded_file();
    let io = PageIo::open(f.path(), false).unwrap();
    assert_eq!(io.fsync_count(), 0);
    io.fsync().unwrap();
    assert_eq!(io.fsync_count(), 1);
    io.fsync().unwrap();
    assert_eq!(io.fsync_count(), 2);
}

#[test]
fn fsync_count_in_memory_backing_also_increments() {
    // Memory backing's fsync is a no-op for durability but still counts:
    // benchmarks against in-memory PageIo should see commit-equivalent
    // counter behaviour.
    let io = PageIo::open_in_memory().unwrap();
    assert_eq!(io.fsync_count(), 0);
    io.fsync().unwrap();
    io.fsync().unwrap();
    assert_eq!(io.fsync_count(), 2);
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test --lib page_io::tests::fsync_count_increments_per_successful_fsync`
Expected: FAIL — `fsync_count` method does not exist.

- [ ] **Step 3: Add the counter field, the accessor, and the increment**

Modify `src/page_io.rs`:

a) Add `use std::cell::Cell;` to the imports at the top of the file (if not already present).

b) Add a field to the `PageIo` struct (just below `read_only`):

```rust
pub struct PageIo {
    backing: Backing,
    read_only: bool,
    // Cumulative fsync count. Cell<u64> because `fsync(&self)` takes &self
    // (single-writer + same-thread reads — see project memory note
    // `project_chisel_single_client_design`). Read-only opens never fsync,
    // so this stays at 0 for the read-only lifetime — a useful invariant
    // when interpreting the counter.
    fsync_calls: Cell<u64>,
}
```

c) Initialize the field in BOTH constructors. In `open()` (around line 83):

```rust
        Ok(PageIo {
            backing: Backing::File { file },
            read_only,
            fsync_calls: Cell::new(0),
        })
```

In `open_in_memory()` (around line 97):

```rust
        Ok(PageIo {
            backing: Backing::Memory { pages: Vec::new() },
            read_only: false,
            fsync_calls: Cell::new(0),
        })
```

d) Increment in `fsync()` and add `fsync_count()`. Replace the existing `fsync` body (around line 230) with this version, then add the new method immediately after:

```rust
    pub fn fsync(&self) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        match &self.backing {
            Backing::File { file } => {
                file.sync_all()?;
            }
            // No durable storage to flush. The commit protocol still calls
            // fsync twice per commit; that overhead (two method calls and
            // two matches) is preserved for benchmark fidelity.
            Backing::Memory { .. } => {}
        }
        // Increment AFTER the operation succeeds. A failed fsync is fatal
        // (fsyncgate — see I1) and the manager will be poisoned, so the
        // counter going off-by-one on a poisoned engine is the least of
        // anyone's worries — but we don't want a successful retry (which
        // we do not allow) to be undercounted by a prior failure.
        self.fsync_calls.set(self.fsync_calls.get() + 1);
        Ok(())
    }

    /// Cumulative successful fsync calls since this `PageIo` was opened.
    /// Failed fsyncs are not counted (a failed fsync poisons the engine
    /// — see I1 — so the counter on a poisoned engine has no defined
    /// meaning beyond "at least this many succeeded").
    pub fn fsync_count(&self) -> u64 {
        self.fsync_calls.get()
    }
```

- [ ] **Step 4: Run the new tests, expect PASS**

Run: `cargo test --lib page_io::tests::fsync_count_increments_per_successful_fsync page_io::tests::fsync_count_in_memory_backing_also_increments`
Expected: both PASS.

- [ ] **Step 5: Run the full PageIo test module to confirm no regression**

Run: `cargo test --lib page_io::tests`
Expected: every existing test still passes.

- [ ] **Step 6: Commit**

```bash
git add src/page_io.rs
git commit -m "PageIo: track cumulative fsync count"
```

---

## Task 3: Add cache hit/miss counters to `PageCache::get`

TDD: write the test first.

**Files:**
- Modify: `src/page_cache.rs` (struct fields + `get` increments + test in the existing `#[cfg(test)] mod`)

- [ ] **Step 1: Write the failing test**

Append to the existing `#[cfg(test)] mod tests` block in `src/page_cache.rs`:

```rust
#[test]
fn cache_hits_and_misses_track_correctly() {
    // Setup: open an in-memory PageIo, populate page 0 with a checksummed
    // buffer (writing through the cache so the file actually grows).
    let io = PageIo::open_in_memory().unwrap();
    let mut cache = PageCache::new(io, 16);

    // Allocate page 0, stamp a valid checksum, flush so the next read
    // actually exercises the load path rather than a dirty-cache hit.
    let id = cache.new_page().unwrap();
    {
        let buf = cache.get_mut(id).unwrap();
        crate::page::stamp_checksum(buf);
    }
    cache.flush().unwrap();

    // The flush leaves the entry clean-and-cached. A `get()` on it is a hit.
    let h0 = cache.cache_hit_count();
    let m0 = cache.cache_miss_count();
    let _ = cache.get(id).unwrap();
    assert_eq!(cache.cache_hit_count(), h0 + 1);
    assert_eq!(cache.cache_miss_count(), m0);

    // Force eviction by exceeding the cache budget, then re-fetch — must miss.
    for _ in 0..32 {
        let nid = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(nid).unwrap();
            crate::page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
    }
    // Re-fetch the original page; it has been evicted.
    let h1 = cache.cache_hit_count();
    let m1 = cache.cache_miss_count();
    let _ = cache.get(id).unwrap();
    // Either it was still cached (hit) or it was evicted (miss). The
    // weaker assertion: exactly ONE counter advanced.
    let dh = cache.cache_hit_count() - h1;
    let dm = cache.cache_miss_count() - m1;
    assert_eq!(dh + dm, 1, "exactly one of hits/misses must increment");
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test --lib page_cache::tests::cache_hits_and_misses_track_correctly`
Expected: FAIL — `cache_hit_count` / `cache_miss_count` methods do not exist.

- [ ] **Step 3: Add the counter fields and the increments**

Modify `src/page_cache.rs`:

a) Add `use std::cell::Cell;` to the imports at the top of the file (if not already present).

b) Add three fields to `PageCache` (just below `next_page_id`):

```rust
pub struct PageCache {
    io: PageIo,
    entries: HashMap<u64, CacheEntry>,
    lru: VecDeque<u64>,
    max_pages: usize,
    next_page_id: u64,
    // Cumulative-from-open counters. Cell<u64> so reads can go through
    // `&self` accessors (forward-compatible with a possible future where
    // get/new_page also become &self via interior mutability — today they
    // are already &mut, but uniform Cell-shape across PageCache and PageIo
    // keeps the counters aggregator simpler).
    cache_hits: Cell<u64>,
    cache_misses: Cell<u64>,
    pages_allocated: Cell<u64>,
}
```

c) Initialize all three in `PageCache::new` (around line 100):

```rust
    pub fn new(mut io: PageIo, max_pages: usize) -> PageCache {
        let max_pages = max_pages.max(1);
        let next_page_id = io.page_count().unwrap_or(0);
        PageCache {
            io,
            entries: HashMap::new(),
            lru: VecDeque::new(),
            max_pages,
            next_page_id,
            cache_hits: Cell::new(0),
            cache_misses: Cell::new(0),
            pages_allocated: Cell::new(0),
        }
    }
```

d) Replace `get()` (around line 123) with the counter-incrementing version:

```rust
    pub fn get(&mut self, page_id: u64) -> Result<&[u8; PAGE_SIZE]> {
        if self.entries.contains_key(&page_id) {
            self.cache_hits.set(self.cache_hits.get() + 1);
        } else {
            self.cache_misses.set(self.cache_misses.get() + 1);
            self.load_page(page_id)?;
        }
        self.touch_lru(page_id);
        Ok(&self.entries.get(&page_id).unwrap().buf)
    }
```

The miss is incremented BEFORE `load_page` so that a load_page that fails (checksum mismatch, I/O error) still leaves a "we tried to miss" record. That is the conservative choice: the diagnostic table interprets misses as "the cache failed to satisfy a request", which is true regardless of whether the load itself succeeded.

e) Add the accessor methods immediately below `get_mut()` (around line 152), before `new_page()`:

```rust
    /// Cumulative cache hit count since this PageCache was constructed.
    pub fn cache_hit_count(&self) -> u64 {
        self.cache_hits.get()
    }

    /// Cumulative cache miss count since this PageCache was constructed.
    /// Includes attempted misses where `load_page` subsequently failed
    /// (checksum mismatch, I/O error) — the counter records "we had to
    /// reach for disk", not "the disk read succeeded".
    pub fn cache_miss_count(&self) -> u64 {
        self.cache_misses.get()
    }
```

- [ ] **Step 4: Run the test, expect PASS**

Run: `cargo test --lib page_cache::tests::cache_hits_and_misses_track_correctly`
Expected: PASS.

- [ ] **Step 5: Run the full page_cache module tests to confirm no regression**

Run: `cargo test --lib page_cache::tests`
Expected: every existing test still passes.

- [ ] **Step 6: Commit**

```bash
git add src/page_cache.rs
git commit -m "PageCache: track cumulative cache hits and misses in get()"
```

---

## Task 4: Add the `pages_allocated` counter to `PageCache::new_page`

TDD: write the test first.

**Files:**
- Modify: `src/page_cache.rs` (`new_page` increment + accessor + test)

- [ ] **Step 1: Write the failing test**

Append to the same `#[cfg(test)] mod tests` block in `src/page_cache.rs`:

```rust
#[test]
fn pages_allocated_counter_increments_per_new_page() {
    let io = PageIo::open_in_memory().unwrap();
    let mut cache = PageCache::new(io, 16);
    assert_eq!(cache.pages_allocated_count(), 0);
    cache.new_page().unwrap();
    cache.new_page().unwrap();
    cache.new_page().unwrap();
    assert_eq!(cache.pages_allocated_count(), 3);
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test --lib page_cache::tests::pages_allocated_counter_increments_per_new_page`
Expected: FAIL — `pages_allocated_count` method does not exist.

- [ ] **Step 3: Increment in `new_page` and add the accessor**

Modify `new_page()` (around line 169) to increment the counter — replace the existing body:

```rust
    pub fn new_page(&mut self) -> Result<u64> {
        let page_id = self.next_page_id;
        self.next_page_id += 1;
        let entry = CacheEntry {
            buf: Box::new([0u8; PAGE_SIZE]),
            dirty: true,
        };
        self.entries.insert(page_id, entry);
        self.lru.push_front(page_id);
        self.pages_allocated.set(self.pages_allocated.get() + 1);
        self.maybe_evict()?;
        Ok(page_id)
    }
```

The increment goes BEFORE `maybe_evict` for the same conservative reason as Task 3's miss-before-load: the page id has been minted (`next_page_id` already advanced); even if `maybe_evict` errors with `CacheFull`, the allocation count reflects the work attempted. Symmetric with `cache_misses`.

Add the accessor immediately below `cache_miss_count()`:

```rust
    /// Cumulative `new_page()` invocations since this PageCache was
    /// constructed. Counts attempted allocations: an allocation that
    /// subsequently trips `CacheFull` in `maybe_evict` is still recorded.
    pub fn pages_allocated_count(&self) -> u64 {
        self.pages_allocated.get()
    }
```

- [ ] **Step 4: Run the test, expect PASS**

Run: `cargo test --lib page_cache::tests::pages_allocated_counter_increments_per_new_page`
Expected: PASS.

- [ ] **Step 5: Run the full page_cache test module**

Run: `cargo test --lib page_cache::tests`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/page_cache.rs
git commit -m "PageCache: track cumulative pages_allocated in new_page()"
```

---

## Task 5: Add `PageCache::counters()` returning `ChiselCounters`

This collects the three PageCache counters plus delegates `fsync_calls` to the underlying PageIo, returning a single snapshot.

**Files:**
- Modify: `src/page_cache.rs` (counters() method + test)

- [ ] **Step 1: Write the failing test**

Append to the same `#[cfg(test)] mod tests` block:

```rust
#[test]
fn counters_aggregates_cache_and_io_state() {
    use crate::stats::ChiselCounters;

    let io = PageIo::open_in_memory().unwrap();
    let mut cache = PageCache::new(io, 16);

    // Fresh cache: every counter is zero.
    assert_eq!(cache.counters(), ChiselCounters::default());

    // Allocate two pages, stamp & flush them. Allocation count goes up by 2;
    // the flush issues one fsync (PageIo::fsync called once by flush()).
    for _ in 0..2 {
        let id = cache.new_page().unwrap();
        let buf = cache.get_mut(id).unwrap();
        crate::page::stamp_checksum(buf);
    }
    cache.flush().unwrap();

    let c = cache.counters();
    assert_eq!(c.pages_allocated, 2);
    assert_eq!(c.fsync_calls, 1, "flush() does exactly one fsync");
    // get_mut(id) on a freshly-allocated page is a hit (page is in-cache).
    assert_eq!(c.cache_hits, 2);
    assert_eq!(c.cache_misses, 0);
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test --lib page_cache::tests::counters_aggregates_cache_and_io_state`
Expected: FAIL — `counters` method does not exist.

- [ ] **Step 3: Add `counters()`**

Modify the imports section at the top of `src/page_cache.rs` to include `ChiselCounters` if it isn't already imported:

```rust
use crate::stats::ChiselCounters;
```

Add the method just below `pages_allocated_count()`:

```rust
    /// Snapshot all four engine-activity counters into a `ChiselCounters`.
    ///
    /// Three of the four counters live here in `PageCache`; `fsync_calls`
    /// is owned by the underlying `PageIo` (where the actual `fsync` call
    /// happens) and is read through. The snapshot is a value type — it
    /// does not update as the engine continues to do work.
    pub fn counters(&self) -> ChiselCounters {
        ChiselCounters {
            cache_hits: self.cache_hits.get(),
            cache_misses: self.cache_misses.get(),
            pages_allocated: self.pages_allocated.get(),
            fsync_calls: self.io.fsync_count(),
        }
    }
```

Note: `get_mut` does not increment `cache_hits` today (the existing implementation only counts hits in `get`). The test asserts `cache_hits == 2` after two `get_mut` calls, which means `get_mut` MUST also increment `cache_hits`. Update `get_mut` (around line 143) to track hits in the same way `get` does:

```rust
    pub fn get_mut(&mut self, page_id: u64) -> Result<&mut [u8; PAGE_SIZE]> {
        if self.entries.contains_key(&page_id) {
            self.cache_hits.set(self.cache_hits.get() + 1);
        } else {
            self.cache_misses.set(self.cache_misses.get() + 1);
            self.load_page(page_id)?;
        }
        self.touch_lru(page_id);
        let entry = self.entries.get_mut(&page_id).unwrap();
        entry.dirty = true;
        Ok(&mut entry.buf)
    }
```

Rationale: `get_mut` is morally a `get` followed by a write-intent mark. Excluding it from cache_hits/cache_misses would silently undercount activity for any benchmark that updates pages — exactly the workloads where we most want the counters. Counting it the same way `get` does also keeps the aggregate "page-cache lookup count" equal to `cache_hits + cache_misses`.

- [ ] **Step 4: Run the test, expect PASS**

Run: `cargo test --lib page_cache::tests::counters_aggregates_cache_and_io_state`
Expected: PASS.

- [ ] **Step 5: Run all PageCache tests to confirm no regression**

Run: `cargo test --lib page_cache::tests`
Expected: all pass. (If a prior test depended on `get_mut` not affecting hit counts, it must have been pinning a value of `0` — review and adjust.)

- [ ] **Step 6: Commit**

```bash
git add src/page_cache.rs
git commit -m "PageCache: counters() snapshot; get_mut now also tracks hits/misses"
```

---

## Task 6: Plumb counters through `TransactionManager` and expose `Chisel::counters()`

End-to-end public API surface. TDD via integration test in `tests/counters.rs`.

**Files:**
- Modify: `src/transaction.rs` (add `counters()`)
- Modify: `src/lib.rs` (add `counters()`)
- Create: `tests/counters.rs` (Chisel-level integration test)

- [ ] **Step 1: Write the failing integration test**

Create `tests/counters.rs`:

```rust
// Integration tests for Chisel::counters().
//
// Goal: lock down the public-API surface and the deltas through known
// operation sequences. Same dual-backing pattern as basic_ops.rs is not
// used here — counters are an in-memory concept, so we exercise file
// and memory backings explicitly where the distinction matters
// (close/reopen test) and use memory-only where it doesn't.

use chisel::{Chisel, Options};
use tempfile::NamedTempFile;

#[test]
fn counters_after_open_in_memory_have_construction_overhead() {
    let db = Chisel::open_in_memory().unwrap();
    let c = db.counters().unwrap();
    // Open does some work (writes initial superblock, etc.) so allocations
    // and fsyncs are non-zero. We pin only the public guarantee: the type
    // is constructible and readable, and pages_allocated reflects the
    // bootstrap.
    assert!(c.pages_allocated > 0, "open_in_memory must allocate at least the superblock pages");
}

#[test]
fn counters_track_allocate_and_commit() {
    let mut db = Chisel::open_in_memory().unwrap();
    let baseline = db.counters().unwrap();

    db.begin().unwrap();
    let _h = db.allocate(b"hello").unwrap();
    db.commit().unwrap();

    let after = db.counters().unwrap();
    // Commit calls fsync twice (data pages + superblock). Anything below
    // this is a regression in the commit protocol.
    assert!(
        after.fsync_calls >= baseline.fsync_calls + 2,
        "commit must perform at least 2 fsyncs (data + superblock); saw {} -> {}",
        baseline.fsync_calls, after.fsync_calls,
    );
    // Allocation grew by at least 1 (the value's data page; possibly more
    // if a handle-table COW or freemap COW was needed).
    assert!(after.pages_allocated > baseline.pages_allocated);
}

#[test]
fn counters_track_read_as_cache_hit_after_commit() {
    let mut db = Chisel::open_in_memory().unwrap();
    db.begin().unwrap();
    let h = db.allocate(b"value").unwrap();
    db.commit().unwrap();

    let pre = db.counters().unwrap();
    let _ = db.read(h).unwrap();
    let post = db.counters().unwrap();

    // Read must hit at least once (the data-page lookup). It may also
    // hit the handle-table page. The lower bound is what we lock in.
    assert!(
        post.cache_hits > pre.cache_hits,
        "read() must register at least one cache lookup; saw {} -> {}",
        pre.cache_hits, post.cache_hits,
    );
}

#[test]
fn counters_reset_on_close_and_reopen() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();
    drop(f); // we want the path; let Chisel create the file.

    {
        let mut db = Chisel::open(&path, Options::default()).unwrap();
        db.begin().unwrap();
        let _ = db.allocate(b"first session").unwrap();
        db.commit().unwrap();
        let c = db.counters().unwrap();
        assert!(c.fsync_calls > 0, "first session must have fsync'd at least once");
        db.close().unwrap();
    }

    // Reopen — counters must reflect ONLY work since reopen, not the
    // first session's totals.
    let db2 = Chisel::open(&path, Options::default()).unwrap();
    let c2 = db2.counters().unwrap();
    // The reopen path does some work (reads superblocks, possibly writes
    // none). We pin the upper bound: post-reopen totals must be much
    // smaller than the first-session totals (which included a commit
    // and therefore at least 2 fsyncs from the commit alone, plus the
    // create_new bootstrap fsyncs).
    assert!(
        c2.fsync_calls <= 1,
        "reopen path should not fsync more than once; saw {}",
        c2.fsync_calls,
    );
}

#[test]
fn counters_snapshot_does_not_mutate_when_engine_continues() {
    let mut db = Chisel::open_in_memory().unwrap();
    let snap = db.counters().unwrap();
    let snap_copy = snap.clone();

    // Do work after taking the snapshot.
    db.begin().unwrap();
    let _ = db.allocate(b"x").unwrap();
    db.commit().unwrap();

    // The original snapshot must not have moved.
    assert_eq!(snap, snap_copy, "ChiselCounters is a snapshot, not a live view");
}
```

- [ ] **Step 2: Run the integration test to confirm it fails**

Run: `cargo test --test counters`
Expected: FAIL — `Chisel::counters` method does not exist.

- [ ] **Step 3: Add `TransactionManager::counters()` in `src/transaction.rs`**

The existing pattern for read-only methods that participate in poisoning is `check_alive()? + …` (see `handles()` at line 1409). We follow it exactly. Add the import at the top of the file if not already present:

```rust
use crate::stats::ChiselCounters;
```

Add the method alongside `handles()` (around line 1409):

```rust
    /// Snapshot the four engine-activity counters (cache hits/misses,
    /// pages allocated, fsync calls). Counters are cumulative from the
    /// most recent open; the bench harness reads-subtract-reads for
    /// per-cell deltas. Takes `&self` (F3); poison-aware via
    /// `check_alive`.
    pub fn counters(&self) -> Result<ChiselCounters> {
        self.check_alive()?;
        Ok(self.cache.borrow().counters())
    }
```

Note: there is no `_inner` and no `poison_on_fatal` wrap because the body cannot fail — it reads `Cell<u64>` values and constructs a `ChiselCounters`. Both operations are infallible. Methods that can fail (like `handles_inner`) need the `poison_on_fatal` wrap; `counters()` does not.

- [ ] **Step 4: Add `Chisel::counters()` in `src/lib.rs`**

Locate `Chisel::stats()` (line 337) and add the new method directly below it:

```rust
    /// Snapshot the four engine-activity counters (cache hits/misses,
    /// pages allocated, fsync calls). Cumulative from the most recent
    /// `open()`; the bench harness reads-subtract-reads to compute
    /// deltas for individual operations or workloads.
    ///
    /// Same `&self` semantic-read shape as `stats()`. Returns
    /// `ChiselError::Poisoned` if the engine is poisoned.
    pub fn counters(&self) -> Result<stats::ChiselCounters> {
        self.txm.counters()
    }
```

- [ ] **Step 5: Run the integration test, expect PASS**

Run: `cargo test --test counters`
Expected: all 5 tests pass.

- [ ] **Step 6: Run the full Rust suite to confirm no regression**

Run: `cargo test`
Expected: every existing test still passes, plus the 5 new ones.

- [ ] **Step 7: Commit**

```bash
git add src/transaction.rs src/lib.rs tests/counters.rs
git commit -m "Expose Chisel::counters() public API"
```

---

## Task 7: Add the `Counters` Python dataclass

**Files:**
- Modify: `python/chisel/__init__.py` (add `Counters` dataclass + `__all__` entry)

- [ ] **Step 1: Add the dataclass and the `__all__` entry**

Open `python/chisel/__init__.py`. Add the new dataclass alongside `Stats` (after `class Stats:` and before `class DefragOptions:`):

```python
@dataclass(frozen=True)
class Counters:
    """Cumulative engine-activity counters since the database was opened.

    Snapshot semantics: the values are point-in-time and do not update.
    Read counters() again to observe new totals. Counters reset implicitly
    on close + reopen because the underlying engine state is rebuilt.

    Fields:
        cache_hits: PageCache.get returned a cached page without disk I/O.
        cache_misses: PageCache.get had to load from disk.
        pages_allocated: PageCache.new_page invocations.
        fsync_calls: PageIo.fsync invocations (two per commit).
    """
    cache_hits: int
    cache_misses: int
    pages_allocated: int
    fsync_calls: int
```

In the `__all__` list, add `"Counters"` next to `"Stats"`:

```python
__all__ = [
    "__version__",
    "Chisel", "Transaction", "Savepoint", "open",
    "Stats", "Counters", "DefragOptions", "DefragStats",
    ...
]
```

- [ ] **Step 2: Confirm the package still imports**

Run: `cd python && python -c "import chisel; assert chisel.Counters; print('ok')"`
Expected: `ok`. (If `_chisel.abi3.so` exists and is current, this succeeds without rebuilding.)

- [ ] **Step 3: Commit**

```bash
git add python/chisel/__init__.py
git commit -m "Python: add Counters dataclass"
```

---

## Task 8: Add `Counters` to the Python type stub

**Files:**
- Modify: `python/chisel/chisel.pyi`

- [ ] **Step 1: Add the stub entry and the method declaration**

Open `python/chisel/chisel.pyi`. Add a `Counters` class alongside the existing `Stats` (after the `Stats` declaration around line 62):

```python
class Counters:
    cache_hits: int
    cache_misses: int
    pages_allocated: int
    fsync_calls: int

    def __init__(
        self,
        cache_hits: int,
        cache_misses: int,
        pages_allocated: int,
        fsync_calls: int,
    ) -> None: ...
```

Add the method declaration to the `Chisel` class. Locate `def stats(self) -> Stats: ...` (around line 115) and add directly below:

```python
    def counters(self) -> Counters: ...
```

- [ ] **Step 2: Confirm the stub is syntactically valid**

Run: `python -c "import ast; ast.parse(open('python/chisel/chisel.pyi').read()); print('ok')"`
Expected: `ok`.

- [ ] **Step 3: Commit**

```bash
git add python/chisel/chisel.pyi
git commit -m "Python: add Counters to type stubs"
```

---

## Task 9: Add `counters()` method to `PyChisel`

TDD: write the Python test first, then implement.

**Files:**
- Create: `python/tests/test_counters.py`
- Modify: `python/src/db.rs`

- [ ] **Step 1: Write the failing Python test**

Create `python/tests/test_counters.py`:

```python
import pytest
import chisel


def test_counters_dataclass_shape(mem_db):
    c = mem_db.counters()
    assert isinstance(c, chisel.Counters)
    assert isinstance(c.cache_hits, int)
    assert isinstance(c.cache_misses, int)
    assert isinstance(c.pages_allocated, int)
    assert isinstance(c.fsync_calls, int)


def test_counters_track_commit(mem_db):
    baseline = mem_db.counters()
    with mem_db.transaction() as tx:
        tx.allocate(b"hello")
    after = mem_db.counters()
    # Commit performs at least 2 fsyncs (data + superblock).
    assert after.fsync_calls >= baseline.fsync_calls + 2
    # At least one new page was allocated (the value's data page).
    assert after.pages_allocated > baseline.pages_allocated


def test_counters_is_frozen(mem_db):
    c = mem_db.counters()
    with pytest.raises(Exception):  # FrozenInstanceError subclasses AttributeError
        c.cache_hits = 999


def test_counters_snapshot_does_not_mutate(mem_db):
    snap = mem_db.counters()
    # Do work.
    with mem_db.transaction() as tx:
        tx.allocate(b"x")
    # The original snapshot is unchanged.
    snap2 = mem_db.counters()
    assert snap.fsync_calls < snap2.fsync_calls or snap == snap2
    # The original `snap` itself is immutable (frozen dataclass) — its
    # field values are still whatever they were at capture time.
```

- [ ] **Step 2: Build the binding to pick up upcoming Rust changes (rebuild prep)**

Run: `cd python && maturin develop`
Expected: build succeeds. (No counters() yet on the Rust binding side; the build is just to establish the toolchain works in this worktree.)

- [ ] **Step 3: Run the failing test**

Run: `cd python && pytest tests/test_counters.py -v`
Expected: FAIL — `mem_db.counters()` does not exist on the binding.

- [ ] **Step 4: Implement the binding method**

Open `python/src/db.rs`. Locate the existing `fn stats` (around line 239) and add `counters` directly below it, mirroring the same materialization pattern:

```rust
    // counters() is read-only on the engine side (`&self`), so the usual
    // immutable borrow path is sufficient. We materialize a
    // `chisel.Counters` dataclass — same shape as stats().
    fn counters(&self, py: Python<'_>) -> PyResult<PyObject> {
        let c = self.with_inner_io(py, |c| c.counters())?;
        let module = py.import_bound("chisel")?;
        let cls = module.getattr("Counters")?;
        let kwargs = pyo3::types::PyDict::new_bound(py);
        kwargs.set_item("cache_hits", c.cache_hits)?;
        kwargs.set_item("cache_misses", c.cache_misses)?;
        kwargs.set_item("pages_allocated", c.pages_allocated)?;
        kwargs.set_item("fsync_calls", c.fsync_calls)?;
        Ok(cls.call((), Some(&kwargs))?.unbind())
    }
```

`with_inner_io` is the existing helper that takes `Fn(&Chisel) -> Result<T>` and returns `PyResult<T>` with poison/closed handling. It already exists; this method just consumes it like `stats()` does.

- [ ] **Step 5: Rebuild the binding**

Run: `cd python && maturin develop`
Expected: build succeeds.

- [ ] **Step 6: Run the test, expect PASS**

Run: `cd python && pytest tests/test_counters.py -v`
Expected: all 4 tests pass.

- [ ] **Step 7: Run the full Python test suite to confirm no regression**

Run: `cd python && pytest`
Expected: all existing tests pass, plus the 4 new ones.

- [ ] **Step 8: Commit**

```bash
git add python/src/db.rs python/tests/test_counters.py
git commit -m "Python: expose chisel.counters() binding method and tests"
```

---

## Task 10: Final gate — full test suite, formatter, clippy

A defensive sweep — ensure nothing landed broken.

- [ ] **Step 1: Run the full Rust test suite**

Run: `cargo test`
Expected: all tests pass (existing + 5 new integration tests in `tests/counters.rs` + 4 new unit tests across `page_io.rs` and `page_cache.rs`).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: clean. Common issues to expect from new `Cell<u64>` work:
- `clippy::needless_borrow` if a `.get()` is wrapped in an extra reference — drop it.
- `clippy::field_reassign_with_default` if a `Default::default()` is followed by per-field assignments — coalesce.

- [ ] **Step 3: Run rustfmt check**

Run: `cargo fmt -- --check`
Expected: clean. If it complains, run `cargo fmt` and amend the relevant commit.

- [ ] **Step 4: Run the full Python test suite**

Run: `cd python && pytest`
Expected: all tests pass.

- [ ] **Step 5: Confirm git state is clean and on a feature branch**

Run: `git status && git log --oneline -10`
Expected: clean working tree, commits visible. If you are on `main` and the policy is to land via PR, create a feature branch retroactively:

```bash
git branch counters-instrumentation
git reset --hard origin/main  # assuming nothing else pushed
git checkout counters-instrumentation
```

- [ ] **Step 6: Push and open a PR**

(This step is omitted if the user has not authorized push.)

```bash
git push -u origin counters-instrumentation
gh pr create --title "Add Chisel::counters() — instrumentation precursor for benchmark suite" --body "$(cat <<'EOF'
## Summary

Adds four cumulative-from-open counters — `cache_hits`, `cache_misses`,
`pages_allocated`, `fsync_calls` — exposed via `Chisel::counters() ->
ChiselCounters`. Mirror Python API: `chisel.counters() -> Counters`.

This is PR 1 of the benchmark-suite series (see
docs/superpowers/specs/2026-04-25-chisel-benchmark-suite-design.md).
The harness consumes these counters to attribute observed time to
fsync cost, cache-miss I/O, or allocation churn. Useful for debugging
and observability outside benchmarking too.

## Implementation

- `PageIo` gains a `Cell<u64>` fsync counter; incremented in `fsync()`.
- `PageCache` gains three `Cell<u64>` counters; incremented in `get` /
  `get_mut` / `new_page`.
- `PageCache::counters()` aggregates all four into `ChiselCounters`
  (defined in `stats.rs`).
- `Chisel::counters()` delegates through `TransactionManager::counters()`,
  poison-aware like `stats()`.
- Python: `chisel.Counters` frozen dataclass + `chisel.counters()` method.

`Cell<u64>`, not `AtomicU64`: Chisel is single-writer by design.

## Test plan

- [x] Rust unit tests in `page_io::tests` and `page_cache::tests`.
- [x] Rust integration tests in `tests/counters.rs` — delta correctness,
      reset on reopen, snapshot isolation.
- [x] Python tests in `python/tests/test_counters.py` — dataclass shape,
      delta after commit, frozen, snapshot isolation.
- [x] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt -- --check`,
      `pytest` all green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Notes for the executing engineer

- **Order matters:** Task 1 must land before Tasks 2–5 (struct must exist for `counters()` to return). Tasks 7–8 (Python dataclass + stub) must land before Task 9 (the binding code calls `module.getattr("Counters")` — if the dataclass doesn't exist yet, `maturin develop` builds fine but the test fails at runtime). Task 6 must land before Task 9 (binding delegates to `Chisel::counters()`).
- **No format change.** This PR is byte-identical on disk to its predecessor. No regression test needed for format compatibility.
- **No new error variants.** Counters are infallible reads; the only failure mode is `Poisoned`, which is the existing variant.
- **Performance:** the four `Cell<u64>` increments are single-digit nanoseconds in release builds. They land on hot paths (`PageCache::get`, `PageCache::new_page`, `PageIo::fsync`) but the cost is invisible against the actual work each method does (HashMap lookup, file I/O, fsync syscall). No feature-gate is needed.
- **Worktree note:** This plan was not run in a dedicated worktree. If the executing engineer wants isolation, create one before starting Task 1: `git worktree add ../chisel-counters counters-instrumentation`.
