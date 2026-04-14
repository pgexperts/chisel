// errors.rs — defines the Python exception hierarchy and converts a
// chisel::ChiselError into the appropriate PyErr. The two-tier split
// (OperationalError / FatalError) mirrors ChiselError::is_fatal() in
// src/error.rs exactly, so `except chisel.FatalError` in Python
// captures the same set of "drop-and-reopen" conditions that would
// poison the Rust TransactionManager.

use chisel::ChiselError as RustChiselError;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

create_exception!(_chisel, ChiselError, PyException);
create_exception!(_chisel, OperationalError, ChiselError);
create_exception!(_chisel, FatalError, ChiselError);

// Operational
create_exception!(_chisel, InvalidHandleError, OperationalError);
create_exception!(_chisel, NoActiveTransactionError, OperationalError);
create_exception!(_chisel, TransactionAlreadyActiveError, OperationalError);
create_exception!(_chisel, SavepointNotFoundError, OperationalError);
create_exception!(_chisel, DuplicateSavepointError, OperationalError);
create_exception!(_chisel, ReadOnlyModeError, OperationalError);
create_exception!(_chisel, DatabaseFileNotFoundError, OperationalError);
create_exception!(_chisel, InvalidRootNameError, OperationalError);
create_exception!(_chisel, RootNameTableFullError, OperationalError);
create_exception!(_chisel, InvalidSuperblockCountError, OperationalError);

// Fatal — matches ChiselError::is_fatal() in src/error.rs exactly.
create_exception!(_chisel, IoError, FatalError);
create_exception!(_chisel, ChecksumMismatchError, FatalError);
create_exception!(_chisel, CorruptSuperblockError, FatalError);
create_exception!(_chisel, FileSizeMismatchError, FatalError);
create_exception!(_chisel, InvalidMagicError, FatalError);
create_exception!(_chisel, LockFailedError, FatalError);
create_exception!(_chisel, UnsupportedFormatVersionError, FatalError);
create_exception!(_chisel, CorruptPageError, FatalError);
create_exception!(_chisel, InvalidPageIdError, FatalError);
create_exception!(_chisel, PoisonedError, FatalError);

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("ChiselError", py.get_type_bound::<ChiselError>())?;
    m.add("OperationalError", py.get_type_bound::<OperationalError>())?;
    m.add("FatalError", py.get_type_bound::<FatalError>())?;

    m.add(
        "InvalidHandleError",
        py.get_type_bound::<InvalidHandleError>(),
    )?;
    m.add(
        "NoActiveTransactionError",
        py.get_type_bound::<NoActiveTransactionError>(),
    )?;
    m.add(
        "TransactionAlreadyActiveError",
        py.get_type_bound::<TransactionAlreadyActiveError>(),
    )?;
    m.add(
        "SavepointNotFoundError",
        py.get_type_bound::<SavepointNotFoundError>(),
    )?;
    m.add(
        "DuplicateSavepointError",
        py.get_type_bound::<DuplicateSavepointError>(),
    )?;
    m.add(
        "ReadOnlyModeError",
        py.get_type_bound::<ReadOnlyModeError>(),
    )?;
    m.add(
        "DatabaseFileNotFoundError",
        py.get_type_bound::<DatabaseFileNotFoundError>(),
    )?;
    m.add(
        "InvalidRootNameError",
        py.get_type_bound::<InvalidRootNameError>(),
    )?;
    m.add(
        "RootNameTableFullError",
        py.get_type_bound::<RootNameTableFullError>(),
    )?;
    m.add(
        "InvalidSuperblockCountError",
        py.get_type_bound::<InvalidSuperblockCountError>(),
    )?;

    m.add("IoError", py.get_type_bound::<IoError>())?;
    m.add(
        "ChecksumMismatchError",
        py.get_type_bound::<ChecksumMismatchError>(),
    )?;
    m.add(
        "CorruptSuperblockError",
        py.get_type_bound::<CorruptSuperblockError>(),
    )?;
    m.add(
        "FileSizeMismatchError",
        py.get_type_bound::<FileSizeMismatchError>(),
    )?;
    m.add(
        "InvalidMagicError",
        py.get_type_bound::<InvalidMagicError>(),
    )?;
    m.add("LockFailedError", py.get_type_bound::<LockFailedError>())?;
    m.add(
        "UnsupportedFormatVersionError",
        py.get_type_bound::<UnsupportedFormatVersionError>(),
    )?;
    m.add("CorruptPageError", py.get_type_bound::<CorruptPageError>())?;
    m.add(
        "InvalidPageIdError",
        py.get_type_bound::<InvalidPageIdError>(),
    )?;
    m.add("PoisonedError", py.get_type_bound::<PoisonedError>())?;

    Ok(())
}

// Convert a chisel::ChiselError into the right PyErr. The match is exhaustive
// — adding a new variant to ChiselError produces a compile error here, which
// is intended: the binding must be updated rather than silently routing to a
// generic error.
#[allow(dead_code)]
pub fn to_py_err(err: RustChiselError) -> PyErr {
    // The Display impl on ChiselError already yields human-readable text.
    let msg = err.to_string();
    match err {
        // Operational
        RustChiselError::InvalidHandle(_) => InvalidHandleError::new_err(msg),
        RustChiselError::NoActiveTransaction => NoActiveTransactionError::new_err(msg),
        RustChiselError::TransactionAlreadyActive => TransactionAlreadyActiveError::new_err(msg),
        RustChiselError::SavepointNotFound(_) => SavepointNotFoundError::new_err(msg),
        RustChiselError::DuplicateSavepoint(_) => DuplicateSavepointError::new_err(msg),
        RustChiselError::ReadOnlyMode => ReadOnlyModeError::new_err(msg),
        RustChiselError::FileNotFound => DatabaseFileNotFoundError::new_err(msg),
        RustChiselError::InvalidRootName => InvalidRootNameError::new_err(msg),
        RustChiselError::RootNameTableFull => RootNameTableFullError::new_err(msg),
        RustChiselError::InvalidSuperblockCount { .. } => InvalidSuperblockCountError::new_err(msg),
        // Fatal
        RustChiselError::IoError(_) => IoError::new_err(msg),
        RustChiselError::ChecksumMismatch { .. } => ChecksumMismatchError::new_err(msg),
        RustChiselError::CorruptSuperblock => CorruptSuperblockError::new_err(msg),
        RustChiselError::FileSizeMismatch { .. } => FileSizeMismatchError::new_err(msg),
        RustChiselError::InvalidMagic => InvalidMagicError::new_err(msg),
        RustChiselError::LockFailed => LockFailedError::new_err(msg),
        RustChiselError::UnsupportedFormatVersion { .. } => {
            UnsupportedFormatVersionError::new_err(msg)
        }
        RustChiselError::CorruptPage { .. } => CorruptPageError::new_err(msg),
        RustChiselError::InvalidPageId { .. } => InvalidPageIdError::new_err(msg),
        RustChiselError::Poisoned => PoisonedError::new_err(msg),
    }
}
