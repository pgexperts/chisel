// page_cache.rs — LRU page cache with dirty tracking and checksum validation.
//
// Architecture layer 3 (per CLAUDE.md): the choke point through which every
// other module (freemap, data_page, overflow, handle_table, transaction)
// accesses pages. It owns the sole `PageIo` instance. Modules above it never
// see raw file I/O.
//
// Key invariants:
// - Checksums are verified on COLD reads only (in `load_page`, before the
//   entry enters `entries`). Cache hits via `get`/`get_mut` do NOT
//   re-validate — the bytes are trusted between load and eviction. Once a
//   page is in `entries`, callers may assume its bytes were valid at load
//   time. Bytes mutated in-cache will not have a valid checksum until the
//   relevant page-type module rewrites one before flush. This trust chain
//   relies on the exclusive flock in `PageIo::open` preventing any other
//   process from scribbling on the file behind our back.
// - Dirty pages are NEVER evicted. `maybe_evict` walks the LRU tail and
//   skips any dirty entry. This is the cache's contribution to shadow
//   paging: once a transaction has touched a page, the in-memory version
//   must survive until `flush()` writes it to disk as part of commit.
// - `new_page()` allocates a FRESH page_id past the current EOF. It never
//   overwrites a live page. This is what makes copy-on-write safe: the old
//   committed page remains untouched on disk until the superblock swap.
// - `next_page_id` is a monotonic allocator. It is seeded from the file's
//   page count at open time, and is bumped on every `new_page()`. Rollback
//   does NOT rewind it (see the note in `discard`); orphaned page IDs are
//   acceptable because they are reclaimed by the freemap after commit or
//   simply re-truncated.
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

use std::cell::Cell;
use std::collections::HashMap;

use crate::error::{ChiselError, Result};
use crate::lru::LruIndex;
use crate::page::{self, PAGE_SIZE};
use crate::page_io::PageIo;
use crate::stats::ChiselCounters;

// Hard-ceiling multiplier on `max_pages` (ISSUES.md I19). The cache's
// soft limit — `max_pages` — is the working-set target; dirty pages
// pin themselves against eviction, so a single transaction can
// legitimately push the cache over the soft limit. But unbounded
// growth is an OOM hazard; this multiplier caps how far over the soft
// limit the cache may grow with every entry dirty before `maybe_evict`
// surfaces `ChiselError::CacheFull` and forces the caller to commit
// or roll back. 8× is a defensive default: generous enough that
// normal write-heavy workloads never see it, small enough that a
// pathological or runaway transaction trips the brake within seconds
// rather than megabytes of memory. Not currently configurable; expose
// via `Options` if a real workload needs to tune it.
const HARD_CEILING_MULTIPLIER: usize = 8;

struct CacheEntry {
    buf: Box<[u8; PAGE_SIZE]>,
    // `dirty` means "modified since last flush, must be written on commit".
    // It is also a pin against LRU eviction — see `maybe_evict`.
    dirty: bool,
}

pub struct PageCache {
    io: PageIo,
    entries: HashMap<u64, CacheEntry>,
    // O(1) LRU index over page IDs. Head of the index = most recently
    // used; `maybe_evict` walks `iter_lru_to_mru()` looking for a clean
    // victim. Backed by a HashMap-of-(prev,next) doubly-linked list —
    // every operation is O(1). See `lru.rs` for the implementation
    // history (the original `VecDeque<u64>`-based design did O(n)
    // `retain` scans on every page touch and showed up at 66% of CPU
    // on a 70k-row INSERT profile, prompting the swap).
    lru: LruIndex,
    /// Count of dirty entries. Maintained incrementally on every
    /// dirty-flag transition (`get_mut`, `new_page`, `claim_page`,
    /// `flush`, `discard`, `discard_all_dirty`, `truncate`). Lets
    /// `maybe_evict` short-circuit when `dirty_count == entries.len()` —
    /// without it, the eviction scan walks the full LRU on every
    /// allocation in a write-heavy transaction (where all pages are
    /// dirty and no victim exists), trivially making page-allocation
    /// O(n) per call. With this counter the early-out is O(1).
    dirty_count: usize,
    max_pages: usize,
    /// Strict upper bound on the spillway sidecar file in bytes
    /// (excluding per-slot headers). 0 means spillway disabled —
    /// overflow trips CacheFull at the cache cap. Set via Options;
    /// runtime-mutable between transactions via set_spillway_max_bytes.
    /// (Activated in Task 9; until then, dead_code is suppressed.)
    #[allow(dead_code)]
    spillway_max_bytes: u64,
    /// LRU position policy for commit-drain rehydrated pages. Captured
    /// from Options at construction; runtime-mutable between
    /// transactions via set_drain_insertion.
    /// (Activated in Task 11; until then, dead_code is suppressed.)
    #[allow(dead_code)]
    drain_insertion: crate::DrainInsertion,
    /// How to lazily open the spillway when a first spill happens. Held
    /// here rather than opening eagerly because no-spill workloads
    /// shouldn't pay any filesystem cost for a feature they never use.
    /// (Activated with the spillway in Task 9.)
    #[allow(dead_code)]
    spillway_location: crate::SpillwayLocation,
    /// Lazily-initialized spillway. None until the first spill needs it.
    /// (Activated in Task 9.)
    #[allow(dead_code)]
    spillway: Option<crate::spillway::Spillway>,
    // Monotonically increasing allocator for shadow-paged new pages. Never
    // reused within a process lifetime except via `truncate()`.
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

impl PageCache {
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

    /// Read a page (cache hit or load from disk with checksum validation).
    ///
    /// On a miss, `load_page` validates the checksum before the bytes enter
    /// the cache. Callers of `get` therefore never see unvalidated data.
    /// Subsequent hits skip re-verification — checksums are validated on
    /// COLD reads (cache misses) only, not on every call to `get`. This is
    /// safe as long as the in-memory buffer is not shared with any external
    /// process (enforced by the exclusive flock in `PageIo::open`) and as
    /// long as no module mutates a cached buffer without going through
    /// `get_mut`, which marks it dirty (and therefore eligible for a fresh
    /// checksum stamp at flush time by the page-type module).
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

    /// Get a mutable reference to a page, marking it dirty.
    ///
    /// IMPORTANT: in a shadow-paging world, mutating a live (already
    /// committed) page in place would corrupt the old snapshot. Higher
    /// layers (handle_table, freemap) are expected to call `new_page()`
    /// first and copy the old contents before mutating — this method does
    /// NOT enforce COW on its own. It exists because some callers (e.g.
    /// writing a page that was itself freshly allocated this txn) are
    /// legitimately mutating their own new pages.
    ///
    /// Marking dirty pins the entry against LRU eviction until the next
    /// `flush()`.
    pub fn get_mut(&mut self, page_id: u64) -> Result<&mut [u8; PAGE_SIZE]> {
        if self.entries.contains_key(&page_id) {
            self.cache_hits.set(self.cache_hits.get() + 1);
        } else {
            self.cache_misses.set(self.cache_misses.get() + 1);
            self.load_page(page_id)?;
        }
        self.touch_lru(page_id);
        let entry = self.entries.get_mut(&page_id).unwrap();
        // Track clean→dirty transitions for the dirty_count counter.
        // No-op on dirty→dirty (entry was already counted).
        if !entry.dirty {
            self.dirty_count += 1;
            entry.dirty = true;
        }
        Ok(&mut entry.buf)
    }

    /// Cumulative cache hit count. Test-only because
    /// `PageCache::counters()` is the public aggregator and is what
    /// production code uses; this accessor exists so unit tests can
    /// check the individual counter without constructing a full
    /// `ChiselCounters`.
    #[cfg(test)]
    fn cache_hit_count(&self) -> u64 {
        self.cache_hits.get()
    }

    /// Cumulative cache miss count. Includes attempted misses where
    /// `load_page` subsequently failed (checksum mismatch, I/O error) —
    /// the counter records "we had to reach for disk", not "the disk
    /// read succeeded". Test-only for the same reason as
    /// `cache_hit_count`.
    #[cfg(test)]
    fn cache_miss_count(&self) -> u64 {
        self.cache_misses.get()
    }

    /// Cumulative `new_page()` invocations. Counts attempted
    /// allocations: an allocation that subsequently trips `CacheFull`
    /// in `maybe_evict` is still recorded. Test-only for the same
    /// reason as `cache_hit_count`.
    #[cfg(test)]
    fn pages_allocated_count(&self) -> u64 {
        self.pages_allocated.get()
    }

    /// Snapshot all four engine-activity counters into a `ChiselCounters`.
    ///
    /// Three of the four counters live here in `PageCache`; `fsync_calls`
    /// is owned by the underlying `PageIo` (where the actual `fsync` call
    /// happens) and is read through. The snapshot is a value type — it
    /// does not update as the engine continues to do work.
    ///
    /// Coverage caveat: only calls to `PageIo::fsync` are counted in
    /// `fsync_calls`. As of this writing, all flush paths go through
    /// that method, so the counter is exhaustive. If a future variant
    /// (e.g. `fdatasync`) is added and called from outside `PageIo::
    /// fsync`, this aggregator would miss it — increment that variant
    /// into the same counter, or extend the aggregator.
    pub fn counters(&self) -> ChiselCounters {
        ChiselCounters {
            cache_hits: self.cache_hits.get(),
            cache_misses: self.cache_misses.get(),
            pages_allocated: self.pages_allocated.get(),
            fsync_calls: self.io.fsync_count(),
        }
    }

    /// Allocate a new zeroed page, mark it dirty, return its page_id.
    ///
    /// This is the heart of shadow paging: every write goes to a brand-new
    /// page past the current high-water mark, so no committed page is ever
    /// overwritten during the transaction. On commit, `flush()` writes the
    /// page to disk (implicitly extending the file) and the superblock
    /// swap makes it visible; on rollback, `discard()` drops the in-memory
    /// buffer and the on-disk bytes (if any) become orphaned garbage that
    /// the next `truncate()` or freemap reclaim can recover.
    ///
    /// Known v1 simplification (per CLAUDE.md): this allocator never
    /// consults the freemap. It always extends past EOF, so freed pages
    /// from previous transactions remain unreclaimed until a defrag pass.
    ///
    /// The page is inserted BEFORE `maybe_evict()` runs, so the new page
    /// itself is never the eviction victim (it is MRU and dirty anyway).
    pub fn new_page(&mut self) -> Result<u64> {
        let page_id = self.next_page_id;
        self.next_page_id += 1;
        let entry = CacheEntry {
            buf: Box::new([0u8; PAGE_SIZE]),
            dirty: true,
        };
        self.entries.insert(page_id, entry);
        self.dirty_count += 1;
        self.lru.push_front(page_id);
        self.pages_allocated.set(self.pages_allocated.get() + 1);
        self.maybe_evict()?;
        Ok(page_id)
    }

    /// Write all dirty pages to disk and fsync.
    ///
    /// This is Phase 1 of commit (see transaction.rs::commit). The order is
    /// critical:
    ///   1. Write every dirty data page to its new location.
    ///   2. fsync — durably persist those writes.
    ///   3. (Caller then writes the superblock and fsyncs again.)
    ///
    /// Skipping the fsync here would let the superblock reach disk before
    /// its referenced pages, turning a crash into silent corruption.
    ///
    /// We collect dirty IDs into a Vec first to sidestep the borrow checker
    /// (we need `&mut self.entries` inside the loop while iterating). The
    /// iteration order is HashMap order — i.e. non-deterministic. That is
    /// fine: all writes share one fsync, so intra-batch ordering is
    /// irrelevant for durability.
    ///
    /// After a successful flush, every entry is clean and therefore
    /// eligible for LRU eviction. The entries are NOT removed from the
    /// cache — subsequent reads can still hit them.
    ///
    /// DURABILITY WINDOW (ISSUES.md I1, C3): flush() clears the dirty flag
    /// on every entry as soon as the page is written, BEFORE the trailing
    /// fsync. Between step 1 and step 2 above, the cache claims the page
    /// is clean but the kernel has not yet acknowledged durability. A
    /// mid-flush I/O error then leaves the cache in a state where it
    /// thinks the pages are durable but they may not be. This is only safe
    /// because the I1 poison model treats any commit-protocol failure as
    /// fatal: on error, `TransactionManager` sets its poison flag and the
    /// caller must drop the handle and reopen. The cache's temporary lie
    /// about durability never gets a chance to matter, because no code
    /// will trust the cache's clean state after a flush error.
    ///
    /// If the poison model is ever weakened to allow in-place retry, this
    /// function will need to stop clearing dirty flags until the fsync
    /// returns OK — otherwise retrying a failed commit would silently skip
    /// the pages it already "flushed".
    pub fn flush(&mut self) -> Result<()> {
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
        // Every dirty entry was flipped clean above; the counter resets.
        self.dirty_count = 0;
        self.io.fsync()?;
        Ok(())
    }

    /// Discard a page from the cache (used during rollback).
    ///
    /// Rollback drops every dirty page allocated during the aborted txn.
    /// This is safe precisely because shadow paging never touched the
    /// committed pages — there is nothing to "undo" on disk, only cached
    /// garbage to throw away.
    ///
    /// Note: `next_page_id` is deliberately NOT rewound. If rollback freed
    /// IDs back to the allocator, two concurrent savepoint rollbacks could
    /// hand the same ID to two different allocations. Leaving `next_page_id`
    /// monotonic sacrifices a tiny amount of address space for correctness.
    pub fn discard(&mut self, page_id: u64) {
        if let Some(entry) = self.entries.remove(&page_id) {
            if entry.dirty {
                self.dirty_count -= 1;
            }
        }
        self.lru.remove(page_id);
    }

    /// Discard every dirty entry from the cache regardless of id
    /// (ISSUES.md R2 rollback path). Used by `rollback()` in concert with
    /// `truncate` to handle BOTH pages extended from the file (id >=
    /// watermark, removed by truncate) AND pages reused from the freemap
    /// (id < watermark, must be explicitly discarded here). The invariant
    /// that makes this safe: `flush()` clears the dirty flag on every
    /// entry, so any dirty entry in the cache was necessarily created by
    /// the current transaction. After discard, the next read for that
    /// page id will re-load the last-committed content from disk.
    ///
    /// Clean entries are preserved — they are read-through caches of
    /// committed disk content and remain correct across rollback.
    pub fn discard_all_dirty(&mut self) {
        let dirty_ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&id, _)| id)
            .collect();
        for id in dirty_ids {
            self.entries.remove(&id);
            self.lru.remove(id);
        }
        // Every removed entry was dirty; the counter resets.
        self.dirty_count = 0;
    }

    /// Return the number of whole pages the underlying file can hold.
    ///
    /// This is the PHYSICAL file size in pages, which may exceed the
    /// logical committed `total_pages` if a transaction is in-flight or a
    /// prior crash left tail garbage.
    pub fn file_page_count(&mut self) -> Result<u64> {
        self.io.page_count()
    }

    /// Truncate the file to `n` pages.
    ///
    /// Drops any cached entries at id >= n, then shrinks the file. This
    /// is the only path that legitimately rewinds `next_page_id`: the
    /// caller is asserting that nothing references pages beyond `n`, so
    /// the allocator can reuse that range on subsequent `new_page()` calls.
    ///
    /// Dirty-page semantics (ISSUES.md I5): dirty entries in the
    /// truncated range are silently discarded. This is NOT a bug in
    /// the current call sites — it is the intended semantics:
    ///
    ///   * `rollback_inner` calls `discard_all_dirty` first to handle
    ///     dirty pages REUSED from the freemap (id < n), then calls
    ///     truncate(committed_total) to drop dirty EXTENDED pages
    ///     (id >= n). Both drops are intentional.
    ///   * `rollback_to_inner` calls truncate(savepoint.watermark)
    ///     directly; any dirty entries at id >= watermark are pages
    ///     allocated AFTER the savepoint, exactly the ones we want
    ///     gone. Savepoint-bearing transactions disable freemap reuse
    ///     (see `allocate_data_page`) so there are no dirty reused-id
    ///     pages to worry about.
    ///
    /// If a future caller needs "truncate without dropping any dirty
    /// pages" semantics, it should assert the invariant externally
    /// (e.g., `debug_assert!(cache.dirty_count_at_or_above(n) == 0)`)
    /// rather than having this method enforce a policy that conflicts
    /// with the existing rollback call sites.
    pub fn truncate(&mut self, n: u64) -> Result<()> {
        let to_remove: Vec<u64> = self
            .entries
            .keys()
            .filter(|&&id| id >= n)
            .copied()
            .collect();
        for id in to_remove {
            if let Some(entry) = self.entries.remove(&id) {
                if entry.dirty {
                    self.dirty_count -= 1;
                }
            }
            self.lru.remove(id);
        }
        self.io.set_page_count(n)?;
        if self.next_page_id > n {
            self.next_page_id = n;
        }
        Ok(())
    }

    /// Expose the PageIo for direct superblock I/O.
    ///
    /// Superblocks live at fixed page IDs `0..N` (where N is the
    /// configurable `superblock_count`, 2..=16 — see ISSUES.md R4)
    /// and must bypass the page cache entirely. Both ends of the
    /// superblock lifecycle use this accessor directly:
    ///
    ///   * `TransactionManager::commit_inner` writes the new superblock
    ///     to its inactive slot via `io_mut().write_page(inactive, ...)`
    ///     where `inactive = txn_counter % superblock_count`.
    ///   * `TransactionManager::open_existing` reads up to
    ///     MAX_SUPERBLOCKS candidate slots via `io_mut().read_page(i)`
    ///     before letting `Superblock::select` pick the winner.
    ///
    /// Caching superblocks would break the N-way alternation protocol:
    /// `commit_inner` writes the inactive slot and expects the file
    /// (not a cache entry) to reflect the change immediately for the
    /// subsequent fsync. A cached superblock buffer could also hand a
    /// reader a stale copy across the rotation boundary, defeating
    /// the whole "highest valid counter wins" selection rule.
    ///
    /// The caller-side discipline is "never call cache.get(id) for
    /// id < superblock_count"; this is a transaction-layer convention,
    /// not enforced by the cache itself. Note that the cache cannot
    /// enforce it on its own because it does not know the value of
    /// `superblock_count` (that field lives on TransactionManager,
    /// not on PageCache).
    pub fn io_mut(&mut self) -> &mut PageIo {
        &mut self.io
    }

    /// Immutable view of the underlying `PageIo`. Used only for cheap
    /// queries like `is_read_only()` that don't need to mutate file
    /// state — keeps the caller's `&self` borrow intact.
    pub fn io(&self) -> &PageIo {
        &self.io
    }

    /// Current value of the next page id counter — i.e. the id that the
    /// next `new_page()` call will return. Used by the transaction layer
    /// as a "watermark": record it before an operation, call the operation
    /// (which may allocate via `new_page()`), then every id in
    /// `[before, after)` is a newly-allocated page that needs tracking
    /// for rollback. See ISSUES.md I7 — this replaces per-module "push
    /// every new page into txn_dirty_pages" plumbing with a single
    /// invariant: if `new_page()` handed it out, it is in the watermark
    /// range.
    pub fn next_page_id(&self) -> u64 {
        self.next_page_id
    }

    /// Reuse a specific page id previously returned to the freemap
    /// (ISSUES.md R2). Inserts a fresh zero-filled dirty cache entry at
    /// `page_id`, overwriting any stale entry that may have been loaded
    /// from disk by a prior reader. This is how the transaction layer
    /// consumes a page id pulled from the freemap bitmap: the caller
    /// receives a clean buffer to initialize without touching the old
    /// content.
    ///
    /// Safe to call even if the cache already has an entry for `page_id`
    /// (the old entry is dropped). Does NOT extend the file — the id
    /// must already exist within the current file size, and the caller
    /// must have acquired it via the freemap before invoking this.
    ///
    /// Warning: if a prior cache entry for `page_id` was dirty, its
    /// pending writes are silently discarded. That is intentional for
    /// the freemap-reuse path (the only legitimate caller) because the
    /// page has just been re-allocated and its pre-existing content is
    /// by definition garbage. But it means this method MUST NOT be
    /// called on a page that has live writes belonging to the current
    /// transaction — the dirty-discard would lose committed-but-unflushed
    /// work.
    pub fn claim_page(&mut self, page_id: u64) -> Result<()> {
        // ISSUES.md I20: enforce the "freemap never returns an already-dirty
        // id" invariant in debug builds. The only legitimate caller is
        // `allocate_data_page` via the freemap, which — post-I18 — keeps the
        // at-risk id sets out of the in-commit free pool. A violation here
        // would silently drop the caller's pending writes on `page_id`; an
        // assertion surfaces the bug at its source rather than hours later
        // as mysterious data loss.
        debug_assert!(
            !self.is_dirty(page_id),
            "claim_page called on a dirty page (page_id={page_id}); freemap returned an id with pending writes from the current transaction"
        );
        // Remove any pre-existing entry so a stale cached copy from a
        // prior reader doesn't leak into the new transaction's view.
        // The debug_assert above guarantees any prior entry was clean,
        // so removing it doesn't change `dirty_count`. Then insert a
        // fresh dirty entry, incrementing the counter.
        // `LruIndex::push_front` auto-removes any prior entry for this
        // id before inserting at MRU, so an explicit LRU remove isn't
        // needed.
        self.entries.remove(&page_id);
        let entry = CacheEntry {
            buf: Box::new([0u8; PAGE_SIZE]),
            dirty: true,
        };
        self.entries.insert(page_id, entry);
        self.dirty_count += 1;
        self.lru.push_front(page_id);
        self.maybe_evict()?;
        Ok(())
    }

    /// Set the next page ID (used when loading from an existing file).
    ///
    /// Called at database open time after the transaction manager has
    /// chosen the authoritative superblock. This overrides the file-length
    /// based seed from `new()` to ensure allocations continue from the
    /// logical high-water mark, not whatever trailing garbage the file
    /// happens to contain after a crash.
    pub fn set_next_page_id(&mut self, id: u64) {
        self.next_page_id = id;
    }

    /// Check if a page is dirty in the cache.
    ///
    /// Used by the transaction layer to reason about whether a page is
    /// safe to drop at savepoint/rollback boundaries.
    pub fn is_dirty(&self, page_id: u64) -> bool {
        self.entries.get(&page_id).is_some_and(|e| e.dirty)
    }

    /// Load a page from disk into the cache, verifying its checksum.
    ///
    /// Evict BEFORE reading, so the new arrival does not temporarily push
    /// us two entries over `max_pages`. Checksum verification happens
    /// BEFORE the entry is inserted: a corrupt page never pollutes the
    /// cache, so a retry could (in principle) succeed if the caller
    /// repaired the file externally.
    ///
    /// A checksum mismatch is a fatal corruption error per CLAUDE.md —
    /// `ChecksumMismatch` signals the database is broken, not merely that
    /// the operation failed.
    fn load_page(&mut self, page_id: u64) -> Result<()> {
        self.maybe_evict()?;
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

    /// Move `page_id` to the MRU (front) of the LRU index.
    ///
    /// O(1). Backed by `LruIndex`, whose `push_front` re-locates an
    /// existing id to the MRU end (or inserts if absent). Originally
    /// O(n) on `VecDeque::retain`; the swap to `LruIndex` happened
    /// after a samply profile of a 70k-row INSERT showed 66% of CPU
    /// in the retain-driven memmoves. See `lru.rs` doc for full
    /// rationale.
    fn touch_lru(&mut self, page_id: u64) {
        self.lru.push_front(page_id);
    }

    /// Evict clean pages from the LRU tail until we're under `max_pages`.
    ///
    /// Walks the LRU deque from back (least recent) toward front looking
    /// for the first CLEAN entry. Dirty pages are skipped entirely: they
    /// are pinned until the next `flush()`, which is the cache's
    /// contribution to shadow-paging correctness — a dirty page written
    /// out early would not have a committed superblock pointing at it, so
    /// losing it to eviction would lose the transaction's work.
    ///
    /// Termination: if every cached page is dirty we break out of the
    /// loop and deliberately exceed `max_pages`. This is a SOFT limit
    /// on the working-set size, not a hard cap on memory. Write-heavy
    /// transactions can legitimately push the cache well past
    /// `max_pages` between flushes.
    ///
    /// Hard ceiling (ISSUES.md I19): after the eviction loop settles,
    /// if we are still above `max_pages * HARD_CEILING_MULTIPLIER`,
    /// we raise `ChiselError::CacheFull`. This caps runaway growth in
    /// pathological workloads (a long transaction that allocates
    /// pages in a loop without committing) while preserving the
    /// soft-limit semantics for normal use. The caller recovers by
    /// committing (flushes → pages become evictable) or rolling back
    /// (discards all dirty pages).
    ///
    /// The `is_none_or(|e| e.dirty)` is awkward: it returns true if the
    /// entry is missing OR dirty, so `!` flips to "entry exists AND is
    /// clean". The missing-entry branch should never fire in practice
    /// (the LRU and the map are meant to stay in sync) but guards against
    /// a stale LRU id.
    fn maybe_evict(&mut self) -> Result<()> {
        while self.entries.len() > self.max_pages {
            // Short-circuit: if every cached entry is dirty there can be
            // no eviction victim, so don't bother walking the LRU. This
            // is the common case during write-heavy transactions, where
            // the unconditional walk was an O(n) cost on every
            // page allocation. With `dirty_count` maintained on every
            // dirty-flag transition we can decide in O(1).
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
                None => break, // Should be unreachable given the short-circuit
                               // above, but kept defensively in case dirty_count
                               // ever drifts from the truth (debug_assert in
                               // tests catches drift; release falls back here).
            }
        }
        let hard_ceiling = self.max_pages.saturating_mul(HARD_CEILING_MULTIPLIER);
        if self.entries.len() > hard_ceiling {
            return Err(ChiselError::CacheFull {
                limit: hard_ceiling,
            });
        }
        Ok(())
    }

    /// Lazy-open the spillway on first spill. Subsequent calls reuse
    /// the existing one. Returns SpillwayFull if `spillway_max_bytes`
    /// is 0 (spillway disabled by configuration); the caller must
    /// fall back to the legacy CacheFull path in that case.
    /// (First caller arrives in Task 9.)
    #[allow(dead_code)]
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_io::PageIo;
    use tempfile::NamedTempFile;

    fn fresh_cache(max_pages: usize) -> PageCache {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        // Leak the tempfile so the PageIo's file handle outlives this
        // function; tests drop the cache at end of scope, which closes
        // the fd and releases the flock cleanly.
        std::mem::forget(file);
        // spillway_max_bytes=0 preserves the legacy "fail fast on cache
        // pressure" contract for all existing page_cache tests.
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        PageCache::new(
            io,
            cache_max_bytes,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        )
    }

    // Regression test for ISSUES.md I19. The cache is a SOFT limit by
    // design: when every cached entry is dirty, `maybe_evict` is forced
    // to break out of its eviction loop and let the cache grow past
    // `max_pages`. Pre-I19 there was no upper bound at all, so a
    // long-running transaction that allocated many `new_page()`s
    // without flushing could exhaust memory silently. I19 adds a hard
    // ceiling — currently `max_pages * HARD_CEILING_MULTIPLIER` — that
    // trips `ChiselError::CacheFull` once exceeded, forcing the caller
    // to commit (which flushes, enabling eviction) or roll back. This
    // is an OPERATIONAL error, not fatal: the cache state is internally
    // consistent and the caller can recover by splitting their work
    // into smaller transactions.
    #[test]
    fn cache_full_fires_when_all_pages_dirty_past_hard_ceiling() {
        let max_pages = 4;
        let mut cache = fresh_cache(max_pages);
        let hard_ceiling = max_pages * HARD_CEILING_MULTIPLIER;

        // Allocate up to the ceiling. Every `new_page()` produces a
        // DIRTY entry, so the LRU eviction loop can never evict.
        for _ in 0..hard_ceiling {
            cache
                .new_page()
                .expect("allocations up to the ceiling must succeed");
        }
        assert_eq!(cache.entries.len(), hard_ceiling);

        // The next new_page must trip CacheFull.
        let err = cache.new_page().unwrap_err();
        assert!(
            matches!(err, ChiselError::CacheFull { limit } if limit == hard_ceiling),
            "expected CacheFull {{ limit: {hard_ceiling} }}, got {err:?}"
        );
    }

    // Flushing the cache clears dirty flags, which means the eviction
    // loop can actually evict again, which means subsequent allocations
    // succeed. Covers the intended "commit to recover from CacheFull"
    // recovery path.
    #[test]
    fn cache_full_is_recoverable_via_flush() {
        let max_pages = 4;
        let mut cache = fresh_cache(max_pages);
        let hard_ceiling = max_pages * HARD_CEILING_MULTIPLIER;

        for _ in 0..hard_ceiling {
            cache.new_page().unwrap();
        }
        assert!(matches!(
            cache.new_page(),
            Err(ChiselError::CacheFull { .. })
        ));

        // Flush clears every dirty flag. The next maybe_evict can then
        // reclaim capacity by evicting clean pages from the LRU tail.
        cache.flush().unwrap();
        // After flush, the cache is still at hard_ceiling entries but
        // all clean; the next new_page evicts one and succeeds.
        cache
            .new_page()
            .expect("post-flush allocation should succeed");
        assert!(cache.entries.len() <= hard_ceiling);
    }

    // Regression test for ISSUES.md I20. claim_page previously silently
    // dropped any prior dirty writes on the claimed id: it unconditionally
    // removed the existing cache entry and inserted a fresh zeroed one.
    // The only legitimate caller is `allocate_data_page` via the freemap,
    // which must never return an id already dirty in the current txn —
    // but the invariant was unenforced. I20 adds a debug_assert so the
    // rule is checked in debug builds; a violation fires immediately
    // rather than surfacing hours later as silent data loss.
    //
    // Gated on debug_assertions because debug_assert! is a no-op in
    // release builds; running this test under `cargo test --release`
    // would (correctly) not panic.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "claim_page called on a dirty page")]
    fn claim_page_asserts_on_dirty_page() {
        let mut cache = fresh_cache(64);

        // new_page produces a fresh dirty entry. claim_page'ing that
        // same id is exactly the forbidden path the I20 assert guards.
        let id = cache.new_page().unwrap();
        assert!(cache.is_dirty(id));
        let _ = cache.claim_page(id);
    }

    #[test]
    fn cache_hits_and_misses_track_correctly() {
        // Setup: open an in-memory PageIo, populate page 0 with a checksummed
        // buffer (writing through the cache so the file actually grows).
        let io = PageIo::open_in_memory().unwrap();
        let mut cache = PageCache::new(
            io,
            16 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );

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

    #[test]
    fn pages_allocated_counter_increments_per_new_page() {
        let io = PageIo::open_in_memory().unwrap();
        let mut cache = PageCache::new(
            io,
            16 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        assert_eq!(cache.pages_allocated_count(), 0);
        cache.new_page().unwrap();
        cache.new_page().unwrap();
        cache.new_page().unwrap();
        assert_eq!(cache.pages_allocated_count(), 3);
    }

    #[test]
    fn counters_aggregates_cache_and_io_state() {
        use crate::stats::ChiselCounters;

        let io = PageIo::open_in_memory().unwrap();
        let mut cache = PageCache::new(
            io,
            16 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );

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
}
