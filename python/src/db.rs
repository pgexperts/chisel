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
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChisel>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    Ok(())
}
