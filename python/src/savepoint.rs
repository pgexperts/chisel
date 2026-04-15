// savepoint.rs — PySavepoint: a named mark within an active transaction,
// usable as a context manager (release on clean exit, rollback_to on
// exception) or driven explicitly. Like PyTransaction, it delegates to
// the PyChisel's internal methods rather than holding engine state of
// its own; the savepoint name is the only thing the object carries.

use pyo3::prelude::*;
use std::cell::Cell;

use crate::db::PyChisel;

#[pyclass(name = "Savepoint", module = "chisel._chisel")]
pub struct PySavepoint {
    db: Py<PyChisel>,
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

    fn release(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.get() {
            return Ok(());
        }
        self.finished.set(true);
        self.db.bind(py).borrow().release_internal(py, &self.name)
    }

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
