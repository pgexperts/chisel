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

use std::fmt;
use std::io;

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
    ChecksumMismatch { page_id: u64 },
    CorruptSuperblock,
    FileSizeMismatch { expected: u64, actual: u64 },
    InvalidMagic,
    LockFailed,
    // Raised when a superblock's checksum is valid but its format_version
    // field does not match the binary's supported version. Distinct from
    // CorruptSuperblock (which means no readable superblock at all) so
    // users can tell "unopenable because damaged" from "unopenable because
    // written by a newer/incompatible Chisel build".
    UnsupportedFormatVersion { found: u32, expected: u32 },
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
    CorruptPage { page_id: u64 },
    // Raised when a caller asks `PageIo::read_page` for a page id that
    // is beyond the current physical file length. Pre-I16, this path
    // surfaced as a generic IoError(UnexpectedEof) which obscured the
    // cause during debugging. The typed variant makes it obvious that
    // the request is an upstream bug (stale handle-table entry,
    // arithmetic error in a cache consumer) rather than a real I/O
    // failure. See ISSUES.md I16.
    InvalidPageId { page_id: u64 },
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
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ChiselError::IoError(_)
                | ChiselError::ChecksumMismatch { .. }
                | ChiselError::CorruptSuperblock
                | ChiselError::FileSizeMismatch { .. }
                | ChiselError::InvalidMagic
                | ChiselError::LockFailed
                | ChiselError::UnsupportedFormatVersion { .. }
                | ChiselError::CorruptPage { .. }
                | ChiselError::InvalidPageId { .. }
        )
    }
}

impl fmt::Display for ChiselError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChiselError::InvalidHandle(h) => write!(f, "invalid handle: {h}"),
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
            ChiselError::IoError(e) => write!(f, "I/O error: {e}"),
            ChiselError::ChecksumMismatch { page_id } => {
                write!(f, "checksum mismatch on page {page_id}")
            }
            ChiselError::CorruptSuperblock => write!(f, "no valid superblock found"),
            ChiselError::FileSizeMismatch { expected, actual } => {
                write!(
                    f,
                    "file size mismatch: expected {expected} bytes, got {actual}"
                )
            }
            ChiselError::InvalidMagic => write!(f, "invalid magic number"),
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
        }
    }
}

impl std::error::Error for ChiselError {}

// Blanket conversion so `?` works on std::io calls in page_io.rs and friends.
// Note: every io::Error becomes a *fatal* IoError — callers that want to
// classify "file not found at open time" as operational must catch and
// remap it before the `?` conversion fires.
impl From<io::Error> for ChiselError {
    fn from(e: io::Error) -> Self {
        ChiselError::IoError(e)
    }
}

/// Crate-wide Result alias. All fallible Chisel APIs return this.
pub type Result<T> = std::result::Result<T, ChiselError>;
