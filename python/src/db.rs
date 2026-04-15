// db.rs — PyChisel wraps chisel::Chisel and provides the top-level
// `open()` constructor. The engine is held in a RefCell<Option<Chisel>>
// so that close() can deterministically drop it; subsequent calls see
// None and (for is_poisoned) report true — a closed handle is,
// semantically, a handle that can never produce new results, which is
// indistinguishable from poisoned from the caller's perspective.

use pyo3::prelude::*;
use pyo3::types::PyString;
use std::cell::RefCell;
use std::path::PathBuf;

use chisel::{Chisel, Options};

use crate::errors::to_py_err;

#[pyclass(name = "Chisel", module = "chisel._chisel")]
pub struct PyChisel {
    // Option<> so close() can take the inner engine and drop it
    // deterministically. After close(), is_poisoned reports true and
    // (future) mutating methods will raise PoisonedError.
    inner: RefCell<Option<Chisel>>,
}

#[pyfunction]
#[pyo3(signature = (
    path = None,
    *,
    cache_size = 1024,
    create_if_missing = true,
    read_only = false,
    superblock_count = 2
))]
pub fn open(
    py: Python<'_>,
    path: Option<PyObject>,
    cache_size: usize,
    create_if_missing: bool,
    read_only: bool,
    superblock_count: u32,
) -> PyResult<PyChisel> {
    // Coerce path to PathBuf under the GIL first. Accept str fast-path
    // and fall back to os.fspath() for any os.PathLike (pathlib.Path, etc).
    let path_buf: Option<PathBuf> = match path {
        None => None,
        Some(obj) => {
            let bound = obj.bind(py);
            let s: String = if let Ok(py_str) = bound.downcast::<PyString>() {
                py_str.to_str()?.to_owned()
            } else {
                let os = py.import_bound("os")?;
                os.call_method1("fspath", (obj,))?.extract()?
            };
            Some(PathBuf::from(s))
        }
    };

    let options = Options {
        cache_size,
        create_if_missing,
        read_only,
        superblock_count,
    };

    // Engine calls can block on I/O (flock, fsync, file creation), so
    // release the GIL while they run. Chisel is Send (single-threaded
    // but owns no !Send primitives), which satisfies allow_threads.
    let result = py.allow_threads(|| -> chisel::Result<Chisel> {
        match path_buf {
            None => Chisel::open_in_memory_with_options(options),
            Some(pb) => Chisel::open(&pb, options),
        }
    });

    let engine = result.map_err(to_py_err)?;
    Ok(PyChisel {
        inner: RefCell::new(Some(engine)),
    })
}

#[pymethods]
impl PyChisel {
    fn close(&self) -> PyResult<()> {
        // Drop the engine; this releases the flock and closes the file.
        // Safe to call repeatedly — second call is a no-op.
        let _ = self.inner.borrow_mut().take();
        Ok(())
    }

    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

    fn __exit__(&self, _exc_type: PyObject, _exc: PyObject, _tb: PyObject) -> PyResult<bool> {
        self.close()?;
        Ok(false) // do not suppress exceptions
    }

    #[getter]
    fn is_poisoned(&self) -> bool {
        let guard = self.inner.borrow();
        match guard.as_ref() {
            Some(c) => c.is_poisoned(),
            None => true, // closed == poisoned from the caller's POV
        }
    }

    fn begin(&self, py: Python<'_>) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.begin())
    }

    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.commit_internal(py)
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        self.rollback_internal(py)
    }

    // `transaction()` is a factory: it calls begin() first, then
    // constructs a PyTransaction that will commit/rollback on __exit__.
    // If begin() fails (e.g., TransactionAlreadyActive), the exception
    // propagates before PyTransaction is constructed — callers never
    // see a half-alive transaction object.
    fn transaction(
        slf: Py<Self>,
        py: Python<'_>,
    ) -> PyResult<Py<crate::transaction::PyTransaction>> {
        slf.bind(py).borrow().begin(py)?;
        Py::new(py, crate::transaction::PyTransaction::new(slf))
    }

    fn allocate(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        self.allocate_internal(py, value)
    }

    fn read<'py>(
        &self,
        py: Python<'py>,
        handle: u64,
    ) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        self.read_internal(py, handle)
    }

    fn update(&self, py: Python<'_>, handle: u64, value: &Bound<'_, PyAny>) -> PyResult<()> {
        self.update_internal(py, handle, value)
    }

    fn delete(&self, py: Python<'_>, handle: u64) -> PyResult<()> {
        self.delete_internal(py, handle)
    }

    fn delete_many(&self, py: Python<'_>, handles: Vec<u64>) -> PyResult<()> {
        self.delete_many_internal(py, &handles)
    }

    fn handles(&self, py: Python<'_>) -> PyResult<Vec<u64>> {
        self.with_inner_io(py, |c| c.handles())
    }

    fn set_root_name(&self, py: Python<'_>, name: &str, handle: u64) -> PyResult<()> {
        self.set_root_name_internal(py, name, handle)
    }

    fn get_root_name(&self, py: Python<'_>, name: &str) -> PyResult<Option<u64>> {
        self.get_root_name_internal(py, name)
    }

    fn clear_root_name(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.clear_root_name_internal(py, name)
    }
}

// Internal helpers — NOT exposed to Python. PyTransaction reaches into
// these pub(crate) methods to share the same engine-access code path as
// the direct PyChisel pymethods. The with_inner_io / with_inner_mut_io
// wrappers are the ONE place the closed/poisoned check lives.
impl PyChisel {
    pub(crate) fn commit_internal(&self, py: Python<'_>) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.commit())
    }

    pub(crate) fn rollback_internal(&self, py: Python<'_>) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.rollback())
    }

    pub(crate) fn allocate_internal(
        &self,
        py: Python<'_>,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<u64> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(py, |c| c.allocate(&bytes))
    }

    pub(crate) fn read_internal<'py>(
        &self,
        py: Python<'py>,
        handle: u64,
    ) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        let data = self.with_inner_io(py, |c| c.read(handle))?;
        Ok(pyo3::types::PyBytes::new_bound(py, &data))
    }

    pub(crate) fn update_internal(
        &self,
        py: Python<'_>,
        handle: u64,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(py, |c| c.update(handle, &bytes))
    }

    pub(crate) fn delete_internal(&self, py: Python<'_>, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.delete(handle))
    }

    pub(crate) fn delete_many_internal(&self, py: Python<'_>, handles: &[u64]) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.delete_many(handles))
    }

    pub(crate) fn set_root_name_internal(
        &self,
        py: Python<'_>,
        name: &str,
        handle: u64,
    ) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.set_root_name(name, handle))
    }

    pub(crate) fn get_root_name_internal(
        &self,
        py: Python<'_>,
        name: &str,
    ) -> PyResult<Option<u64>> {
        self.with_inner_io(py, |c| c.get_root_name(name))
    }

    pub(crate) fn clear_root_name_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.clear_root_name(name))
    }

    pub(crate) fn savepoint_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.savepoint(name))
    }

    pub(crate) fn release_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.release(name))
    }

    pub(crate) fn rollback_to_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.rollback_to(name))
    }

    // These two helpers are the ONLY place the closed/poisoned
    // distinction collapses to PoisonedError — callers above never see
    // a bare None. We intentionally do NOT release the GIL around the
    // engine call: Chisel owns `Cell<bool>` internally so `&Chisel`
    // is not `Sync`, and the single-client embedded design means
    // there is no concurrent Python work to overlap with anyway (see
    // MEMORY: project_chisel_single_client_design). Python's GIL also
    // prevents concurrent re-entry into the RefCell; a borrow_mut
    // panic here would mean a genuine reentrancy bug.
    fn with_inner_io<R>(
        &self,
        _py: Python<'_>,
        f: impl FnOnce(&Chisel) -> chisel::Result<R>,
    ) -> PyResult<R> {
        let guard = self.inner.borrow();
        let c = guard
            .as_ref()
            .ok_or_else(|| to_py_err(chisel::ChiselError::Poisoned))?;
        f(c).map_err(to_py_err)
    }

    fn with_inner_mut_io<R>(
        &self,
        _py: Python<'_>,
        f: impl FnOnce(&mut Chisel) -> chisel::Result<R>,
    ) -> PyResult<R> {
        let mut guard = self.inner.borrow_mut();
        let c = guard
            .as_mut()
            .ok_or_else(|| to_py_err(chisel::ChiselError::Poisoned))?;
        f(c).map_err(to_py_err)
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChisel>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    Ok(())
}
