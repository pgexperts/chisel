// errors.rs — defines the Python exception hierarchy and converts a
// chisel::ChiselError into the appropriate PyErr. The two-tier split
// (OperationalError / FatalError) mirrors ChiselError::is_fatal() in
// src/error.rs exactly, so `except chisel.FatalError` in Python
// captures the same set of "drop-and-reopen" conditions that would
// poison the Rust TransactionManager.
//
// Invariant: every CURRENT ChiselError variant maps to exactly one concrete
// exception class via an explicit arm in `to_py_err`. ChiselError is
// #[non_exhaustive] (I36), so the match carries a catchall `_` arm and is NOT
// compile-time exhaustive. To keep the two-tier contract intact for a future
// un-enumerated variant, that catchall routes by `is_fatal()` to the correct
// tier base — OperationalError or FatalError — never the abstract ChiselError
// base (I138). So a new variant always lands under the right poison-contract
// parent even before it gets its own concrete class; adding the concrete arm
// is still preferred. No test enumerates the Python classes (ISSUES.md I139);
// the engine-side is_fatal() exhaustiveness test (I104) guards the
// classification this fallback depends on.
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
//         IoError                  (ALSO subclasses builtin OSError — see register)
//         ChecksumMismatchError
//         CorruptSuperblockError
//         FileSizeMismatchError
//         LockFailedError
//         UnsupportedFormatVersionError
//         UnsupportedPageSizeError
//         CorruptPageError
//         InvalidPageIdError
//         PoisonedError

use chisel::ChiselError as RustChiselError;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyOSError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use std::sync::OnceLock;

// IoError is the one exception that needs TWO bases — chisel.FatalError (the
// two-tier poison contract) AND the builtin OSError (idiomatic disk-error
// handling, native `.errno`) — which `create_exception!` (single base) cannot
// express. We build it at module init via Python's 3-arg `type()` and cache the
// class here so `to_py_err` raises instances of THAT multiply-inherited class
// rather than a single-base macro type. `Py<PyType>` is Send+Sync, so a plain
// std OnceLock suffices (no GIL token needed to read it back).
static IO_ERROR_CLASS: OnceLock<Py<PyType>> = OnceLock::new();

/// Build the `IoError` class with bases `(FatalError, OSError)`. The MRO is
/// consistent (C3 succeeds): FatalError's chain (ChiselError -> Exception) and
/// OSError's chain (Exception) share only Exception/BaseException as a tail.
fn build_io_error_class<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyType>> {
    let bases = (py.get_type::<FatalError>(), py.get_type::<PyOSError>());
    let namespace = PyDict::new(py);
    namespace.set_item("__module__", "_chisel")?;
    namespace.set_item(
        "__doc__",
        "Fatal I/O error. Subclasses both chisel.FatalError and the builtin \
         OSError, so it is catchable as either; `.errno` is OSError's native \
         attribute and `.kind` is the std::io::ErrorKind debug string.",
    )?;
    let type_ctor = py.import("builtins")?.getattr("type")?;
    type_ctor
        .call1(("IoError", bases, namespace))?
        .cast_into::<PyType>()
        .map_err(PyErr::from)
}

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
// Raised by delete_tagged when the caller supplies a tag that does not
// match the handle's stored tag. The chunk and membership index are
// left unmodified — the mismatch is purely a caller error, not a
// data-integrity problem.
create_exception!(_chisel, TagMismatchError, OperationalError);
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
// IoError is NOT declared here: it needs two bases (FatalError + OSError) and is
// built in `register` via `build_io_error_class` / cached in `IO_ERROR_CLASS`.
create_exception!(_chisel, ChecksumMismatchError, FatalError);
create_exception!(_chisel, CorruptSuperblockError, FatalError);
create_exception!(_chisel, FileSizeMismatchError, FatalError);
create_exception!(_chisel, LockFailedError, FatalError);
create_exception!(_chisel, UnsupportedFormatVersionError, FatalError);
create_exception!(_chisel, UnsupportedPageSizeError, FatalError);
create_exception!(_chisel, CorruptPageError, FatalError);
create_exception!(_chisel, InvalidPageIdError, FatalError);
create_exception!(_chisel, PoisonedError, FatalError);

// Manually register each exception class on the module. `create_exception!`
// creates the type but does NOT attach it to the module; `m.add` does
// that. Order within each tier does not matter — Python's `isinstance`
// cares only about the parent chain, which was set at class creation.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("ChiselError", py.get_type::<ChiselError>())?;
    m.add("OperationalError", py.get_type::<OperationalError>())?;
    m.add("FatalError", py.get_type::<FatalError>())?;

    m.add("InvalidHandleError", py.get_type::<InvalidHandleError>())?;
    m.add(
        "NoActiveTransactionError",
        py.get_type::<NoActiveTransactionError>(),
    )?;
    m.add(
        "TransactionAlreadyActiveError",
        py.get_type::<TransactionAlreadyActiveError>(),
    )?;
    m.add(
        "SavepointNotFoundError",
        py.get_type::<SavepointNotFoundError>(),
    )?;
    m.add(
        "DuplicateSavepointError",
        py.get_type::<DuplicateSavepointError>(),
    )?;
    m.add("ReadOnlyModeError", py.get_type::<ReadOnlyModeError>())?;
    m.add(
        "DatabaseFileNotFoundError",
        py.get_type::<DatabaseFileNotFoundError>(),
    )?;
    m.add(
        "InvalidRootNameError",
        py.get_type::<InvalidRootNameError>(),
    )?;
    m.add(
        "RootNameTableFullError",
        py.get_type::<RootNameTableFullError>(),
    )?;
    m.add(
        "InvalidSuperblockCountError",
        py.get_type::<InvalidSuperblockCountError>(),
    )?;
    m.add("CacheFullError", py.get_type::<CacheFullError>())?;
    m.add("SpillwayFullError", py.get_type::<SpillwayFullError>())?;
    m.add(
        "TransactionInProgressError",
        py.get_type::<TransactionInProgressError>(),
    )?;
    m.add("ClosedError", py.get_type::<ClosedError>())?;
    m.add(
        "AlreadyFinishedError",
        py.get_type::<AlreadyFinishedError>(),
    )?;
    m.add("TagMismatchError", py.get_type::<TagMismatchError>())?;

    // IoError multiply-inherits (FatalError, OSError); register that class and
    // cache it so `to_py_err` constructs instances of it (not a single-base
    // type). `set` is a no-op if a prior init already cached it.
    let io_error = build_io_error_class(py)?;
    m.add("IoError", &io_error)?;
    let _ = IO_ERROR_CLASS.set(io_error.unbind());
    m.add(
        "ChecksumMismatchError",
        py.get_type::<ChecksumMismatchError>(),
    )?;
    m.add(
        "CorruptSuperblockError",
        py.get_type::<CorruptSuperblockError>(),
    )?;
    m.add(
        "FileSizeMismatchError",
        py.get_type::<FileSizeMismatchError>(),
    )?;
    m.add("LockFailedError", py.get_type::<LockFailedError>())?;
    m.add(
        "UnsupportedFormatVersionError",
        py.get_type::<UnsupportedFormatVersionError>(),
    )?;
    m.add(
        "UnsupportedPageSizeError",
        py.get_type::<UnsupportedPageSizeError>(),
    )?;
    m.add("CorruptPageError", py.get_type::<CorruptPageError>())?;
    m.add("InvalidPageIdError", py.get_type::<InvalidPageIdError>())?;
    m.add("PoisonedError", py.get_type::<PoisonedError>())?;

    Ok(())
}

// Convert a chisel::ChiselError into the right PyErr. The match is
// NON-exhaustive: ChiselError is #[non_exhaustive] (I36), so the match
// carries a catchall `_` arm and a new variant will NOT produce a compile
// error. The catchall routes by is_fatal() to the correct tier base
// (FatalError or OperationalError); adding a concrete arm for each new
// variant is still preferred over relying on the catchall.
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
    // Captured before the match consumes `err`; used only by the I138 catchall
    // below to pick the correct tier base for a future un-enumerated variant.
    let fatal = err.is_fatal();
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
        // TagMismatch is the "wrong tag on delete_tagged" caller error:
        // the chunk and membership index are left intact, so it is
        // operational — distinct from any data-integrity problem.
        RustChiselError::TagMismatch { .. } => TagMismatchError::new_err(msg),
        // Fatal
        //
        // I42 (ISSUES.md, 2026-05-22): expose the inner io::Error's errno
        // and kind on the resulting Python IoError instance so callers can
        // programmatically distinguish ENOSPC vs EACCES vs EIO without
        // string-parsing the message. The metadata is attached as ordinary
        // Python attributes via setattr; the .pyi stub declares them so
        // type-checkers see them. setattr failures are swallowed because
        // the message-only PyErr is still a valid exception and we don't
        // want a setattr fluke to mask the real I/O error.
        RustChiselError::IoError(io_err) => {
            let errno = io_err.raw_os_error();
            let kind = format!("{:?}", io_err.kind());
            Python::attach(|py| {
                // IO_ERROR_CLASS is populated by register() at module init.
                // The fallback branch (OnceLock empty) is unreachable in normal
                // operation; it only fires if to_py_err is called before
                // register() completes, which cannot happen today — the module
                // init sequence sets the lock before any Python code can call
                // into the binding. The fallback raises a plain FatalError so
                // the error is never silently lost and the two-tier poison
                // contract is preserved even in that hypothetical ordering.
                let cls_owned;
                let cls: &Bound<'_, pyo3::types::PyType> = match IO_ERROR_CLASS.get() {
                    Some(cached) => {
                        cls_owned = cached.bind(py).clone();
                        &cls_owned
                    }
                    None => {
                        return FatalError::new_err(msg);
                    }
                };
                // Construct via OSError's 2-arg (errno, strerror) form when we
                // have an errno so CPython populates `.errno`/`.strerror`
                // natively; otherwise the message-only form (.errno -> None).
                let instance = match errno {
                    Some(n) => cls.call1((n, &msg)),
                    None => cls.call1((&msg,)),
                };
                match instance {
                    Ok(value) => {
                        // `.kind` is Chisel-specific — no OSError native slot.
                        let _ = value.setattr("kind", &kind);
                        PyErr::from_value(value)
                    }
                    // Constructing the exception itself failed (should not
                    // happen): surface that error rather than masking it.
                    Err(e) => e,
                }
            })
        }
        RustChiselError::ChecksumMismatch { .. } => ChecksumMismatchError::new_err(msg),
        RustChiselError::CorruptSuperblock { .. } => CorruptSuperblockError::new_err(msg),
        RustChiselError::FileSizeMismatch { .. } => FileSizeMismatchError::new_err(msg),
        RustChiselError::LockFailed => LockFailedError::new_err(msg),
        RustChiselError::UnsupportedFormatVersion { .. } => {
            UnsupportedFormatVersionError::new_err(msg)
        }
        RustChiselError::UnsupportedPageSize { .. } => UnsupportedPageSizeError::new_err(msg),
        RustChiselError::CorruptPage { .. } => CorruptPageError::new_err(msg),
        RustChiselError::InvalidPageId { .. } => InvalidPageIdError::new_err(msg),
        RustChiselError::Poisoned => PoisonedError::new_err(msg),
        // I138 (ISSUES.md, 2026-06-21): ChiselError is #[non_exhaustive] (I36),
        // so this catchall is required for any variant not enumerated above.
        // Route it through the Operational/Fatal split by is_fatal() — NOT the
        // abstract ChiselError base it used to use. Previously a future *fatal*
        // variant landed under the abstract base, so `except chisel.FatalError:`
        // poison-recovery handlers silently missed it, breaking the documented
        // FatalError contract — the same fail-open class as engine I104. The
        // concrete arms above are still preferred; this only fixes the fallback's
        // parent so a new variant is at least catchable at the correct tier.
        _ => {
            if fatal {
                FatalError::new_err(msg)
            } else {
                OperationalError::new_err(msg)
            }
        }
    }
}
