// error.rs — Error types for Chisel. Part of the foundation layer (layer 1:
// pure types, no I/O) alongside page.rs and superblock.rs.
//
// The split between Operational and Fatal is a deliberate contract for callers:
// Operational variants mean the database on disk is still consistent and the
// handle remains usable — the caller made a mistake (bad handle, nested txn,
// etc.) and can recover by issuing a different request. Fatal variants mean
// integrity invariants have been violated; the caller should stop using the
// Chisel handle because further operations may read torn or corrupt state.
// Anything promoted from Operational to Fatal (or vice versa) is a breaking
// change for users doing error-class matching.

use crate::superblock::SlotDefect;
use std::fmt;
use std::io;

/// I36 (ISSUES.md, 2026-05-22): `#[non_exhaustive]` so adding a new
/// error variant (a future operational signal, a new fatal-corruption
/// flavor) is not a breaking change. External `match` arms over
/// `ChiselError` need a `_ => …` catchall; the conventional path is
/// `if e.is_fatal() { … } else { … }` which never enumerates
/// variants and is therefore unaffected. The `Display` impl below is
/// exhaustive — adding a variant still requires updating its prose.
#[non_exhaustive]
#[derive(Debug)]
pub enum ChiselError {
    // Operational — recoverable; database file is intact.
    InvalidHandle(u64),
    NoActiveTransaction,
    TransactionAlreadyActive,
    SavepointNotFound(String),
    DuplicateSavepoint(String),
    ReadOnlyMode,
    FileNotFound,
    // Caller passed a named-root name that is empty, longer than the
    // fixed 24-byte slot, contains a null byte, or is not valid UTF-8.
    // See ISSUES.md F2.
    InvalidRootName,
    // All slots in the superblock's fixed-size named-root table are in
    // use and the caller tried to set a new (previously unused) name.
    // See ISSUES.md F2.
    RootNameTableFull,
    // Options.superblock_count is out of the supported range (ISSUES.md
    // R4). Only raised at open time when the caller passed a value
    // < MIN_SUPERBLOCKS (2) or > MAX_SUPERBLOCKS (16). Operational —
    // the caller fixes their Options and tries again.
    InvalidSuperblockCount {
        value: u32,
    },
    // The page cache has reached its strict cap (`max_pages`) with
    // every cached entry dirty and the spillway disabled
    // (`spillway_max_bytes == 0`), so there is no clean page available
    // for eviction and no spillway to absorb the overflow. Operational:
    // the DB on disk is still fine. Recovery is to commit (which flushes
    // dirty pages and clears the backlog) or roll back (which discards
    // the in-flight work entirely). The pre-spillway 8x
    // HARD_CEILING_MULTIPLIER design is gone — see spec
    // 2026-05-03-chisel-spillway-design.md.
    CacheFull {
        limit: usize,
    },
    // The spillway file has reached its `spillway_max_bytes` cap with
    // every cached entry dirty, so there is neither room in the cache
    // nor room in the spillway. Operational: the DB on disk is still
    // intact. Recovery is to commit (which drains the spillway and
    // resets it) or roll back. Spec 2026-05-03-chisel-spillway-design.md.
    SpillwayFull {
        limit_bytes: u64,
    },
    // Raised when a configuration mutator (e.g. set_cache_max_bytes,
    // set_spillway_max_bytes, set_drain_insertion) is called while a
    // transaction is in flight. Operational: caller commits or rolls
    // back, then retries. The mutators only operate on between-
    // transactions state; mid-transaction shrink would either reject
    // or silently spill, neither of which is a clean story.
    TransactionInProgress,
    // delete_tagged was given a tag that does not match the chunk's actual tag.
    // Operational: the caller passed the wrong tag; the chunk and the membership
    // index are untouched, so the transaction may continue.
    TagMismatch {
        handle: u64,
        expected: u32,
        actual: u32,
    },

    // Fatal — database integrity is in question. Close and re-open
    // before attempting further work. The reopen will re-run superblock
    // selection, which CAN recover from `CorruptSuperblock` on the
    // currently-active slot (the previous slot is still valid) but
    // cannot recover from `ChecksumMismatch` on a data/handle-table
    // page — those indicate the last-committed snapshot itself is
    // damaged. Under the I1 poison model, any fatal error poisons the
    // TransactionManager, so `close-and-reopen` is the only legitimate
    // response regardless of which fatal variant fired.
    IoError(io::Error),
    ChecksumMismatch {
        page_id: u64,
    },
    // Raised when `Superblock::select` finds no usable slot. "Usable"
    // means `deserialize` returned Some, which in turn requires a valid
    // XXH3 checksum, correct magic, AND a `superblock_count` inside
    // `MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS`. A slot that passes checksum
    // but has an out-of-range `superblock_count` counts as corrupt for
    // this purpose. `defects` contains one `SlotDefect` per candidate slot
    // that failed validation, so operators can pinpoint the exact cause
    // without reaching for the raw slot bytes (I106).
    CorruptSuperblock {
        defects: Vec<SlotDefect>,
    },
    FileSizeMismatch {
        expected: u64,
        actual: u64,
    },
    LockFailed,
    // Raised when a superblock's checksum is valid but its format_version
    // field does not match the binary's supported version. Distinct from
    // CorruptSuperblock (which means no readable superblock at all) so
    // users can tell "unopenable because damaged" from "unopenable because
    // written by a newer/incompatible Chisel build".
    UnsupportedFormatVersion {
        found: u32,
        expected: u32,
    },
    // Raised by every operation on a TransactionManager that has previously
    // seen a fatal error (commit I/O failure, checksum mismatch on read,
    // etc.). See ISSUES.md I1: modeled after std::sync::Mutex poisoning.
    // The only legal recovery is to drop the Chisel handle and reopen; the
    // shadow-paging crash-recovery path then returns the database to the
    // last durable state. Named Poisoned (not "Closed"/"Dead") to match
    // Rust conventions and make the recovery idiom obvious.
    Poisoned,
    // Raised when a page's structural contents violate the format's
    // invariants (e.g., an overflow chain with a cycle, a next_page
    // pointer that loops, a chain longer than its advertised
    // total_length). Distinct from `ChecksumMismatch` because the
    // checksum may be valid — the bytes are "structurally wrong"
    // rather than "bit-flipped". See ISSUES.md I14.
    CorruptPage {
        page_id: u64,
    },
    // Raised when a caller asks `PageIo::read_page` for a page id that
    // is beyond the current physical file length. Pre-I16, this path
    // surfaced as a generic IoError(UnexpectedEof) which obscured the
    // cause during debugging. The typed variant makes it obvious that
    // the request is an upstream bug (stale handle-table entry,
    // arithmetic error in a cache consumer) rather than a real I/O
    // failure. See ISSUES.md I16.
    InvalidPageId {
        page_id: u64,
    },
    // Raised at open time when the superblock's `page_size` field does not
    // match the `PAGE_SIZE` compiled into this binary. A mismatch means
    // the file was written with a different page geometry and every page
    // boundary calculation would be wrong; the open is refused before any
    // data is read. Distinct from `UnsupportedFormatVersion` (which guards
    // the format-version field) so operators can distinguish a compile-
    // configuration mismatch from an actual version incompatibility.
    UnsupportedPageSize {
        stored: u32,
        compiled: u32,
    },

    // Operational — the caller supplied the wrong key material or none, or
    // asked an unencrypted-only build to open an encrypted DB. The on-disk
    // file is untouched; the caller fixes their `Options` and retries.
    //
    // Raised at open time when the superblock declares encryption but
    // `Options::encryption_key` was None.
    NoEncryptionKey,
    // Raised at open time when a key was supplied but no key-slot's wrapped
    // DEK could be unwrapped under the derived KEK (wrong passphrase / raw
    // key). Operational: the DB is intact; supply the right key and reopen.
    InvalidEncryptionKey,
    // Raised when a key was supplied to open a *plaintext* DB, or an
    // encrypted DB is opened by a build that the on-disk crypto-header
    // algorithm id is unknown to. Operational: the request is a mismatch,
    // not corruption.
    EncryptionNotSupported,
    // Operational — key-management (add/rotate/remove) ran out of room: all
    // KEY_SLOT_COUNT (8) wrapped-DEK slots are occupied, so there is nowhere
    // to stage a new credential. The DB is intact; remove an unused key first.
    NoFreeKeySlot,
    // Operational — refusing to remove the last active key slot, which would
    // leave the database with zero usable credentials (permanently unopenable).
    LastKeySlot,
    // Fatal — an AEAD authentication failure while decrypting a page that
    // was already located and read off disk. The ciphertext, tag, nonce, or
    // session DEK disagree, so the last-committed snapshot cannot be trusted;
    // poisons the manager (I1) exactly like ChecksumMismatch. Distinct from
    // InvalidEncryptionKey (a *key-slot* unwrap failure at open, before any
    // page is served) — this fires mid-session on a real data/handle page.
    DecryptionFailed {
        page_id: u64,
    },
}

impl ChiselError {
    /// True for error variants that indicate a violation of storage
    /// integrity or an unrecoverable I/O condition. Operational errors
    /// (caller mistakes like `InvalidHandle`) return false. Used by
    /// `TransactionManager` to decide whether an error should poison the
    /// manager — see ISSUES.md I1.
    ///
    /// `Poisoned` itself is NOT fatal by this definition: it just means
    /// the manager is already dead, so seeing it again should not trigger
    /// a redundant state change.
    ///
    /// INVARIANT: this match must list exactly the variants in the "Fatal"
    /// block of the enum above. Adding a fatal variant without adding it here
    /// silently leaves it classified operational, so the manager would keep
    /// serving requests over questionable on-disk state — a durability hole,
    /// not a compile error (the `#[non_exhaustive]` enum has no exhaustiveness
    /// check to catch the omission).
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ChiselError::IoError(_)
                | ChiselError::ChecksumMismatch { .. }
                | ChiselError::CorruptSuperblock { .. }
                | ChiselError::FileSizeMismatch { .. }
                | ChiselError::LockFailed
                | ChiselError::UnsupportedFormatVersion { .. }
                | ChiselError::CorruptPage { .. }
                | ChiselError::InvalidPageId { .. }
                | ChiselError::UnsupportedPageSize { .. }
                | ChiselError::DecryptionFailed { .. }
        )
    }
}

impl fmt::Display for ChiselError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChiselError::InvalidHandle(h) => write!(f, "invalid handle: {h}"),
            ChiselError::TagMismatch { handle, expected, actual } => {
                write!(f, "handle {handle} has tag {actual}, not the expected {expected}")
            }
            ChiselError::NoActiveTransaction => write!(f, "no active transaction"),
            ChiselError::TransactionAlreadyActive => write!(f, "transaction already active"),
            ChiselError::SavepointNotFound(name) => write!(f, "savepoint not found: {name}"),
            ChiselError::DuplicateSavepoint(name) => write!(f, "duplicate savepoint: {name}"),
            ChiselError::ReadOnlyMode => write!(f, "database is read-only"),
            ChiselError::FileNotFound => write!(f, "database file not found"),
            ChiselError::InvalidRootName => write!(
                f,
                "invalid named-root name (empty, too long, non-UTF-8, or contains NUL)"
            ),
            ChiselError::RootNameTableFull => {
                write!(f, "named-root table is full (all slots are in use)")
            }
            ChiselError::InvalidSuperblockCount { value } => {
                write!(f, "invalid superblock_count {value}; must be in 2..=16")
            }
            ChiselError::CacheFull { limit } => write!(
                f,
                "page cache full: {limit} dirty pages held; commit or roll back to free cache"
            ),
            ChiselError::SpillwayFull { limit_bytes } => {
                // limit_bytes == 0 is the "spillway disabled" sentinel, not a
                // zero-capacity overflow: the cache filled and there is no
                // spillway configured to absorb it. Reworded so operators do
                // not chase a phantom spill area that was never enabled.
                if *limit_bytes == 0 {
                    write!(
                        f,
                        "spillway is disabled (spillway_max_bytes=0); commit or roll back to free cache"
                    )
                } else {
                    write!(
                        f,
                        "spillway full: {limit_bytes}-byte limit reached; commit or roll back to free cache and spillway"
                    )
                }
            },
            ChiselError::TransactionInProgress => write!(
                f,
                "configuration changes are only allowed between transactions; commit or roll back first"
            ),
            ChiselError::IoError(e) => write!(f, "I/O error: {e}"),
            ChiselError::ChecksumMismatch { page_id } => {
                write!(f, "checksum mismatch on page {page_id}")
            }
            ChiselError::CorruptSuperblock { defects } => {
                write!(f, "no valid superblock found")?;
                for (i, sd) in defects.iter().enumerate() {
                    let sep = if i == 0 { ":" } else { ";" };
                    write!(f, "{sep} slot {}: {}", sd.slot, sd.defect)?;
                }
                Ok(())
            }
            ChiselError::FileSizeMismatch { expected, actual } => {
                write!(
                    f,
                    "file size mismatch: expected {expected} bytes, got {actual}"
                )
            }
            ChiselError::LockFailed => write!(f, "failed to acquire exclusive file lock"),
            ChiselError::UnsupportedFormatVersion { found, expected } => write!(
                f,
                "unsupported on-disk format version: found {found}, this build supports {expected}"
            ),
            ChiselError::Poisoned => write!(
                f,
                "database handle is poisoned after a previous fatal error; drop and reopen"
            ),
            ChiselError::CorruptPage { page_id } => {
                write!(f, "corrupt page structure at page {page_id}")
            }
            ChiselError::InvalidPageId { page_id } => {
                write!(f, "invalid page id {page_id} (out of range for file)")
            }
            ChiselError::UnsupportedPageSize { stored, compiled } => write!(
                f,
                "page size mismatch: file was written with {stored}-byte pages, this build uses {compiled}-byte pages"
            ),
            ChiselError::NoEncryptionKey => write!(
                f,
                "database is encrypted but no encryption_key was supplied"
            ),
            ChiselError::InvalidEncryptionKey => write!(
                f,
                "encryption key does not match any key slot (wrong passphrase or raw key)"
            ),
            ChiselError::EncryptionNotSupported => write!(
                f,
                "encryption not supported for this open (key supplied for a plaintext database, or unknown crypto algorithm)"
            ),
            ChiselError::NoFreeKeySlot => write!(
                f,
                "no free key slot: all 8 key slots are occupied (remove an unused key first)"
            ),
            ChiselError::LastKeySlot => write!(
                f,
                "refusing to remove the last active key slot (the database would become permanently unopenable)"
            ),
            ChiselError::DecryptionFailed { page_id } => {
                write!(f, "decryption/authentication failed for page {page_id}")
            }
        }
    }
}

impl std::error::Error for ChiselError {
    /// I41 (ISSUES.md, 2026-05-22): expose the wrapped `io::Error` from the
    /// `IoError` variant so error-chain walkers (`anyhow::Error::root_cause`,
    /// structured-logging adapters, `eyre` reports, `std::error::Error::chain`)
    /// can see the underlying cause. Returns `None` for every other variant
    /// because no other variant carries an inner error today. If a future
    /// variant grows an inner cause, extend this match.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ChiselError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

// Conversion so `?` works on std::io calls in page_io.rs and friends.
//
// I105 (ISSUES.md, 2026-06-21): inspect ErrorKind instead of blanket-classifying
// every io::Error as a *fatal* IoError. `NotFound` is demoted to the operational
// `FileNotFound`: it can only arise from resolving a missing PATH (open() of a
// non-existent file), never from a read/write on the already-open, flock'd fd, so
// it signals a caller mistake (bad path), not on-disk corruption — the DB is
// untouched and the caller can retry. (Chisel::open already pre-checks the
// missing-file case in lib.rs and returns FileNotFound directly; this just makes
// any stray NotFound that reaches a `?` classify consistently rather than
// poisoning.) Every other kind stays fatal: under the poison model over-poisoning
// is the safe direction (reopen recovers), and demoting a kind that might signal
// real corruption would be the same fail-open hazard as I104's is_fatal default.
//
// One other site CAN surface a NotFound after the DB is open: the spillway is a
// second file opened lazily mid-transaction (Spillway::open_file). A NotFound
// there is a degraded in-flight cache state, NOT a recoverable bad-path mistake,
// so that call site maps its open error to the fatal IoError directly and never
// routes through this demotion (review 2026-06-22). The invariant this blanket
// NotFound→operational rule relies on — "NotFound only means the initial open
// path" — therefore still holds for every `?` that reaches here.
impl From<io::Error> for ChiselError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => ChiselError::FileNotFound,
            _ => ChiselError::IoError(e),
        }
    }
}

impl From<crate::crypto::CryptoError> for ChiselError {
    // A CryptoError reaching the engine through `?` in the create/open/key-management
    // paths is always a key-or-KDF problem on intact on-disk data, so it maps to the
    // operational InvalidEncryptionKey. The page-read path (Phase 3) does NOT use this
    // blanket conversion — it maps decrypt failures explicitly to the fatal
    // ChiselError::DecryptionFailed { page_id } via `.map_err(...)`. The operational
    // cases that are NOT CryptoError-derived (no key supplied for an encrypted DB; a
    // key supplied for a plaintext DB) are returned explicitly as NoEncryptionKey /
    // EncryptionNotSupported at their decision sites.
    fn from(_: crate::crypto::CryptoError) -> Self {
        ChiselError::InvalidEncryptionKey
    }
}

/// Crate-wide Result alias. All fallible Chisel APIs return this.
pub type Result<T> = std::result::Result<T, ChiselError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    // I41 regression: ChiselError::IoError(inner) must expose `inner` via
    // source() so error-chain walkers can recover the io::Error and inspect
    // its kind/raw_os_error. Pre-I41 the trait impl was empty (`{}`) so
    // source() returned None and the inner cause was unreachable from
    // callers that didn't pattern-match on the variant.
    #[test]
    fn ioerror_source_returns_inner_io_error() {
        let inner = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let e: ChiselError = inner.into();
        let src = e
            .source()
            .expect("IoError must expose its inner error via source()");
        let downcast: &io::Error = src
            .downcast_ref()
            .expect("source() should downcast back to io::Error");
        assert_eq!(downcast.kind(), io::ErrorKind::PermissionDenied);
    }

    // I41 corollary: every variant that does NOT wrap an inner error must
    // return None from source(). Sampling a few representative ones is
    // enough; a future variant that grows an inner cause will need the
    // source() match arm extended.
    #[test]
    fn non_io_variants_have_no_source() {
        let cases: [ChiselError; 5] = [
            ChiselError::InvalidHandle(0),
            ChiselError::NoActiveTransaction,
            ChiselError::ChecksumMismatch { page_id: 0 },
            ChiselError::CorruptSuperblock { defects: vec![] },
            ChiselError::Poisoned,
        ];
        for e in &cases {
            assert!(
                e.source().is_none(),
                "expected source() == None for {e:?}, got Some(...)"
            );
        }
    }

    // I70 (ISSUES.md, 2026-05-22): unit test for the typed
    // CorruptPage variant. Group F's I45 + I49 convert two
    // previously-panicking sites to return CorruptPage errors; the
    // fault-injection paths needed to trigger those sites from a
    // black-box test would be invasive (would need test-only
    // force_lru_desync / force_handle_table_desync helpers). This
    // test covers the variant's value-level contract instead:
    // page_id round-trips through construction, the variant is
    // classified as fatal (so it poisons the manager per I1), and
    // Display formats the page id. Integration coverage of the
    // CorruptPage path against on-disk corruption already exists in
    // src/recovery_tests.rs (the I14 test).
    #[test]
    fn corrupt_page_variant_contract() {
        let e = ChiselError::CorruptPage { page_id: 42 };
        // is_fatal() returns true → TransactionManager will poison.
        assert!(
            e.is_fatal(),
            "CorruptPage must be fatal so the manager poisons"
        );
        // source() returns None — CorruptPage does not wrap an inner cause.
        assert!(e.source().is_none(), "CorruptPage has no inner source");
        // Display includes the page id so log inspectors can identify
        // the bad page without round-tripping through Debug.
        let msg = format!("{e}");
        assert!(
            msg.contains("42"),
            "Display message {msg:?} should mention page id 42"
        );
    }

    // I106: the CorruptSuperblock Display is the operator-facing diagnostic, so
    // pin its exact format — the empty/single/multiple separator logic (":" on
    // the first slot, ";" between, no trailing punctuation) is easy to break.
    #[test]
    fn corrupt_superblock_display_format() {
        use crate::superblock::{SlotDefect, SuperblockDefect};
        // No defects (the test-constructor case): bare message, no punctuation.
        let empty = ChiselError::CorruptSuperblock { defects: vec![] };
        assert_eq!(format!("{empty}"), "no valid superblock found");
        // One defect: colon-introduced slot detail.
        let one = ChiselError::CorruptSuperblock {
            defects: vec![SlotDefect {
                slot: 0,
                defect: SuperblockDefect::BadChecksum,
            }],
        };
        assert_eq!(
            format!("{one}"),
            "no valid superblock found: slot 0: bad checksum"
        );
        // Multiple: colon first, semicolons between, no trailing separator; also
        // exercises BadCount's value rendering.
        let many = ChiselError::CorruptSuperblock {
            defects: vec![
                SlotDefect {
                    slot: 0,
                    defect: SuperblockDefect::BadChecksum,
                },
                SlotDefect {
                    slot: 1,
                    defect: SuperblockDefect::BadMagic,
                },
                SlotDefect {
                    slot: 2,
                    defect: SuperblockDefect::BadCount(99),
                },
            ],
        };
        assert_eq!(
            format!("{many}"),
            "no valid superblock found: slot 0: bad checksum; slot 1: bad magic; slot 2: bad superblock_count 99"
        );
    }

    // I104 (ISSUES.md, 2026-06-21): exhaustiveness guard for is_fatal(). The
    // `documented_is_fatal` helper classifies every variant via a match with NO
    // `_` arm; because this is the DEFINING crate, #[non_exhaustive] does not
    // suppress the exhaustiveness check here, so adding a ChiselError variant
    // fails to COMPILE until it is placed in the Fatal or Operational block —
    // the compile-time signal is_fatal()'s `matches!` cannot provide. The test
    // then asserts is_fatal() agrees with the documented classification for a
    // constructed instance of every variant, catching a misclassification in
    // EITHER direction (a fatal variant left out of is_fatal(), or an operational
    // one wrongly listed). The two encodings are independent on purpose:
    // documented_is_fatal does NOT call is_fatal().
    #[test]
    fn is_fatal_matches_documented_classification_for_every_variant() {
        fn documented_is_fatal(e: &ChiselError) -> bool {
            match e {
                // Operational — database file is intact; caller can recover.
                ChiselError::InvalidHandle(_)
                | ChiselError::NoActiveTransaction
                | ChiselError::TransactionAlreadyActive
                | ChiselError::SavepointNotFound(_)
                | ChiselError::DuplicateSavepoint(_)
                | ChiselError::ReadOnlyMode
                | ChiselError::FileNotFound
                | ChiselError::InvalidRootName
                | ChiselError::RootNameTableFull
                | ChiselError::InvalidSuperblockCount { .. }
                | ChiselError::CacheFull { .. }
                | ChiselError::SpillwayFull { .. }
                | ChiselError::TransactionInProgress
                | ChiselError::TagMismatch { .. }
                // Poisoned is operational by is_fatal()'s definition: the manager
                // is already dead, so re-seeing it must not re-poison.
                | ChiselError::Poisoned
                // Encryption credential errors: the DB is intact; supply the
                // right key and retry, or manage key slots before retrying.
                | ChiselError::NoEncryptionKey
                | ChiselError::InvalidEncryptionKey
                | ChiselError::EncryptionNotSupported
                | ChiselError::NoFreeKeySlot
                | ChiselError::LastKeySlot => false,
                // Fatal — integrity in question; close and reopen.
                ChiselError::IoError(_)
                | ChiselError::ChecksumMismatch { .. }
                | ChiselError::CorruptSuperblock { .. }
                | ChiselError::FileSizeMismatch { .. }
                | ChiselError::LockFailed
                | ChiselError::UnsupportedFormatVersion { .. }
                | ChiselError::CorruptPage { .. }
                | ChiselError::InvalidPageId { .. }
                | ChiselError::UnsupportedPageSize { .. }
                | ChiselError::DecryptionFailed { .. } => true,
            }
        }

        // One instance of every variant. If a variant is added to the enum,
        // documented_is_fatal stops compiling first; this list then needs the
        // new variant too so is_fatal() is actually exercised against it.
        let all = [
            ChiselError::InvalidHandle(0),
            ChiselError::NoActiveTransaction,
            ChiselError::TransactionAlreadyActive,
            ChiselError::SavepointNotFound("s".to_string()),
            ChiselError::DuplicateSavepoint("s".to_string()),
            ChiselError::ReadOnlyMode,
            ChiselError::FileNotFound,
            ChiselError::InvalidRootName,
            ChiselError::RootNameTableFull,
            ChiselError::InvalidSuperblockCount { value: 0 },
            ChiselError::CacheFull { limit: 0 },
            ChiselError::SpillwayFull { limit_bytes: 0 },
            ChiselError::TransactionInProgress,
            ChiselError::TagMismatch {
                handle: 0,
                expected: 0,
                actual: 0,
            },
            ChiselError::Poisoned,
            ChiselError::IoError(io::Error::other("io")),
            ChiselError::ChecksumMismatch { page_id: 0 },
            ChiselError::CorruptSuperblock { defects: vec![] },
            ChiselError::FileSizeMismatch {
                expected: 0,
                actual: 0,
            },
            ChiselError::LockFailed,
            ChiselError::UnsupportedFormatVersion {
                found: 0,
                expected: 0,
            },
            ChiselError::CorruptPage { page_id: 0 },
            ChiselError::InvalidPageId { page_id: 0 },
            ChiselError::UnsupportedPageSize {
                stored: 0,
                compiled: 0,
            },
            ChiselError::NoEncryptionKey,
            ChiselError::InvalidEncryptionKey,
            ChiselError::EncryptionNotSupported,
            ChiselError::NoFreeKeySlot,
            ChiselError::LastKeySlot,
            ChiselError::DecryptionFailed { page_id: 0 },
        ];
        for e in &all {
            assert_eq!(
                e.is_fatal(),
                documented_is_fatal(e),
                "is_fatal() disagrees with the documented Fatal/Operational block for {e:?}"
            );
        }
        // Tripwire: exactly 10 variants are fatal today. If this count moves, the
        // Fatal/Operational split changed — confirm that was intentional (it is a
        // breaking change for callers doing error-class matching, per the header).
        assert_eq!(all.iter().filter(|e| e.is_fatal()).count(), 10);
    }

    // Phase 4: the three operational encryption errors are recoverable (the
    // on-disk DB is intact — the caller supplied the wrong/no key, or asked an
    // old binary to read a v2 file), so is_fatal() is false. DecryptionFailed
    // is fatal: an AEAD tag failure on a page read means the ciphertext or DEK
    // is wrong and the snapshot can't be trusted, so it must poison (I1).
    #[test]
    fn encryption_error_classification() {
        assert!(!ChiselError::NoEncryptionKey.is_fatal());
        assert!(!ChiselError::InvalidEncryptionKey.is_fatal());
        assert!(!ChiselError::EncryptionNotSupported.is_fatal());
        assert!(!ChiselError::NoFreeKeySlot.is_fatal());
        assert!(!ChiselError::LastKeySlot.is_fatal());
        assert!(ChiselError::DecryptionFailed { page_id: 7 }.is_fatal());

        // Display carries the page id for the fatal variant.
        let msg = format!("{}", ChiselError::DecryptionFailed { page_id: 7 });
        assert!(
            msg.contains('7'),
            "Display {msg:?} should mention page id 7"
        );

        // source() is None for all four — none wrap an inner cause.
        use std::error::Error;
        for e in [
            ChiselError::NoEncryptionKey,
            ChiselError::InvalidEncryptionKey,
            ChiselError::EncryptionNotSupported,
            ChiselError::NoFreeKeySlot,
            ChiselError::LastKeySlot,
            ChiselError::DecryptionFailed { page_id: 0 },
        ] {
            assert!(e.source().is_none());
        }
    }
}
