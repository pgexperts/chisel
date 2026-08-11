// errors.rs — defines the Python exception hierarchy and converts a
// chisel::ChiselError into the appropriate PyErr. The two-tier split
// (OperationalError / FatalError) mirrors ChiselError::is_fatal() in
// src/error.rs (with one deliberate exception, PoisonedError — see the
// Fatal block below), so `except chisel.FatalError` in Python
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
// is still preferred. The engine-side is_fatal() exhaustiveness test (I104)
// guards the classification this fallback depends on.
//
// TEST COVERAGE OF THIS MAPPING IS ASYMMETRIC. Read the split below before
// assuming a rearrangement of `to_py_err`'s arms would be caught:
//   - TIER — complete. All 33 concrete classes have an assertion placing them
//     under OperationalError or FatalError: test_errors.py's
//     test_operational_hierarchy (14 names) and test_fatal_hierarchy (10),
//     test_exception_contract.py's test_operational_hierarchy_missing_classes
//     (SpillwayFull / TransactionInProgress / TagMismatch) and
//     test_decryption_failed_is_fatal_hierarchy, and the trailing
//     `isinstance(..., OperationalError)` of the five encryption and
//     key-rotation contract tests. test_errors.py's
//     test_exception_sweep_covers_every_class_the_module_defines compares
//     chisel.__all__ against dir(chisel._chisel) BY EQUALITY, so a class added
//     here but not re-exported fails there rather than quietly dropping out of
//     the parametrized __module__/pickle sweeps.
//   - ARM — partial. Of the 31 concrete arms in `to_py_err`, 20 are raised
//     end-to-end from Python AND pinned to the exact class, so swapping one of
//     those fails loudly. TransactionAlreadyActive is raised but caught only at
//     the ChiselError base (test_transactions.py::test_nested_transactions_raise).
//     The remaining ten are never raised from Python at all — they are held
//     only by the tier assertions above, so a swap among them passes the whole
//     suite: InvalidHandleError, InvalidArgon2ParamsError, DecryptionFailedError,
//     ChecksumMismatchError, FileSizeMismatchError, UnsupportedFormatVersionError,
//     UnsupportedPageSizeError, CorruptPageError, InvalidPageIdError,
//     PoisonedError. (InvalidArgon2ParamsError is not merely untested but
//     unreachable from Python by construction — see its registration below.)
//   - The fatal half is the thin one: only 3 of the 11 fatal arms (IoError,
//     LockFailed, CorruptSuperblock) are driven end-to-end.
//     test_exception_contract.py section 11 flags that itself and names the fix
//     — a Rust-side unit test of `to_py_err`, which needs binding-crate test
//     infrastructure.
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
//         InvalidArgon2ParamsError
//         CacheFullError
//         SpillwayFullError
//         TransactionInProgressError
//         NoEncryptionKeyError
//         InvalidEncryptionKeyError
//         EncryptionNotSupportedError
//         NoFreeKeySlotError
//         LastKeySlotError
//         ClosedError              (I25: db.close() raced a live txn/sp)
//         AlreadyFinishedError     (I22/I24: double-drive a finished txn/sp)
//         TagMismatchError         (delete_tagged supplied a tag that does
//                                   not match the handle's stored tag)
//       FatalError                       (drop-and-reopen recovery only)
//         IoError                  (ALSO subclasses builtin OSError — see register)
//         DecryptionFailedError    (encrypted page/superblock failed AEAD auth)
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
/// PYTHON-7 (issue #105): `__module__` must be the DOTTED path
/// `chisel._chisel`, not the bare `_chisel`. maturin installs the extension as
/// a SUBMODULE (`module-name = "chisel._chisel"` in pyproject.toml), so
/// `_chisel` is not importable on its own — and pickle resolves an exception
/// class by importing `__module__` and looking up `__name__` there. With the
/// bare name, `pickle.dumps(chisel.PoisonedError('x'))` raised
/// `PicklingError: import of module '_chisel' failed`.
///
/// That matters beyond cosmetics: an exception raised in a
/// ProcessPoolExecutor / multiprocessing worker is pickled to be re-raised in
/// the parent, so the original ChiselError was being replaced by a
/// PicklingError — destroying the two-tier operational/fatal contract exactly
/// where it matters most, a background writer reporting a FatalError.
///
/// The `#[pyclass]` types (Chisel, PyTransaction, ...) always had this right
/// via an explicit `module = "chisel._chisel"`; only the exceptions did not.
fn build_io_error_class<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyType>> {
    let bases = (py.get_type::<FatalError>(), py.get_type::<PyOSError>());
    let namespace = PyDict::new(py);
    namespace.set_item("__module__", "chisel._chisel")?;
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

create_exception!(chisel._chisel, ChiselError, PyException);
create_exception!(chisel._chisel, OperationalError, ChiselError);
create_exception!(chisel._chisel, FatalError, ChiselError);

// Operational
create_exception!(chisel._chisel, InvalidHandleError, OperationalError);
create_exception!(chisel._chisel, NoActiveTransactionError, OperationalError);
create_exception!(
    chisel._chisel,
    TransactionAlreadyActiveError,
    OperationalError
);
create_exception!(chisel._chisel, SavepointNotFoundError, OperationalError);
create_exception!(chisel._chisel, DuplicateSavepointError, OperationalError);
create_exception!(chisel._chisel, ReadOnlyModeError, OperationalError);
create_exception!(chisel._chisel, DatabaseFileNotFoundError, OperationalError);
create_exception!(chisel._chisel, InvalidRootNameError, OperationalError);
create_exception!(chisel._chisel, RootNameTableFullError, OperationalError);
create_exception!(
    chisel._chisel,
    InvalidSuperblockCountError,
    OperationalError
);
// PUBLIC-API-8 (issue #106): Options.argon2_params carries cost values the
// KDF cannot use. Raised before the file is touched, so it is never a
// credential failure — the Rust variant it maps to used to surface as
// InvalidEncryptionKeyError, which on a database being CREATED cannot be true
// (no key slot exists yet to mismatch).
//
// NOT REACHABLE FROM PYTHON TODAY: `chisel.open()` exposes `encryption_key`
// but not `argon2_params`, so no Python caller can supply the values that
// raise it. It is registered anyway so the mapping is complete at the point
// the Rust variant was added — `to_py_err`'s catchall routes unmapped
// variants by `is_fatal()` alone, so a missing arm is not a compile error
// here, just a silent downgrade to the generic OperationalError. Wiring an
// `argon2_params` kwarg later then needs no error-plumbing work.
create_exception!(chisel._chisel, InvalidArgon2ParamsError, OperationalError);
create_exception!(chisel._chisel, CacheFullError, OperationalError);
// Byte-budget analogue of CacheFull: raised when the spillway sidecar's
// byte limit is reached during a transaction. Database intact; commit or
// roll back to drain the spillway and resume normal operation.
create_exception!(chisel._chisel, SpillwayFullError, OperationalError);
// Raised when a configuration mutator is called while a transaction is
// active. Analogous to TransactionAlreadyActiveError — both are
// operational "wrong state" errors; the database is unharmed.
create_exception!(chisel._chisel, TransactionInProgressError, OperationalError);
// Raised by delete_tagged when the caller supplies a tag that does not
// match the handle's stored tag. The chunk and membership index are
// left unmodified — the mismatch is purely a caller error, not a
// data-integrity problem.
create_exception!(chisel._chisel, TagMismatchError, OperationalError);
// Encryption-related operational errors: the database is intact; the caller
// supplied wrong or missing key material. All three have is_fatal() = false.
// NoEncryptionKey: encrypted DB opened without an encryption_key argument.
create_exception!(chisel._chisel, NoEncryptionKeyError, OperationalError);
// InvalidEncryptionKey: encryption_key was supplied but unwraps no key slot
// (wrong passphrase or wrong raw bytes).
create_exception!(chisel._chisel, InvalidEncryptionKeyError, OperationalError);
// EncryptionNotSupported: encryption_key was supplied but the DB is plaintext.
create_exception!(
    chisel._chisel,
    EncryptionNotSupportedError,
    OperationalError
);
// NoFreeKeySlot: add_key/rotate_key attempted but the 8-slot key table is full.
create_exception!(chisel._chisel, NoFreeKeySlotError, OperationalError);
// LastKeySlot: remove_key would leave the DB with no active key — rejected.
create_exception!(chisel._chisel, LastKeySlotError, OperationalError);
// ISSUES.md I25: raised by PyChisel's with_inner_io/with_inner_mut_io
// helpers when `inner` has been cleared by a prior close(). Distinct
// from PoisonedError because close() is a user action — the DB file
// is intact, only this handle is done. Typical repro: close() inside
// an enclosing `with db.transaction()` block — the __exit__ tries to
// commit and surfaces this instead of PoisonedError.
create_exception!(chisel._chisel, ClosedError, OperationalError);
// ISSUES.md I22/I24: raised when a PyTransaction or PySavepoint whose
// one-shot `finished` guard is already set is driven again. Pre-I22 these
// calls silently succeeded; returning an explicit error makes "called the
// wrong object" bugs visible. The __exit__ path stays idempotent (the guard
// short-circuits without raising), so context-manager usage is unaffected.
//
// What sets the guard: `.commit()` / `.rollback()` on a transaction,
// `.release()` on a savepoint, and `__exit__` on either. `.rollback_to()`
// does NOT — it is repeatable (PYTHON-3, issue #105), matching the engine,
// which deliberately keeps the mark on the stack. It still CHECKS the guard,
// so a rollback_to after a release raises this rather than reaching the
// engine and coming back as SavepointNotFound.
//
// PYTHON-1 (issue #105): a transaction's ~20 DATA methods now check the guard
// too, not just commit/rollback. They used to delegate unconditionally, so a
// finished `tx` object wrote into whatever transaction happened to be open —
// the guard's stated purpose, defeated on precisely the methods that mutate.
create_exception!(chisel._chisel, AlreadyFinishedError, OperationalError);

// Fatal — matches ChiselError::is_fatal() in src/error.rs, plus PoisonedError:
// it is a FatalError subclass (a drop-and-reopen condition for the caller) even
// though ChiselError::Poisoned is classified non-fatal by is_fatal() — Poisoned
// just means the manager is already dead, not a fresh fatal.
// IoError is NOT declared here: it needs two bases (FatalError + OSError) and is
// built in `register` via `build_io_error_class` / cached in `IO_ERROR_CLASS`.
// DecryptionFailed: a page-read failed MAC verification after a successful open,
// or (issue #119) the sealed superblock body failed it during open itself, on a
// file whose key slot the supplied credential had already unwrapped — proving
// the credential correct and the file damaged. is_fatal() = true in both cases —
// data integrity cannot be confirmed; treat as poison. Note the open-time case
// is a behaviour change: such a file used to raise the OPERATIONAL
// InvalidEncryptionKeyError, inviting a caller to retry credentials forever.
create_exception!(chisel._chisel, DecryptionFailedError, FatalError);
create_exception!(chisel._chisel, ChecksumMismatchError, FatalError);
create_exception!(chisel._chisel, CorruptSuperblockError, FatalError);
create_exception!(chisel._chisel, FileSizeMismatchError, FatalError);
create_exception!(chisel._chisel, LockFailedError, FatalError);
create_exception!(chisel._chisel, UnsupportedFormatVersionError, FatalError);
create_exception!(chisel._chisel, UnsupportedPageSizeError, FatalError);
create_exception!(chisel._chisel, CorruptPageError, FatalError);
create_exception!(chisel._chisel, InvalidPageIdError, FatalError);
create_exception!(chisel._chisel, PoisonedError, FatalError);

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
    m.add(
        "InvalidArgon2ParamsError",
        py.get_type::<InvalidArgon2ParamsError>(),
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
    m.add(
        "NoEncryptionKeyError",
        py.get_type::<NoEncryptionKeyError>(),
    )?;
    m.add(
        "InvalidEncryptionKeyError",
        py.get_type::<InvalidEncryptionKeyError>(),
    )?;
    m.add(
        "EncryptionNotSupportedError",
        py.get_type::<EncryptionNotSupportedError>(),
    )?;
    m.add("NoFreeKeySlotError", py.get_type::<NoFreeKeySlotError>())?;
    m.add("LastKeySlotError", py.get_type::<LastKeySlotError>())?;

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
    m.add(
        "DecryptionFailedError",
        py.get_type::<DecryptionFailedError>(),
    )?;

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
        RustChiselError::InvalidArgon2Params { .. } => InvalidArgon2ParamsError::new_err(msg),
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
        // Encryption: operational (wrong/missing key, plaintext DB).
        RustChiselError::NoEncryptionKey => NoEncryptionKeyError::new_err(msg),
        RustChiselError::InvalidEncryptionKey => InvalidEncryptionKeyError::new_err(msg),
        RustChiselError::EncryptionNotSupported => EncryptionNotSupportedError::new_err(msg),
        // Key-rotation operational errors: the DB is intact; caller hit a
        // capacity limit (table full) or a safety guard (last slot).
        RustChiselError::NoFreeKeySlot => NoFreeKeySlotError::new_err(msg),
        RustChiselError::LastKeySlot => LastKeySlotError::new_err(msg),
        // Fatal encryption: MAC verification failed on a page read after open.
        RustChiselError::DecryptionFailed { .. } => DecryptionFailedError::new_err(msg),
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
