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
    // call (including after __exit__ has run, or after an earlier
    // rollback_to). Pre-I22 this was a silent no-op; that masked
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

    // Explicit rollback_to(): same idempotency-as-error policy as
    // release(). Note that the engine does NOT remove this savepoint —
    // `rollback_to_inner` ends with `savepoints.truncate(idx + 1)`, which
    // pops only the savepoints layered ON TOP and deliberately retains
    // this one so it can be rolled back to again or released
    // (src/transaction/savepoints.rs:47-49). So a second engine-level
    // rollback_to would SUCCEED, not fail with SavepointNotFound.
    //
    // The guard is therefore load-bearing, not a re-labelling of an error
    // the engine would raise anyway: it is the only thing enforcing the
    // Python contract that a Savepoint object is single-use, which is what
    // makes `with`-block exit and an explicit call unambiguous. Removing it
    // would silently turn repeated rollback_to into a working operation.
    fn rollback_to(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(already_finished_err());
        }
        self.finished.store(true, Ordering::SeqCst);
        self.db.bind(py).borrow().rollback_to_internal(&self.name)
    }
}

// Used by both explicit release() and explicit rollback_to(). Phrased
// generically ("savepoint already finished") so the same message works
// whether the prior finish was a release, a rollback_to, or a __exit__.
fn already_finished_err() -> PyErr {
    crate::errors::AlreadyFinishedError::new_err(
        "savepoint already finished (released or rolled back)",
    )
}
