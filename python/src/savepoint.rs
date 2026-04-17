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
use std::cell::Cell;

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
    finished: Cell<bool>,
}

impl PySavepoint {
    pub fn new(db: Py<PyChisel>, name: String) -> Self {
        Self {
            db,
            name,
            finished: Cell::new(false),
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
        exc_type: PyObject,
        _exc: PyObject,
        _tb: PyObject,
    ) -> PyResult<bool> {
        if self.finished.get() {
            return Ok(false);
        }
        self.finished.set(true);
        let db = self.db.bind(py).borrow();
        if !exc_type.is_none(py) {
            db.rollback_to_internal(py, &self.name)?;
        } else {
            db.release_internal(py, &self.name)?;
        }
        Ok(false)
    }

    // Explicit release(): idempotent. A second call (or a call after
    // __exit__ ran) is a no-op rather than raising SavepointNotFound,
    // because the user's intent ("make sure this savepoint is gone")
    // is already satisfied.
    fn release(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.get() {
            return Ok(());
        }
        self.finished.set(true);
        self.db.bind(py).borrow().release_internal(py, &self.name)
    }

    // Explicit rollback_to(): same idempotency rationale as release().
    // Note that after rollback_to, the savepoint mark itself is also
    // gone on the engine side — the engine pops the stack down to and
    // including this savepoint. So a subsequent rollback_to() on the
    // same object would fail at the engine layer with SavepointNotFound;
    // the `finished` guard here converts that into a silent no-op.
    fn rollback_to(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.get() {
            return Ok(());
        }
        self.finished.set(true);
        self.db
            .bind(py)
            .borrow()
            .rollback_to_internal(py, &self.name)
    }
}
