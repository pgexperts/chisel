// convert.rs — value coercion for the write path. Accepts anything
// exposing the buffer protocol (bytes, bytearray, memoryview, array.array).
// Rejects str explicitly with a helpful TypeError pointing at .encode().
//
// Bytes are copied into a Vec<u8> at the boundary. The Rust Chisel API
// takes &[u8] and does not retain the slice, so the copy is unavoidable
// for owned-buffer safety; zero-copy would require a buffer-protocol-aware
// lower-level API on the Rust side.

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyString;

pub fn coerce_value(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    // str is bytes-like-shaped (has __buffer__ on some paths in 3.12+)
    // but is semantically wrong for a storage-engine write. Reject
    // explicitly so users get a clear message rather than a silent
    // encoding choice.
    if value.is_instance_of::<PyString>() {
        return Err(PyTypeError::new_err(
            "values must be bytes-like; got str — encode first, e.g. s.encode('utf-8')",
        ));
    }

    let buf: PyBuffer<u8> = PyBuffer::get_bound(value).map_err(|_| {
        PyTypeError::new_err(
            "values must be bytes-like (implement the buffer protocol)",
        )
    })?;

    if !buf.is_c_contiguous() {
        return Err(PyTypeError::new_err("bytes-like buffer must be C-contiguous"));
    }

    let len = buf.len_bytes();
    let mut out = vec![0u8; len];
    buf.copy_to_slice(value.py(), &mut out).map_err(|_| {
        PyTypeError::new_err("could not read bytes-like buffer")
    })?;
    Ok(out)
}
