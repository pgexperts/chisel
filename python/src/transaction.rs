// transaction.rs — PyTransaction: the context-manager-friendly front
// to an active Chisel transaction. Holds a reference back to the
// PyChisel via Py<PyChisel> so the transaction cannot outlive the db.
//
// Semantics:
//   __enter__:  db.transaction() already called begin() when this was
//               constructed; __enter__ is just a hand-off.
//   __exit__:   commits on clean exit, rolls back on exception. Never
//               suppresses (returns Ok(false)).
//   .commit(),
//   .rollback(): explicit drive (ISSUES.md I24). Both set the
//               `finished` guard before the engine call, so a
//               subsequent __exit__ short-circuits silently; a second
//               explicit call raises AlreadyFinishedError.
//
// Design note: PyTransaction is a stateless wrapper around the active
// transaction on the PyChisel. The underlying chisel::Chisel tracks
// "transaction is active" itself, so this object stores no transaction
// identity — only a db reference and a `finished` guard.
//
// Explicit commit() / rollback() (ISSUES.md I24) mirror the PySavepoint
// shape: they drive the engine and set `finished`, so a subsequent
// __exit__ short-circuits silently without a second engine call. A
// second explicit call (after another commit, rollback, or __exit__
// has already run) raises AlreadyFinishedError — matches the explicit-
// versus-__exit__ asymmetry on PySavepoint: context-manager exits are
// idempotent, explicit calls aren't.
//
// Ordering invariant: because only one chisel transaction is active
// at a time, nesting `with db.transaction()` inside another
// `db.transaction()` will fail at begin() with TransactionAlreadyActive.
// That is correct behaviour — Chisel has savepoints for nesting, not
// nested transactions.

use pyo3::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::db::PyChisel;

#[pyclass(name = "Transaction", module = "chisel._chisel")]
pub struct PyTransaction {
    // Owning a `Py<PyChisel>` pins the PyChisel on the Python heap for
    // at least as long as this transaction object is live. Combined
    // with PyChisel's `RefCell<Option<Chisel>>`, this prevents the
    // engine from being dropped mid-transaction — though the user CAN
    // still call `db.close()`, which nulls out the Option and turns
    // subsequent transaction calls into PoisonedError.
    db: Py<PyChisel>,
    // I75 (ISSUES.md, 2026-05-22): AtomicBool (not Cell<bool>) because
    // PyO3 0.24+ requires `#[pyclass]` types to be Sync. The original
    // rationale ("__exit__ takes &self, need interior mutability for
    // the one-shot guard, bool is Copy so Cell suffices") is still
    // true — we just need the Sync flavor. Ordering::SeqCst for the
    // same reason as PySavepoint: overkill but safest.
    finished: AtomicBool,
}

impl PyTransaction {
    pub fn new(db: Py<PyChisel>) -> Self {
        Self {
            db,
            finished: AtomicBool::new(false),
        }
    }
}

#[pymethods]
impl PyTransaction {
    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

    // Commit-on-success / rollback-on-exception. Only the *type* of the
    // raised exception is checked; we don't care about value or traceback.
    // A `finished` short-circuit prevents double-commit if the user
    // somehow re-entered __exit__, and reserves space for a future
    // explicit `.commit()` / `.rollback()` API on PyTransaction without
    // changing this logic.
    //
    // Returns Ok(false) unconditionally: we must NEVER suppress the
    // original Python exception. If the rollback itself fails (for
    // example because the engine is now poisoned), the caller gets the
    // rollback error instead of the original — matching standard
    // context-manager semantics for secondary errors on cleanup.
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: PyObject,
        _exc: PyObject,
        _tb: PyObject,
    ) -> PyResult<bool> {
        if self.finished.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.finished.store(true, Ordering::SeqCst);
        let db = self.db.bind(py).borrow();
        if !exc_type.is_none(py) {
            db.rollback_internal(py)?;
        } else {
            db.commit_internal(py)?;
        }
        Ok(false)
    }

    // Explicit commit() — drives the engine and sets `finished` so a
    // subsequent __exit__ short-circuits without a second engine call.
    // Raises AlreadyFinishedError if called a second time (after
    // another commit, a rollback, or a __exit__ has already run).
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(already_finished_err());
        }
        self.finished.store(true, Ordering::SeqCst);
        self.db.bind(py).borrow().commit_internal(py)
    }

    // Explicit rollback() — same `finished` semantics as commit(). Useful
    // when a user wants to abort a transaction manually without raising
    // an exception through the enclosing `with` block.
    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(already_finished_err());
        }
        self.finished.store(true, Ordering::SeqCst);
        self.db.bind(py).borrow().rollback_internal(py)
    }

    fn allocate(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        self.db.bind(py).borrow().allocate_internal(py, value)
    }

    fn read<'py>(
        &self,
        py: Python<'py>,
        handle: u64,
    ) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        self.db.bind(py).borrow().read_internal(py, handle)
    }

    fn update(&self, py: Python<'_>, handle: u64, value: &Bound<'_, PyAny>) -> PyResult<()> {
        self.db.bind(py).borrow().update_internal(py, handle, value)
    }

    fn delete(&self, py: Python<'_>, handle: u64) -> PyResult<()> {
        self.db.bind(py).borrow().delete_internal(py, handle)
    }

    fn delete_many(&self, py: Python<'_>, handles: Vec<u64>) -> PyResult<()> {
        self.db.bind(py).borrow().delete_many_internal(py, &handles)
    }

    fn set_root_name(&self, py: Python<'_>, name: &str, handle: u64) -> PyResult<()> {
        self.db
            .bind(py)
            .borrow()
            .set_root_name_internal(py, name, handle)
    }

    fn get_root_name(&self, py: Python<'_>, name: &str) -> PyResult<Option<u64>> {
        self.db.bind(py).borrow().get_root_name_internal(py, name)
    }

    fn clear_root_name(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.db.bind(py).borrow().clear_root_name_internal(py, name)
    }

    // PySavepoint takes its OWN Py<PyChisel> (via clone_ref, which
    // bumps the Python refcount), so the savepoint object outlives
    // this PyTransaction reference-wise. That is harmless: both
    // objects route through PyChisel, and the engine rejects
    // savepoint ops outside an active transaction.
    fn savepoint(&self, py: Python<'_>, name: &str) -> PyResult<Py<crate::savepoint::PySavepoint>> {
        self.db.bind(py).borrow().savepoint_internal(py, name)?;
        Py::new(
            py,
            crate::savepoint::PySavepoint::new(self.db.clone_ref(py), name.to_string()),
        )
    }
}

// Message phrased generically so the same text works whether the prior
// finish was a commit, a rollback, or a __exit__.
fn already_finished_err() -> PyErr {
    crate::errors::AlreadyFinishedError::new_err(
        "transaction already finished (committed or rolled back)",
    )
}
