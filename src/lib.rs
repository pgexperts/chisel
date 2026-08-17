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
// The README is the entry point for every downstream user, and every Rust
// snippet in it was a compile error against the current API — the `defrag`
// module went `pub(crate)`, `DefragOptions`/`Options` became
// `#[non_exhaustive]`, and tags/handles became `Tag`/`Handle` newtypes, none
// of which the examples followed. Nothing caught it because the README was
// not wired into the build at all. Including it here makes every fence a
// doctest, so `cargo test` fails the next time an API change outruns the
// docs. Fences that would touch the filesystem or need a live failure to
// demonstrate are marked `no_run`: still compiled, just not executed.
#![doc = include_str!("../README.md")]

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
pub(crate) mod crypto;
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
pub(crate) mod rekey;
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
// SlotDefect and SuperblockDefect were public before this branch (pre-existing API).
pub use superblock::{
    SlotDefect, SuperblockDefect, DEFAULT_SUPERBLOCK_COUNT, MAX_SUPERBLOCKS, MIN_SUPERBLOCKS,
    NAMED_ROOT_COUNT, NAMED_ROOT_NAME_LEN,
};
// format_major was public before this branch (I29 read-dispatch).
pub use page::format_major;
// Key and Argon2Params are public API (callers need them to open encrypted DBs).
// Crypto internals (PageCipher, CryptoError, raw constants) are pub(crate) in
// their source modules and not re-exported here.
// The MIN/MAX_ARGON2_* bounds are public because falling outside them is a
// hard failure: `Options::argon2_params` outside the range makes `open` return
// `InvalidArgon2Params` before the file is touched. A caller tuning cost
// parameters needs to see the range rather than discover it by failing.
pub use crypto::{
    Argon2Params, Key, MAX_ARGON2_M_COST, MAX_ARGON2_P_COST, MAX_ARGON2_T_COST, MIN_ARGON2_M_COST,
    MIN_ARGON2_P_COST, MIN_ARGON2_T_COST,
};
// PUBLIC-API-9 (issue #119): `Zeroizing` is ALREADY part of this crate's public
// surface — the public `Key` variants' fields have type `Zeroizing<..>`, so a
// `zeroize` major bump is already a chisel major bump whether or not this line
// exists. Re-exporting therefore adds no new semver exposure; it makes the
// existing exposure NAMEABLE, so a caller who holds a `Zeroizing` (from a
// keyring crate, say) or who matches on `Key::Raw(z)` does not need a second,
// separately-versioned `zeroize` in their own manifest just to spell the type.
// `Key::raw` / `Key::passphrase` cover the construction case without it.
pub use zeroize::Zeroizing;

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
    /// Encryption key for an encrypted database. `None` (default) opens or
    /// creates a plaintext DB. On create, `Some(key)` makes a new encrypted
    /// DB sealed under a random DEK wrapped by this key. On reopen, the key
    /// must unwrap one of the on-disk key slots or `open` returns
    /// `InvalidEncryptionKey`. Supplying a key to open a plaintext DB returns
    /// `EncryptionNotSupported`; omitting it on an encrypted DB returns
    /// `NoEncryptionKey`. A key that DOES unwrap a slot but then fails to
    /// authenticate the sealed superblock body returns `DecryptionFailed`,
    /// which is a corrupt file rather than a credential problem.
    pub encryption_key: Option<Key>,
    /// Argon2id cost parameters used to derive the KEK from a `Key::Passphrase`
    /// on *create*. `None` uses `Argon2Params::default()` (OWASP: 19 MiB / t=2 /
    /// p=1). Ignored for `Key::Raw` (HKDF, no cost params) and on reopen (the
    /// params are read from the key slot the file was written with).
    pub argon2_params: Option<Argon2Params>,
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
            encryption_key: None,
            argon2_params: None,
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
    /// Set the number of superblock slots. Valid range is 2..=16; any other
    /// value is accepted here and rejected at open time with
    /// `ChiselError::InvalidSuperblockCount`. This is only consulted on
    /// initial file creation; existing files use the count baked into their
    /// header.
    pub fn superblock_count(mut self, count: u32) -> Self {
        self.superblock_count = count;
        self
    }

    /// Set the encryption key. On create, a fresh DEK is generated and
    /// wrapped into key-slot 0 under a KEK derived from this key; the
    /// superblock is stamped MAJOR=2. On open, the key is used to unwrap
    /// the stored DEK from the matching slot. See [`Options::encryption_key`]
    /// for the full create-vs-reopen semantics.
    pub fn encryption_key(mut self, key: Key) -> Self {
        self.encryption_key = Some(key);
        self
    }
    /// Set the Argon2id cost parameters used when deriving a KEK from a
    /// passphrase on database creation. No effect for raw keys or on reopen
    /// (the stored slot carries its own params). See [`Options::argon2_params`].
    ///
    /// Values must lie within [`MIN_ARGON2_M_COST`]..=[`MAX_ARGON2_M_COST`],
    /// [`MIN_ARGON2_T_COST`]..=[`MAX_ARGON2_T_COST`] and
    /// [`MIN_ARGON2_P_COST`]..=[`MAX_ARGON2_P_COST`], and must additionally
    /// satisfy argon2's own `m_cost >= p_cost * 8` (8 KiB of memory per lane).
    /// Anything else makes `open`, `open_in_memory_with_options` and `rekey`
    /// fail with `InvalidArgon2Params` — carrying all three values — before the
    /// file is opened or created.
    ///
    /// The upper caps do double duty: they are also what stops a hostile file's
    /// key slot from driving the Argon2 allocator at open time, which is why
    /// `derive_kek` re-checks them on the read path regardless of what any
    /// caller passed here.
    pub fn argon2_params(mut self, params: Argon2Params) -> Self {
        self.argon2_params = Some(params);
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
    /// The "exists" check deliberately treats a **zero-length** file as
    /// nonexistent: a freshly-created-but-unwritten file (e.g. from a crash
    /// between `creat(2)` and the first superblock write, or from a user
    /// `touch`) has no valid superblock and must go through the
    /// `create_new` path. Without this, `open_existing` would try to parse
    /// an empty file and fail with a corruption error.
    ///
    /// A file that is non-empty but shorter than one page is *not* treated
    /// as nonexistent. It cannot be a valid database, but it is somebody's
    /// data, so `open` refuses with `CorruptSuperblock` rather than creating
    /// over it — regardless of `create_if_missing`.
    ///
    /// Acquires an exclusive `flock` on the file before any parsing, so a
    /// second concurrent `open()` on the same path fails fast with
    /// `LockFailed` rather than racing on the superblock.
    ///
    /// # Errors
    /// `InvalidSuperblockCount` (the `superblock_count` option is out of
    /// range), `InvalidArgon2Params` (the `argon2_params` option carries cost
    /// values the KDF cannot use — checked before the file is touched, so it
    /// is never confused with a credential failure),
    /// `FileNotFound` (no file at `path` and `create_if_missing` is
    /// false), `CorruptSuperblock` (the file has content but is shorter than
    /// one page, so it cannot be a database and must not be created over),
    /// or `LockFailed` (another handle holds the exclusive flock).
    /// For an encrypted database: `NoEncryptionKey` (file is encrypted but
    /// no `encryption_key` given), `InvalidEncryptionKey` (key unwraps no
    /// key slot), `EncryptionNotSupported` (key given for a plaintext
    /// file), or `DecryptionFailed` (the key unwrapped a slot but the sealed
    /// superblock body then failed authentication — the file is damaged, not
    /// the credential, so retrying other keys cannot help).
    /// When reopening an existing file, parsing the superblock can
    /// also yield `UnsupportedFormatVersion`, `CorruptSuperblock`,
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
        // PUBLIC-API-8: same treatment for argon2_params, and for the same
        // reason — a malformed Options must be named here, before the file is
        // touched, not diagnosed from whatever error the KDF happens to raise
        // several layers down.
        validate_argon2_params(options.argon2_params)?;

        let file_exists = path.exists()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);

        if !file_exists && !options.create_if_missing {
            return Err(ChiselError::FileNotFound);
        }

        let mut io = PageIo::open(path, options.read_only)?;
        // I143: decide create-vs-open from the file length observed AFTER the
        // flock is held (page_count() returns the count cached from the post-lock
        // length), NOT from the pre-lock `file_exists` stat. The pre-lock stat
        // races a concurrent creator: another process can create + commit +
        // release the lock between our stat and our lock, and the stale boolean
        // would then run create_new over its just-committed data. `file_exists`
        // stays above only for the create_if_missing gate, which must remain
        // pre-lock so a refused open never materializes an empty file.
        let existed = io.page_count() > 0;
        if !existed {
            // `existed` is `len / stride > 0`, so it is false for BOTH an empty
            // file and a file that has content but is shorter than one page.
            // Those two need opposite answers, and only a post-lock length can
            // tell them apart — the `file_exists` stat above is length-based
            // but must stay pre-lock, so it cannot serve here.
            //
            // Creating over a short non-empty file destroys its contents
            // irrecoverably, and nothing shorter than a page can be a valid
            // database, so refuse. A mistyped path that lands on a small user
            // file is the motivating case.
            let len = io.byte_len()?;
            if len > 0 {
                return Err(ChiselError::CorruptSuperblock {
                    defects: vec![SlotDefect {
                        slot: 0,
                        defect: SuperblockDefect::TooShort,
                    }],
                });
            }
            // Empty file. The pre-lock gate already refused this when
            // `create_if_missing` is false, so reaching here with it false
            // means the file was removed between that stat and our lock —
            // honour the option rather than creating anyway. (Our own
            // `.create(true)` has materialized an empty file by now; that is
            // the pre-existing cost of deciding after the lock, and it is
            // strictly better than adopting a file we were told not to create.)
            if !options.create_if_missing {
                return Err(ChiselError::FileNotFound);
            }
        }
        let cache = PageCache::new(
            io,
            options.cache_max_bytes,
            options.spillway_max_bytes,
            options.drain_insertion,
            SpillwayLocation::Path(path.to_path_buf()),
        );

        let txm = if existed {
            // Existing database: N is discovered from the on-disk
            // superblock. options.superblock_count is ignored here.
            TransactionManager::open_existing(cache, options.encryption_key.clone())?
        } else {
            TransactionManager::create_new(
                cache,
                options.superblock_count,
                options.encryption_key.clone(),
                options.argon2_params,
            )?
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
    /// `options.superblock_count` is out of range, `InvalidArgon2Params` if
    /// `options.argon2_params` carries cost values the KDF cannot use, or a
    /// bootstrap `IoError`.
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
        validate_argon2_params(options.argon2_params)?;

        let io = PageIo::open_in_memory()?;
        let cache = PageCache::new(
            io,
            options.cache_max_bytes,
            options.spillway_max_bytes,
            options.drain_insertion,
            SpillwayLocation::InMemory,
        );
        let txm = TransactionManager::create_new(
            cache,
            options.superblock_count,
            options.encryption_key.clone(),
            options.argon2_params,
        )?;
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
    /// `ReadOnlyMode` if the handle cannot write — either
    /// [`Options::read_only`] was set, or the file's format minor version is
    /// newer than this build's and `open` forced the handle read-only on an
    /// otherwise-successful open; `TransactionAlreadyActive` if a transaction
    /// is already open.
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
    /// `NoActiveTransaction` if none is open — and that is the ONLY error this
    /// method can return without poisoning the handle. It is checked before
    /// the commit protocol starts; everything after that point poisons.
    ///
    /// This inverts the crate-wide operational/fatal contract described on
    /// [`Chisel`], and it does so deliberately. Once `cache.flush()` has run,
    /// the manager is in a partial-commit state, so even an ordinarily
    /// operational error — `CacheFull` or `SpillwayFull` from a working set
    /// that exceeds the caps — leaves nothing safe to continue from.
    /// `commit` therefore poisons on *any* error it can reach.
    ///
    /// Practically: do not catch `CacheFull` from `commit` and call
    /// `rollback` to recover, the way the operational contract would suggest.
    /// The handle is already poisoned and `rollback` will return `Poisoned`.
    /// Drop the handle and reopen; the previous committed state is intact.
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
    /// u64 identifiers assigned from a monotonic counter in the superblock,
    /// and are stable across updates, defrag, and reopens. Physical location
    /// may change; the handle will not.
    ///
    /// A handle is never reused **once the transaction that minted it has
    /// committed**, including after the value is deleted. Before that, it can
    /// be: the counter lives in the roots snapshot, so `rollback`,
    /// `rollback_to` and crash recovery all rewind it, and the next
    /// `allocate` hands the same id out again for different bytes. Nothing in
    /// the entry distinguishes the two — there is no generation counter — so
    /// only commit the transaction before recording a handle anywhere outside
    /// the database (a log, an external index, another process).
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
    /// Untagged values go through plain [`allocate`](Self::allocate); a `Tag` is
    /// non-zero by construction, so this method always updates the index. The
    /// engine's "untagged" sentinel is still a stored `0` — a value no `Tag` can
    /// produce.
    ///
    /// # Errors
    /// As [`allocate`](Self::allocate) (`NoActiveTransaction`, `CacheFull`,
    /// `SpillwayFull`); the reverse membership-index insert is subject to the
    /// same cap errors.
    pub fn allocate_tagged(&mut self, value: &[u8], tag: Tag) -> Result<Handle> {
        self.txm.allocate_tagged(value, tag.get()).map(Handle::from)
    }

    /// Return the tag stored in the handle-table entry for `handle`.
    /// Returns `None` for untagged handles: "no tag" is the ABSENCE of a `Tag`,
    /// not a zero one, because `Tag(0)` is unconstructable. The engine still
    /// stores untagged as `0`; the `0 -> None` mapping happens here, at the
    /// public boundary. Takes `&self` (F3).
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
    /// no handles carry it. Untagged values are never entered in the membership
    /// index, and the `Tag` parameter makes that case unaskable — there is no
    /// tag 0 to pass. Takes `&self` (F3).
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
    /// `InvalidRootName` if `name` could never have been stored in the first
    /// place — empty, over `NAMED_ROOT_NAME_LEN` bytes, or containing a NUL.
    /// The lookup path runs the same validation `set_root_name` does, so an
    /// unrepresentable name is rejected rather than reported as unbound. A
    /// merely *unbound* (but valid) name returns `Ok(None)`. Otherwise only
    /// on poisoning.
    pub fn get_root_name(&self, name: &str) -> Result<Option<Handle>> {
        Ok(self.txm.get_root_name(name)?.map(Handle::from))
    }

    /// Remove a named root. No-op if the name is not bound. Requires an
    /// active transaction; becomes durable on commit.
    ///
    /// # Errors
    /// `NoActiveTransaction` if no transaction is open; `InvalidRootName` if
    /// `name` is empty, over `NAMED_ROOT_NAME_LEN` bytes, or contains a NUL —
    /// the clear path runs the same validation `set_root_name` does, so an
    /// unrepresentable name is rejected rather than treated as a no-op.
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
    /// from `page_count * stride` rather than `stat(2)` so it reflects
    /// the page-aligned view the engine has, not any trailing partial page
    /// that might exist mid-extend. `stride` is the on-disk unit size —
    /// `PAGE_SIZE` for a plaintext database, `ENC_PAGE_SIZE` (8232) for an
    /// encrypted one, whose per-page nonce and tag make every unit larger.
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
            file_size_bytes: page_count.saturating_mul(self.txm.file_stride() as u64),
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
    /// `page_count × stride` — where `stride` is `PAGE_SIZE` for a plaintext
    /// database and `ENC_PAGE_SIZE` (8232) for an encrypted one. Same number
    /// `stats().file_size_bytes` returns, but without the handle-table scan
    /// that `stats()` does to populate `handle_count`.
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
        Ok(page_count.saturating_mul(self.txm.file_stride() as u64))
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
    /// The freemap crash-orphan sweep (step 7) is SKIPPED while a savepoint is
    /// active, since the sweep COWs the freemap and `rollback_to` does not
    /// rewind the structural recycle streams. Run defrag outside any savepoint
    /// scope to reclaim crash-orphaned freemap pages.
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

    /// Add a second credential that unlocks this database. `existing` must
    /// already unlock it; `new` is wrapped over the same data key into a free
    /// key slot. After this returns, either credential opens the database. O(1)
    /// superblock commit — no page is re-encrypted.
    ///
    /// # Errors
    /// `EncryptionNotSupported` if the database has no encryption;
    /// `InvalidEncryptionKey` if `existing` unlocks no slot; `NoFreeKeySlot` if
    /// all 8 key slots are full; `TransactionInProgress` if a transaction is
    /// active — the key slots are rewritten through their own superblock
    /// commit, which cannot interleave with one; `ReadOnlyMode` if the handle
    /// cannot write — either [`Options::read_only`] was set, or the file's
    /// format minor version is newer than this build's and `open` forced the
    /// handle read-only on an otherwise-successful open. An fsync/superblock
    /// failure is fatal and poisons the handle.
    pub fn add_key(&mut self, existing: &crypto::Key, new: &crypto::Key) -> Result<()> {
        self.txm.add_key(existing, new)
    }

    /// Replace `old` with `new`: `new` is added and `old` is revoked in one
    /// atomic superblock commit. After this returns, `old` no longer opens the
    /// database and `new` does. O(1) — the data key is unchanged, no page is
    /// re-encrypted.
    ///
    /// # This is revocation, not cryptographic erasure
    ///
    /// The data-encryption key does not change, so `old` is being denied a way
    /// IN — it is not being cut off from data it has already seen. Two limits
    /// follow, and neither is a defect:
    ///
    /// * Anyone who captured the DEK while `old` was valid (a process memory
    ///   dump, say) keeps the ability to read every page, including pages
    ///   written afterwards. Recovering from a compromised DEK needs a full
    ///   re-encryption pass, which is not implemented — see the tracking issue
    ///   for bulk DEK rotation.
    /// * An OLD COPY of the file — a backup taken before the revocation — is
    ///   still fully readable with `old`. Revocation rewrites the live file, not
    ///   copies of it.
    ///
    /// What this DOES guarantee, as of the fix for CRYPTO-1, is that no slot of
    /// the live file still carries `old`'s wrapped DEK. Previously the
    /// pre-revocation key-slot table survived verbatim in the sibling superblock
    /// slots, so the revoked credential could recover the current DEK from the
    /// current file with nothing but read access.
    ///
    /// # Errors
    /// `EncryptionNotSupported` if the database has no encryption;
    /// `InvalidEncryptionKey` if `old` unlocks no slot; `NoFreeKeySlot` if all 8
    /// key slots are full (no room to stage `new` before revoking `old`);
    /// `TransactionInProgress` if a transaction is active — the key slots are
    /// rewritten through their own superblock commit, which cannot interleave
    /// with one; `ReadOnlyMode` if the handle cannot write — either
    /// [`Options::read_only`] was set, or the file's format minor version is
    /// newer than this build's and `open` forced the handle read-only on an
    /// otherwise-successful open. An fsync/superblock failure is fatal and
    /// poisons the handle.
    pub fn rotate_key(&mut self, old: &crypto::Key, new: &crypto::Key) -> Result<()> {
        self.txm.rotate_key(old, new)
    }

    /// Revoke the credential `key`. After this returns, `key` no longer opens
    /// the database; any other credentials are unaffected. Refuses to remove
    /// the only remaining credential. O(1) — the data key is unchanged, no
    /// page is re-encrypted.
    ///
    /// Read the "This is revocation, not cryptographic erasure" section on
    /// [`Chisel::rotate_key`] before relying on this for incident response: the
    /// DEK is unchanged, so this closes a door rather than re-keying the data.
    ///
    /// # Errors
    /// `EncryptionNotSupported` if the database has no encryption;
    /// `InvalidEncryptionKey` if `key` unlocks no slot; `LastKeySlot` if
    /// `key` is the only active credential (removing it would make the database
    /// permanently unopenable — nothing is changed); `TransactionInProgress` if
    /// a transaction is active — the key slots are rewritten through their own
    /// superblock commit, which cannot interleave with one; `ReadOnlyMode` if
    /// the handle cannot write — either [`Options::read_only`] was set, or the
    /// file's format minor version is newer than this build's and `open` forced
    /// the handle read-only on an otherwise-successful open. An fsync/superblock
    /// failure is fatal and poisons the handle.
    pub fn remove_key(&mut self, key: &crypto::Key) -> Result<()> {
        self.txm.remove_key(key)
    }

    /// Re-encrypt the entire database under a freshly generated data-encryption
    /// key. The heavy sibling of [`Chisel::rotate_key`].
    ///
    /// Reach for this only when the DEK ITSELF is believed compromised — a
    /// process memory dump, a core file, a debugger session. If what leaked was
    /// a *credential*, [`Chisel::rotate_key`] is the right tool: it is O(1),
    /// touches only the superblock, and is the common operational need
    /// (password change, key rollover, adding a second credential).
    ///
    /// # Why this is an offline operation on a path
    ///
    /// It takes a path rather than `&mut self` because it rewrites the whole
    /// file and replaces it by `rename`. After a successful rotation, any file
    /// descriptor opened before the call refers to the ORIGINAL, now-unlinked
    /// inode — reads and writes through it would silently target a deleted
    /// file. Handing back a live handle would be handing back that hazard, so
    /// there is no handle to misuse: close any open `Chisel` first, call this,
    /// then [`Chisel::open`] again.
    ///
    /// The exclusive `flock` is held for the whole rotation, so no other
    /// process can open the database while it is in flight.
    ///
    /// # Crash safety
    ///
    /// The rotated database is built in a scratch file beside the original and
    /// published with an atomic `rename`. The database path therefore only ever
    /// names a complete file — the fully-rotated one, or the untouched
    /// original. A crash at any point leaves one or the other, never a mix.
    ///
    /// In-place rotation would NOT be safe, and shadow paging does not rescue
    /// it: shadow paging protects writes that go to *new* pages, while this
    /// rewrites pages where they are. A crash halfway would leave some pages
    /// under the new DEK and some under the old, with no way to tell which.
    ///
    /// # This collapses the key-slot table to `key` alone
    ///
    /// Every other credential is revoked. That is not a shortcut — it is
    /// forced: each slot's KEK is derived from its own credential, so re-wrapping
    /// the new DEK for a slot requires the credential that slot belongs to, and
    /// only `key` was supplied. Add the others back with [`Chisel::add_key`]
    /// afterwards.
    ///
    /// It is also the safer default for the situation this exists for. If you
    /// are rotating because the DEK leaked, quietly preserving every credential
    /// that could reach it is not what you want.
    ///
    /// # Cost
    ///
    /// O(total_pages) I/O and, transiently, a second copy of the database on
    /// disk. Not something to schedule routinely.
    ///
    /// # Errors
    /// `InvalidArgon2Params` if `argon2_params` carries unusable cost values;
    /// `FileNotFound` if `path` does not exist; `EncryptionNotSupported` if the
    /// database is plaintext; `InvalidEncryptionKey` if `key` unlocks no slot;
    /// `IoError` for any failure building or publishing the replacement — in
    /// which case the original file is untouched and the scratch file removed.
    pub fn rekey(
        path: &Path,
        key: &crypto::Key,
        argon2_params: Option<crypto::Argon2Params>,
    ) -> Result<()> {
        // Checked here rather than inside rekey(): rekey builds a whole
        // replacement file before publishing it, so a parameter that the KDF
        // will reject should stop the operation before any of that work — and
        // before the caller has to distinguish "bad params" from "wrong key".
        validate_argon2_params(argon2_params)?;
        rekey::rekey(path, key, argon2_params)
    }
}

/// Reject `Options::argon2_params` values the KDF cannot use, at the API
/// boundary, before any file is opened or created.
///
/// PUBLIC-API-8 (issue #106): these values used to reach `Params::new` deep
/// inside `open`, fail as `CryptoError::Kdf`, and be collapsed by the blanket
/// `From<CryptoError> for ChiselError` into `InvalidEncryptionKey` — a variant
/// whose documented meaning is "a key was supplied but no key-slot's wrapped
/// DEK could be unwrapped". On a database being CREATED there is no key slot,
/// so that diagnosis was not merely unhelpful but impossible, and the actual
/// cause (a rejected cost parameter) appeared nowhere in the error.
///
/// `None` means "use the per-slot stored params on open, or the OWASP default
/// on create" and is always valid; only an explicitly supplied value is
/// checked.
fn validate_argon2_params(params: Option<Argon2Params>) -> Result<()> {
    match params {
        Some(p) if crypto::argon2_params_out_of_range(&p) => {
            Err(ChiselError::InvalidArgon2Params {
                m_cost: p.m_cost,
                t_cost: p.t_cost,
                p_cost: p.p_cost,
            })
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod options_encryption_tests {
    use super::*;

    // The two encryption fields default to None (a plaintext DB) and round-trip
    // through the chained-setter builder, preserving #[non_exhaustive] (callers
    // can't struct-literal, so the setters are the only construction path).
    #[test]
    fn encryption_options_default_none_and_set() {
        let o = Options::default();
        assert!(o.encryption_key.is_none());
        assert!(o.argon2_params.is_none());

        let raw = Key::Raw(zeroize::Zeroizing::new(vec![0u8; 32]));
        let o = Options::default()
            .encryption_key(raw)
            .argon2_params(Argon2Params {
                m_cost: 19456,
                t_cost: 2,
                p_cost: 1,
            });
        assert!(matches!(o.encryption_key, Some(Key::Raw(_))));
        let p = o.argon2_params.expect("set above");
        assert_eq!((p.m_cost, p.t_cost, p.p_cost), (19456, 2, 1));
    }
}
