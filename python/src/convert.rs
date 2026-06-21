// convert.rs — value coercion for the write path. Accepts anything
// exposing the buffer protocol (bytes, bytearray, memoryview, array.array).
// Rejects str explicitly with a helpful TypeError pointing at .encode().
//
// Bytes are copied into a Vec<u8> at the boundary. The Rust Chisel API
// takes &[u8] and does not retain the slice, so the copy is unavoidable
// for owned-buffer safety; zero-copy would require a buffer-protocol-aware
// lower-level API on the Rust side.
//
// Read path: no symmetric coerce_value is needed — reads always return
// a fresh Python `bytes` object built by PyBytes::new from the
// Vec<u8> that chisel::Chisel::read returns. The copy there is also
// unavoidable because the engine's page cache owns the source bytes
// and may reuse the buffer for other pages. See db.rs::read.

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

    // Try <u8> first (bytes, bytearray, memoryview of bytes). If the
    // buffer format is signed (e.g., array.array("b", ...)), retry as
    // <i8> and reinterpret — the underlying bytes are the same; only
    // pyo3's type check rejects the mismatch. Both paths still require
    // item-size 1 and C-contiguity, which the PyBuffer wrappers enforce.
    //
    // Larger element widths (array.array("i"), array.array("d"),
    // numpy arrays with dtype != {u8, i8}) are NOT accepted: the
    // storage engine stores raw octets, and silently reinterpreting a
    // multi-byte-per-element buffer would hide endianness and padding
    // bugs. Callers who want that should do `.tobytes()` explicitly.
    if let Ok(buf) = PyBuffer::<u8>::get(value) {
        return copy_buffer_u8(value.py(), &buf);
    }
    if let Ok(buf) = PyBuffer::<i8>::get(value) {
        if !buf.is_c_contiguous() {
            return Err(PyTypeError::new_err(
                "bytes-like buffer must be C-contiguous",
            ));
        }
        let len = buf.len_bytes();
        let mut out = vec![0i8; len];
        buf.copy_to_slice(value.py(), &mut out)
            .map_err(|_| PyTypeError::new_err("could not read bytes-like buffer"))?;
        // Reinterpret signed bytes as unsigned; the on-disk format does
        // not care about sign — it stores raw octets.
        //
        // Cost: this is an O(n) extra pass over the bytes, unavoidable
        // unless we transmute the Vec<i8> to Vec<u8> (same layout but
        // requires unsafe). Signed-buffer inputs are expected to be
        // rare enough that the extra copy is not worth unsafe code.
        return Ok(out.into_iter().map(|b| b as u8).collect());
    }

    Err(PyTypeError::new_err(
        "values must be bytes-like (implement the buffer protocol)",
    ))
}

// C-contiguity check: non-contiguous buffers (e.g. a strided
// memoryview slice) would require a strided read; PyBuffer's
// copy_to_slice requires contiguous input. We fail fast with a
// clear error rather than silently allocating-and-failing later.
fn copy_buffer_u8(py: Python<'_>, buf: &PyBuffer<u8>) -> PyResult<Vec<u8>> {
    if !buf.is_c_contiguous() {
        return Err(PyTypeError::new_err(
            "bytes-like buffer must be C-contiguous",
        ));
    }
    let len = buf.len_bytes();
    let mut out = vec![0u8; len];
    buf.copy_to_slice(py, &mut out)
        .map_err(|_| PyTypeError::new_err("could not read bytes-like buffer"))?;
    Ok(out)
}
