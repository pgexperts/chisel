// errors.rs — defines the Python exception hierarchy and converts a
// chisel::ChiselError into the appropriate PyErr. The two-tier split
// (OperationalError / FatalError) mirrors ChiselError::is_fatal() in
// src/error.rs exactly, so `except chisel.FatalError` in Python
// captures the same set of "drop-and-reopen" conditions that would
// poison the Rust TransactionManager.
//
// Invariant: every ChiselError variant maps to exactly one concrete
// exception class. The match in `to_py_err` is exhaustive, so adding a
// new ChiselError variant produces a compile error here rather than a
// silent fallback. That is the ONLY safety mechanism keeping the
// Python exception surface in sync with the Rust error enum — there is
// no test that enumerates variants.
//
// Class hierarchy (matches both the .pyi stubs and __init__.py re-exports):
//
//   Exception
//     ChiselError                        (base for all binding errors)
//       OperationalError                 (database intact; user or transient)
//         InvalidHandleError
//         NoActiveTransactionError
//         TransactionAlreadyActiveError
//         SavepointNotFoundError
//         DuplicateSavepointError
//         ReadOnlyModeError
//         DatabaseFileNotFoundError
//         InvalidRootNameError
//         RootNameTableFullError
//         InvalidSuperblockCountError
//         CacheFullError
//         SpillwayFullError
//         TransactionInProgressError
//         ClosedError              (I25: db.close() raced a live txn/sp)
//         AlreadyFinishedError     (I22/I24: double-drive a finished txn/sp)
//       FatalError                       (drop-and-reopen recovery only)
//         IoError
//         ChecksumMismatchError
//         CorruptSuperblockError
//         FileSizeMismatchError
//         InvalidMagicError
//         LockFailedError
//         UnsupportedFormatVersionError
//         CorruptPageError
//         InvalidPageIdError
//         PoisonedError

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
create_exception!(_chisel, CacheFullError, OperationalError);
// Byte-budget analogue of CacheFull: raised when the spillway sidecar's
// byte limit is reached during a transaction. Database intact; commit or
// roll back to drain the spillway and resume normal operation.
create_exception!(_chisel, SpillwayFullError, OperationalError);
// Raised when a configuration mutator is called while a transaction is
// active. Analogous to TransactionAlreadyActiveError — both are
// operational "wrong state" errors; the database is unharmed.
create_exception!(_chisel, TransactionInProgressError, OperationalError);
// ISSUES.md I25: raised by PyChisel's with_inner_io/with_inner_mut_io
// helpers when `inner` has been cleared by a prior close(). Distinct
// from PoisonedError because close() is a user action — the DB file
// is intact, only this handle is done. Typical repro: close() inside
// an enclosing `with db.transaction()` block — the __exit__ tries to
// commit and surfaces this instead of PoisonedError.
create_exception!(_chisel, ClosedError, OperationalError);
// ISSUES.md I22/I24: raised when an explicit `.commit()` / `.rollback()` /
// `.release()` / `.rollback_to()` is called a second time on a
// PyTransaction or PySavepoint whose one-shot `finished` guard is
// already set. Pre-I22 these calls silently succeeded; returning an
// explicit error makes "called the wrong object" bugs visible. Note
// that the __exit__ path is still idempotent (the guard short-
// circuits without raising) so context-manager usage is unaffected.
create_exception!(_chisel, AlreadyFinishedError, OperationalError);

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

// Manually register each exception class on the module. `create_exception!`
// creates the type but does NOT attach it to the module; `m.add` does
// that. Order within each tier does not matter — Python's `isinstance`
// cares only about the parent chain, which was set at class creation.
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
    m.add("CacheFullError", py.get_type_bound::<CacheFullError>())?;
    m.add(
        "SpillwayFullError",
        py.get_type_bound::<SpillwayFullError>(),
    )?;
    m.add(
        "TransactionInProgressError",
        py.get_type_bound::<TransactionInProgressError>(),
    )?;
    m.add("ClosedError", py.get_type_bound::<ClosedError>())?;
    m.add(
        "AlreadyFinishedError",
        py.get_type_bound::<AlreadyFinishedError>(),
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
//
// Called from every engine-facing site in db.rs (via with_inner_io /
// with_inner_mut_io) and from db::open when the initial open/create
// fails. The GIL is always held at call sites.
pub fn to_py_err(err: RustChiselError) -> PyErr {
    // The Display impl on ChiselError already yields human-readable text.
    // We do NOT attach the Rust error as __cause__ — the string is the
    // only cross-boundary contract, and round-tripping back into Rust
    // from Python isn't supported.
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
        RustChiselError::CacheFull { .. } => CacheFullError::new_err(msg),
        // SpillwayFull is the byte-budget analogue of CacheFull: both are
        // operational "buffer full, commit or roll back to free space"
        // conditions; the database is intact and can continue after draining.
        RustChiselError::SpillwayFull { .. } => SpillwayFullError::new_err(msg),
        // TransactionInProgress is the configuration-mutator analogue of
        // TransactionAlreadyActive: both are operational "wrong state for
        // this call" errors that the database recovers from without harm.
        RustChiselError::TransactionInProgress => TransactionInProgressError::new_err(msg),
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
        // I36: ChiselError is #[non_exhaustive], so even a path-deps
        // crate has to keep a catchall arm. Any variant not enumerated
        // above routes to the abstract `ChiselError` base class with
        // the Rust-side Display message. The internal compile-time
        // exhaustiveness check (this match used to enforce it via no `_`
        // arm) is gone, but the Display impl in src/error.rs is itself
        // exhaustive, so a new variant is still caught there at compile
        // time inside the chisel crate. A new ChiselError variant
        // landed without updating this match will still get a sensible
        // (if generic) Python exception class.
        _ => ChiselError::new_err(msg),
    }
}
