// page_cache.rs — LRU page cache with dirty tracking and checksum validation.
//
// Architecture layer 3 (per ARCHITECTURE.md): the choke point through which every
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

use std::cell::Cell;

use rustc_hash::FxHashMap;

use crate::crypto::ENC_PAGE_SIZE;
use crate::error::{ChiselError, Result};
use crate::lru::LruIndex;
use crate::page::{self, PAGE_SIZE};
use crate::page_io::PageIo;
use crate::stats::ChiselCounters;

// Cache size discipline (replaces the pre-spillway HARD_CEILING_MULTIPLIER):
// `max_pages` is now a strict upper bound. Overflow dirty pages are spilled
// to a sidecar `Spillway` file rather than growing the cache. Workloads
// that explicitly want the legacy "fail fast at the cache ceiling" semantics
// can set Options::spillway_max_bytes = 0; CacheFull then fires at
// max_pages itself, with no elasticity. See spec
// 2026-05-03-chisel-spillway-design.md.

struct CacheEntry {
    buf: Box<[u8; PAGE_SIZE]>,
    // `dirty` means "modified since last flush, must be written on commit".
    // It is also a pin against LRU eviction — see `maybe_evict`.
    dirty: bool,
}

pub struct PageCache {
    io: PageIo,
    // I77: FxHashMap, not the default SipHash. Keys are trusted local u64 page
    // ids — no DoS surface — so SipHash's DoS-resistance is pure cost. This map
    // is probed ~twice per `get` plus the LRU touch, and a read descends it
    // once per handle-table level, so the per-probe hash cost is hot.
    entries: FxHashMap<u64, CacheEntry>,
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
    ///
    /// INVARIANT: this counts ONLY dirty entries currently resident in
    /// `entries`, never pages that have been spilled to the spillway.
    /// That is exactly why the `dirty_count == entries.len()`
    /// short-circuit is sound — both sides measure the in-cache set. A
    /// Phase-B spill decrements this as it removes the victim from
    /// `entries` (and re-increments on a failed spill that restores it),
    /// so `dirty_count <= entries.len()` always holds; `forget_above`
    /// (truncate's spillway prune) deliberately does NOT touch it,
    /// because spilled pages were never counted here.
    dirty_count: usize,
    /// I52 (ISSUES.md, 2026-05-22): reusable scratch buffer for
    /// `flush()` and `discard_all_dirty()`. Both functions iterate
    /// `self.entries` to collect dirty page ids, then iterate that
    /// collection while mutating `self.entries` — a borrow-checker
    /// situation that demands an intermediate `Vec`. Holding the Vec
    /// on `self` lets us reuse its allocation across commits/rollbacks.
    /// For a 10K-page transaction this saves 80 KB of transient
    /// allocation per commit. The std::mem::take/restore dance below
    /// keeps the borrow checker happy: we take the empty vec out,
    /// fill it, drain it, then put it back. On the error path we drop
    /// the vec — acceptable since errors are rare and one realloc on
    /// the next commit is cheap.
    dirty_scratch: Vec<u64>,
    max_pages: usize,
    /// Strict upper bound on the spillway sidecar file in bytes
    /// (excluding per-slot headers). 0 means spillway disabled —
    /// overflow trips CacheFull at the cache cap. Set via Options;
    /// runtime-mutable between transactions via set_spillway_max_bytes.
    spillway_max_bytes: u64,
    /// LRU position policy for commit-drain rehydrated pages. Captured
    /// from Options at construction; runtime-mutable between
    /// transactions via set_drain_insertion. Controls where rehydrated
    /// spillway pages land in the LRU after flush's drain phase:
    /// LruTail = first eviction candidate (good when drained pages are
    /// unlikely to be needed again soon); Mru = recently-touched
    /// semantics (good when the workload revisits recently-committed pages).
    drain_insertion: crate::DrainInsertion,
    /// How to lazily open the spillway when a first spill happens. Held
    /// here rather than opening eagerly because no-spill workloads
    /// shouldn't pay any filesystem cost for a feature they never use.
    spillway_location: crate::SpillwayLocation,
    /// Lazily-initialized spillway. None until the first spill needs it.
    pub(crate) spillway: Option<crate::spillway::Spillway>,
    // Monotonically increasing allocator for shadow-paged new pages. Never
    // reused within a process lifetime except via `truncate()`.
    next_page_id: u64,
    /// Page sealer/opener for encrypted DBs; `None` for plaintext. When `Some`,
    /// `entries` always holds PLAINTEXT while the main file and spillway hold
    /// CIPHERTEXT. Seal happens once at flush (Phase 1a) and at evict-to-spillway
    /// (Task 3.4); open happens at cold load before `verify_checksum`. The caller
    /// MUST have already called `io.set_stride(ENC_PAGE_SIZE)` before installing
    /// the cipher so that on-disk offsets use the 8232-byte encrypted stride.
    cipher: Option<crate::crypto::PageCipher>,
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
        io: PageIo,
        cache_max_bytes: u64,
        spillway_max_bytes: u64,
        drain_insertion: crate::DrainInsertion,
        spillway_location: crate::SpillwayLocation,
    ) -> PageCache {
        let max_pages = (cache_max_bytes / PAGE_SIZE as u64).max(1) as usize;
        let next_page_id = io.page_count().unwrap_or(0);
        PageCache {
            io,
            entries: FxHashMap::default(),
            lru: LruIndex::new(),
            dirty_count: 0,
            dirty_scratch: Vec::new(),
            max_pages,
            spillway_max_bytes,
            drain_insertion,
            spillway_location,
            spillway: None,
            next_page_id,
            cipher: None,
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
        // I48 INVARIANT: entry is guaranteed present — either the hit
        // branch (contains_key was true and no mutation has happened
        // since) or the miss branch just succeeded at load_page, which
        // inserts page_id into self.entries before returning Ok.
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
        // I48 INVARIANT: same as get() above — entry is present on the
        // hit branch (contains_key was true) and on the miss branch
        // because load_page inserts before returning Ok.
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

    /// Test-only: drop a single page from the in-memory cache (and its LRU
    /// slot) without touching the backing file, forcing the next `get` to
    /// re-read it from disk and re-run the checksum verification. Used by the
    /// corrupt-dead-page sweep test to make a checksum-invalid on-disk page
    /// actually surface as `ChecksumMismatch` on read.
    #[cfg(test)]
    pub(crate) fn test_drop_from_cache(&mut self, page_id: u64) {
        self.entries.remove(&page_id);
        self.lru.remove(page_id);
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

    /// I74 (ISSUES.md, 2026-05-22): snapshot the spillway's current
    /// logical-bytes use and max-bytes cap. Returns `None` if the
    /// spillway has never been opened — it is lazily constructed on
    /// the first spill, so `None` distinguishes "no overflow has
    /// happened yet" from "spillway is empty and ready." Returns
    /// `Some((logical_bytes, max_bytes))` otherwise; the ratio
    /// answers "how close to `SpillwayFull` are we?"
    pub fn spillway_capacity(&self) -> Option<(u64, u64)> {
        self.spillway
            .as_ref()
            .map(|spw| (spw.logical_bytes(), spw.max_bytes()))
    }

    /// Allocate a new zeroed page, mark it dirty, return its page_id.
    ///
    /// This is the heart of shadow paging: every write goes to a brand-new
    /// page past the current high-water mark, so no committed page is ever
    /// overwritten during the transaction. On commit, `flush()` writes the
    /// page to disk (implicitly extending the file) and the superblock
    /// swap makes it visible; on rollback, the watermark truncate
    /// (`discard_all_dirty()` + `truncate()`, I3) drops the in-memory
    /// buffer and the on-disk bytes (if any) become orphaned garbage that
    /// the next `truncate()` or freemap reclaim can recover.
    ///
    /// Known v1 simplification (per ARCHITECTURE.md): this allocator never
    /// consults the freemap — it always extends past EOF. Freed pages from
    /// previous transactions are still reclaimed, but via the freemap-aware
    /// reuse path (`cow_alloc` -> `claim_page`, reuse-before-extend), which
    /// is where steady-state allocation goes; `new_page` is only the
    /// fallback when the freemap has no id to hand out.
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
    /// iteration order is unspecified map order (under the Fx hasher it is
    /// deterministic, but we rely on neither). That is fine: all writes share
    /// one fsync, so intra-batch ordering is irrelevant for durability.
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
        // Phase 1a: write every dirty in-cache page to the main file and
        // clear its dirty flag. The early-return path in transaction.rs
        // relies on flush() being idempotent between commits — dirty_count
        // is reset to zero here, not decremented per page, to survive any
        // future reordering of the loop body.
        //
        // I52: reuse self.dirty_scratch across calls. We take it out
        // (now self.dirty_scratch is empty but PageCache still owns it
        // via the destination of the assignment at function end), fill
        // it, drain it, then restore. The take/restore pattern avoids
        // the disjoint-borrow ambiguity of iterating &self.dirty_scratch
        // while calling self.entries.get_mut.
        let mut dirty_scratch = std::mem::take(&mut self.dirty_scratch);
        dirty_scratch.extend(
            self.entries
                .iter()
                .filter(|(_, e)| e.dirty)
                .map(|(&id, _)| id),
        );
        for &page_id in &dirty_scratch {
            // I48 INVARIANT: dirty_scratch was just populated from
            // self.entries (filtered by dirty=true). Nothing between
            // the extend and this loop touches self.entries, so each
            // id is still present and still dirty.
            //
            // Copy the plaintext out before calling write_sealed: write_sealed
            // needs `&mut self` (to call self.io) while `entry` borrows
            // self.entries. The 8 KB stack copy is the same cost as the COW
            // paths elsewhere in the cache that already pay it.
            let plaintext: [u8; PAGE_SIZE] = *self.entries.get(&page_id).unwrap().buf;
            self.write_sealed(page_id, &plaintext)?;
            // Mark clean only AFTER the write succeeds. On error the page
            // stays dirty and the I1 poison model discards it.
            self.entries.get_mut(&page_id).unwrap().dirty = false;
        }
        dirty_scratch.clear();
        self.dirty_scratch = dirty_scratch;
        self.dirty_count = 0;

        // Phase 1b: drain the spillway in batches. For each batch:
        //   - rehydrate (checksum-verify) bytes from the spillway file
        //   - write to the main file
        //   - drop from spillway resident-set (via forget)
        //   - re-insert into cache as clean, per drain_insertion policy
        //   - evict back to max_pages if the insertions over-filled the cache
        //
        // No per-batch fsync is issued. The single trailing fsync in Phase 2
        // covers every write here, preserving the commit's fsync count (the
        // three-fsync protocol: I28 pre-drain + data flush + superblock).
        // A crash before Phase 2's fsync is a rolled-back transaction —
        // no main-file bytes are committed without the superblock swap that
        // follows flush().
        //
        // Batches are sized to `self.max_pages` (the drain_batch argument
        // below) so peak resident memory during the drain stays bounded at
        // ~2× max_pages: each batch re-inserts at most max_pages clean
        // entries, then the trailing eviction loop trims back to the cap
        // before the next batch is fetched. Draining the whole spillway in
        // one shot would instead rehydrate every spilled page into the cache
        // simultaneously, defeating the strict cap exactly when memory is
        // already tightest.
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
                // Rehydrate into a local buffer, then drop the spillway borrow
                // before touching self.entries or self.lru (both need &mut self).
                // I48 INVARIANT: the outer loop's `match self.spillway.as_mut()`
                // matched `Some(_)` to populate `batch`. Nothing between that
                // match and here can drop the spillway — there is no mutating
                // call to self that could call `self.spillway = None`, and
                // drain_spillway holds &mut self exclusively.
                // DURABILITY WINDOW (analogous to Phase 1a above): `forget`
                // drops the spillway's resident copy BEFORE `write_page` makes
                // the main-file copy durable. If `write_page` then fails, the
                // only surviving copy of this dirty page is the local `buf`,
                // which is dropped on the `?` early-return — the page is lost.
                // This is safe ONLY because the I1 poison model treats any
                // commit-protocol failure as fatal: on a drain error the
                // TransactionManager poisons and the caller must drop+reopen, so
                // the half-drained cache is never trusted and the transaction is
                // effectively rolled back. If commit is ever made retryable,
                // reorder to write_page-then-forget so a failed drain leaves the
                // page recoverable from the spillway (review 2026-06-22).
                // Drain: the spillway slot holds either a sealed 8232-byte
                // ciphertext blob (encrypted DB) or a plaintext 8192-byte
                // page image (plaintext DB). In both cases we copy it VERBATIM
                // to the main file via write_page_unit — never re-seal.
                //
                // For the cache re-insertion we need the plaintext page, so:
                //   - Encrypted: open the sealed blob → plaintext.
                //   - Plaintext: the blob IS the plaintext.
                //
                // Seal-once invariant: the page was sealed exactly once, at
                // eviction in maybe_evict Phase B. A second seal here would
                // produce a new nonce and corrupt the on-disk unit.
                let plaintext: Box<[u8; page::PAGE_SIZE]> = {
                    let spw = self.spillway.as_mut().unwrap();
                    let blob = spw.rehydrate(page_id)?;
                    spw.forget(page_id);
                    match &self.cipher {
                        Some(c) => {
                            // blob is ENC_PAGE_SIZE bytes (sealed at eviction).
                            // Copy verbatim to the main file, then open for cache.
                            let unit: [u8; ENC_PAGE_SIZE] = blob
                                .as_slice()
                                .try_into()
                                .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                            self.io.write_page_unit(page_id, &unit)?;
                            let pt = c
                                .open(page_id, &unit)
                                .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                            Box::new(pt)
                        }
                        None => {
                            // blob is PAGE_SIZE bytes; write directly.
                            let pt: [u8; page::PAGE_SIZE] = blob
                                .try_into()
                                .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                            self.io.write_page_unit(page_id, &pt)?;
                            Box::new(pt)
                        }
                    }
                };
                // Re-insert as clean: the bytes are now on the main file,
                // so the cache entry is a valid read-through cache.
                let entry = CacheEntry {
                    buf: plaintext,
                    dirty: false,
                };
                // Use the Entry API to avoid the clippy::map_entry pattern:
                // a `contains_key` + `insert` pair does two hash lookups;
                // `entry(...).or_insert_with(...)` does one.
                if let std::collections::hash_map::Entry::Vacant(e) = self.entries.entry(page_id) {
                    // Not present: insert and register in the LRU index.
                    e.insert(entry);
                    match drain_policy {
                        crate::DrainInsertion::LruTail => self.lru.push_back(page_id),
                        crate::DrainInsertion::Mru => self.lru.push_front(page_id),
                    }
                }
                // If already present — a same-id spill within a batch — skip:
                // the existing clean entry is correct and the bytes were
                // written above.
            }
            // Evict back to max_pages if drain insertions over-filled the
            // cache. Only clean pages are evicted — dirty pages cannot be
            // removed without writing first, and there should be none left at
            // this point (Phase 1a cleared them all). I136: shared loop (its
            // dirty_count early-out is a no-op here — nothing is dirty).
            self.evict_clean_to_cap();
        }
        // Truncate the spillway file to zero once all batches are drained.
        // The file will be reused for the next transaction's overflow without
        // paying an open() cost.
        if let Some(spw) = self.spillway.as_mut() {
            spw.truncate()?;
        }

        // Phase 2: single fsync covers in-cache writes (Phase 1a) and every
        // drained-batch write (Phase 1b). This is the SECOND of three
        // fsyncs in the commit protocol — see Chisel::commit's docstring
        // for the full enumeration. The first is TransactionManager's
        // I28 pre-drain flush (which calls flush() once before this one,
        // to keep CacheFull off the persist_freemap path); the third is
        // the superblock fsync issued by TransactionManager::commit_inner
        // after writing the alternate slot.
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
    ///
    /// `#[allow(dead_code)]`: the original rollback path called this
    /// per-page. Post-I3 (watermark rollback) the production path uses
    /// `discard_all_dirty` + `truncate` instead. Kept here because a
    /// targeted per-page discard might come back when partial-rollback
    /// machinery grows up; the single-id shape is harder to recover
    /// than to delete.
    #[allow(dead_code)]
    pub fn discard(&mut self, page_id: u64) {
        if let Some(entry) = self.entries.remove(&page_id) {
            self.untrack_dirty(entry.dirty);
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
        // I52: same scratch-reuse pattern as flush() — see the comment
        // on the `dirty_scratch` field. Rollback is the rarer call site
        // but uses the same shape so the buffer's peak capacity tracks
        // the larger of the two transactions seen so far.
        let mut dirty_scratch = std::mem::take(&mut self.dirty_scratch);
        dirty_scratch.extend(
            self.entries
                .iter()
                .filter(|(_, e)| e.dirty)
                .map(|(&id, _)| id),
        );
        for &id in &dirty_scratch {
            self.entries.remove(&id);
            self.lru.remove(id);
        }
        dirty_scratch.clear();
        self.dirty_scratch = dirty_scratch;
        // Every removed entry was dirty; the counter resets.
        self.dirty_count = 0;

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
    }

    /// Return the number of whole pages the underlying file can hold.
    ///
    /// This is the PHYSICAL file size in pages, which may exceed the
    /// logical committed `total_pages` if a transaction is in-flight or a
    /// prior crash left tail garbage.
    pub fn file_page_count(&self) -> Result<u64> {
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
    ///     (see `cow_alloc`) so there are no dirty reused-id
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
                self.untrack_dirty(entry.dirty);
            }
            self.lru.remove(id);
        }
        // Drop spillway entries with id >= n. This matches the main
        // file's truncate semantics: anything past the watermark is
        // gone. Matters for rollback_to_inner where we shrink to a
        // savepoint's watermark — the spilled pages with id >= that
        // watermark are pages allocated AFTER the savepoint.
        if let Some(spw) = self.spillway.as_mut() {
            spw.forget_above(n);
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
        // `cow_alloc` (the shared freemap-aware allocator), which — post-I18 — keeps the
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
        // Reusing a freed page is an allocation too: count it so
        // `pages_allocated` reflects total allocation activity, not just file
        // extensions. With the handle table / membership index now allocating
        // COW pages through the freemap (reuse-before-extend), the bulk of a
        // steady-state workload's allocations arrive here, not via `new_page`.
        self.pages_allocated.set(self.pages_allocated.get() + 1);
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

    /// Update the cache's strict upper bound to `bytes`. Caller must
    /// ensure no transaction is in flight (TransactionManager checks
    /// this). Shrinking evicts clean LRU-tail entries until we fit;
    /// dirty entries (which shouldn't exist between transactions) are
    /// preserved and may push the cache temporarily over the new cap
    /// — the next allocation reasserts the limit via maybe_evict.
    ///
    /// I40 (ISSUES.md, 2026-05-22): `Result<()>` here is a deliberate
    /// hedge for plausible future fallibility — shrinking the cache
    /// could surface a `CacheFull`-shaped failure if a future
    /// refactor allows pinned dirty pages to survive between
    /// transactions. Kept now for API stability across that change.
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        let new_max_pages = (bytes / PAGE_SIZE as u64).max(1) as usize;
        self.max_pages = new_max_pages;
        // Best-effort shrink: evict clean entries from the LRU tail (I136).
        self.evict_clean_to_cap();
        Ok(())
    }

    /// Update the spillway cap. The spillway is empty between
    /// transactions (truncated at every commit/rollback), so resize
    /// is a state-free operation. Shrinking to 0 disables the
    /// spillway; subsequent overflow trips CacheFull.
    ///
    /// I40 (ISSUES.md, 2026-05-22): same `Result<()>` hedge as
    /// `set_cache_max_bytes` above — kept for API stability against
    /// future world where shrinking observes in-flight spillway state.
    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.spillway_max_bytes = bytes;
        if let Some(spw) = self.spillway.as_mut() {
            spw.set_max_bytes(bytes);
        }
        Ok(())
    }

    /// Update the drain insertion policy. Captured for use by the next
    /// `flush()` invocation.
    ///
    /// I40 (ISSUES.md, 2026-05-22): infallible at this layer (state-free
    /// assignment), so it returns `()`. The caller-facing wrappers
    /// (`TransactionManager::set_drain_insertion`, `Chisel::set_drain_insertion`)
    /// keep `Result<()>` because they perform real fallibility checks
    /// (poison + active-txn). Only the bottom layer can honestly
    /// shed the `Result`.
    pub fn set_drain_insertion(&mut self, policy: crate::DrainInsertion) {
        self.drain_insertion = policy;
    }

    /// Install the page cipher for an encrypted DB. The caller MUST have
    /// already called `self.io_mut().set_stride(ENC_PAGE_SIZE)` so that
    /// on-disk offset arithmetic uses the 8232-byte encrypted stride.
    /// Set once at open time after the DEK is unwrapped (or generated).
    pub fn set_cipher(&mut self, cipher: crate::crypto::PageCipher) {
        self.cipher = Some(cipher);
    }

    /// Seal a plaintext page and write its on-disk unit.
    ///
    /// For an encrypted DB (cipher present): seals the 8192-byte plaintext
    /// into the 8232-byte `ct‖tag‖nonce` blob then calls `write_page_unit`.
    /// For a plaintext DB: calls `write_page_unit` directly with the 8192-byte
    /// image (stride == PAGE_SIZE, so the unit is the page image itself).
    ///
    /// This is the seal point for flush Phase 1a (in-cache dirty pages →
    /// main file). Evict-to-spillway seals independently in maybe_evict
    /// Phase B (seal-once invariant: each page is sealed exactly once).
    fn write_sealed(&mut self, page_id: u64, plaintext: &[u8; PAGE_SIZE]) -> Result<()> {
        match &self.cipher {
            Some(c) => {
                let blob = c.seal(page_id, plaintext);
                self.io.write_page_unit(page_id, &blob)
            }
            None => self.io.write_page_unit(page_id, plaintext),
        }
    }

    /// Check if a page is dirty in the cache.
    ///
    /// Used by the transaction layer to reason about whether a page is
    /// safe to drop at savepoint/rollback boundaries.
    pub fn is_dirty(&self, page_id: u64) -> bool {
        self.entries.get(&page_id).is_some_and(|e| e.dirty)
    }

    /// Copy `src`'s page bytes into `dst`, replacing dst's contents.
    ///
    /// I134 (ISSUES.md, 2026-06-21): the COW-clone step shared by the handle
    /// table and membership index (five near-identical copies that bounced the
    /// bytes through a named 8 KB stack array). The stack array is required by
    /// the borrow checker — `get(src)` and `get_mut(dst)` can't both borrow the
    /// cache at once — so naming it once keeps the call sites honest.
    pub(crate) fn copy_page(&mut self, src: u64, dst: u64) -> Result<()> {
        let bytes: [u8; PAGE_SIZE] = *self.get(src)?;
        *self.get_mut(dst)? = bytes;
        Ok(())
    }

    /// Load a page from disk into the cache, verifying its checksum.
    ///
    /// Evict BEFORE reading, so the new arrival does not temporarily push
    /// us two entries over `max_pages`. Checksum verification happens
    /// BEFORE the entry is inserted: a corrupt page never pollutes the
    /// cache, so a retry could (in principle) succeed if the caller
    /// repaired the file externally.
    ///
    /// A checksum mismatch is a fatal corruption error per ARCHITECTURE.md —
    /// `ChecksumMismatch` signals the database is broken, not merely that
    /// the operation failed.
    ///
    /// The encrypted cold-load path has the same severity: a `DecryptionFailed`
    /// from `PageCipher::open` (AEAD tag rejection) is `is_fatal()` — it poisons
    /// the manager exactly like `ChecksumMismatch`, not an operational error the
    /// caller can retry. The XXH3 checksum and the AEAD tag are peer integrity
    /// checks: either failing means the persisted bytes are untrustworthy.
    fn load_page(&mut self, page_id: u64) -> Result<()> {
        self.maybe_evict()?;

        // Check spillway first — a resident page is by definition dirty
        // (it was dirty when spilled). Disk read would return the stale
        // pre-transaction bytes.
        if let Some(spw) = self.spillway.as_mut() {
            if spw.is_resident(page_id) {
                // Rehydrate: the spillway holds either a sealed blob (encrypted DB)
                // or raw plaintext (plaintext DB). Open ciphertext → plaintext
                // before re-inserting into the cache. The page is still dirty
                // (it was dirty when it was evicted to the spillway).
                let blob = spw.rehydrate(page_id)?;
                spw.forget(page_id);
                let buf: Box<[u8; page::PAGE_SIZE]> = match &self.cipher {
                    Some(c) => {
                        let unit: [u8; ENC_PAGE_SIZE] = blob
                            .as_slice()
                            .try_into()
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                        let pt = c
                            .open(page_id, &unit)
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?;
                        Box::new(pt)
                    }
                    None => Box::new(
                        blob.try_into()
                            .map_err(|_| ChiselError::DecryptionFailed { page_id })?,
                    ),
                };
                self.entries.insert(
                    page_id,
                    CacheEntry {
                        buf,
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
        //
        // Use a stack-allocated ENC_PAGE_SIZE buffer (the maximum on-disk unit)
        // and read only the first `stride` bytes — no heap allocation on this
        // hot path regardless of whether the DB is encrypted. The cipher branch
        // verifies the AEAD tag (anti-tamper) before the plaintext is cached;
        // the plaintext branch runs verify_checksum on the raw page bytes as before.
        let mut on_disk = [0u8; ENC_PAGE_SIZE];
        let stride = self.io.stride();
        self.io
            .read_page_unit_into(page_id, &mut on_disk[..stride])?;

        let plaintext: [u8; PAGE_SIZE] = match &self.cipher {
            Some(c) => {
                // The on-disk unit is exactly ENC_PAGE_SIZE at this stride;
                // the try_into is infallible (ENC_PAGE_SIZE == 8232 == stride).
                let unit: [u8; ENC_PAGE_SIZE] = on_disk;
                c.open(page_id, &unit)
                    .map_err(|_| ChiselError::DecryptionFailed { page_id })?
            }
            // Plaintext: the first PAGE_SIZE bytes of the on_disk buffer ARE the
            // page image (stride == PAGE_SIZE here, so the copy takes exactly 8192 bytes).
            None => {
                let mut buf = [0u8; PAGE_SIZE];
                buf.copy_from_slice(&on_disk[..PAGE_SIZE]);
                buf
            }
        };

        if !page::verify_checksum(&plaintext) {
            return Err(ChiselError::ChecksumMismatch { page_id });
        }
        self.entries.insert(
            page_id,
            CacheEntry {
                buf: Box::new(plaintext),
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

    /// Decrement `dirty_count` for one just-removed entry, saturating.
    ///
    /// I116 (ISSUES.md, 2026-06-21): the single decrement site for every
    /// entry-removal path that can drop a dirty page — `discard`, `truncate`,
    /// and `maybe_evict` Phase B. Guarding on `was_dirty` keeps the count
    /// correct; `saturating_sub` makes a (buggy) `entries`/`dirty_count` desync
    /// degrade to "cap not strictly enforced until the next flush/rollback
    /// resets the count to 0" rather than an underflow — a debug panic, or a
    /// release wrap to `usize::MAX` that would defeat the `dirty_count ==
    /// entries.len()` short-circuit. (`set_cache_max_bytes` and
    /// `evict_clean_to_cap` evict CLEAN entries only, so they never decrement.)
    fn untrack_dirty(&mut self, was_dirty: bool) {
        if was_dirty {
            self.dirty_count = self.dirty_count.saturating_sub(1);
        }
    }

    /// Evict clean LRU-tail entries until the cache is within `max_pages`.
    ///
    /// I136 (ISSUES.md, 2026-06-21): the one clean-victim loop shared by
    /// `maybe_evict` Phase A, `set_cache_max_bytes`, and `flush` Phase 1b
    /// (previously three near-identical copies — Phase 1b's was missing the
    /// `dirty_count` early-out). Dirty pages are skipped: they are pinned until
    /// `flush()` writes them. The `dirty_count == entries.len()` early-out
    /// avoids an O(n) LRU walk on every allocation in a write-heavy transaction
    /// (every entry dirty → nothing clean to evict; the caller's Phase B /
    /// CacheFull path takes over). Adding it to the Phase 1b path is a no-op
    /// there — Phase 1a already cleared every dirty flag — so one authoritative
    /// loop is safe. Stops early if no clean victim remains.
    fn evict_clean_to_cap(&mut self) {
        while self.entries.len() > self.max_pages {
            if self.dirty_count == self.entries.len() {
                break;
            }
            let victim = self
                .lru
                .iter_lru_to_mru()
                .find(|&id| self.entries.get(&id).is_some_and(|e| !e.dirty));
            match victim {
                Some(id) => {
                    self.entries.remove(&id);
                    self.lru.remove(id);
                }
                None => break,
            }
        }
    }

    /// Enforce the strict `max_pages` cap, evicting or spilling as needed.
    ///
    /// Phase A: evict clean LRU-tail entries until we are within the cap.
    /// Dirty pages are skipped — they are pinned until `flush()` writes
    /// them as part of commit. The `dirty_count` short-circuit avoids an
    /// O(n) LRU walk on every allocation in a write-heavy transaction
    /// where all entries are dirty and no victim exists.
    ///
    /// Phase B: if we are still over the cap and every entry is dirty,
    /// spill the LRU-tail dirty page to the spillway sidecar file. This
    /// keeps the cache at exactly `max_pages` rather than letting it grow
    /// without bound. With `spillway_max_bytes == 0` (spillway disabled),
    /// `CacheFull` fires immediately at the strict cap — the pre-spillway
    /// 8× HARD_CEILING_MULTIPLIER elasticity is gone.
    ///
    /// `is_some_and(|e| !e.dirty)` reads as "entry exists AND is clean"
    /// — exactly the eviction predicate. The missing-entry branch
    /// (`None`) guards against a stale LRU id; it should never fire
    /// in practice because the LRU and the entries map stay in sync.
    fn maybe_evict(&mut self) -> Result<()> {
        // Phase A: evict clean LRU-tail entries until we fit (I136: shared
        // loop, which also carries the dirty_count early-out for Phase B).
        self.evict_clean_to_cap();

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
            //
            // I49 (ISSUES.md, 2026-05-22): translate to a typed
            // CorruptPage error instead of panicking. victim_id was
            // produced by `self.lru.iter_lru_to_mru().next()` above,
            // so `self.entries.remove(&victim_id)` should always be
            // Some — the LRU index and the entries map are kept in
            // sync by `discard`/`truncate`/`flush`. If we observe
            // None here, the two-data-structure invariant broke
            // (most likely a future refactor that touches one without
            // the other), which is genuinely a corruption signal.
            let entry = self
                .entries
                .remove(&victim_id)
                .ok_or(ChiselError::CorruptPage { page_id: victim_id })?;
            self.lru.remove(victim_id);
            // The victim is dirty here (Phase B runs only when every entry is
            // dirty), so this always fires today; `untrack_dirty` (I116) guards
            // on the flag and saturates anyway, so a future change that let a
            // clean page sit at the LRU tail during a spill can't underflow.
            self.untrack_dirty(entry.dirty);

            // Attempt the spill. On failure the victim's uncommitted bytes must
            // NOT be lost: SpillwayFull is operational (non-fatal, doesn't
            // poison — error.rs is_fatal()), and the public contract (lib.rs)
            // says operational errors leave the handle usable, so a caller may
            // continue or commit afterward. Dropping the removed victim here
            // would let `flush()` write a cache that no longer holds this page,
            // and the committed superblock would point at a handle-table page
            // that silently reverted to stale committed content — a durability/
            // atomicity violation (review 2026-06-22). So restore the victim
            // (entries + LRU tail + dirty_count) before propagating: a later
            // commit then flushes it correctly, and a rollback discards it
            // cleanly. `entry` was lifted out of `self.entries` above precisely
            // so `&entry.buf` does not conflict with the `&mut self` borrow that
            // `ensure_spillway` takes. An `ensure_spillway()` open error (I/O)
            // is restored the same way.
            // Seal-once invariant (Task 3.4): for an encrypted DB, seal the
            // plaintext page here before handing the bytes to the spillway.
            // The spillway then holds ciphertext for the lifetime of the txn.
            // Drain will copy that ciphertext verbatim to the main file (no
            // second seal). Rehydrate (load_page spillway branch) will open it.
            // For plaintext DBs the page bytes are stored as-is (no cipher).
            let spill_blob: Vec<u8> = match &self.cipher {
                Some(c) => c.seal(victim_id, entry.buf.as_ref()).to_vec(),
                None => entry.buf.as_ref().to_vec(),
            };
            let spill_result = self
                .ensure_spillway()
                .and_then(|spw| spw.spill(victim_id, &spill_blob));
            if let Err(e) = spill_result {
                if entry.dirty {
                    self.dirty_count += 1;
                }
                self.lru.push_back(victim_id); // back to the LRU tail it came from
                self.entries.insert(victim_id, entry);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Lazy-open the spillway on first spill. Subsequent calls reuse
    /// the existing one. Returns SpillwayFull if `spillway_max_bytes`
    /// is 0 (spillway disabled by configuration); the caller must
    /// fall back to the legacy CacheFull path in that case.
    fn ensure_spillway(&mut self) -> Result<&mut crate::spillway::Spillway> {
        // Caller contract: ensure_spillway is only called when
        // spillway_max_bytes > 0. The single caller (maybe_evict's
        // Phase B) guards on this and routes to CacheFull when the
        // spillway is disabled. This debug_assert documents the
        // invariant — release builds rely on the caller's guard.
        debug_assert!(
            self.spillway_max_bytes != 0,
            "ensure_spillway called with spillway disabled — caller missed the guard"
        );
        // Invariant: ensure_spillway is only called from spill paths
        // (maybe_evict's dirty-overflow branch in Task 9 onward). Read-
        // only databases raise ReadOnlyMode at begin() before any page
        // mutation, so dirty pages cannot exist on a read-only db, and
        // this method is therefore unreachable on a read-only db. If a
        // future task changes that invariant, add an explicit
        // self.io.is_read_only() guard here that returns
        // ChiselError::ReadOnlyMode — Spillway::open_file would
        // otherwise create a sidecar file for what is supposed to be a
        // read-only open.
        if self.spillway.is_none() {
            // Encrypted DBs store the sealed 8232-byte unit in each slot so the
            // spillway NEVER holds plaintext (seal-once invariant, Task 3.4).
            // Plaintext DBs use the historical 8192-byte payload; slot layout is
            // SLOT_HEADER_SIZE + payload_size in both cases.
            let payload_size = if self.cipher.is_some() {
                ENC_PAGE_SIZE
            } else {
                page::PAGE_SIZE
            };
            let spw = match &self.spillway_location {
                crate::SpillwayLocation::Path(p) => {
                    crate::spillway::Spillway::open_file(p, self.spillway_max_bytes, payload_size)?
                }
                crate::SpillwayLocation::InMemory => {
                    crate::spillway::Spillway::open_memory(self.spillway_max_bytes, payload_size)
                }
            };
            self.spillway = Some(spw);
        }
        // I48 INVARIANT: the `if self.spillway.is_none()` block above
        // just set self.spillway = Some(spw); if it didn't run, the
        // spillway was already Some on entry. Either way, unwrap is safe.
        Ok(self.spillway.as_mut().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_io::{Fault, PageIo};
    use tempfile::{NamedTempFile, TempDir};

    fn fresh_cache(max_pages: usize) -> (TempDir, PageCache) {
        // I66 (ISSUES.md, 2026-05-22): use TempDir over NamedTempFile +
        // std::mem::forget(file). The pre-I66 helper leaked the file
        // path on every call so the PageIo's fd would outlive the
        // helper; with TempDir, the dir (and everything in it) is
        // RAII-cleaned at end of the test scope when the caller drops
        // the returned `_dir`. spillway.rs:open_file_truncates_existing_content
        // is the in-tree model for this pattern.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let io = PageIo::open(&db_path, false).unwrap();
        // spillway_max_bytes=0 preserves the legacy "fail fast on cache
        // pressure" contract for all existing page_cache tests.
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        // Spillway is intentionally InMemory even though PageIo is file-
        // backed: tests that exercise the spillway will set spillway_max_bytes
        // > 0, but those tests don't care about on-disk spillway artifacts —
        // the InMemory backing keeps them filesystem-independent. Tests that
        // DO want a file-backed spillway should construct PageCache::new
        // directly with SpillwayLocation::Path(...).
        let cache = PageCache::new(
            io,
            cache_max_bytes,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        (dir, cache)
    }

    // Regression test for spec §"Failure surface" — when spillway is
    // disabled (max_bytes = 0), CacheFull fires at the strict cache
    // cap, with no elasticity. (Replaces the pre-spillway test that
    // exercised the 8× HARD_CEILING_MULTIPLIER.)
    #[test]
    fn cache_full_fires_at_strict_cap_when_spillway_disabled() {
        let max_pages = 4;
        let (_dir, mut cache) = fresh_cache(max_pages);
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

    // Flushing the cache clears dirty flags, which means the eviction
    // loop can actually evict again, which means subsequent allocations
    // succeed. Covers the intended "commit to recover from CacheFull"
    // recovery path.
    #[test]
    fn cache_full_is_recoverable_via_flush() {
        let max_pages = 4;
        let (_dir, mut cache) = fresh_cache(max_pages);
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

    /// New cache helper that ENABLES the spillway. Used by spillway-
    /// path tests; existing tests use fresh_cache (spillway disabled)
    /// to preserve their CacheFull semantics.
    fn fresh_cache_with_spillway(
        max_pages: usize,
        spillway_max_bytes: u64,
    ) -> (TempDir, PageCache) {
        // I66: TempDir for RAII cleanup (see comment on fresh_cache above).
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let io = PageIo::open(&db_path, false).unwrap();
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        let cache = PageCache::new(
            io,
            cache_max_bytes,
            spillway_max_bytes,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        (dir, cache)
    }

    #[test]
    fn dirty_overflow_spills_when_spillway_enabled() {
        let max_pages = 4;
        // Spillway has room for 8 spilled pages.
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);
        // Allocate 8 dirty pages — 4 in cache, 4 spilled.
        for _ in 0..8 {
            cache
                .new_page()
                .expect("allocations should spill, not fail");
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
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);
        for _ in 0..(max_pages + 4) {
            cache.new_page().unwrap();
        }
        // The 9th allocation must trip SpillwayFull.
        let err = cache.new_page().unwrap_err();
        assert!(
            matches!(err, ChiselError::SpillwayFull { limit_bytes } if limit_bytes == spillway_bytes),
            "expected SpillwayFull {{ limit_bytes: {spillway_bytes} }}, got {err:?}"
        );
        // After SpillwayFull NOTHING is dropped (review 2026-06-22 fix): the
        // eviction victim that could not be spilled is restored to the cache, so
        // it transiently holds max_pages + 1 dirty pages (the new allocation
        // plus the restored victim). The caller must rollback or surface the
        // error — neither path loses a page. Previously this asserted
        // max_pages, which encoded the silent victim-drop bug.
        assert_eq!(cache.entries.len(), max_pages + 1);
        assert_eq!(cache.dirty_count, max_pages + 1);
    }

    // Regression (review 2026-06-22): on SpillwayFull during Phase-B eviction,
    // maybe_evict removed the dirty LRU-tail victim from the cache BEFORE the
    // spill was attempted and then dropped it on the `?` error. Because
    // SpillwayFull is operational (non-fatal), a caller could continue and
    // commit, at which point the committed superblock pointed at a page that had
    // silently reverted to stale committed content. The victim — with its
    // uncommitted write — must survive a failed spill.
    #[test]
    fn spillway_full_during_evict_preserves_victim_page() {
        let max_pages = 4;
        let spillway_bytes = 4 * PAGE_SIZE as u64; // 8 dirty pages before SpillwayFull
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);

        // Fill cache (4) + spillway (4) to the brink: the next allocation's
        // Phase-B spill will fail.
        for _ in 0..(max_pages + 4) {
            cache.new_page().unwrap();
        }

        // Identify the page that WILL be evicted (the LRU tail) and stamp a
        // sentinel into it via direct field access — `get_mut` would touch the
        // LRU and change which page is the victim.
        let victim = cache.lru.iter_lru_to_mru().next().unwrap();
        cache.entries.get_mut(&victim).unwrap().buf[0] = 0xCD;

        // Trigger the failing spill.
        let err = cache.new_page().unwrap_err();
        assert!(
            matches!(err, ChiselError::SpillwayFull { .. }),
            "expected SpillwayFull, got {err:?}"
        );

        // The victim must still be retrievable WITH its uncommitted byte. With
        // the bug it was dropped, so get() would (mis)read stale/disk content.
        assert!(
            cache.entries.contains_key(&victim),
            "victim was silently dropped from the cache on SpillwayFull"
        );
        assert_eq!(
            cache.get(victim).unwrap()[0],
            0xCD,
            "victim's uncommitted write was lost on SpillwayFull"
        );
    }

    #[test]
    fn rehydrate_after_spill_returns_in_flight_bytes_not_disk() {
        let max_pages = 2;
        let spillway_bytes = 4 * PAGE_SIZE as u64;
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);

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

    // Regression test for ISSUES.md I20. claim_page previously silently
    // dropped any prior dirty writes on the claimed id: it unconditionally
    // removed the existing cache entry and inserted a fresh zeroed one.
    // The only legitimate caller is `cow_alloc` (the shared freemap-aware
    // allocator), which must never return an id already dirty in the current txn —
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
        let (_dir, mut cache) = fresh_cache(64);

        // new_page produces a fresh dirty entry. claim_page'ing that
        // same id is exactly the forbidden path the I20 assert guards.
        let id = cache.new_page().unwrap();
        assert!(cache.is_dirty(id));
        let _ = cache.claim_page(id);
    }

    #[test]
    fn claim_page_keeps_dirty_count_consistent() {
        // I114: the I20 debug_assert! in claim_page is a no-op under
        // `cargo test --release` (the wheel gate, wheels.yml). This asserts the
        // OBSERVABLE accounting consequence that holds in EVERY profile: claiming
        // a CLEAN page (the legitimate freemap-reuse path) adds exactly one dirty
        // entry. A regression that mis-tracks dirty_count fails here in release
        // too — complementing the debug-only claim_page_asserts_on_dirty_page,
        // which catches the illegitimate (claim-a-dirty-page) path.
        let (_dir, mut cache) = fresh_cache(64);
        let id = cache.new_page().unwrap();
        cache.flush().unwrap(); // page becomes clean
        assert!(
            !cache.is_dirty(id),
            "precondition: page clean before reclaim"
        );

        let before = cache.dirty_count;
        cache.claim_page(id).unwrap();
        assert!(cache.is_dirty(id), "reclaimed page is dirty");
        assert_eq!(
            cache.dirty_count,
            before + 1,
            "claim of a clean page must add exactly one dirty entry (I20 accounting)"
        );
        // The real I20 invariant is counter/flag CONSISTENCY: dirty_count must
        // equal the actual number of dirty entries. A mis-accounting claim (the
        // bug the debug_assert guards) would desync the counter from the flags
        // even on this legitimate clean-page path — the delta assertion above
        // alone could not catch a desync that predated the claim.
        let actual_dirty = cache.entries.values().filter(|e| e.dirty).count();
        assert_eq!(
            cache.dirty_count, actual_dirty,
            "dirty_count must equal the live dirty-entry count (I20 accounting)"
        );
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
        // Re-fetch the original page; it has been evicted (33 pages total > 16-page
        // capacity). The re-fetch must be a cache MISS, not a hit.
        let h1 = cache.cache_hit_count();
        let m1 = cache.cache_miss_count();
        let _ = cache.get(id).unwrap();
        assert_eq!(
            cache.cache_hit_count(),
            h1,
            "evicted page re-fetch must not be a cache hit"
        );
        assert_eq!(
            cache.cache_miss_count(),
            m1 + 1,
            "evicted page re-fetch must record a cache miss"
        );
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
        for _ in 0..3 {
            let id = cache.new_page().unwrap();
            crate::page::stamp_checksum(cache.get_mut(id).unwrap());
        }
        assert_eq!(cache.pages_allocated_count(), 3);

        // Flush so the pages are clean; claim_page asserts the reused id is not
        // dirty (the freemap never hands back an id with pending writes).
        cache.flush().unwrap();

        // Reuse via claim_page also counts as an allocation: it is the same
        // allocation work (cache insert + maybe_evict), just reusing a freed
        // page id instead of extending the file. Once the handle table and
        // membership index allocate COW pages through the freemap-aware path,
        // most allocations are reuses; a counter that ignored them would read
        // ~0 for a steady-state mutating workload.
        cache.claim_page(0).unwrap();
        assert_eq!(cache.pages_allocated_count(), 4);
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

    #[test]
    fn discard_all_dirty_truncates_spillway() {
        let max_pages = 2;
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);
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
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);
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
            assert!(
                !spw.is_resident(id),
                "page {id} should be gone after truncate(3)"
            );
        }
    }

    #[test]
    fn flush_drains_spilled_pages_to_main_file() {
        // Verifies the Phase 1b drain loop: spilled pages must land on the
        // main file after flush(), be readable back through the cache, and
        // leave the spillway empty. Uses max_pages=2 so that allocating 4
        // pages forces 2 into the spillway.
        let max_pages = 2;
        let spillway_bytes = 8 * PAGE_SIZE as u64;
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);

        // Allocate 4 pages — 2 in cache, 2 overflow to spillway.
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = cache.new_page().unwrap();
            ids.push(id);
            // Write a distinguishable sentinel per page and stamp a valid
            // checksum so the spillway's per-slot checksum verifies it.
            let buf = cache.get_mut(id).unwrap();
            buf[0] = i as u8;
            crate::page::stamp_checksum(buf);
        }
        assert_eq!(
            cache.spillway.as_ref().unwrap().slot_count(),
            2,
            "two pages should have been spilled"
        );

        // flush() must drain the spillway into the main file and emit a single
        // fsync covering both in-cache and drained writes.
        cache.flush().unwrap();

        // Spillway is now empty (truncated). None is also acceptable if
        // the spillway was never opened (no spill occurred).
        if let Some(spw) = cache.spillway.as_ref() {
            assert_eq!(spw.slot_count(), 0, "spillway must be empty after flush");
        }

        // Each page can be read back from the cache (or, if evicted, from disk)
        // with its sentinel byte intact. This confirms the drained bytes were
        // committed to the main file before the fsync.
        for (i, &id) in ids.iter().enumerate() {
            let buf = cache.get(id).unwrap();
            assert_eq!(
                buf[0], i as u8,
                "page {id} lost its sentinel after flush drain"
            );
        }
    }

    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──
    //
    // The originals constructed a PageCache directly via the constructor
    // arguments the integration test relied on (cache_max_bytes,
    // spillway_max_bytes, DrainInsertion, SpillwayLocation). Spelling
    // those out here rather than calling `fresh_cache` preserves the
    // original test intent (file-backed, in-memory spillway, distinct
    // cache caps) verbatim.

    #[test]
    fn test_cache_write_and_read() {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut cache = PageCache::new(
            io,
            16 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );

        let page_id = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(page_id).unwrap();
            buf[0] = 0x42;
            buf[100] = 0xFF;
            crate::page::stamp_checksum(buf);
        }
        let buf = cache.get(page_id).unwrap();
        assert_eq!(buf[0], 0x42);
        assert_eq!(buf[100], 0xFF);
    }

    #[test]
    fn test_cache_flush_persists_to_disk() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_owned();

        {
            let io = PageIo::open(&path, false).unwrap();
            let mut cache = PageCache::new(
                io,
                16 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let page_id = cache.new_page().unwrap();
            {
                let buf = cache.get_mut(page_id).unwrap();
                buf[0] = 0xAB;
                crate::page::stamp_checksum(buf);
            }
            cache.flush().unwrap();
        }

        {
            let mut io = PageIo::open(&path, false).unwrap();
            let buf = io.read_page(0).unwrap();
            assert_eq!(buf[0], 0xAB);
            assert!(crate::page::verify_checksum(&buf));
        }
    }

    // Test that dirty pages are never silently evicted. Three pages are
    // allocated into a cache with max_pages=3 — all three stay in memory,
    // and their writes are visible without a flush (dirty-pin invariant).
    // Using max_pages=3 rather than 2 because the cache is now a STRICT
    // bound: with spillway_max_bytes=0 and max_pages=2, allocating a 3rd
    // dirty page trips CacheFull at the cap rather than growing past it.
    // The test intent (dirty pages are retained until flush) is preserved.
    #[test]
    fn test_cache_eviction_does_not_evict_dirty() {
        let file = NamedTempFile::new().unwrap();
        let io = PageIo::open(file.path(), false).unwrap();
        let mut cache = PageCache::new(
            io,
            3 * PAGE_SIZE as u64,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );

        let p0 = cache.new_page().unwrap();
        let p1 = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(p0).unwrap();
            buf[0] = 0x01;
            crate::page::stamp_checksum(buf);
        }
        {
            let buf = cache.get_mut(p1).unwrap();
            buf[0] = 0x02;
            crate::page::stamp_checksum(buf);
        }

        let p2 = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(p2).unwrap();
            buf[0] = 0x03;
            crate::page::stamp_checksum(buf);
        }

        assert_eq!(cache.get(p0).unwrap()[0], 0x01);
        assert_eq!(cache.get(p1).unwrap()[0], 0x02);
        assert_eq!(cache.get(p2).unwrap()[0], 0x03);
    }

    #[test]
    fn failed_flush_fsync_leaves_pages_clean_but_nondurable() {
        // I112 (durability window — see the flush() comment): flush clears each
        // page's dirty flag in phase 1a BEFORE the trailing fsync. If that fsync
        // fails, the pages are now CLEAN in the cache yet NOT durable on disk —
        // a state that is safe ONLY because the transaction manager poisons on
        // the fatal IoError (proven in transaction.rs). This makes the hazard
        // observable. spillway_max_bytes=0 keeps flush to a single fsync.
        let (_dir, mut cache) = fresh_cache_with_spillway(64, 0);
        let id = cache.new_page().unwrap();
        assert!(cache.is_dirty(id));
        assert_eq!(cache.dirty_count, 1);

        cache.io().arm_fault(Fault::FailFsync(0));
        let result = cache.flush();
        assert!(
            matches!(result, Err(ChiselError::IoError(_))),
            "flush must surface the fsync IoError, got {result:?}"
        );
        // The dirty flags were already cleared (phase 1a) before the failed
        // fsync. This is the durability window; poison (asserted in
        // transaction.rs) is what makes it safe. Cross-check the counter against
        // the ACTUAL entry flags: both the `dirty_count` counter and every
        // entry's `dirty` bit are cleared even though the fsync — and thus
        // durability — failed. (The `Err(IoError)` assertion above is what
        // proves the fault actually fired; this pair documents the hazard.)
        assert_eq!(
            cache.dirty_count, 0,
            "phase-1a cleared dirty flags before the (failed) fsync"
        );
        assert_eq!(
            cache.entries.values().filter(|e| e.dirty).count(),
            0,
            "phase-1a cleared the real dirty flags, not just the counter"
        );
    }

    // -----------------------------------------------------------------------
    // Encrypted page-cache tests (Task 3.3)
    // -----------------------------------------------------------------------

    use crate::crypto::{random_dek, PageCipher, ENC_PAGE_SIZE};

    /// Build a file-backed cache with stride=ENC_PAGE_SIZE and a PageCipher
    /// installed. The stride must be set on the PageIo BEFORE construction so
    /// page_count() is seeded correctly; the cipher is installed via set_cipher.
    fn fresh_encrypted_cache(max_pages: usize) -> (TempDir, PageCache) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let mut io = PageIo::open(&db_path, false).unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        let mut cache = PageCache::new(
            io,
            cache_max_bytes,
            0,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        cache.set_cipher(PageCipher::new(random_dek()));
        (dir, cache)
    }

    /// A plaintext data page written, flushed, evicted, and cold-loaded must
    /// round-trip its exact bytes through the plaintext path (no cipher branch).
    ///
    /// NOTE: use offsets < CHECKSUM_OFFSET (8184) — stamp_checksum overwrites
    /// bytes 8184..8192 with the XXH3 hash, which is what cold-load verifies.
    #[test]
    fn plaintext_page_roundtrip_unaffected() {
        let (_dir, mut cache) = fresh_cache(8);
        let pid = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(pid).unwrap();
            buf[0] = 0x11;
            buf[4096] = 0xFF; // mid-page, before checksum region (8184..8192)
            page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
        cache.test_drop_from_cache(pid);
        let read = cache.get(pid).unwrap();
        assert_eq!(read[0], 0x11);
        assert_eq!(read[4096], 0xFF);
    }

    /// Encrypted data-page round-trip: write → flush (seals to disk) → evict
    /// → cold load (opens from disk) → plaintext bytes match original.
    ///
    /// NOTE: use offsets < CHECKSUM_OFFSET (8184) — stamp_checksum overwrites
    /// bytes 8184..8192 with the XXH3 hash, which is what cold-load verifies.
    #[test]
    fn encrypted_page_round_trips_through_seal_open() {
        let (_dir, mut cache) = fresh_encrypted_cache(8);
        let pid = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(pid).unwrap();
            buf[0] = 0x9C;
            buf[4096] = 0xC9; // middle of the page body, before the checksum region
            page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
        // Force a cold read: drop from cache so load_page hits the disk unit.
        cache.test_drop_from_cache(pid);
        let read = cache.get(pid).unwrap();
        assert_eq!(read[0], 0x9C, "first byte must survive seal/open");
        assert_eq!(read[4096], 0xC9, "mid-page byte must survive seal/open");
    }

    // -----------------------------------------------------------------------
    // Encrypted spillway tests (Task 3.4)
    // -----------------------------------------------------------------------

    /// Encrypted cache + spillway helper: stride=ENC_PAGE_SIZE, cipher installed,
    /// spillway enabled (InMemory for filesystem independence). `max_pages` is
    /// the strict cache cap; `spillway_pages` is the spillway capacity in pages
    /// (each spilled slot is ENC_PAGE_SIZE bytes, so spillway_max_bytes reflects that).
    fn fresh_encrypted_cache_with_spillway(
        max_pages: usize,
        spillway_pages: usize,
    ) -> (TempDir, PageCache) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.chisel");
        let mut io = PageIo::open(&db_path, false).unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        let cache_max_bytes = max_pages as u64 * PAGE_SIZE as u64;
        // Spillway slots hold ENC_PAGE_SIZE bytes each for encrypted DBs.
        let spillway_max_bytes =
            (spillway_pages as u64) * (crate::spillway::SLOT_HEADER_SIZE + ENC_PAGE_SIZE) as u64;
        let mut cache = PageCache::new(
            io,
            cache_max_bytes,
            spillway_max_bytes,
            crate::DrainInsertion::LruTail,
            crate::SpillwayLocation::InMemory,
        );
        cache.set_cipher(PageCipher::new(random_dek()));
        (dir, cache)
    }

    /// Spilling an encrypted page must store ciphertext in the spillway slot,
    /// not plaintext. We write a known sentinel byte to page A, force it to
    /// spill by allocating enough pages to overflow the cache, then read the
    /// raw spillway slot bytes and assert the sentinel is NOT present verbatim.
    #[test]
    fn encrypted_spill_stores_ciphertext_not_plaintext() {
        // 2-page cache; spillway fits 4 encrypted pages.
        let (_dir, mut cache) = fresh_encrypted_cache_with_spillway(2, 4);

        let id_a = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(id_a).unwrap();
            // Distinctive 8-byte sentinel at offset 0 (before checksum region).
            buf[..8].copy_from_slice(b"SENTINEL");
            page::stamp_checksum(buf);
        }

        // Force id_a to spill: two more allocations overflow the 2-page cache.
        cache.new_page().unwrap();
        cache.new_page().unwrap();

        // id_a is now in the spillway, not the cache.
        assert!(
            !cache.entries.contains_key(&id_a),
            "page A should have spilled out of the cache"
        );
        {
            let spw = cache.spillway.as_ref().unwrap();
            assert!(spw.is_resident(id_a), "page A must be in the spillway");
        }
        // Read the spillway slot bytes and confirm the plaintext sentinel is absent.
        // `rehydrate` returns the stored blob (verifying the slot checksum); for an
        // encrypted DB that blob is XChaCha20-Poly1305 ciphertext — the sentinel
        // must NOT appear verbatim anywhere in it.
        let blob = cache.spillway.as_mut().unwrap().rehydrate(id_a).unwrap();
        let sentinel_pos = blob.windows(8).position(|w| w == b"SENTINEL");
        assert!(
            sentinel_pos.is_none(),
            "plaintext sentinel found verbatim in spillway slot — spill did not encrypt"
        );
    }

    /// After spilling encrypted pages and flushing (drain → main file), a cold
    /// read of each spilled page must return the correct plaintext.
    #[test]
    fn encrypted_spill_drain_and_cold_read_round_trips() {
        // 2-page cache; spillway for 4 encrypted pages. Allocate 4 total: 2 in
        // cache, 2 spilled. Flush drains to the main file; then evict all and
        // cold-read all four back.
        let (_dir, mut cache) = fresh_encrypted_cache_with_spillway(2, 4);

        let mut ids = Vec::new();
        for n in 0..4u8 {
            let pid = cache.new_page().unwrap();
            {
                let buf = cache.get_mut(pid).unwrap();
                buf[0] = 0x40 | n; // distinct sentinel per page
                page::stamp_checksum(buf);
            }
            ids.push(pid);
        }
        // Drain spilled pages to main file and write remaining in-cache dirty pages.
        cache.flush().unwrap();

        // Evict all entries so every subsequent get() is a cold disk read.
        for &pid in &ids {
            cache.test_drop_from_cache(pid);
        }

        // Cold read: each page must return its plaintext sentinel.
        for (n, &pid) in ids.iter().enumerate() {
            let buf = cache.get(pid).unwrap();
            assert_eq!(
                buf[0],
                0x40 | n as u8,
                "page {pid} cold-read returned wrong byte after encrypted spill+drain"
            );
        }
    }

    /// Rehydrate path: reading a page that is STILL resident in the spillway
    /// (not yet drained) must return the correct plaintext, NOT disk content.
    #[test]
    fn encrypted_rehydrate_returns_correct_plaintext() {
        let (_dir, mut cache) = fresh_encrypted_cache_with_spillway(2, 4);

        let id_a = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(id_a).unwrap();
            buf[0] = 0xEE;
            page::stamp_checksum(buf);
        }

        // Force id_a to spill.
        cache.new_page().unwrap();
        cache.new_page().unwrap();

        assert!(
            !cache.entries.contains_key(&id_a),
            "precondition: id_a must be spilled, not cached"
        );
        assert!(cache.spillway.as_ref().unwrap().is_resident(id_a));

        // Reading id_a rehydrates from the spillway — must not see disk zeros.
        let buf = cache.get(id_a).unwrap();
        assert_eq!(
            buf[0], 0xEE,
            "rehydrated encrypted page must return in-flight plaintext"
        );
    }

    /// Full end-to-end: encrypted DB, several pages force spill, commit
    /// (flush drains), DB is reopened (simulated by cold-evicting all entries),
    /// all pages cold-read back correctly.
    #[test]
    fn encrypted_spill_full_round_trip_multiple_pages() {
        let (_dir, mut cache) = fresh_encrypted_cache_with_spillway(2, 8);

        let mut ids = Vec::new();
        for n in 0..6u8 {
            let pid = cache.new_page().unwrap();
            {
                let buf = cache.get_mut(pid).unwrap();
                buf[0] = n;
                buf[100] = n.wrapping_mul(7);
                page::stamp_checksum(buf);
            }
            ids.push(pid);
        }

        cache.flush().unwrap();

        for &pid in &ids {
            cache.test_drop_from_cache(pid);
        }

        for (n, &pid) in ids.iter().enumerate() {
            let buf = cache.get(pid).unwrap();
            assert_eq!(
                buf[0], n as u8,
                "page {pid} byte[0] mismatch after full round trip"
            );
            assert_eq!(
                buf[100],
                (n as u8).wrapping_mul(7),
                "page {pid} byte[100] mismatch after full round trip"
            );
        }
    }

    /// Plaintext spill/drain must be byte-identical to before Task 3.4 —
    /// no cipher, slot is PAGE_SIZE, drain goes through write_page_unit.
    #[test]
    fn plaintext_spill_drain_unchanged() {
        let max_pages = 2;
        let spillway_bytes = 4 * PAGE_SIZE as u64;
        let (_dir, mut cache) = fresh_cache_with_spillway(max_pages, spillway_bytes);

        let mut ids = Vec::new();
        for n in 0..4u8 {
            let pid = cache.new_page().unwrap();
            {
                let buf = cache.get_mut(pid).unwrap();
                buf[0] = 0xB0 | n;
                page::stamp_checksum(buf);
            }
            ids.push(pid);
        }
        cache.flush().unwrap();

        for &pid in &ids {
            cache.test_drop_from_cache(pid);
        }
        for (n, &pid) in ids.iter().enumerate() {
            let buf = cache.get(pid).unwrap();
            assert_eq!(
                buf[0],
                0xB0 | n as u8,
                "plaintext spill/drain page {pid} byte mismatch"
            );
        }
    }

    /// Tampering with a ciphertext byte must surface DecryptionFailed (AEAD
    /// authentication failure). This proves the AEAD tag is verified on open.
    #[test]
    fn tampered_ciphertext_surfaces_decryption_failed() {
        let (_dir, mut cache) = fresh_encrypted_cache(8);
        let pid = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(pid).unwrap();
            buf[10] = 0x42;
            page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
        cache.test_drop_from_cache(pid);
        // Flip one byte inside the 8192-byte ciphertext region (byte 42 of the
        // 8232-byte on-disk unit, well before the 16-byte tag at bytes 8192..8208).
        {
            let mut blob = cache.io_mut().read_page_unit(pid).unwrap();
            blob[42] ^= 0x01;
            cache.io_mut().write_page_unit(pid, &blob).unwrap();
        }
        let err = cache.get(pid).unwrap_err();
        assert!(
            matches!(err, ChiselError::DecryptionFailed { page_id } if page_id == pid),
            "expected DecryptionFailed, got {err:?}"
        );
    }
}
