use pyo3::prelude::*;

mod convert;
mod db;
mod errors;
mod savepoint;
mod transaction;

#[pymodule]
fn _chisel(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register(m)?;
    db::register(m)?;
    m.add_class::<transaction::PyTransaction>()?;
    m.add_class::<savepoint::PySavepoint>()?;
    Ok(())
}
