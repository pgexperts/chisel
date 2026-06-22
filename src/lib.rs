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

// I124: enforce `# Errors` rustdoc on every public fallible method. The lint
// only fires on truly-public items, and the entire public API lives in this
// file, so internal `pub(crate)` modules are unaffected. CI runs clippy with
// `-D warnings`, which promotes this to a hard error — a new public `-> Result`
// method without an `# Errors` section fails the build.
#![warn(clippy::missing_errors_doc)]

// I35 (ISSUES.md, 2026-05-22): every storage-internals module is
// pub(crate). The supported public surface is the curated re-export
// list further down (Chisel, Options, DrainInsertion, ChiselError,
// Result, Stats, ChiselCounters, DefragOptions, DefragStats, PAGE_SIZE,
// plus the superblock layout constants). Internal types like
// TransactionManager / PageCache / HandleEntry / Superblock /
// PageType are NOT part of the API stability contract; reaching for
// them from a downstream crate requires either a path-dep with
// #[cfg(test)] access (the bench subcrate does this implicitly
// through the public API) or copying the relevant logic out.
pub(crate) mod data_page;
pub(crate) mod defrag;
pub(crate) mod error;
pub(crate) mod freemap;
pub(crate) mod freemap_tree;
pub(crate) mod handle;
pub(crate) mod handle_table;
mod lru;
pub(crate) mod membership_index;
pub(crate) mod overflow;
pub(crate) mod page;
pub(crate) mod page_cache;
pub(crate) mod page_io;
mod spillway;
pub(crate) mod stats;
pub(crate) mod superblock;
pub(crate) mod transaction;

// I35: crash-recovery integration tests need direct access to internal
// types (Superblock, PageType, page format constants) for corruption
// injection. The I35 pub→pub(crate) reshape locks these down, so the
// suite moved from tests/crash_recovery.rs into src/. cfg(test)-only so
// it adds nothing to release builds.
#[cfg(test)]
mod recovery_tests;

pub use error::{ChiselError, Result};

// Re-exports of the curated public surface. The internal modules these
// items live in are pub(crate) (ISSUES.md I35, landed in PR #11); these
// re-exports define the supported access paths and keep the documented
// API at the crate root.
pub use defrag::{DefragOptions, DefragStats};
pub use handle::{Handle, Tag, TagDropProgress};
pub use page::PAGE_SIZE;
pub use stats::{ChiselCounters, Stats};
pub use superblock::{
    SlotDefect, SuperblockDefect, DEFAULT_SUPERBLOCK_COUNT, MAX_SUPERBLOCKS, MIN_SUPERBLOCKS,
    NAMED_ROOT_COUNT, NAMED_ROOT_NAME_LEN,
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
/// `spillway_max_bytes` is a strict upper bound on the spillway's LIVE
/// resident set, in bytes (excluding per-slot 16-byte headers). When the cache
/// is full and dirty, overflow dirty pages are written to the spillway
/// rather than aborting; exceeding this limit (live spilled pages ×
/// `PAGE_SIZE`) trips `ChiselError::SpillwayFull`. The cap is charged against
/// LIVE residency, not cumulative spill volume: a page read back and respilled
/// within a transaction does not count twice, so the limit is predictable for
/// long transactions. The physical sidecar FILE may transiently grow past this
/// cap (the write cursor is monotonic within a transaction); that tail is
/// reclaimed when the spillway is truncated at commit/rollback. Default
/// `1024 * cache_max_bytes` (8 GiB at the default cache size). Setting to 0
/// disables the spillway entirely — overflow then trips
/// `ChiselError::CacheFull` at the strict cache cap, with no 8× elasticity (the
/// previous `HARD_CEILING_MULTIPLIER` is removed).
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
/// I36 (ISSUES.md, 2026-05-22): `#[non_exhaustive]` so adding a future
/// field (a tuning knob for cache warmup, an fsync-coalescing hint,
/// etc.) is not a breaking change. External callers must construct via
/// `Options { ..Options::default() }` rather than a full struct
/// literal; `Default` is implemented below.
#[non_exhaustive]
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
///
/// I36: `#[non_exhaustive]` so a third drain policy (e.g. a hint-based
/// split between recently-touched and cold) can land without breaking
/// callers. External `match` arms need a `_ => …` catchall.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainInsertion {
    LruTail,
    Mru,
}

/// How to open a spillway sidecar. `Path` for file-backed databases
/// (path is the main db path; spillway will be at `<path>.spillway`),
/// `InMemory` for memory-backed.
///
/// I37 (ISSUES.md, 2026-05-22): pub(crate) because the only legitimate
/// constructors are inside `Chisel::open` and
/// `Chisel::open_in_memory_with_options`. External callers route
/// through those — there's no API path that needs them to construct
/// a `SpillwayLocation` directly.
#[derive(Debug, Clone)]
pub(crate) enum SpillwayLocation {
    Path(std::path::PathBuf),
    InMemory,
}

impl Default for Options {
    fn default() -> Options {
        let cache_max_bytes = 8 * 1024 * 1024; // 8 MiB = 1024 × 8 KiB pages
        Options {
            cache_max_bytes,
            // saturating_mul (not `*`) so this default stays total if the
            // hardcoded cache_max_bytes above is ever edited up near u64::MAX:
            // it clamps to u64::MAX rather than wrapping to a tiny spillway cap.
            // Harmless at the current 8 MiB (product is 8 GiB, nowhere near overflow).
            spillway_max_bytes: cache_max_bytes.saturating_mul(1024),
            drain_insertion: DrainInsertion::LruTail,
            create_if_missing: true,
            read_only: false,
            superblock_count: superblock::DEFAULT_SUPERBLOCK_COUNT,
        }
    }
}

// I36: chained setters paired with the #[non_exhaustive] attribute on
// Options above. External crates can't construct via a struct literal
// — even with `..Options::default()` — so the supported way to build a
// customized Options is `Options::default().cache_max_bytes(…)…`. The
// setters take and return `Self` by value (move semantics) so a chain
// builds the final value in one expression and never holds a `&mut Options`
// borrow.
//
// Method names match the field names, not `with_*`-prefixed. Rust resolves
// the field-vs-method ambiguity by context: `o.cache_max_bytes` is field
// access; `o.cache_max_bytes(N)` is a method call. The unprefixed form
// is consistent with sqlx, redb, and most modern crates; `with_*` is the
// older convention and uses more vertical space.
impl Options {
    pub fn cache_max_bytes(mut self, bytes: u64) -> Self {
        self.cache_max_bytes = bytes;
        self
    }
    pub fn spillway_max_bytes(mut self, bytes: u64) -> Self {
        self.spillway_max_bytes = bytes;
        self
    }
    pub fn drain_insertion(mut self, policy: DrainInsertion) -> Self {
        self.drain_insertion = policy;
        self
    }
    pub fn create_if_missing(mut self, create: bool) -> Self {
        self.create_if_missing = create;
        self
    }
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
    pub fn superblock_count(mut self, count: u32) -> Self {
        self.superblock_count = count;
        self
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
///
/// # Errors and poisoning
///
/// One rule is shared by every fallible method below: once the handle is
/// poisoned (above), the method returns [`ChiselError::Poisoned`] — reads
/// included. Each method's own `# Errors` section therefore lists only the
/// *operational* (recoverable, non-poisoning) errors specific to that call;
/// it does not re-list `Poisoned` or the fatal I/O and corruption errors,
/// which are universal and all funnel into the poison model. Operational
/// errors leave the handle usable: fix the condition (or `rollback`) and
/// continue. The constructors (`open`, `open_in_memory*`) have no handle to
/// poison, so their errors are fully enumerated in place.
// I68 (ISSUES.md, 2026-05-22): `Chisel` has no explicit `Drop` impl
// because shadow paging guarantees the on-disk state is always the
// last successfully committed state — whether the value goes out of
// scope via an explicit `close()`, a panic unwind, or a forgotten
// `_db` binding at the end of `main`. A reader coming from Postgres
// or RocksDB might expect `Drop` to fsync or to discard uncommitted
// work explicitly; here, the COW protocol makes both redundant. The
// type-level doc below documents the user-facing semantics.
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
    ///
    /// # Errors
    /// `InvalidSuperblockCount` (the `superblock_count` option is out of
    /// range), `FileNotFound` (no file at `path` and `create_if_missing` is
    /// false), or `LockFailed` (another handle holds the exclusive flock). When
    /// reopening an existing file, parsing the superblock can also yield
    /// `InvalidMagic`, `UnsupportedFormatVersion`, `CorruptSuperblock`,
    /// `ChecksumMismatch`, `FileSizeMismatch`, or `IoError`.
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
    ///
    /// # Errors
    /// Only a bootstrap `IoError` from the initial superblock write — the
    /// memory backing makes this practically infallible. See
    /// [`open_in_memory_with_options`](Self::open_in_memory_with_options).
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
    ///
    /// # Errors
    /// `ReadOnlyMode` if `options.read_only` is set (a fresh memory database
    /// must be writable to bootstrap), `InvalidSuperblockCount` if
    /// `options.superblock_count` is out of range, or a bootstrap `IoError`.
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
    ///
    /// I38 (ISSUES.md, 2026-05-22): `#[must_use]` with a custom message
    /// so callers who drop the result without explicit `let _ = …` get
    /// a lint warning. `Result` is already `#[must_use]` by default;
    /// the custom message adds the human-readable rationale.
    ///
    /// # Errors
    /// Currently never — `close` always returns `Ok`. The `Result` is reserved
    /// so a future release can surface fsync/close errors without an API break.
    #[must_use = "Chisel::close may surface fsync/close errors in a future release; \
                  ignore explicitly with `let _ = db.close();` if intentional"]
    pub fn close(self) -> Result<()> {
        drop(self);
        Ok(())
    }

    /// Begin a new transaction. All mutating operations below require an
    /// active transaction; `allocate`/`update`/`delete` will return
    /// `NoActiveTransaction` otherwise. Only one transaction is active at a
    /// time — there is no nesting beyond savepoints.
    ///
    /// # Errors
    /// `TransactionAlreadyActive` if a transaction is already open.
    pub fn begin(&mut self) -> Result<()> {
        self.txm.begin()
    }

    /// Commit the active transaction. Performs three fsyncs before
    /// returning — this is the point at which changes become durable:
    ///
    /// 1. **I28 pre-drain flush.** `TransactionManager::commit_inner`
    ///    pre-drains the cache before `persist_freemap` to keep
    ///    `CacheFull` off the commit path (see ISSUES.md I28).
    /// 2. **Main data-pages flush.** `PageCache::flush` phase 2 issues
    ///    one fsync that covers every in-cache write plus every
    ///    drained-batch write.
    /// 3. **Superblock fsync.** The alternate-slot superblock is
    ///    written and fsynced; this is the linearization point.
    ///
    /// A crash before the superblock fsync leaves the previous
    /// committed state intact — recovery picks the older superblock
    /// via `Superblock::select` and the partially-written shadow
    /// pages become unreachable garbage.
    ///
    /// The spillway, when engaged, adds zero additional fsyncs to
    /// this protocol (its content does not need to survive a crash).
    /// `tests/spillway_integration.rs::no_spill_workload_preserves_two_fsync_commit`
    /// pins the count to `== 3`; the test name retains the older
    /// "two_fsync" label from the original spec.
    ///
    /// # Errors
    /// `NoActiveTransaction` if none is open. Operationally, `CacheFull` or
    /// `SpillwayFull` if the transaction's working set exceeds the cache /
    /// spillway caps. A failure inside the fsync/superblock protocol is fatal
    /// and poisons the handle — the previous committed state stays intact.
    pub fn commit(&mut self) -> Result<()> {
        self.txm.commit()
    }

    /// Abort the active transaction. Pages written during the transaction
    /// become unreachable garbage (they are never linked from a superblock),
    /// so rollback is effectively free — no undo log to replay.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open.
    pub fn rollback(&mut self) -> Result<()> {
        self.txm.rollback()
    }

    // Savepoint API: named marks within the active transaction. Implemented
    // by snapshotting the in-memory roots — cheap because the on-disk pages
    // written since the savepoint are simply abandoned on `rollback_to`, the
    // same way a full rollback abandons the whole transaction.

    /// Create a named savepoint within the active transaction.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `DuplicateSavepoint` if
    /// `name` is already a live savepoint in this transaction.
    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        self.txm.savepoint(name)
    }

    /// Roll the active transaction back to a named savepoint, discarding work
    /// done since (pages written meanwhile are abandoned, as in a full rollback).
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `SavepointNotFound` if
    /// `name` is not a live savepoint.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        self.txm.rollback_to(name)
    }

    /// Discard a named savepoint without rolling back, folding its scope into
    /// the surrounding transaction.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `SavepointNotFound` if
    /// `name` is not a live savepoint.
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
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `CacheFull` or
    /// `SpillwayFull` if the value's pages do not fit within the cache /
    /// spillway caps.
    pub fn allocate(&mut self, value: &[u8]) -> Result<Handle> {
        self.txm.allocate(value).map(Handle::from)
    }

    /// Store `value` tagged with `tag` and return a freshly minted stable handle.
    /// Like `allocate`, but additionally registers the handle in the reverse
    /// membership index (tag→handles) so `handles_with_tag(tag)` can enumerate it.
    /// Tag 0 is the "untagged" sentinel — prefer plain `allocate` for untagged
    /// values; the membership index is not updated for tag 0.
    ///
    /// # Errors
    /// As [`allocate`](Self::allocate) (`NoActiveTransaction`, `CacheFull`,
    /// `SpillwayFull`); the reverse membership-index insert is subject to the
    /// same cap errors.
    pub fn allocate_tagged(&mut self, value: &[u8], tag: Tag) -> Result<Handle> {
        self.txm.allocate_tagged(value, tag.get()).map(Handle::from)
    }

    /// Return the tag stored in the handle-table entry for `handle`.
    /// Returns 0 for untagged handles. Takes `&self` (F3).
    ///
    /// # Errors
    /// `InvalidHandle` if `handle` is unknown or deleted.
    pub fn tag(&self, handle: Handle) -> Result<Option<Tag>> {
        self.txm.tag(handle.get()).map(Tag::new) // stored 0 -> None
    }

    /// Return the opaque client byte stored in the handle-table entry for
    /// `handle`. Returns 0 for chunks whose byte was never set (including all
    /// chunks created before this feature). Chisel never interprets it. Takes
    /// `&self` (F3).
    ///
    /// # Errors
    /// `InvalidHandle` if `handle` is unknown or deleted.
    pub fn client_byte(&self, handle: Handle) -> Result<u8> {
        self.txm.client_byte(handle.get())
    }

    /// Set the opaque client byte for `handle`. Requires an active
    /// transaction; durable on commit, reverted on rollback.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidHandle` if
    /// `handle` is unknown or deleted.
    pub fn set_client_byte(&mut self, handle: Handle, byte: u8) -> Result<()> {
        self.txm.set_client_byte(handle.get(), byte)
    }

    /// Enumerate all live handles that carry `tag`. Returns an empty Vec if
    /// no handles with that tag exist. Tag 0 always returns an empty Vec
    /// (the membership index is not updated for untagged values). Takes `&self` (F3).
    ///
    /// Stability: the same within-session repeatability contract as `handles` —
    /// repeated calls return an identical `Vec` while the set of live handles
    /// carrying `tag` is unchanged and no `defrag` has run. The order is
    /// unspecified and may differ after a reopen or `defrag`.
    ///
    /// # Errors
    /// Only on poisoning — an empty or absent index is not an error and returns
    /// an empty `Vec`.
    pub fn handles_with_tag(&self, tag: Tag) -> Result<Vec<Handle>> {
        Ok(self
            .txm
            .handles_with_tag(tag.get())?
            .into_iter()
            .map(Handle::from)
            .collect())
    }

    /// Read the current value for `handle`. Takes `&self` — the page cache
    /// is mutated on miss (LRU bookkeeping, page loading) via interior
    /// mutability (see F3 in ISSUES.md). The returned `Vec<u8>` is a copy;
    /// the cache retains its own page. Not `Sync` — a `Chisel` is single-
    /// threaded by design, so this `&self` only enables `&self`-taking
    /// read APIs in downstream wrappers (e.g. the client's `StorageEngine`
    /// trait), not cross-thread sharing.
    ///
    /// # Errors
    /// `InvalidHandle` if `handle` is unknown or deleted. A structural
    /// disagreement between the handle table and the data page surfaces as the
    /// fatal `CorruptPage`, which poisons the handle.
    pub fn read(&self, handle: Handle) -> Result<Vec<u8>> {
        self.txm.read(handle.get())
    }

    /// Replace the value for `handle`. The handle is preserved; the value
    /// is written to a new slot (and, if it crosses the inline threshold,
    /// to a new overflow chain). The handle-table entry is rewritten via
    /// COW, so the update is invisible until commit.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidHandle` if
    /// `handle` is unknown or deleted; `CacheFull` or `SpillwayFull` if the new
    /// value's pages do not fit the caps.
    pub fn update(&mut self, handle: Handle, value: &[u8]) -> Result<()> {
        self.txm.update(handle.get(), value)
    }

    /// Remove a handle. The handle itself is retired (not reused); any
    /// overflow pages it owned are queued for release on commit.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidHandle` if
    /// `handle` is unknown or already deleted.
    pub fn delete(&mut self, handle: Handle) -> Result<()> {
        self.txm.delete(handle.get())
    }

    /// Remove a handle only if its tag equals `tag`. Returns
    /// `ChiselError::TagMismatch` (leaving the chunk and membership index
    /// untouched) if the stored tag differs. On success, delegates to
    /// `delete`, so the membership index is self-maintained.
    ///
    /// Use this when the caller wants to assert ownership (a stale or
    /// mis-directed handle should not silently delete the wrong chunk).
    /// `delete` remains the unchecked fast path for callers that trust
    /// their handle provenance.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidHandle` if
    /// `handle` is unknown or deleted; `TagMismatch` if the stored tag differs
    /// from `tag` (nothing is deleted in that case).
    pub fn delete_tagged(&mut self, handle: Handle, tag: Tag) -> Result<()> {
        self.txm.delete_tagged(handle.get(), tag.get())
    }

    /// Delete up to `max` chunks carrying `tag`, returning the handles dropped
    /// this pass and whether the tag is now fully drained (`complete`). Loop
    /// `begin -> delete_with_tag -> commit` until `complete` for an incremental,
    /// bounded-time relation drop. `max == 0` is a no-op (`complete == false`).
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open. A mid-pass error returns
    /// only `Err` — the
    /// [`TagDropProgress`] is NOT produced, so the set of handles already
    /// dropped this pass is not reported and is unrecoverable from the return
    /// value. Each individual delete is atomic (a non-fatal `CacheFull`/
    /// `SpillwayFull` leaves that one chunk untouched in both the handle table
    /// and the membership index — see the I-series / shadow-paging invariants),
    /// so the in-transaction state after the error is always consistent: every
    /// chunk dropped before the failure is fully tombstoned, the failed one is
    /// untouched. The caller therefore has two safe recoveries — `rollback()`
    /// (discard the whole pass) or `commit()` (keep the consistent partial
    /// drop) — and can simply re-enumerate via `handles_with_tag`/re-run the
    /// bounded loop to finish. A fatal error additionally poisons the manager
    /// (drop and reopen). To learn exactly which handles were dropped, commit
    /// in single-element passes (`max == 1`) and read each success's progress.
    pub fn delete_with_tag(&mut self, tag: Tag, max: usize) -> Result<TagDropProgress> {
        let (ids, complete) = self.txm.delete_with_tag(tag.get(), max)?;
        Ok(TagDropProgress {
            deleted: ids.into_iter().map(Handle::from).collect(),
            complete,
        })
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
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidHandle` if any
    /// handle in `handles` is unknown or already deleted (handles before the
    /// failure remain deleted in the current transaction).
    pub fn delete_many(&mut self, handles: &[Handle]) -> Result<()> {
        // Copy to a raw Vec; bulk delete is far below the fsync floor, so the
        // allocation is immaterial. (The bench adapter does the zero-copy
        // reinterpret where it matters; the engine API stays simple here.)
        let raw: Vec<u64> = handles.iter().map(|h| h.get()).collect();
        self.txm.delete_many(&raw)
    }

    /// Bind `name` to `handle` in the named-root table (ISSUES.md F2).
    /// Names are short mnemonic labels for long-lived handles — typically
    /// one or two per database (e.g. a meta B-tree root). Requires an
    /// active transaction; becomes durable on commit, reverts on
    /// rollback/rollback_to. See `TransactionManager::set_root_name` for
    /// validation rules and the fixed table-size limit.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidRootName` if
    /// `name` violates the validation rules; `RootNameTableFull` if the
    /// fixed-size table has no free slot.
    pub fn set_root_name(&mut self, name: &str, handle: Handle) -> Result<()> {
        self.txm.set_root_name(name, handle.get())
    }

    /// Look up a named root. Returns `Ok(None)` if the name is not bound.
    /// Reads see the transactional view (pending sets/clears are visible
    /// inside an active transaction). Takes `&self` (F3).
    ///
    /// # Errors
    /// Only on poisoning — an unbound `name` returns `Ok(None)`.
    pub fn get_root_name(&self, name: &str) -> Result<Option<Handle>> {
        Ok(self.txm.get_root_name(name)?.map(Handle::from))
    }

    /// Remove a named root. No-op if the name is not bound. Requires an
    /// active transaction; becomes durable on commit.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open.
    pub fn clear_root_name(&mut self, name: &str) -> Result<()> {
        self.txm.clear_root_name(name)
    }

    /// Enumerate all live handles. Walks the handle-table radix tree; cost
    /// is proportional to the number of live handles, not to the historical
    /// maximum. Takes `&self` for the same reason `read` does (F3).
    ///
    /// Stability: within a single open instance, repeated calls return an
    /// identical `Vec` — the same handles in the same order — as long as the
    /// live set is unchanged between calls (changed only by `allocate*` /
    /// `delete*`; `read` and `update` do not change it) and no `defrag` has run.
    /// The order itself is unspecified: it is not sorted, not insertion order,
    /// and may differ after a reopen or `defrag`, or across Chisel versions.
    /// Rely on within-session repeatability; do not rely on the order.
    ///
    /// # Errors
    /// Only on poisoning (e.g. a fatal `IoError` while walking the handle table).
    pub fn handles(&self) -> Result<Vec<Handle>> {
        Ok(self.txm.handles()?.into_iter().map(Handle::from).collect())
    }

    /// Summary statistics derived by scanning the current handle table and
    /// querying the underlying file length. `file_size_bytes` is computed
    /// from `page_count * PAGE_SIZE` rather than `stat(2)` so it reflects
    /// the page-aligned view the engine has, not any trailing partial page
    /// that might exist mid-extend.
    ///
    /// # Errors
    /// Only on poisoning — a fatal `IoError` while scanning the handle table or
    /// reading the file length poisons the handle.
    pub fn stats(&self) -> Result<Stats> {
        // Both calls below route through the poison-aware wrappers on
        // TransactionManager, so a fatal I/O error in either one will
        // poison the manager just as if it had come from `read()` or
        // `commit()`. Takes `&self` (F3) — `stats` is semantically a read.
        let handles = self.txm.handles()?;
        let page_count = self.txm.file_page_count()?;
        // I74 (ISSUES.md, 2026-05-22): spillway capacity peek. Returns
        // None until the spillway is first opened (lazy construction
        // on first overflow); Some((logical, max)) otherwise. The
        // tuple is split into the two Option<u64> fields below.
        let spillway_cap = self.txm.spillway_capacity()?;
        Ok(Stats {
            handle_count: handles.len() as u64,
            total_pages: page_count,
            // I47 (ISSUES.md, 2026-05-22): saturating_mul guards against
            // u64 overflow at the absurd-extreme. The product overflows
            // at page_count > u64::MAX / 8192 ≈ 2.25 × 10^15 pages (18
            // EiB), unreachable for any real database — but unannotated
            // multiplication is a smell. The saturate-to-u64::MAX
            // behaviour is the right semantic here: "as big as a u64
            // can represent" is closer to truth than "wrapped to a
            // small number".
            file_size_bytes: page_count.saturating_mul(PAGE_SIZE as u64),
            spillway_logical_bytes: spillway_cap.map(|(logical, _)| logical),
            spillway_max_bytes: spillway_cap.map(|(_, max)| max),
        })
    }

    /// Snapshot the four engine-activity counters (cache hits/misses,
    /// pages allocated, fsync calls). Cumulative from the most recent
    /// `open()`; the bench harness reads-subtract-reads to compute
    /// deltas for individual operations or workloads.
    ///
    /// Same `&self` semantic-read shape as `stats()`.
    ///
    /// # Errors
    /// Only on poisoning.
    pub fn counters(&self) -> Result<ChiselCounters> {
        self.txm.counters()
    }

    /// Page-aligned on-disk size of the database, computed as
    /// `page_count × PAGE_SIZE`. Same number `stats().file_size_bytes`
    /// returns, but without the handle-table scan that `stats()` does
    /// to populate `handle_count`.
    ///
    /// I53 (ISSUES.md, 2026-05-22): broken out for the bench harness,
    /// which calls this per measurement cell — `stats()` walks all
    /// live handles via `handles()` (O(live handles)), which adds
    /// milliseconds per call at 100K-handle scale. Reading just
    /// `file_size_bytes` shouldn't pay that cost. `stats()` keeps its
    /// current shape because the typical caller wants all three
    /// fields together; this is the dedicated single-field accessor.
    ///
    /// # Errors
    /// Only on poisoning (a fatal `IoError` reading the file length).
    pub fn file_size_bytes(&self) -> Result<u64> {
        let page_count = self.txm.file_page_count()?;
        Ok(page_count.saturating_mul(PAGE_SIZE as u64))
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
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `CacheFull` if the
    /// relocation working set exceeds the cache cap.
    pub fn defrag(&mut self, options: DefragOptions) -> Result<DefragStats> {
        defrag::defrag(&mut self.txm, &options)
    }

    /// Resize the in-memory cache cap. Shrinking evicts clean LRU-tail entries
    /// to fit; growing takes effect on the next allocation. See spec
    /// §"Runtime mutability".
    ///
    /// # Errors
    /// `TransactionInProgress` if a transaction is active.
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.txm.set_cache_max_bytes(bytes)
    }

    /// Resize the spillway cap. Setting to 0 disables the spillway
    /// (subsequent overflow trips CacheFull at the cache cap).
    /// Returns `ChiselError::TransactionInProgress` if a transaction
    /// is active. The spillway is empty between transactions, so
    /// resize is state-free.
    ///
    /// # Errors
    /// `TransactionInProgress` if a transaction is active.
    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.txm.set_spillway_max_bytes(bytes)
    }

    /// Update the drain insertion policy used at the next commit.
    /// Returns `ChiselError::TransactionInProgress` if a transaction
    /// is active.
    ///
    /// # Errors
    /// `TransactionInProgress` if a transaction is active.
    pub fn set_drain_insertion(&mut self, policy: DrainInsertion) -> Result<()> {
        self.txm.set_drain_insertion(policy)
    }
}
