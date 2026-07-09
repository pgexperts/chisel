// lib.rs — crate root for the `chisel-py` PyO3 binding. Builds as
// `_chisel.abi3.so`; the user-facing Python package is `chisel`
// (see python/chisel/__init__.py), which re-exports everything here
// plus a few dataclasses.
//
// Role in system: this file does nothing but wire up submodules. All
// real PyO3 surface lives in the submodules:
//   - errors.rs      — exception hierarchy + ChiselError → PyErr mapping
//   - db.rs          — PyChisel (the `Chisel` class) and the `open()` fn
//   - transaction.rs — PyTransaction context manager
//   - savepoint.rs   — PySavepoint context manager
//   - convert.rs     — bytes-like value coercion on the write path
//
// Registration order matters: errors must be registered before db so
// that `to_py_err` can reach the exception types (they are module-level
// Python objects, resolved by string lookup at call time, but the
// convention here is to put them up first).
//
// abi3: the crate is compiled against the stable Python ABI (pyo3
// `abi3-py311`), so one shared library works across Python 3.11+
// without per-version rebuilds. See python/Cargo.toml for the feature.

use pyo3::prelude::*;

mod convert;
mod db;
mod errors;
mod savepoint;
mod transaction;

// `_chisel` is the extension-module name seen by Python. The user
// package `chisel` re-exports from it; keeping the Rust module private
// (underscore-prefixed) signals that users should import from `chisel`,
// not from `chisel._chisel`.
#[pymodule]
fn _chisel(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register(m)?;
    db::register(m)?;
    m.add_class::<transaction::PyTransaction>()?;
    m.add_class::<savepoint::PySavepoint>()?;
    Ok(())
}
