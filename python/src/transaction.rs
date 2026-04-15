// transaction.rs — PyTransaction: the context-manager-friendly front
// to an active Chisel transaction. Holds a reference back to the
// PyChisel via Py<PyChisel> so the transaction cannot outlive the db.
//
// Semantics:
//   __enter__: db.transaction() already called begin() when this was
//              constructed; __enter__ is just a hand-off.
//   __exit__:  commits on clean exit, rolls back on exception. Never
//              suppresses (returns Ok(false)).

use pyo3::prelude::*;
use std::cell::Cell;

use crate::db::PyChisel;

#[pyclass(name = "Transaction", module = "chisel._chisel")]
pub struct PyTransaction {
    db: Py<PyChisel>,
    finished: Cell<bool>,
}

impl PyTransaction {
    pub fn new(db: Py<PyChisel>) -> Self {
        Self {
            db,
            finished: Cell::new(false),
        }
    }
}

#[pymethods]
impl PyTransaction {
    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

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
            db.rollback_internal(py)?;
        } else {
            db.commit_internal(py)?;
        }
        Ok(false)
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
}
