// error.rs — Error types for Chisel.
// Operational errors are caller mistakes (database is fine).
// Fatal errors indicate possible corruption (must close and reopen).

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum ChiselError {
    // Operational
    InvalidHandle(u64),
    NoActiveTransaction,
    TransactionAlreadyActive,
    SavepointNotFound(String),
    DuplicateSavepoint(String),
    ReadOnlyMode,
    FileNotFound,

    // Fatal
    IoError(io::Error),
    ChecksumMismatch { page_id: u64 },
    CorruptSuperblock,
    FileSizeMismatch { expected: u64, actual: u64 },
    InvalidMagic,
    LockFailed,
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
        }
    }
}

impl std::error::Error for ChiselError {}

impl From<io::Error> for ChiselError {
    fn from(e: io::Error) -> Self {
        ChiselError::IoError(e)
    }
}

pub type Result<T> = std::result::Result<T, ChiselError>;
