// lib.rs — Chisel: a transactional slot-based storage engine.
//
// Role in system: top of the dependency graph. This file is a thin surface
// over `TransactionManager`; it owns no storage logic of its own. Its job is
// to (a) present a stable public API, (b) translate `Options` into the right
// open/create path, and (c) re-export error and result types. All real work
// lives below in `transaction.rs` and further down.
//
// Concurrency model: a `Chisel` value is NOT `Sync` and is intended for
// single-threaded use. All mutating methods take `&mut self`, which by
// construction serializes access through the borrow checker. There is no
// internal locking beyond that.
//
// Process model: `PageIo` acquires an exclusive advisory `flock` on open, so
// at most one `Chisel` (in any process, on the same host/filesystem) can hold
// a given database file at a time. A second `open()` on the same path returns
// `LockFailed` rather than blocking.
//
// Durability model: see `transaction.rs`. Commits are shadow-paged and
// finalized by a superblock swap; there is no WAL and no background writer.

pub mod data_page;
pub mod defrag;
pub mod error;
pub mod freemap;
pub mod handle_table;
mod lru;
pub mod overflow;
pub mod page;
pub mod page_cache;
pub mod page_io;
mod spillway;
pub mod stats;
pub mod superblock;
pub mod transaction;

pub use error::{ChiselError, Result};

// Re-exports of the curated public surface. The internal modules these
// items live in will be locked down to `pub(crate)` in a follow-up commit
// (ISSUES.md I35); these re-exports define the supported access paths and
// keep the documented API at the crate root.
pub use defrag::{DefragOptions, DefragStats};
pub use page::PAGE_SIZE;
pub use stats::{ChiselCounters, Stats};
pub use superblock::{
    DEFAULT_SUPERBLOCK_COUNT, MAX_SUPERBLOCKS, MIN_SUPERBLOCKS, NAMED_ROOT_COUNT,
    NAMED_ROOT_NAME_LEN,
};

use std::path::Path;

use page_cache::PageCache;
use page_io::PageIo;
use transaction::TransactionManager;

/// Open-time options. These are consumed once during `Chisel::open` and not
/// retained on the live handle; changing them later requires reopening.
///
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

/// How to open a spillway sidecar. `Path` for file-backed databases
/// (path is the main db path; spillway will be at `<path>.spillway`),
/// `InMemory` for memory-backed.
#[derive(Debug, Clone)]
pub enum SpillwayLocation {
    Path(std::path::PathBuf),
    InMemory,
}

impl Default for Options {
    fn default() -> Options {
        let cache_max_bytes = 8 * 1024 * 1024; // 8 MiB = 1024 × 8 KiB pages
        Options {
            cache_max_bytes,
            spillway_max_bytes: cache_max_bytes.saturating_mul(1024),
            drain_insertion: DrainInsertion::LruTail,
            create_if_missing: true,
            read_only: false,
            superblock_count: superblock::DEFAULT_SUPERBLOCK_COUNT,
        }
    }
}

/// A live handle to an open Chisel database.
///
/// Owns (transitively) the page cache and the current in-memory view of
/// the superblock roots. For file-backed databases it also owns the
/// exclusive `flock`; memory-backed databases (opened via
/// `open_in_memory[_with_options]`) have no lock because the `Vec`-backed
/// `PageIo` is itself the database and cannot be opened twice by
/// construction. Dropping a `Chisel` releases the page cache and closes
/// the underlying file, which in turn releases the `flock` on the file
/// path (the lock is tied to the file descriptor, so drop order is what
/// matters — not an explicit unlock call).
///
/// IMPORTANT: dropping without calling `commit()` on an in-flight transaction
/// discards that transaction. Shadow paging guarantees the on-disk state is
/// still the last committed state, not a partial write.
///
/// Poison model (see ISSUES.md I1): if any method returns a fatal error
/// (I/O failure, checksum mismatch, corrupt superblock, commit protocol
/// failure), the `Chisel` handle becomes *poisoned*. Every subsequent call
/// — including reads — returns `ChiselError::Poisoned`. The only legal
/// recovery is to drop this `Chisel` and call `Chisel::open` again; the
/// shadow-paging crash-recovery path on reopen returns the database to the
/// last durable state. This mirrors `std::sync::Mutex` poisoning and is
/// necessary because Linux `fsync` semantics (fsyncgate, 2018) do not
/// permit safely retrying a failed fsync — the kernel may have discarded
/// the dirty pages before reporting the error.
pub struct Chisel {
    txm: TransactionManager,
}

impl Chisel {
    /// Open or create a Chisel database at `path`.
    ///
    /// The "exists" check deliberately treats a zero-length file as
    /// nonexistent: a freshly-created-but-unwritten file (e.g. from a crash
    /// between `creat(2)` and the first superblock write, or from a user
    /// `touch`) has no valid superblock and must go through the
    /// `create_new` path. Without this, `open_existing` would try to parse
    /// an empty file and fail with a corruption error.
    ///
    /// Acquires an exclusive `flock` on the file before any parsing, so a
    /// second concurrent `open()` on the same path fails fast with
    /// `LockFailed` rather than racing on the superblock.
    pub fn open(path: &Path, options: Options) -> Result<Chisel> {
        // R4: validate superblock_count before touching the file.
        // Only meaningful on the create path, but we check it always
        // so a malformed Options is caught up front rather than after
        // the file has been opened.
        if options.superblock_count < superblock::MIN_SUPERBLOCKS
            || options.superblock_count > superblock::MAX_SUPERBLOCKS
        {
            return Err(ChiselError::InvalidSuperblockCount {
                value: options.superblock_count,
            });
        }

        let file_exists = path.exists()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);

        if !file_exists && !options.create_if_missing {
            return Err(ChiselError::FileNotFound);
        }

        let io = PageIo::open(path, options.read_only)?;
        let cache = PageCache::new(
            io,
            options.cache_max_bytes,
            options.spillway_max_bytes,
            options.drain_insertion,
            SpillwayLocation::Path(path.to_path_buf()),
        );

        let txm = if file_exists {
            // Existing database: N is discovered from the on-disk
            // superblock. options.superblock_count is ignored here.
            TransactionManager::open_existing(cache)?
        } else {
            TransactionManager::create_new(cache, options.superblock_count)?
        };

        Ok(Chisel { txm })
    }

    /// Open a non-durable, memory-backed Chisel database. Intended for
    /// benchmark comparisons against SQLite `:memory:` and for tests that
    /// do not need filesystem persistence. All data is lost when the
    /// returned `Chisel` is dropped.
    ///
    /// Uses default `Options`. For a tuned cache size or superblock count,
    /// use `open_in_memory_with_options`.
    pub fn open_in_memory() -> Result<Chisel> {
        Self::open_in_memory_with_options(Options::default())
    }

    /// Open a memory-backed Chisel database with explicit options.
    ///
    /// `options.read_only` must be `false`: a fresh memory database must
    /// be writable for the initial superblock bootstrap, and there is no
    /// prior file to reopen read-only. `options.create_if_missing` is
    /// ignored — memory mode always creates a fresh database. All other
    /// options (cache_max_bytes, spillway_max_bytes, drain_insertion,
    /// superblock_count) flow through normally.
    pub fn open_in_memory_with_options(options: Options) -> Result<Chisel> {
        if options.read_only {
            // Fail fast rather than bootstrapping and then blocking the
            // superblock write with ReadOnlyMode: the caller almost
            // certainly passed `read_only: true` by mistake.
            return Err(ChiselError::ReadOnlyMode);
        }
        if options.superblock_count < superblock::MIN_SUPERBLOCKS
            || options.superblock_count > superblock::MAX_SUPERBLOCKS
        {
            return Err(ChiselError::InvalidSuperblockCount {
                value: options.superblock_count,
            });
        }

        let io = PageIo::open_in_memory()?;
        let cache = PageCache::new(
            io,
            options.cache_max_bytes,
            options.spillway_max_bytes,
            options.drain_insertion,
            SpillwayLocation::InMemory,
        );
        let txm = TransactionManager::create_new(cache, options.superblock_count)?;
        Ok(Chisel { txm })
    }

    /// Explicit close. Exists for API symmetry and so callers can observe a
    /// `Result` at teardown; functionally identical to letting the value
    /// drop, since release of the flock and file descriptor happens in
    /// `Drop`. The `Result` return is currently always `Ok`, but is kept so
    /// future implementations can surface fsync/close errors without a
    /// breaking change.
    pub fn close(self) -> Result<()> {
        drop(self);
        Ok(())
    }

    /// Begin a new transaction. All mutating operations below require an
    /// active transaction; `allocate`/`update`/`delete` will return
    /// `NoActiveTransaction` otherwise. Only one transaction is active at a
    /// time — there is no nesting beyond savepoints.
    pub fn begin(&mut self) -> Result<()> {
        self.txm.begin()
    }

    /// Commit the active transaction. Performs two fsyncs (dirty data pages,
    /// then the alternate superblock) before returning — this is the point
    /// at which changes become durable. A crash before the superblock fsync
    /// leaves the previous committed state intact.
    pub fn commit(&mut self) -> Result<()> {
        self.txm.commit()
    }

    /// Abort the active transaction. Pages written during the transaction
    /// become unreachable garbage (they are never linked from a superblock),
    /// so rollback is effectively free — no undo log to replay.
    pub fn rollback(&mut self) -> Result<()> {
        self.txm.rollback()
    }

    // Savepoint API: named marks within the active transaction. Implemented
    // by snapshotting the in-memory roots — cheap because the on-disk pages
    // written since the savepoint are simply abandoned on `rollback_to`, the
    // same way a full rollback abandons the whole transaction.

    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        self.txm.savepoint(name)
    }

    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        self.txm.rollback_to(name)
    }

    pub fn release(&mut self, name: &str) -> Result<()> {
        self.txm.release(name)
    }

    /// Store `value` and return a freshly minted stable handle. Handles are
    /// u64 identifiers assigned from a monotonic counter in the superblock;
    /// they are never reused within a database's lifetime and are stable
    /// across updates, defrag, and reopens. Physical location may change;
    /// the handle will not.
    ///
    /// Values up to `transaction::MAX_INLINE_VALUE` are packed into a slot
    /// on a data page (R1 packing — multiple values share a page); larger
    /// values are written to an overflow chain in `overflow.rs`. The
    /// caller cannot tell which path was taken except by consulting
    /// stats; all reads go through the same `read()` entry point.
    pub fn allocate(&mut self, value: &[u8]) -> Result<u64> {
        self.txm.allocate(value)
    }

    /// Read the current value for `handle`. Takes `&self` — the page cache
    /// is mutated on miss (LRU bookkeeping, page loading) via interior
    /// mutability (see F3 in ISSUES.md). The returned `Vec<u8>` is a copy;
    /// the cache retains its own page. Not `Sync` — a `Chisel` is single-
    /// threaded by design, so this `&self` only enables `&self`-taking
    /// read APIs in downstream wrappers (e.g. the client's `StorageEngine`
    /// trait), not cross-thread sharing.
    pub fn read(&self, handle: u64) -> Result<Vec<u8>> {
        self.txm.read(handle)
    }

    /// Replace the value for `handle`. The handle is preserved; the value
    /// is written to a new slot (and, if it crosses the inline threshold,
    /// to a new overflow chain). The handle-table entry is rewritten via
    /// COW, so the update is invisible until commit.
    pub fn update(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        self.txm.update(handle, value)
    }

    /// Remove a handle. The handle itself is retired (not reused); any
    /// overflow pages it owned are queued for release on commit.
    pub fn delete(&mut self, handle: u64) -> Result<()> {
        self.txm.delete(handle)
    }

    /// Delete many handles in one transaction (ISSUES.md F1 / I12).
    ///
    /// Motivating use case (from the primary Chisel client): bulk
    /// operations like `drop_table` / `drop_index_table` need to remove
    /// large handle sets without leaking pages. This is a convenience
    /// wrapper around a loop of `delete()` calls inside the caller's
    /// active transaction — the atomicity guarantee comes from the
    /// enclosing transaction, not from anything special in this method.
    ///
    /// On error, partial progress remains visible in the current
    /// transaction: rollback or commit to decide whether the half-done
    /// batch should be kept.
    pub fn delete_many(&mut self, handles: &[u64]) -> Result<()> {
        self.txm.delete_many(handles)
    }

    /// Bind `name` to `handle` in the named-root table (ISSUES.md F2).
    /// Names are short mnemonic labels for long-lived handles — typically
    /// one or two per database (e.g. a meta B-tree root). Requires an
    /// active transaction; becomes durable on commit, reverts on
    /// rollback/rollback_to. See `TransactionManager::set_root_name` for
    /// validation rules and the fixed table-size limit.
    pub fn set_root_name(&mut self, name: &str, handle: u64) -> Result<()> {
        self.txm.set_root_name(name, handle)
    }

    /// Look up a named root. Returns `Ok(None)` if the name is not bound.
    /// Reads see the transactional view (pending sets/clears are visible
    /// inside an active transaction). Takes `&self` (F3).
    pub fn get_root_name(&self, name: &str) -> Result<Option<u64>> {
        self.txm.get_root_name(name)
    }

    /// Remove a named root. No-op if the name is not bound. Requires an
    /// active transaction; becomes durable on commit.
    pub fn clear_root_name(&mut self, name: &str) -> Result<()> {
        self.txm.clear_root_name(name)
    }

    /// Enumerate all live handles. Walks the handle-table radix tree; cost
    /// is proportional to the number of live handles, not to the historical
    /// maximum. Order is unspecified and callers must not depend on it.
    /// Takes `&self` for the same reason `read` does (F3).
    pub fn handles(&self) -> Result<Vec<u64>> {
        self.txm.handles()
    }

    /// Summary statistics derived by scanning the current handle table and
    /// querying the underlying file length. `file_size_bytes` is computed
    /// from `page_count * PAGE_SIZE` rather than `stat(2)` so it reflects
    /// the page-aligned view the engine has, not any trailing partial page
    /// that might exist mid-extend.
    pub fn stats(&self) -> Result<Stats> {
        // Both calls below route through the poison-aware wrappers on
        // TransactionManager, so a fatal I/O error in either one will
        // poison the manager just as if it had come from `read()` or
        // `commit()`. Takes `&self` (F3) — `stats` is semantically a read.
        let handles = self.txm.handles()?;
        let page_count = self.txm.file_page_count()?;
        Ok(Stats {
            handle_count: handles.len() as u64,
            total_pages: page_count,
            file_size_bytes: page_count * PAGE_SIZE as u64,
        })
    }

    /// Snapshot the four engine-activity counters (cache hits/misses,
    /// pages allocated, fsync calls). Cumulative from the most recent
    /// `open()`; the bench harness reads-subtract-reads to compute
    /// deltas for individual operations or workloads.
    ///
    /// Same `&self` semantic-read shape as `stats()`. Returns
    /// `ChiselError::Poisoned` if the engine is poisoned.
    pub fn counters(&self) -> Result<ChiselCounters> {
        self.txm.counters()
    }

    /// Returns true if this database handle has been poisoned by a
    /// previous fatal error. A poisoned handle returns
    /// `ChiselError::Poisoned` from every operation; the caller must drop
    /// it and reopen the database to recover. See the type-level docs for
    /// the full recovery protocol.
    pub fn is_poisoned(&self) -> bool {
        self.txm.is_poisoned()
    }

    /// Run a defragmentation pass. The caller must have an active
    /// transaction (see `defrag.rs` for why). This method does NOT begin or
    /// commit one on the caller's behalf — defrag is composable with other
    /// work in the same transaction and atomic with it on commit.
    pub fn defrag(&mut self, options: DefragOptions) -> Result<DefragStats> {
        defrag::defrag(&mut self.txm, &options)
    }

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
}
