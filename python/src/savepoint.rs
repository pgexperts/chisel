// savepoint.rs — PySavepoint: a named mark within an active transaction,
// usable as a context manager (release on clean exit, rollback_to on
// exception) or driven explicitly. Like PyTransaction, it delegates to
// the PyChisel's internal methods rather than holding engine state of
// its own; the savepoint name is the only thing the object carries.
//
// Savepoint lifecycle: the engine maintains a stack of savepoints
// keyed by name; release() removes the mark (making pages written
// since the savepoint non-revertable), rollback_to() restores the
// in-memory roots to the snapshot taken at savepoint creation and
// abandons the pages written since. Neither operation ends the
// enclosing transaction.
//
// `finished` is a one-shot guard that prevents (a) an explicit
// release()/rollback_to() call followed by __exit__ from double-
// driving the engine, and (b) an __exit__ from firing after an
// explicit call. Mirrors the same pattern in PyTransaction.

use pyo3::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::db::PyChisel;

#[pyclass(name = "Savepoint", module = "chisel._chisel")]
pub struct PySavepoint {
    // Independent Py<PyChisel> ref (not shared with the PyTransaction
    // that created us) so a savepoint can outlive the specific
    // transaction-object's Py reference as long as the enclosing
    // transaction on the engine is still active.
    db: Py<PyChisel>,
    // `#[pyo3(get)]` exposes this as a read-only Python attribute
    // `sp.name`. String (owned) rather than &str because the object
    // outlives any Python-side name literal.
    #[pyo3(get)]
    name: String,
    // true once the savepoint has been released or rolled back; prevents
    // a context-manager __exit__ from double-firing after an explicit call.
    // I75 (ISSUES.md, 2026-05-22): AtomicBool (not Cell<bool>) because
    // PyO3 0.24+ requires `#[pyclass]` types to be Sync. Same one-shot
    // semantics; Ordering::SeqCst is overkill for a flag that's only
    // read after being written from the same thread, but it's the
    // safest default and the perf delta is unmeasurable.
    finished: AtomicBool,
}

impl PySavepoint {
    pub fn new(db: Py<PyChisel>, name: String) -> Self {
        Self {
            db,
            name,
            finished: AtomicBool::new(false),
        }
    }
}

#[pymethods]
impl PySavepoint {
    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

    // Exception → rollback_to; clean exit → release. Returns Ok(false)
    // to avoid suppressing any in-flight exception, matching the
    // PyTransaction __exit__ policy.
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Py<PyAny>,
        _exc: Py<PyAny>,
        _tb: Py<PyAny>,
    ) -> PyResult<bool> {
        if self.finished.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.finished.store(true, Ordering::SeqCst);
        let db = self.db.bind(py).borrow();
        if !exc_type.is_none(py) {
            db.rollback_to_internal(&self.name)?;
        } else {
            db.release_internal(&self.name)?;
        }
        Ok(false)
    }

    // Explicit release(): raises AlreadyFinishedError on a second
    // call (including after __exit__ has run). Note an earlier `rollback_to`
    // does NOT finish the savepoint — see rollback_to below.
    // Pre-I22 this was a silent no-op; that masked
    // "called release() on the wrong sp object" bugs. The __exit__
    // path itself stays idempotent (guard short-circuits without
    // raising), so normal context-manager usage is unaffected.
    fn release(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(already_finished_err());
        }
        self.finished.store(true, Ordering::SeqCst);
        self.db.bind(py).borrow().release_internal(&self.name)
    }

    // Explicit rollback_to(): REPEATABLE. It CHECKS `finished` but does not
    // SET it, and that asymmetry is the whole point (PYTHON-3, issue #105).
    //
    // The engine deliberately keeps the mark: `rollback_to_inner` ends with
    // `savepoints.truncate(idx + 1)`, popping only the savepoints layered on
    // top, and its doc says so outright — "The named savepoint itself remains
    // on the stack and can be rolled back to again or released." Setting
    // `finished` here made the binding strictly MORE restrictive than the
    // engine, and, because `__exit__` short-circuits on the same flag, it also
    // leaked the mark: a savepoint whose body called rollback_to was never
    // released, so its name stayed taken for the rest of the transaction and
    // `tx.savepoint(same_name)` raised DuplicateSavepointError. An engine
    // capability the engine documents was unreachable from Python, and the
    // name was burned with no operation able to free it — release() was
    // blocked by this guard and re-creation by the engine.
    //
    // The retained `load` check is NOT vestigial: it is what makes
    // `sp.release(); sp.rollback_to()` raise AlreadyFinishedError instead of
    // reaching the engine and getting SavepointNotFound, which would report a
    // wrong-object bug as a missing-savepoint bug.
    fn rollback_to(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(already_finished_err());
        }
        self.db.bind(py).borrow().rollback_to_internal(&self.name)
    }
}

// Raised when a Savepoint object is driven after it has been RELEASED —
// explicitly, or by `__exit__`. `rollback_to` no longer finishes a savepoint
// (PYTHON-3), so "released" is now the accurate word: a rolled-back-to
// savepoint is still live and can be rolled back to again.
fn already_finished_err() -> PyErr {
    crate::errors::AlreadyFinishedError::new_err("savepoint already released")
}
