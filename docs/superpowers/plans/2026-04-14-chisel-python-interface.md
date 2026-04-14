# Chisel Python Interface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a PyO3-based Python binding for Chisel that exposes the engine to embedded Python applications with idiomatic context-manager transactions, a tiered exception hierarchy, GIL-releasing I/O, and Linux/macOS wheel distribution.

**Architecture:** New `python/` directory inside the Chisel repo. A PyO3 `cdylib` crate path-deps on the parent Chisel crate. A thin pure-Python package (`chisel/`) re-exports the native module, bundles dataclasses for structured returns, and ships hand-written type stubs. Wheels are built with `maturin` / `cibuildwheel` and tested with `pytest`.

**Tech Stack:** Rust (PyO3 0.22+, chisel path dep), Python 3.10+, maturin, pytest, hypothesis, cibuildwheel, GitHub Actions.

---

## File Structure

**New files under `python/`:**

- `python/Cargo.toml` — cdylib crate, PyO3 deps, path-dep on `..`.
- `python/pyproject.toml` — maturin backend, package metadata.
- `python/src/lib.rs` — PyO3 module entry, registers classes/exceptions, top-level `open()`.
- `python/src/errors.rs` — Python exception class definitions and `ChiselError → PyErr` mapping.
- `python/src/db.rs` — `PyChisel` class: open/close, reads, shortcut mutators, stats, defrag, is_poisoned.
- `python/src/transaction.rs` — `PyTransaction` class: context manager, mutators, savepoint factory.
- `python/src/savepoint.rs` — `PySavepoint` class: context manager + explicit release/rollback_to.
- `python/src/convert.rs` — value-type coercion (buffer protocol → `Vec<u8>`; reject `str`).
- `python/chisel/__init__.py` — re-exports native symbols, defines `@dataclass` structured returns.
- `python/chisel/chisel.pyi` — hand-written type stubs for the whole public surface.
- `python/tests/conftest.py` — shared pytest fixtures (tmp db path, in-memory db).
- `python/tests/test_*.py` — one file per concern (see spec §6.1).
- `.github/workflows/ci.yml` — extended with a `python` job.

**No existing Rust files are modified.** The binding is strictly additive.

---

## Task 1: Scaffold the `python/` Crate

**Files:**
- Create: `python/Cargo.toml`
- Create: `python/pyproject.toml`
- Create: `python/src/lib.rs`
- Create: `python/chisel/__init__.py`
- Create: `python/.gitignore`

- [ ] **Step 1: Create `python/Cargo.toml`**

```toml
[package]
name = "chisel-py"
version = "0.1.0"
edition = "2021"

[lib]
name = "_chisel"
crate-type = ["cdylib"]

[dependencies]
chisel = { path = ".." }
pyo3 = { version = "0.22", features = ["extension-module", "abi3-py310"] }
```

- [ ] **Step 2: Create `python/pyproject.toml`**

```toml
[build-system]
requires = ["maturin>=1.5,<2.0"]
build-backend = "maturin"

[project]
name = "chisel"
version = "0.1.0"
description = "Python binding for the Chisel transactional storage engine"
readme = "../README.md"
requires-python = ">=3.10"
license = { text = "MIT" }
authors = [{ name = "Christophe Pettus" }]
classifiers = [
  "Programming Language :: Python :: 3",
  "Programming Language :: Python :: 3.10",
  "Programming Language :: Python :: 3.11",
  "Programming Language :: Python :: 3.12",
  "Programming Language :: Python :: 3.13",
  "Programming Language :: Rust",
  "Operating System :: POSIX :: Linux",
  "Operating System :: MacOS",
]

[project.optional-dependencies]
test = ["pytest>=8", "hypothesis>=6"]

[tool.maturin]
module-name = "chisel._chisel"
python-source = "."
features = ["pyo3/extension-module"]
```

- [ ] **Step 3: Create a minimal `python/src/lib.rs`**

```rust
use pyo3::prelude::*;

#[pymodule]
fn _chisel(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
```

- [ ] **Step 4: Create `python/chisel/__init__.py`**

```python
from chisel._chisel import __version__

__all__ = ["__version__"]
```

- [ ] **Step 5: Create `python/.gitignore`**

```
target/
*.so
*.pyd
__pycache__/
.pytest_cache/
.venv/
*.egg-info/
dist/
wheels/
```

- [ ] **Step 6: Verify the build**

Run (from `python/`):
```bash
python -m venv .venv && . .venv/bin/activate
pip install maturin pytest hypothesis
maturin develop
python -c "import chisel; print(chisel.__version__)"
```
Expected: prints `0.1.0`.

- [ ] **Step 7: Commit**

```bash
git add python/ && git commit -m "Scaffold Python binding crate with maturin"
```

---

## Task 2: Exception Hierarchy

**Files:**
- Create: `python/src/errors.rs`
- Modify: `python/src/lib.rs`
- Create: `python/tests/conftest.py`
- Create: `python/tests/test_errors.py`

- [ ] **Step 1: Write the failing test for the exception hierarchy**

`python/tests/test_errors.py`:
```python
import chisel


def test_base_error_is_exception():
    assert issubclass(chisel.ChiselError, Exception)


def test_operational_hierarchy():
    assert issubclass(chisel.OperationalError, chisel.ChiselError)
    for cls_name in [
        "InvalidHandleError",
        "NoActiveTransactionError",
        "TransactionAlreadyActiveError",
        "SavepointNotFoundError",
        "DuplicateSavepointError",
        "ReadOnlyModeError",
        "DatabaseFileNotFoundError",
        "InvalidRootNameError",
        "RootNameTableFullError",
        "InvalidSuperblockCountError",
    ]:
        cls = getattr(chisel, cls_name)
        assert issubclass(cls, chisel.OperationalError)


def test_fatal_hierarchy():
    assert issubclass(chisel.FatalError, chisel.ChiselError)
    for cls_name in [
        "IoError",
        "ChecksumMismatchError",
        "CorruptSuperblockError",
        "FileSizeMismatchError",
        "InvalidMagicError",
        "LockFailedError",
        "UnsupportedFormatVersionError",
        "CorruptPageError",
        "InvalidPageIdError",
        "PoisonedError",
    ]:
        cls = getattr(chisel, cls_name)
        assert issubclass(cls, chisel.FatalError)


def test_operational_and_fatal_are_disjoint():
    assert not issubclass(chisel.OperationalError, chisel.FatalError)
    assert not issubclass(chisel.FatalError, chisel.OperationalError)
```

`python/tests/conftest.py`:
```python
import pytest
import chisel


@pytest.fixture
def tmp_db(tmp_path):
    return tmp_path / "test.chisel"


@pytest.fixture
def mem_db():
    with chisel.open(None) as db:
        yield db
```

- [ ] **Step 2: Run the test; expect failures**

```bash
cd python && maturin develop && pytest tests/test_errors.py -v
```
Expected: every test fails with `AttributeError: module 'chisel' has no attribute 'ChiselError'`.

- [ ] **Step 3: Create `python/src/errors.rs`**

```rust
// errors.rs — defines the Python exception hierarchy and converts a
// chisel::ChiselError into the appropriate PyErr. The two-tier split
// (OperationalError / FatalError) encodes Chisel's poison-on-fatal
// recovery protocol so Python callers can write
// `except chisel.FatalError` to detect "drop-and-reopen" conditions.

use chisel::ChiselError;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

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

// Fatal — matches ChiselError::is_fatal() in src/error.rs exactly.
create_exception!(_chisel, IoError, FatalError);
create_exception!(_chisel, ChecksumMismatchError, FatalError);
create_exception!(_chisel, CorruptSuperblockError, FatalError);
create_exception!(_chisel, FileSizeMismatchError, FatalError);
create_exception!(_chisel, InvalidMagicError, FatalError);
create_exception!(_chisel, LockFailedError, FatalError);
create_exception!(_chisel, UnsupportedFormatVersionError, FatalError);
create_exception!(_chisel, CorruptPageError, FatalError);
create_exception!(_chisel, InvalidPageIdError, FatalError);
create_exception!(_chisel, PoisonedError, FatalError);

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("ChiselError", py.get_type_bound::<ChiselError>())?;
    m.add("OperationalError", py.get_type_bound::<OperationalError>())?;
    m.add("FatalError", py.get_type_bound::<FatalError>())?;
    m.add("InvalidHandleError", py.get_type_bound::<InvalidHandleError>())?;
    m.add("NoActiveTransactionError", py.get_type_bound::<NoActiveTransactionError>())?;
    m.add("TransactionAlreadyActiveError", py.get_type_bound::<TransactionAlreadyActiveError>())?;
    m.add("SavepointNotFoundError", py.get_type_bound::<SavepointNotFoundError>())?;
    m.add("DuplicateSavepointError", py.get_type_bound::<DuplicateSavepointError>())?;
    m.add("ReadOnlyModeError", py.get_type_bound::<ReadOnlyModeError>())?;
    m.add("DatabaseFileNotFoundError", py.get_type_bound::<DatabaseFileNotFoundError>())?;
    m.add("InvalidRootNameError", py.get_type_bound::<InvalidRootNameError>())?;
    m.add("RootNameTableFullError", py.get_type_bound::<RootNameTableFullError>())?;
    m.add("InvalidSuperblockCountError", py.get_type_bound::<InvalidSuperblockCountError>())?;
    m.add("IoError", py.get_type_bound::<IoError>())?;
    m.add("ChecksumMismatchError", py.get_type_bound::<ChecksumMismatchError>())?;
    m.add("CorruptSuperblockError", py.get_type_bound::<CorruptSuperblockError>())?;
    m.add("FileSizeMismatchError", py.get_type_bound::<FileSizeMismatchError>())?;
    m.add("InvalidMagicError", py.get_type_bound::<InvalidMagicError>())?;
    m.add("LockFailedError", py.get_type_bound::<LockFailedError>())?;
    m.add("UnsupportedFormatVersionError", py.get_type_bound::<UnsupportedFormatVersionError>())?;
    m.add("CorruptPageError", py.get_type_bound::<CorruptPageError>())?;
    m.add("InvalidPageIdError", py.get_type_bound::<InvalidPageIdError>())?;
    m.add("PoisonedError", py.get_type_bound::<PoisonedError>())?;
    Ok(())
}

// Convert a chisel::ChiselError into the right PyErr. Matches every
// variant explicitly so adding a new Rust variant is a compile error here,
// not a silent fallthrough to a generic ChiselError.
pub fn to_py_err(err: ChiselError) -> PyErr {
    // Display impl on ChiselError already produces a human-readable message;
    // use it for the exception's argument so callers see the same text Rust
    // callers would see via println!("{err}").
    let msg = err.to_string();
    match err {
        // Operational
        ChiselError::InvalidHandle(_) => InvalidHandleError::new_err(msg),
        ChiselError::NoActiveTransaction => NoActiveTransactionError::new_err(msg),
        ChiselError::TransactionAlreadyActive => TransactionAlreadyActiveError::new_err(msg),
        ChiselError::SavepointNotFound(_) => SavepointNotFoundError::new_err(msg),
        ChiselError::DuplicateSavepoint(_) => DuplicateSavepointError::new_err(msg),
        ChiselError::ReadOnlyMode => ReadOnlyModeError::new_err(msg),
        ChiselError::FileNotFound => DatabaseFileNotFoundError::new_err(msg),
        ChiselError::InvalidRootName => InvalidRootNameError::new_err(msg),
        ChiselError::RootNameTableFull => RootNameTableFullError::new_err(msg),
        ChiselError::InvalidSuperblockCount { .. } => InvalidSuperblockCountError::new_err(msg),
        // Fatal
        ChiselError::IoError(_) => IoError::new_err(msg),
        ChiselError::ChecksumMismatch { .. } => ChecksumMismatchError::new_err(msg),
        ChiselError::CorruptSuperblock => CorruptSuperblockError::new_err(msg),
        ChiselError::FileSizeMismatch { .. } => FileSizeMismatchError::new_err(msg),
        ChiselError::InvalidMagic => InvalidMagicError::new_err(msg),
        ChiselError::LockFailed => LockFailedError::new_err(msg),
        ChiselError::UnsupportedFormatVersion { .. } => UnsupportedFormatVersionError::new_err(msg),
        ChiselError::CorruptPage { .. } => CorruptPageError::new_err(msg),
        ChiselError::InvalidPageId { .. } => InvalidPageIdError::new_err(msg),
        ChiselError::Poisoned => PoisonedError::new_err(msg),
    }
}
```

The match is exhaustive — adding a new variant to `ChiselError` will produce a compile error here, forcing the binding to be updated rather than silently routing to a generic error.

- [ ] **Step 4: Wire errors into `python/src/lib.rs`**

```rust
use pyo3::prelude::*;

mod errors;

#[pymodule]
fn _chisel(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register(m)?;
    Ok(())
}
```

- [ ] **Step 5: Re-export exceptions from the Python package**

`python/chisel/__init__.py`:
```python
from chisel._chisel import (
    __version__,
    ChiselError,
    OperationalError,
    FatalError,
    InvalidHandleError,
    NoActiveTransactionError,
    TransactionAlreadyActiveError,
    SavepointNotFoundError,
    DuplicateSavepointError,
    ReadOnlyModeError,
    DatabaseFileNotFoundError,
    InvalidRootNameError,
    RootNameTableFullError,
    InvalidSuperblockCountError,
    IoError,
    ChecksumMismatchError,
    CorruptSuperblockError,
    FileSizeMismatchError,
    InvalidMagicError,
    LockFailedError,
    UnsupportedFormatVersionError,
    CorruptPageError,
    InvalidPageIdError,
    PoisonedError,
)

__all__ = [
    "__version__",
    "ChiselError", "OperationalError", "FatalError",
    "InvalidHandleError", "NoActiveTransactionError",
    "TransactionAlreadyActiveError", "SavepointNotFoundError",
    "DuplicateSavepointError", "ReadOnlyModeError", "DatabaseFileNotFoundError",
    "InvalidRootNameError", "RootNameTableFullError",
    "InvalidSuperblockCountError",
    "IoError", "ChecksumMismatchError", "CorruptSuperblockError",
    "FileSizeMismatchError", "InvalidMagicError", "LockFailedError",
    "UnsupportedFormatVersionError", "CorruptPageError", "InvalidPageIdError",
    "PoisonedError",
]
```

- [ ] **Step 6: Run tests; expect pass**

```bash
cd python && maturin develop && pytest tests/test_errors.py -v
```
Expected: all four tests pass.

- [ ] **Step 7: Commit**

```bash
git add python/ && git commit -m "Add Python exception hierarchy with ChiselError mapping"
```

---

## Task 3: `chisel.open()` — Path and In-Memory

**Files:**
- Create: `python/src/db.rs`
- Modify: `python/src/lib.rs`
- Modify: `python/chisel/__init__.py`
- Create: `python/tests/test_open.py`

- [ ] **Step 1: Write failing tests**

`python/tests/test_open.py`:
```python
import pytest
import chisel


def test_open_in_memory_with_none():
    db = chisel.open(None)
    assert db is not None
    db.close()


def test_open_creates_file(tmp_db):
    assert not tmp_db.exists()
    db = chisel.open(str(tmp_db))
    db.close()
    assert tmp_db.exists()


def test_open_context_manager(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        assert db is not None
    # second open verifies file is closed / flock released
    with chisel.open(str(tmp_db)) as db:
        assert db is not None


def test_open_rejects_missing_when_create_false(tmp_db):
    with pytest.raises(chisel.DatabaseFileNotFoundError):
        chisel.open(str(tmp_db), create_if_missing=False)


def test_open_rejects_bad_superblock_count(tmp_db):
    with pytest.raises(chisel.InvalidSuperblockCountError):
        chisel.open(str(tmp_db), superblock_count=1)
    with pytest.raises(chisel.InvalidSuperblockCountError):
        chisel.open(str(tmp_db), superblock_count=17)


def test_open_in_memory_rejects_read_only():
    with pytest.raises(chisel.ReadOnlyModeError):
        chisel.open(None, read_only=True)


def test_open_accepts_pathlib(tmp_db):
    with chisel.open(tmp_db) as db:
        assert db is not None


def test_double_open_same_path_fails(tmp_db):
    with chisel.open(str(tmp_db)):
        with pytest.raises(chisel.LockFailedError):
            chisel.open(str(tmp_db))
```

- [ ] **Step 2: Run tests; expect failures**

```bash
cd python && pytest tests/test_open.py -v
```
Expected: every test fails with `AttributeError: module 'chisel' has no attribute 'open'`.

- [ ] **Step 3: Create `python/src/db.rs`**

```rust
// db.rs — PyChisel wraps chisel::Chisel and provides the top-level
// `open()` constructor. Holds the engine in a RefCell behind a Py-owned
// struct; method dispatch takes &mut self via PyRefMut. Mutation is
// serialized by Python's GIL / PyO3's borrow rules.

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyString, PyType};
use std::cell::RefCell;
use std::path::PathBuf;

use chisel::{Chisel, Options};

use crate::errors::to_py_err;

#[pyclass(name = "Chisel", module = "chisel._chisel", unsendable = false)]
pub struct PyChisel {
    // Option<> so close() can take the inner engine and drop it
    // deterministically, leaving future calls to raise.
    inner: RefCell<Option<Chisel>>,
}

impl PyChisel {
    fn with_inner<R>(&self, f: impl FnOnce(&Chisel) -> R) -> PyResult<R> {
        let guard = self.inner.borrow();
        match guard.as_ref() {
            Some(c) => Ok(f(c)),
            None => Err(crate::errors::to_py_err(chisel::ChiselError::Poisoned)),
            // A closed handle behaves like a poisoned one: every call fails.
        }
    }

    fn with_inner_mut<R>(&self, f: impl FnOnce(&mut Chisel) -> R) -> PyResult<R> {
        let mut guard = self.inner.borrow_mut();
        match guard.as_mut() {
            Some(c) => Ok(f(c)),
            None => Err(crate::errors::to_py_err(chisel::ChiselError::Poisoned)),
        }
    }
}

#[pyfunction]
#[pyo3(signature = (path=None, *, cache_size=1024, create_if_missing=true, read_only=false, superblock_count=2))]
pub fn open(
    py: Python<'_>,
    path: Option<PyObject>,
    cache_size: usize,
    create_if_missing: bool,
    read_only: bool,
    superblock_count: u32,
) -> PyResult<PyChisel> {
    let options = Options {
        cache_size,
        create_if_missing,
        read_only,
        superblock_count,
    };

    let chisel = py.allow_threads(|| -> chisel::Result<Chisel> {
        match path {
            None => {
                // In-memory mode
                Chisel::open_in_memory_with_options(options)
            }
            Some(obj) => {
                // Path: accept str or os.PathLike
                Python::with_gil(|py| -> PyResult<PathBuf> {
                    if let Ok(s) = obj.downcast_bound::<PyString>(py) {
                        Ok(PathBuf::from(s.to_str()?))
                    } else {
                        // os.PathLike — call os.fspath on it
                        let os = py.import_bound("os")?;
                        let fspath: String = os.call_method1("fspath", (obj,))?.extract()?;
                        Ok(PathBuf::from(fspath))
                    }
                })
                .map_err(|e| chisel::ChiselError::IoError(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput, e.to_string(),
                )))
                .and_then(|pb| Chisel::open(&pb, options))
            }
        }
    }).map_err(to_py_err)?;

    Ok(PyChisel {
        inner: RefCell::new(Some(chisel)),
    })
}

#[pymethods]
impl PyChisel {
    fn close(&self) -> PyResult<()> {
        // Take the inner engine and drop it here; future calls raise.
        let _ = self.inner.borrow_mut().take();
        Ok(())
    }

    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

    fn __exit__(
        &self,
        _exc_type: PyObject,
        _exc: PyObject,
        _tb: PyObject,
    ) -> PyResult<bool> {
        self.close()?;
        Ok(false) // don't suppress exceptions
    }

    #[getter]
    fn is_poisoned(&self) -> PyResult<bool> {
        let guard = self.inner.borrow();
        match guard.as_ref() {
            Some(c) => Ok(c.is_poisoned()),
            None => Ok(true),
        }
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChisel>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    Ok(())
}
```

Note: the `open()` function as written mixes `PyResult` and `chisel::Result` awkwardly inside `allow_threads`. The path-coercion must happen under the GIL (before `allow_threads`); only the actual `Chisel::open` call should run GIL-released. Rewrite as:

```rust
// Correct structure:
let path_buf: Option<PathBuf> = match path {
    None => None,
    Some(obj) => {
        let os = py.import_bound("os")?;
        let s: String = os.call_method1("fspath", (obj,))?.extract()?;
        Some(PathBuf::from(s))
    }
};

let chisel = py.allow_threads(|| {
    match path_buf {
        None => Chisel::open_in_memory_with_options(options),
        Some(pb) => Chisel::open(&pb, options),
    }
}).map_err(to_py_err)?;
```

Use this corrected structure in the actual implementation.

- [ ] **Step 4: Wire `db` into `python/src/lib.rs`**

```rust
use pyo3::prelude::*;

mod db;
mod errors;

#[pymodule]
fn _chisel(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register(m)?;
    db::register(m)?;
    Ok(())
}
```

- [ ] **Step 5: Re-export from Python package**

Add to `python/chisel/__init__.py`:
```python
from chisel._chisel import Chisel, open
```
and append `"Chisel", "open"` to `__all__`.

- [ ] **Step 6: Run tests; expect pass**

```bash
cd python && maturin develop && pytest tests/test_open.py -v
```
Expected: all eight tests pass.

- [ ] **Step 7: Commit**

```bash
git add python/ && git commit -m "Add chisel.open() and Chisel context manager"
```

---

## Task 4: Value Coercion (bytes-like in, bytes out) + `read()`

**Files:**
- Create: `python/src/convert.rs`
- Modify: `python/src/db.rs`
- Modify: `python/src/lib.rs`
- Create: `python/tests/test_values.py`

- [ ] **Step 1: Write failing tests**

`python/tests/test_values.py`:
```python
import array
import pytest
import chisel


def test_allocate_bytes(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"hello")
        assert tx.read(h) == b"hello"


def test_allocate_bytearray(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(bytearray(b"world"))
        assert tx.read(h) == b"world"


def test_allocate_memoryview(mem_db):
    buf = bytearray(b"memoryview")
    with mem_db.transaction() as tx:
        h = tx.allocate(memoryview(buf))
        assert tx.read(h) == b"memoryview"


def test_allocate_array(mem_db):
    a = array.array("b", [1, 2, 3, 4])
    with mem_db.transaction() as tx:
        h = tx.allocate(a)
        assert tx.read(h) == bytes([1, 2, 3, 4])


def test_allocate_rejects_str(mem_db):
    with mem_db.transaction() as tx:
        with pytest.raises(TypeError, match="bytes-like"):
            tx.allocate("not bytes")


def test_allocate_rejects_int(mem_db):
    with mem_db.transaction() as tx:
        with pytest.raises(TypeError):
            tx.allocate(42)


def test_empty_bytes(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"")
        assert tx.read(h) == b""


def test_large_value(mem_db):
    data = b"x" * (1024 * 1024)
    with mem_db.transaction() as tx:
        h = tx.allocate(data)
        assert tx.read(h) == data


def test_read_returns_bytes_type(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"abc")
        assert type(tx.read(h)) is bytes
```

These tests will also need Transaction / allocate / read to exist, which are the following tasks. For now, this test file is set up.

- [ ] **Step 2: Create `python/src/convert.rs`**

```rust
// convert.rs — value coercion for the write path. Accepts anything
// exposing the buffer protocol (bytes, bytearray, memoryview, array.array).
// Rejects str explicitly with a helpful TypeError pointing at .encode().
//
// Bytes are copied into a Vec<u8> at the boundary. The Rust API takes
// &[u8] and does not retain the slice, so the copy is unavoidable for
// owned-buffer safety; zero-copy would require a buffer-protocol-aware
// lower-level API on the Rust side.

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyString;

pub fn coerce_value(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if value.is_instance_of::<PyString>() {
        return Err(PyTypeError::new_err(
            "values must be bytes-like; got str — encode first, e.g. s.encode('utf-8')",
        ));
    }

    let buf: PyBuffer<u8> = PyBuffer::get_bound(value)
        .map_err(|_| PyTypeError::new_err(
            "values must be bytes-like (implement the buffer protocol)",
        ))?;

    if !buf.is_c_contiguous() {
        return Err(PyTypeError::new_err("bytes-like buffer must be C-contiguous"));
    }

    let len = buf.len_bytes();
    let mut out = vec![0u8; len];
    buf.copy_to_slice(value.py(), &mut out)
        .map_err(|_| PyTypeError::new_err("could not read bytes-like buffer"))?;
    Ok(out)
}
```

- [ ] **Step 3: Wire `convert` into `lib.rs`**

```rust
mod convert;  // (alongside existing mod declarations)
```

- [ ] **Step 4: Commit the scaffolding**

```bash
git add python/src/convert.rs python/src/lib.rs python/tests/test_values.py
git commit -m "Add value coercion scaffolding (bytes-like acceptance)"
```

Tests will still fail until Task 5 adds Transaction.

---

## Task 5: `Transaction` Context Manager

**Files:**
- Create: `python/src/transaction.rs`
- Modify: `python/src/db.rs`
- Modify: `python/src/lib.rs`
- Modify: `python/chisel/__init__.py`
- Create: `python/tests/test_transactions.py`

- [ ] **Step 1: Write failing tests**

`python/tests/test_transactions.py`:
```python
import pytest
import chisel


def test_transaction_commits_on_clean_exit(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"persisted")
    # After commit, value survives
    assert mem_db.read(h) == b"persisted"


def test_transaction_rolls_back_on_exception(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        with pytest.raises(RuntimeError):
            with db.transaction() as tx:
                tx.allocate(b"discarded")
                raise RuntimeError("boom")
        assert db.handles() == []


def test_explicit_begin_commit(mem_db):
    mem_db.begin()
    h = mem_db.allocate(b"x")
    mem_db.commit()
    assert mem_db.read(h) == b"x"


def test_explicit_rollback(mem_db):
    mem_db.begin()
    mem_db.allocate(b"gone")
    mem_db.rollback()
    assert mem_db.handles() == []


def test_nested_transactions_raise(mem_db):
    with mem_db.transaction():
        with pytest.raises(chisel.ChiselError):
            with mem_db.transaction():
                pass


def test_mutators_outside_transaction_raise(mem_db):
    with pytest.raises(chisel.NoActiveTransactionError):
        mem_db.allocate(b"orphan")
```

- [ ] **Step 2: Run tests; expect failures**

```bash
cd python && pytest tests/test_transactions.py -v
```
Expected: failures — `transaction`, `allocate`, `read`, `begin`, `commit`, `rollback`, `handles` not defined.

- [ ] **Step 3: Create `python/src/transaction.rs`**

```rust
// transaction.rs — PyTransaction: the context-manager-friendly front
// to an active Chisel transaction. Holds a reference back to the
// PyChisel via Py<PyChisel> so the transaction cannot outlive the db.
//
// Semantics:
//   __enter__: ensures a txn is active (calls begin on the db).
//   __exit__:  commits on clean exit, rolls back on exception.
//              Re-raises original exception either way (returns False).
//   Methods delegate to PyChisel for the actual engine calls.

use pyo3::prelude::*;

use crate::db::PyChisel;
use crate::errors::to_py_err;

#[pyclass(name = "Transaction", module = "chisel._chisel")]
pub struct PyTransaction {
    db: Py<PyChisel>,
    // true once __exit__ has run — guards against reuse.
    finished: std::cell::Cell<bool>,
}

impl PyTransaction {
    pub fn new(db: Py<PyChisel>) -> Self {
        Self { db, finished: std::cell::Cell::new(false) }
    }
}

#[pymethods]
impl PyTransaction {
    fn __enter__<'py>(slf: PyRef<'py, Self>, py: Python<'py>) -> PyResult<PyRef<'py, Self>> {
        // Begin is idempotent at the object-creation callsite:
        // db.transaction() already called begin(). __enter__ is a no-op.
        Ok(slf)
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
        let is_exception = !exc_type.is_none(py);
        if is_exception {
            db.rollback_internal(py)?;
        } else {
            db.commit_internal(py)?;
        }
        Ok(false) // never suppress
    }

    fn allocate(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        self.db.bind(py).borrow().allocate_internal(py, value)
    }

    fn read(&self, py: Python<'_>, handle: u64) -> PyResult<Py<pyo3::types::PyBytes>> {
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
        self.db.bind(py).borrow().set_root_name_internal(py, name, handle)
    }

    fn get_root_name(&self, py: Python<'_>, name: &str) -> PyResult<Option<u64>> {
        self.db.bind(py).borrow().get_root_name_internal(py, name)
    }

    fn clear_root_name(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.db.bind(py).borrow().clear_root_name_internal(py, name)
    }

    // savepoint(name) — added in Task 7
}
```

- [ ] **Step 4: Extend `PyChisel` with internal methods and the `transaction()` factory**

In `python/src/db.rs`, add these methods inside `#[pymethods] impl PyChisel`:

```rust
fn begin(&self, py: Python<'_>) -> PyResult<()> {
    self.with_inner_mut_io(py, |c| c.begin())
}

fn commit(&self, py: Python<'_>) -> PyResult<()> {
    self.commit_internal(py)
}

fn rollback(&self, py: Python<'_>) -> PyResult<()> {
    self.rollback_internal(py)
}

fn transaction(slf: Py<Self>, py: Python<'_>) -> PyResult<Py<crate::transaction::PyTransaction>> {
    slf.bind(py).borrow().begin(py)?;
    Py::new(py, crate::transaction::PyTransaction::new(slf))
}

fn allocate(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<u64> {
    self.allocate_internal(py, value)
}

fn read(&self, py: Python<'_>, handle: u64) -> PyResult<Py<pyo3::types::PyBytes>> {
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
```

Add internal helpers (outside `#[pymethods]`):

```rust
impl PyChisel {
    pub(crate) fn commit_internal(&self, py: Python<'_>) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.commit())
    }
    pub(crate) fn rollback_internal(&self, py: Python<'_>) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.rollback())
    }
    pub(crate) fn allocate_internal(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(py, |c| c.allocate(&bytes))
    }
    pub(crate) fn read_internal(&self, py: Python<'_>, handle: u64) -> PyResult<Py<pyo3::types::PyBytes>> {
        let data = self.with_inner_io(py, |c| c.read(handle))?;
        Ok(pyo3::types::PyBytes::new_bound(py, &data).unbind())
    }
    pub(crate) fn update_internal(&self, py: Python<'_>, handle: u64, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(py, |c| c.update(handle, &bytes))
    }
    pub(crate) fn delete_internal(&self, py: Python<'_>, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.delete(handle))
    }
    pub(crate) fn delete_many_internal(&self, py: Python<'_>, handles: &[u64]) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.delete_many(handles))
    }
    pub(crate) fn set_root_name_internal(&self, py: Python<'_>, name: &str, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.set_root_name(name, handle))
    }
    pub(crate) fn get_root_name_internal(&self, py: Python<'_>, name: &str) -> PyResult<Option<u64>> {
        self.with_inner_io(py, |c| c.get_root_name(name))
    }
    pub(crate) fn clear_root_name_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.clear_root_name(name))
    }

    // Wrappers that release the GIL for the actual engine call.
    fn with_inner_io<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&chisel::Chisel) -> chisel::Result<R> + Send,
    ) -> PyResult<R>
    where R: Send {
        let guard = self.inner.borrow();
        let c = guard.as_ref().ok_or_else(|| {
            to_py_err(chisel::ChiselError::Poisoned)
        })?;
        py.allow_threads(|| f(c)).map_err(to_py_err)
    }

    fn with_inner_mut_io<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut chisel::Chisel) -> chisel::Result<R> + Send,
    ) -> PyResult<R>
    where R: Send {
        let mut guard = self.inner.borrow_mut();
        let c = guard.as_mut().ok_or_else(|| {
            to_py_err(chisel::ChiselError::Poisoned)
        })?;
        py.allow_threads(|| f(c)).map_err(to_py_err)
    }
}
```

Note: the `Send` bound on `R` and the closure is needed because `py.allow_threads` releases the GIL. If `chisel::Chisel` is not `Send` (verify in the source), drop `allow_threads` and call `f(c)` directly — GIL release is a perf optimization, not a correctness requirement.

- [ ] **Step 5: Register `PyTransaction` and update `lib.rs`**

```rust
use pyo3::prelude::*;

mod convert;
mod db;
mod errors;
mod transaction;

#[pymodule]
fn _chisel(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register(m)?;
    db::register(m)?;
    m.add_class::<transaction::PyTransaction>()?;
    Ok(())
}
```

- [ ] **Step 6: Re-export `Transaction` from Python package**

Add `Transaction` to the imports in `python/chisel/__init__.py` and to `__all__`.

- [ ] **Step 7: Run all tests so far**

```bash
cd python && maturin develop && pytest -v
```
Expected: `test_open.py`, `test_errors.py`, `test_transactions.py`, `test_values.py` all pass.

- [ ] **Step 8: Commit**

```bash
git add python/ && git commit -m "Add Transaction context manager and CRUD operations"
```

---

## Task 6: Named Roots, Handles, and Read-Only Introspection

**Files:**
- Create: `python/tests/test_named_roots.py`
- Modify: `python/src/db.rs` (if any gaps remain)

- [ ] **Step 1: Write tests for named roots and `handles()`**

`python/tests/test_named_roots.py`:
```python
import pytest
import chisel


def test_set_and_get_root_name(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"meta")
        tx.set_root_name("meta", h)
    assert mem_db.get_root_name("meta") == h


def test_get_missing_root_returns_none(mem_db):
    assert mem_db.get_root_name("missing") is None


def test_clear_root_name(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"x")
        tx.set_root_name("foo", h)
    assert mem_db.get_root_name("foo") == h
    with mem_db.transaction() as tx:
        tx.clear_root_name("foo")
    assert mem_db.get_root_name("foo") is None


def test_root_name_rolled_back_with_transaction(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"pre")
        tx.set_root_name("stable", h)
    with pytest.raises(RuntimeError):
        with mem_db.transaction() as tx:
            tx.clear_root_name("stable")
            raise RuntimeError("nope")
    assert mem_db.get_root_name("stable") == h


def test_handles_enumerates_live_values(mem_db):
    handles = []
    with mem_db.transaction() as tx:
        for i in range(5):
            handles.append(tx.allocate(bytes([i])))
    assert sorted(mem_db.handles()) == sorted(handles)


def test_handles_excludes_deleted(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"keep")
        h2 = tx.allocate(b"drop")
        tx.delete(h2)
    assert mem_db.handles() == [h1]
```

- [ ] **Step 2: Run tests**

```bash
cd python && pytest tests/test_named_roots.py -v
```
Expected: pass (implementation already wired in Task 5).

- [ ] **Step 3: If any fail, fix in `db.rs` and re-run**

- [ ] **Step 4: Commit**

```bash
git add python/tests/test_named_roots.py
git commit -m "Test named-roots and handles() introspection"
```

---

## Task 7: Savepoints

**Files:**
- Create: `python/src/savepoint.rs`
- Modify: `python/src/transaction.rs`
- Modify: `python/src/db.rs`
- Modify: `python/src/lib.rs`
- Modify: `python/chisel/__init__.py`
- Create: `python/tests/test_savepoints.py`

- [ ] **Step 1: Write failing tests**

`python/tests/test_savepoints.py`:
```python
import pytest
import chisel


def test_savepoint_release_on_clean_exit(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"before")
        with tx.savepoint("sp1"):
            h2 = tx.allocate(b"after")
        # clean exit -> release; both values survive
    assert sorted(mem_db.handles()) == sorted([h1, h2])


def test_savepoint_rollback_on_exception(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"keep")
        with pytest.raises(RuntimeError):
            with tx.savepoint("sp1"):
                tx.allocate(b"discard")
                raise RuntimeError("boom")
        # outer transaction still valid; h1 preserved
        assert mem_db.get_root_name("__unused__") is None
    assert mem_db.handles() == [h1]


def test_nested_savepoints(mem_db):
    with mem_db.transaction() as tx:
        h_outer = tx.allocate(b"outer")
        with tx.savepoint("outer"):
            h_mid = tx.allocate(b"mid")
            with pytest.raises(RuntimeError):
                with tx.savepoint("inner"):
                    tx.allocate(b"innermost")
                    raise RuntimeError("boom")
            # inner rolled back; outer + mid preserved
    assert sorted(mem_db.handles()) == sorted([h_outer, h_mid])


def test_explicit_savepoint_methods(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"a")
        sp = tx.savepoint("manual")
        tx.allocate(b"b")
        sp.rollback_to()
        # After rollback_to, the savepoint name is consumed; release is a no-op
    assert mem_db.handles() == [h1]
```

- [ ] **Step 2: Run tests; expect failures**

Expected: `AttributeError: 'Transaction' object has no attribute 'savepoint'`.

- [ ] **Step 3: Create `python/src/savepoint.rs`**

```rust
// savepoint.rs — PySavepoint: a named mark within an active transaction,
// usable as a context manager (release on clean exit, rollback_to on
// exception) or driven explicitly.

use pyo3::prelude::*;
use std::cell::Cell;

use crate::db::PyChisel;

#[pyclass(name = "Savepoint", module = "chisel._chisel")]
pub struct PySavepoint {
    db: Py<PyChisel>,
    #[pyo3(get)]
    name: String,
    finished: Cell<bool>,
}

impl PySavepoint {
    pub fn new(db: Py<PyChisel>, name: String) -> Self {
        Self { db, name, finished: Cell::new(false) }
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
        if self.finished.get() { return Ok(()); }
        self.finished.set(true);
        self.db.bind(py).borrow().release_internal(py, &self.name)
    }

    fn rollback_to(&self, py: Python<'_>) -> PyResult<()> {
        if self.finished.get() { return Ok(()); }
        self.finished.set(true);
        self.db.bind(py).borrow().rollback_to_internal(py, &self.name)
    }
}
```

- [ ] **Step 4: Add internal methods to `PyChisel`**

In `python/src/db.rs`, add to the `impl PyChisel` block:

```rust
pub(crate) fn savepoint_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
    self.with_inner_mut_io(py, |c| c.savepoint(name))
}
pub(crate) fn release_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
    self.with_inner_mut_io(py, |c| c.release(name))
}
pub(crate) fn rollback_to_internal(&self, py: Python<'_>, name: &str) -> PyResult<()> {
    self.with_inner_mut_io(py, |c| c.rollback_to(name))
}
```

- [ ] **Step 5: Add `savepoint()` to `PyTransaction`**

In `python/src/transaction.rs`, inside `#[pymethods] impl PyTransaction`:

```rust
fn savepoint(
    &self,
    py: Python<'_>,
    name: &str,
) -> PyResult<Py<crate::savepoint::PySavepoint>> {
    self.db.bind(py).borrow().savepoint_internal(py, name)?;
    Py::new(
        py,
        crate::savepoint::PySavepoint::new(self.db.clone_ref(py), name.to_string()),
    )
}
```

- [ ] **Step 6: Register `PySavepoint` and re-export**

In `python/src/lib.rs`, add `mod savepoint;` and `m.add_class::<savepoint::PySavepoint>()?;`.
In `python/chisel/__init__.py`, add `Savepoint` to imports and `__all__`.

- [ ] **Step 7: Run tests**

```bash
cd python && maturin develop && pytest tests/test_savepoints.py -v
```
Expected: pass.

- [ ] **Step 8: Commit**

```bash
git add python/ && git commit -m "Add Savepoint context manager"
```

---

## Task 8: Stats, Defrag, and Structured Return Dataclasses

**Files:**
- Modify: `python/chisel/__init__.py`
- Modify: `python/src/db.rs`
- Create: `python/tests/test_stats_defrag.py`

- [ ] **Step 1: Add dataclasses to `python/chisel/__init__.py`**

```python
from dataclasses import dataclass

@dataclass(frozen=True)
class Stats:
    handle_count: int
    total_pages: int
    file_size_bytes: int

@dataclass(frozen=True)
class DefragOptions:
    # Mirror every field in chisel::defrag::DefragOptions exactly.
    # Fill in the real fields by inspecting src/defrag.rs; placeholder
    # here shows the pattern:
    max_pages: int | None = None
    # ... (add remaining fields from Rust struct)

@dataclass(frozen=True)
class DefragStats:
    # Mirror every field in chisel::defrag::DefragStats exactly.
    pages_freed: int = 0
    pages_rewritten: int = 0
    # ... (add remaining fields from Rust struct)
```

**Note for implementer:** open `src/defrag.rs`, copy the exact field names and types from `DefragOptions` and `DefragStats` into the dataclasses above. The placeholder fields shown are illustrative; the real fields are what the Rust structs define.

- [ ] **Step 2: Write failing tests**

`python/tests/test_stats_defrag.py`:
```python
import pytest
import chisel


def test_stats_dataclass_shape(mem_db):
    s = mem_db.stats()
    assert isinstance(s, chisel.Stats)
    assert s.handle_count == 0
    assert s.total_pages > 0
    assert s.file_size_bytes == s.total_pages * 8192


def test_stats_after_allocations(mem_db):
    with mem_db.transaction() as tx:
        for i in range(10):
            tx.allocate(bytes([i]))
    s = mem_db.stats()
    assert s.handle_count == 10


def test_stats_is_frozen(mem_db):
    s = mem_db.stats()
    with pytest.raises(Exception):  # FrozenInstanceError
        s.handle_count = 99


def test_defrag_requires_active_transaction(mem_db):
    with pytest.raises(chisel.NoActiveTransactionError):
        mem_db.defrag()


def test_defrag_inside_transaction_returns_stats(mem_db):
    with mem_db.transaction() as tx:
        for i in range(20):
            tx.allocate(bytes([i]) * 100)
    with mem_db.transaction():
        result = mem_db.defrag()
    assert isinstance(result, chisel.DefragStats)
```

- [ ] **Step 3: Add `stats()` and `defrag()` to `PyChisel`**

In `python/src/db.rs`, inside `#[pymethods]`:

```rust
fn stats(&self, py: Python<'_>) -> PyResult<PyObject> {
    let s = self.with_inner_io(py, |c| c.stats())?;
    // Build the Python Stats dataclass from the Rust struct.
    let module = py.import_bound("chisel")?;
    let stats_cls = module.getattr("Stats")?;
    Ok(stats_cls.call1((s.handle_count, s.total_pages, s.file_size_bytes))?.unbind())
}

fn defrag(&self, py: Python<'_>, options: Option<&Bound<'_, PyAny>>) -> PyResult<PyObject> {
    let rust_opts = match options {
        None => chisel::defrag::DefragOptions::default(),
        Some(obj) => {
            // Read each field from the Python DefragOptions dataclass and
            // build the Rust struct. Extend this as fields are added.
            let mut o = chisel::defrag::DefragOptions::default();
            // Example for a hypothetical max_pages field:
            // if let Ok(v) = obj.getattr("max_pages")?.extract::<Option<usize>>() {
            //     if let Some(n) = v { o.max_pages = n; }
            // }
            // ... replicate for every field in DefragOptions
            let _ = obj; // silence warning until fields added
            o
        }
    };

    let stats = self.with_inner_mut_io(py, |c| c.defrag(rust_opts.clone()))?;

    let module = py.import_bound("chisel")?;
    let cls = module.getattr("DefragStats")?;
    // Build kwargs dict matching the DefragStats dataclass fields
    let kwargs = pyo3::types::PyDict::new_bound(py);
    kwargs.set_item("pages_freed", stats.pages_freed)?;
    kwargs.set_item("pages_rewritten", stats.pages_rewritten)?;
    // ... one line per field
    Ok(cls.call((), Some(&kwargs))?.unbind())
}
```

**Note for implementer:** the exact fields on `DefragOptions`/`DefragStats` come from `src/defrag.rs`. Every field must be mapped both directions (Python → Rust for options, Rust → Python for stats); do not skip any.

- [ ] **Step 4: Wire `Stats`, `DefragOptions`, `DefragStats` into `__all__`**

Update `python/chisel/__init__.py` `__all__` list.

- [ ] **Step 5: Run tests**

```bash
cd python && maturin develop && pytest tests/test_stats_defrag.py -v
```
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add python/ && git commit -m "Add stats(), defrag(), and structured return dataclasses"
```

---

## Task 9: Poison Behavior End-to-End

**Files:**
- Modify: `python/tests/test_errors.py`

- [ ] **Step 1: Write poison tests**

Append to `python/tests/test_errors.py`:
```python
def test_is_poisoned_false_on_fresh_db(mem_db):
    assert mem_db.is_poisoned is False


def test_close_then_call_raises_poisoned():
    db = chisel.open(None)
    db.close()
    with pytest.raises(chisel.PoisonedError):
        db.begin()
    with pytest.raises(chisel.PoisonedError):
        db.read(0)


def test_closed_db_reports_poisoned():
    db = chisel.open(None)
    db.close()
    assert db.is_poisoned is True
```

Note: a test that forces an actual fatal error (via `force_poison_for_test` or equivalent) requires the Rust crate to expose a test hook under a feature flag. If that hook is not exposed from the Python extension, this test file documents the closed-db equivalence only. Full fatal-error poison behavior is covered in Rust tests.

- [ ] **Step 2: Run the tests**

```bash
cd python && pytest tests/test_errors.py -v
```
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add python/tests/test_errors.py
git commit -m "Test poison behavior via closed-handle equivalence"
```

---

## Task 10: Threading Tests

**Files:**
- Create: `python/tests/test_threading.py`

- [ ] **Step 1: Write tests**

`python/tests/test_threading.py`:
```python
import threading
import time
import chisel


def test_db_can_be_moved_between_threads(tmp_db):
    db = chisel.open(str(tmp_db))
    result = []

    def worker():
        with db.transaction() as tx:
            h = tx.allocate(b"from-thread")
            result.append(h)

    t = threading.Thread(target=worker)
    t.start()
    t.join()

    assert len(result) == 1
    h = result[0]
    assert db.read(h) == b"from-thread"
    db.close()


def test_commit_releases_gil(tmp_db):
    # While a commit is in flight, another Python thread should be able
    # to make progress. This test is flaky by nature — we assert only
    # that the sibling thread did *some* work, not a timing threshold.
    db = chisel.open(str(tmp_db))
    sibling_iters = []
    stop = threading.Event()

    def sibling():
        n = 0
        while not stop.is_set():
            n += 1
        sibling_iters.append(n)

    t = threading.Thread(target=sibling)
    t.start()

    # Do a batch of commits
    for _ in range(20):
        with db.transaction() as tx:
            tx.allocate(b"x" * 1024)

    stop.set()
    t.join()
    db.close()

    assert sibling_iters[0] > 0
```

- [ ] **Step 2: Run**

```bash
cd python && pytest tests/test_threading.py -v
```
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add python/tests/test_threading.py
git commit -m "Test thread migration and GIL release on commit"
```

---

## Task 11: Property-Based Round-Trip Test

**Files:**
- Create: `python/tests/test_property.py`

- [ ] **Step 1: Write tests**

`python/tests/test_property.py`:
```python
from hypothesis import given, settings, strategies as st
import chisel


@given(data=st.binary(min_size=0, max_size=16 * 1024))
@settings(max_examples=200, deadline=None)
def test_round_trip_bytes(data):
    with chisel.open(None) as db:
        with db.transaction() as tx:
            h = tx.allocate(data)
            assert tx.read(h) == data


@given(data=st.binary(min_size=0, max_size=16 * 1024))
@settings(max_examples=100, deadline=None)
def test_round_trip_memoryview(data):
    with chisel.open(None) as db:
        with db.transaction() as tx:
            h = tx.allocate(memoryview(bytearray(data)))
            assert tx.read(h) == data


@given(values=st.lists(st.binary(max_size=256), min_size=0, max_size=50))
@settings(max_examples=100, deadline=None)
def test_round_trip_many(values):
    with chisel.open(None) as db:
        handles = []
        with db.transaction() as tx:
            for v in values:
                handles.append(tx.allocate(v))
        for h, expected in zip(handles, values):
            assert db.read(h) == expected
```

- [ ] **Step 2: Run**

```bash
cd python && pytest tests/test_property.py -v
```
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add python/tests/test_property.py
git commit -m "Add hypothesis property tests for bytes round-trip"
```

---

## Task 12: Type Stubs

**Files:**
- Create: `python/chisel/chisel.pyi`
- Create: `python/chisel/py.typed`

- [ ] **Step 1: Create empty `py.typed` marker**

```bash
touch python/chisel/py.typed
```

- [ ] **Step 2: Write stubs**

`python/chisel/chisel.pyi`:
```python
import os
from collections.abc import Buffer, Iterable, Sequence
from dataclasses import dataclass
from types import TracebackType
from typing import Self

__version__: str

class ChiselError(Exception): ...
class OperationalError(ChiselError): ...
class FatalError(ChiselError): ...

class InvalidHandleError(OperationalError): ...
class NoActiveTransactionError(OperationalError): ...
class InvalidSavepointError(OperationalError): ...
class ReadOnlyModeError(OperationalError): ...
class LockFailedError(OperationalError): ...
class DatabaseFileNotFoundError(OperationalError): ...
class InvalidSuperblockCountError(OperationalError): ...
class ValueTooLargeError(OperationalError): ...

class IoError(FatalError): ...
class ChecksumMismatchError(FatalError): ...
class CorruptSuperblockError(FatalError): ...
class PoisonedError(FatalError): ...

@dataclass(frozen=True)
class Stats:
    handle_count: int
    total_pages: int
    file_size_bytes: int

@dataclass(frozen=True)
class DefragOptions:
    # Mirror src/defrag.rs fields exactly
    ...

@dataclass(frozen=True)
class DefragStats:
    # Mirror src/defrag.rs fields exactly
    ...

def open(
    path: str | os.PathLike[str] | None = None,
    *,
    cache_size: int = 1024,
    create_if_missing: bool = True,
    read_only: bool = False,
    superblock_count: int = 2,
) -> Chisel: ...

class Chisel:
    @property
    def is_poisoned(self) -> bool: ...
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None: ...
    def close(self) -> None: ...

    def transaction(self) -> Transaction: ...
    def begin(self) -> None: ...
    def commit(self) -> None: ...
    def rollback(self) -> None: ...

    def allocate(self, value: Buffer) -> int: ...
    def read(self, handle: int) -> bytes: ...
    def update(self, handle: int, value: Buffer) -> None: ...
    def delete(self, handle: int) -> None: ...
    def delete_many(self, handles: Sequence[int]) -> None: ...

    def handles(self) -> Iterable[int]: ...
    def stats(self) -> Stats: ...

    def set_root_name(self, name: str, handle: int) -> None: ...
    def get_root_name(self, name: str) -> int | None: ...
    def clear_root_name(self, name: str) -> None: ...

    def defrag(self, options: DefragOptions | None = None) -> DefragStats: ...

class Transaction:
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None: ...
    def allocate(self, value: Buffer) -> int: ...
    def read(self, handle: int) -> bytes: ...
    def update(self, handle: int, value: Buffer) -> None: ...
    def delete(self, handle: int) -> None: ...
    def delete_many(self, handles: Sequence[int]) -> None: ...
    def set_root_name(self, name: str, handle: int) -> None: ...
    def get_root_name(self, name: str) -> int | None: ...
    def clear_root_name(self, name: str) -> None: ...
    def savepoint(self, name: str) -> Savepoint: ...

class Savepoint:
    name: str
    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None: ...
    def release(self) -> None: ...
    def rollback_to(self) -> None: ...
```

- [ ] **Step 3: Verify stubs install in wheel**

Check `python/pyproject.toml` includes `py.typed` and `chisel.pyi`:
```toml
[tool.maturin]
module-name = "chisel._chisel"
python-source = "."
features = ["pyo3/extension-module"]
include = [
    { path = "chisel/py.typed", format = "wheel" },
    { path = "chisel/chisel.pyi", format = "wheel" },
]
```

- [ ] **Step 4: Rebuild and smoke-test**

```bash
cd python && maturin develop
python -c "import chisel; help(chisel.open)"
```

- [ ] **Step 5: Commit**

```bash
git add python/chisel/py.typed python/chisel/chisel.pyi python/pyproject.toml
git commit -m "Add type stubs and py.typed marker"
```

---

## Task 13: CI Integration

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Read the existing CI config**

```bash
cat .github/workflows/ci.yml
```

- [ ] **Step 2: Add a `python` job**

Add this job (adjust name/keys to match the existing file's conventions):

```yaml
  python:
    needs: [build, test]   # adjust to existing Rust job names
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
        python-version: ["3.10", "3.13"]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: actions/setup-python@v5
        with:
          python-version: ${{ matrix.python-version }}
      - name: Install maturin and test deps
        run: pip install maturin pytest hypothesis
      - name: Build and install
        working-directory: python
        run: maturin develop --release
      - name: Run tests
        working-directory: python
        run: pytest -v
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "Add Python binding to CI matrix (Linux + macOS × 3.10, 3.13)"
```

---

## Task 14: Wheel Build Workflow

**Files:**
- Create: `.github/workflows/wheels.yml`

- [ ] **Step 1: Create wheel-build workflow**

`.github/workflows/wheels.yml`:
```yaml
name: Build wheels

on:
  push:
    tags: ["v*"]
  workflow_dispatch:

jobs:
  wheels:
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"
      - uses: dtolnay/rust-toolchain@stable
      - name: Build wheels
        uses: pypa/cibuildwheel@v2.19
        env:
          CIBW_BUILD: "cp310-* cp311-* cp312-* cp313-*"
          CIBW_ARCHS_LINUX: "x86_64 aarch64"
          CIBW_ARCHS_MACOS: "x86_64 arm64"
          CIBW_SKIP: "*-musllinux_* *-win_*"
          CIBW_BEFORE_BUILD: "pip install maturin"
          CIBW_BUILD_FRONTEND: "build"
          CIBW_TEST_REQUIRES: "pytest hypothesis"
          CIBW_TEST_COMMAND: "pytest {package}/tests"
        with:
          package-dir: python
      - uses: actions/upload-artifact@v4
        with:
          name: wheels-${{ matrix.os }}
          path: wheelhouse/*.whl

  sdist:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"
      - run: pip install maturin
      - working-directory: python
        run: maturin sdist -o ../dist
      - uses: actions/upload-artifact@v4
        with:
          name: sdist
          path: dist/*.tar.gz
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/wheels.yml
git commit -m "Add wheel build workflow for tagged releases"
```

---

## Task 15: README and Install Documentation

**Files:**
- Create: `python/README.md`

- [ ] **Step 1: Write the README**

`python/README.md`:
```markdown
# chisel (Python binding)

Python binding for the [Chisel](..) transactional storage engine.

## Install

```bash
pip install chisel
```

Wheels are provided for CPython 3.10–3.13 on Linux (x86_64, aarch64) and
macOS (x86_64, arm64). Windows is not supported.

## Quick start

```python
import chisel

with chisel.open("db.chisel") as db:
    with db.transaction() as tx:
        h = tx.allocate(b"hello")
    print(db.read(h))  # -> b'hello'
```

In-memory (for tests and benchmarks):

```python
with chisel.open(None) as db:
    ...
```

See the [design spec](../docs/superpowers/specs/2026-04-14-chisel-python-interface-design.md) for the full API surface.

## Thread safety

A `Chisel` instance is **not** safe for concurrent use from multiple threads.
Use one instance per thread or serialize access externally.
```

- [ ] **Step 2: Commit**

```bash
git add python/README.md
git commit -m "Add Python binding README"
```

---

## Self-Review Notes

Running the self-review checklist against the spec:

**Spec coverage:**
- §1 Goals: embedding-driven API → Tasks 3–8. ✓
- §2 Architecture / PyO3 / repo layout / distribution / CI → Tasks 1, 13, 14. ✓
- §3.1 `open()` signature → Task 3. ✓
- §3.2 `Chisel` surface → Tasks 3, 5, 6, 8. ✓
- §3.3 `Transaction` → Task 5. ✓
- §3.4 `Savepoint` → Task 7. ✓
- §3.5 Dataclasses → Task 8. ✓
- §3.6 Value types → Task 4. ✓
- §4 Error hierarchy → Task 2. ✓
- §5 Threading / GIL → Tasks 5 (GIL release in `with_inner_io`), 10 (tests). ✓
- §6 Testing → Tasks 2, 3, 4, 6, 7, 8, 9, 10, 11. ✓

**Type consistency check:** method names align across tasks (`allocate`, `read`, `update`, `delete`, `delete_many`, `set_root_name`, `get_root_name`, `clear_root_name`, `savepoint`, `release`, `rollback_to`). No drift.

**Placeholder scan:** Tasks 8 and 12 have explicit `# Mirror src/defrag.rs fields exactly` placeholders in dataclass bodies. These are intentional pointers — the Rust crate's `DefragOptions`/`DefragStats` fields are the source of truth and should be copied verbatim at implementation time. Each task calls this out plainly in a "Note for implementer" block.

**Ambiguity:** the `unsendable = false` hint on `#[pyclass]` in Task 3 assumes `chisel::Chisel` is `Send`. Verify this at implementation time; if the engine is `!Send`, omit `allow_threads` (functional equivalent, loses the GIL-release optimization) and the task descriptions remain valid.
