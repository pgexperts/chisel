// db.rs — PyChisel wraps chisel::Chisel and provides the top-level
// `open()` constructor. The engine is held in a `Mutex<Option<Chisel>>`
// so that close() can deterministically drop it; subsequent mutating
// or reading calls raise `ClosedError` (ISSUES.md I25, distinct from
// `PoisonedError` so the user can tell "I closed this" from "Rust-side
// corruption"). The `is_poisoned` getter treats closed as poisoned
// because from a "can this handle still do work?" perspective the two
// are equivalent — the distinct exception class lets code that DOES
// care about the cause tell them apart.
//
// Why Mutex (and not RefCell) — I75: the original design used RefCell
// (Chisel is a single-client embedded engine, see MEMORY:
// project_chisel_single_client_design, and the Python GIL already
// serializes cross-thread access), but PyO3 0.24+ requires `#[pyclass]`
// types to be `Sync` and RefCell is `!Sync`. Mutex provides Sync at
// near-zero uncontended cost and still allows the cross-thread HANDOFF
// the engine supports (a handle migrating between threads, one writer at
// a time — see test_threading.py), which `unsendable` would have
// forbidden. The full rationale lives on the `inner` field below. Since
// I156 the Mutex is also the ONLY serialization point: every engine call
// releases the GIL before taking it (see `with_inner_io`).
//
// Re-entry hazard (ISSUES.md I21): a SAME-THREAD re-entry — e.g. a future
// engine API that takes a Python callback, dispatches into Python while
// holding the lock, and the callback calls back into this PyChisel on the
// same thread — would DEADLOCK (std::sync::Mutex is non-reentrant), not
// panic as the old RefCell did. Both are noisy at the point of misuse. No
// such callback API exists today — every public method takes Python
// values and returns them; no engine callback escapes back into Python.
// Such a callback would also have to re-attach to the GIL while holding
// the lock, which breaks the detach-then-lock order and can deadlock
// across threads too. If one is introduced, release the lock before
// calling back into Python.
//
// Lifetime pinning: PyTransaction and PySavepoint each hold a
// `Py<PyChisel>` (a strong Python reference), so the Rust-side
// `Chisel` engine cannot be dropped while any transaction or
// savepoint object is still live on the Python side. Conversely,
// `close()` only clears the `Option` — if a transaction is still
// holding the `Py<PyChisel>`, the PyChisel itself survives until
// GC'd, but further calls through it see `None` and raise ClosedError.
// Calling `close()` inside `with db.transaction()` therefore causes
// the __exit__'s commit/rollback to surface ClosedError on the way
// out, which is intentionally distinguishable from PoisonedError.

use pyo3::prelude::*;
use pyo3::types::PyString;
use std::path::PathBuf;
use std::sync::Mutex;

use chisel::Chisel;

use crate::errors::to_py_err;

/// Map a Python key argument to a `chisel::Key`. `bytes` → `Key::Raw` (any
/// non-empty length is accepted; empty key material is refused deep in the
/// engine and surfaces as InvalidEncryptionKeyError via to_py_err). `str` →
/// `Key::Passphrase`. Anything else raises
/// a Python `TypeError`. Key material is wrapped in `Zeroizing` immediately so
/// it is scrubbed when the `Key` is dropped; we never log or repr the value.
///
/// Mirrors the `encryption_key` kwarg coercion in `open()` so the binding
/// has one key vocabulary. Factor is shared because add_key / rotate_key each
/// need two independent key values, and duplicating the coercion inline would
/// be both verbose and a maintenance hazard.
fn py_key(obj: &Bound<'_, PyAny>) -> PyResult<chisel::Key> {
    if let Ok(b) = obj.cast::<pyo3::types::PyBytes>() {
        Ok(chisel::Key::Raw(zeroize::Zeroizing::new(
            b.as_bytes().to_vec(),
        )))
    } else if let Ok(s) = obj.cast::<PyString>() {
        Ok(chisel::Key::Passphrase(zeroize::Zeroizing::new(
            s.to_str()?.to_owned(),
        )))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            "key must be bytes (raw) or str (passphrase)",
        ))
    }
}

/// Convert a Python-supplied `u32` tag into a non-zero `chisel::Tag`, raising
/// Python `ValueError` on `0`. Tag `0` is no longer a valid value — "untagged"
/// is expressed by calling `allocate` (not `allocate_tagged`), and `tag()`
/// returns `None` for an untagged handle (ISSUES.md I126). Handles cross the
/// boundary as plain `u64` (Python `int`); only tags carry this validation.
fn require_tag(tag: u32) -> PyResult<chisel::Tag> {
    chisel::Tag::new(tag)
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("tag must be non-zero"))
}

#[pyclass(name = "Chisel", module = "chisel._chisel")]
pub struct PyChisel {
    // Option<> so close() can take the inner engine and drop it
    // deterministically. After close(), is_poisoned reports true and
    // (future) mutating methods will raise PoisonedError.
    //
    // I75 (ISSUES.md, 2026-05-22): Mutex (not RefCell) because PyO3
    // 0.24+ requires `#[pyclass]` types to be Sync. RefCell is !Sync;
    // Mutex provides Sync at near-zero uncontended cost. PyO3 0.24
    // enforces Sync as a *compile-time* property the GIL cannot
    // satisfy. Since I156 engine calls run with the GIL released, so
    // this Mutex is what actually serializes them (lock order: see
    // `with_inner_io`). Mutex also preserves the cross-thread-handoff
    // use case (test_threading.py asserts a Chisel handle can migrate
    // between threads), which the `unsendable` annotation would have
    // forbidden.
    //
    // I21 (still relevant): same-thread re-entry on the lock would
    // deadlock (std::sync::Mutex is non-reentrant) rather than
    // panicking as RefCell did. The bug-surfacing property is
    // preserved — both forms are noisy at the point of misuse. No
    // current API surface invites such re-entry; if a future callback
    // API is added, release the lock before calling back into Python.
    // (`try_lock()` is no longer a reentrancy detector: with the GIL
    // released, a held lock usually just means another thread is
    // mid-call.)
    inner: Mutex<Option<Chisel>>,
}

// `DrainInsertion` mirrors `chisel::DrainInsertion` one-to-one. We use a
// dedicated PyO3 pyclass enum rather than re-using the Rust enum so the
// Python type is owned by this binding (its module path, repr, and any
// future Python-side methods are ours to evolve) and so we don't depend
// on the Rust enum implementing the PyO3 conversion traits.
//
// Variant names match the Rust spelling (`LruTail`, `Mru`) rather than
// PEP-8 `LRU_TAIL` / `MRU` so cross-referencing ARCHITECTURE.md
// and the Rust source stays mechanical. `eq` / `eq_int` give Python users
// `chisel.DrainInsertion.LruTail == chisel.DrainInsertion.LruTail` and
// efficient int-based comparison under the hood.
//
// Deliberately not `Copy`: `from_py_object` extracts by calling `.clone()`,
// and from Rust 1.99 clippy's `clone_on_copy` flags that generated call on a
// `Copy` type. Nothing here needs the enum to be `Copy`.
#[pyclass(
    eq,
    eq_int,
    from_py_object,
    module = "chisel._chisel",
    name = "DrainInsertion"
)]
#[derive(Clone, PartialEq)]
pub enum PyDrainInsertion {
    LruTail,
    Mru,
}

impl From<PyDrainInsertion> for chisel::DrainInsertion {
    fn from(v: PyDrainInsertion) -> Self {
        match v {
            PyDrainInsertion::LruTail => chisel::DrainInsertion::LruTail,
            PyDrainInsertion::Mru => chisel::DrainInsertion::Mru,
        }
    }
}

// Keyword-only args after `path` (note the `*`) so users can't
// positionally supply cache_max_bytes by accident; the defaults mirror
// `chisel::Options::default()` so `open(None)` and `open("f.db")`
// behave the same as the Rust API with default options.
//
// Defaults MUST stay in sync with chisel::Options::default() — both
// sides hard-code them rather than sharing a constant because the Rust
// side's constants are not exposed through the PyO3 signature syntax.
// 8_388_608 = 8 MiB = 1024 × 8 KiB pages, matching Options::default().
//
// `spillway_max_bytes = None` is the sentinel for "use Rust's computed
// default of 1024 × cache_max_bytes" (8 GiB at the 8 MiB cache default).
// Explicit 0 disables the spillway and restores CacheFull-at-cap
// semantics; any positive integer is the cap in bytes. We resolve None
// at runtime rather than via the signature literal because the literal
// would not scale with a user-overridden cache_max_bytes.
#[pyfunction]
#[pyo3(signature = (
    path = None,
    *,
    cache_max_bytes = 8_388_608,
    spillway_max_bytes = None,
    drain_insertion = PyDrainInsertion::LruTail,
    create_if_missing = true,
    read_only = false,
    superblock_count = 2,
    encryption_key = None
))]
// open() has 8 args; clippy warns at 7. All but `path` are keyword-only
// (note the `*` in the pyo3(signature) above), so the user can never
// pass them positionally and confuse argument order. Each arg maps to
// a distinct field of chisel::Options that we want to expose; bundling
// them into a struct would require Python users to construct one,
// which is less ergonomic than kwargs.
#[allow(clippy::too_many_arguments)]
pub fn open(
    py: Python<'_>,
    path: Option<Py<PyAny>>,
    cache_max_bytes: u64,
    spillway_max_bytes: Option<u64>,
    drain_insertion: PyDrainInsertion,
    create_if_missing: bool,
    read_only: bool,
    superblock_count: u32,
    encryption_key: Option<Py<PyAny>>,
) -> PyResult<PyChisel> {
    // Coerce path to PathBuf under the GIL first. Accept str fast-path
    // and fall back to os.fspath() for any os.PathLike (pathlib.Path, etc).
    //
    // Path None is the in-memory-mode sentinel (matches the stubs'
    // `path: ... | None`); any non-None value must either be a str or
    // implement os.PathLike. We do this extraction before releasing the
    // GIL so a bad argument raises a Python TypeError synchronously.
    let path_buf: Option<PathBuf> = match path {
        None => None,
        Some(obj) => {
            let bound = obj.bind(py);
            let s: String = if let Ok(py_str) = bound.cast::<PyString>() {
                py_str.to_str()?.to_owned()
            } else {
                let os = py.import("os")?;
                os.call_method1("fspath", (obj,))?.extract()?
            };
            Some(PathBuf::from(s))
        }
    };

    // Coerce encryption_key under the GIL (before py.detach): `bytes` →
    // Key::Raw, `str` → Key::Passphrase. Done here so a bad type raises a
    // synchronous Python TypeError, matching the path coercion above.
    // Key material is wrapped in Zeroizing immediately; the bytes/str borrow
    // is released before the GIL drop so no key material escapes the GIL window.
    let key: Option<chisel::Key> = match encryption_key {
        None => None,
        Some(obj) => {
            let bound = obj.bind(py);
            if let Ok(b) = bound.cast::<pyo3::types::PyBytes>() {
                Some(chisel::Key::Raw(zeroize::Zeroizing::new(
                    b.as_bytes().to_vec(),
                )))
            } else if let Ok(s) = bound.cast::<PyString>() {
                Some(chisel::Key::Passphrase(zeroize::Zeroizing::new(
                    s.to_str()?.to_owned(),
                )))
            } else {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "encryption_key must be bytes (raw key) or str (passphrase)",
                ));
            }
        }
    };

    // Resolve the spillway cap. None → Rust's 1024 × cache_max_bytes
    // default (8 GiB at the 8 MiB cache default); explicit 0 disables
    // and falls back to CacheFull-at-cap. The 1024 multiplier matches
    // chisel::Options::default() exactly.
    // I64 (ISSUES.md, 2026-05-22): `saturating_mul` mirrors the Rust
    // side (`Options::default` in src/lib.rs). For the default
    // cache_max_bytes = 8 MiB the product fits in u64 with plenty of
    // headroom; a Python caller passing cache_max_bytes = 1 << 54
    // (16 PiB) would silently overflow plain `*` to a small spillway
    // cap, falling back to CacheFull-at-cap behaviour rather than
    // saturating to u64::MAX as intended.
    let resolved_spillway_max_bytes =
        spillway_max_bytes.unwrap_or_else(|| cache_max_bytes.saturating_mul(1024));

    // I36: chisel::Options is #[non_exhaustive] so external callers
    // (including this binding, which is a separate crate even though
    // it's path-deps on chisel) must build via the chained-setter
    // builder. Every field is set explicitly because Python kwargs
    // already encode the caller's intent — there are no
    // "leave at default" cases at this boundary.
    let mut options = chisel::Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(resolved_spillway_max_bytes)
        .drain_insertion(drain_insertion.into())
        .create_if_missing(create_if_missing)
        .read_only(read_only)
        .superblock_count(superblock_count);
    if let Some(k) = key {
        options = options.encryption_key(k);
    }

    // Engine calls can block on I/O (flock, fsync, file creation), so
    // release the GIL while they run. Chisel is Send (single-threaded
    // but owns no !Send primitives), which satisfies detach.
    let result = py.detach(|| -> chisel::Result<Chisel> {
        match path_buf {
            None => Chisel::open_in_memory_with_options(options),
            Some(pb) => Chisel::open(&pb, options),
        }
    });

    let engine = result.map_err(to_py_err)?;
    Ok(PyChisel {
        inner: Mutex::new(Some(engine)),
    })
}

#[pymethods]
impl PyChisel {
    fn close(&self) -> PyResult<()> {
        // Drop the engine; this releases the flock and closes the file.
        // Safe to call repeatedly — second call is a no-op.
        //
        // Runs detached like every engine call (I156): Drop does file
        // I/O, and if another thread is mid-call this waits on the Mutex
        // until that call finishes — with the GIL held, the wait would
        // freeze every Python thread for the length of the other call.
        // The engine is dropped inside the closure, under the Mutex: a
        // returned Option<Chisel> would be dropped back on the GIL side,
        // and holding the lock through the drop means a thread queued
        // behind close() sees None (ClosedError), never a half-dropped
        // engine. Errors from Drop are not reportable (currently none
        // are produced); an in-flight transaction is silently discarded
        // by shadow paging and the last committed state remains durable
        // on disk.
        self.locked(|slot| drop(slot.take()));
        Ok(())
    }

    // Context manager support: `with chisel.open(...) as db:` closes on
    // exit. __exit__ returns False so exceptions are never suppressed.
    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

    fn __exit__(&self, _exc_type: Py<PyAny>, _exc: Py<PyAny>, _tb: Py<PyAny>) -> PyResult<bool> {
        self.close()?;
        Ok(false) // do not suppress exceptions
    }

    #[getter]
    fn is_poisoned(&self) -> bool {
        // Cheap once the lock is held, but detached anyway: a health
        // check polled from another thread would otherwise sit on the
        // GIL for as long as a concurrent commit or key operation holds
        // the Mutex (I156).
        self.locked(|slot| match slot.as_ref() {
            Some(c) => c.is_poisoned(),
            None => true, // closed == poisoned from the caller's POV
        })
    }

    fn begin(&self) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.begin())
    }

    pub(crate) fn commit(&self) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.commit())
    }

    pub(crate) fn rollback(&self) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.rollback())
    }

    // `transaction()` is a factory: it calls begin() first, then
    // constructs a PyTransaction that will commit/rollback on __exit__.
    // If begin() fails (e.g., TransactionAlreadyActive), the exception
    // propagates before PyTransaction is constructed — callers never
    // see a half-alive transaction object.
    //
    // If begin() succeeds but Py::new fails (e.g., allocation failure),
    // we roll back the just-begun transaction so the engine returns to a
    // clean no-active-transaction state. Without this rollback the engine
    // would have a live active transaction with no Python owner to ever
    // commit or roll it back, and the next begin() would raise
    // TransactionAlreadyActive. Py::new failures are extremely unlikely
    // (Python-level allocation error) but we close the window regardless.
    //
    // The `slf: Py<Self>` (rather than `&self`) is the PyO3 idiom for
    // "I need to store a Python-side reference to my own object."
    // PyTransaction holds that Py<PyChisel>, which keeps the PyChisel
    // alive for the lifetime of the transaction object regardless of
    // whether the user still holds a reference to the db.
    fn transaction(
        slf: Py<Self>,
        py: Python<'_>,
    ) -> PyResult<Py<crate::transaction::PyTransaction>> {
        slf.bind(py).borrow().begin()?;
        Py::new(
            py,
            crate::transaction::PyTransaction::new(slf.clone_ref(py)),
        )
        .inspect_err(|_| {
            // Py::new failed after begin() succeeded — roll back so the engine
            // is not left with a live transaction that nothing can close.
            let _ = slf.bind(py).borrow().rollback();
        })
    }

    pub(crate) fn allocate(&self, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(|c| c.allocate(&bytes).map(|h| h.get()))
    }

    pub(crate) fn read<'py>(
        &self,
        py: Python<'py>,
        handle: u64,
    ) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        let data = self.with_inner_io(|c| c.read(chisel::Handle::from(handle)))?;
        Ok(pyo3::types::PyBytes::new(py, &data))
    }

    pub(crate) fn update(&self, handle: u64, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(|c| c.update(chisel::Handle::from(handle), &bytes))
    }

    pub(crate) fn delete(&self, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.delete(chisel::Handle::from(handle)))
    }

    pub(crate) fn delete_many(&self, handles: Vec<u64>) -> PyResult<()> {
        let hs: Vec<chisel::Handle> = handles.into_iter().map(chisel::Handle::from).collect();
        self.with_inner_mut_io(|c| c.delete_many(&hs))
    }

    pub(crate) fn handles(&self) -> PyResult<Vec<u64>> {
        self.with_inner_io(|c| {
            c.handles()
                .map(|v| v.into_iter().map(|h| h.get()).collect())
        })
    }

    pub(crate) fn allocate_tagged(&self, value: &Bound<'_, PyAny>, tag: u32) -> PyResult<u64> {
        let tag = require_tag(tag)?;
        let bytes = crate::convert::coerce_value(value)?;
        self.with_inner_mut_io(|c| c.allocate_tagged(&bytes, tag).map(|h| h.get()))
    }

    /// Returns the tag as a Python `int`, or `None` for an untagged handle
    /// (I126: the old sentinel `0` is gone).
    pub(crate) fn tag(&self, handle: u64) -> PyResult<Option<u32>> {
        self.with_inner_io(|c| {
            c.tag(chisel::Handle::from(handle))
                .map(|o| o.map(|t| t.get()))
        })
    }

    pub(crate) fn handles_with_tag(&self, tag: u32) -> PyResult<Vec<u64>> {
        let tag = require_tag(tag)?;
        self.with_inner_io(|c| {
            c.handles_with_tag(tag)
                .map(|v| v.into_iter().map(|h| h.get()).collect())
        })
    }

    pub(crate) fn client_byte(&self, handle: u64) -> PyResult<u8> {
        self.with_inner_io(|c| c.client_byte(chisel::Handle::from(handle)))
    }

    pub(crate) fn set_client_byte(&self, handle: u64, byte: u8) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_client_byte(chisel::Handle::from(handle), byte))
    }

    pub(crate) fn delete_tagged(&self, handle: u64, tag: u32) -> PyResult<()> {
        let tag = require_tag(tag)?;
        self.with_inner_mut_io(|c| c.delete_tagged(chisel::Handle::from(handle), tag))
    }

    /// Returns (deleted: list[int], complete: bool).
    pub(crate) fn delete_with_tag(&self, tag: u32, max: usize) -> PyResult<(Vec<u64>, bool)> {
        let tag = require_tag(tag)?;
        self.with_inner_mut_io(|c| {
            c.delete_with_tag(tag, max)
                .map(|p| (p.deleted.into_iter().map(|h| h.get()).collect(), p.complete))
        })
    }

    pub(crate) fn set_root_name(&self, name: &str, handle: u64) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_root_name(name, chisel::Handle::from(handle)))
    }

    pub(crate) fn get_root_name(&self, name: &str) -> PyResult<Option<u64>> {
        self.with_inner_io(|c| c.get_root_name(name).map(|o| o.map(|h| h.get())))
    }

    pub(crate) fn clear_root_name(&self, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.clear_root_name(name))
    }

    // stats() is read-only on the engine side (`&self`), so the usual
    // immutable borrow path is sufficient. We materialize a
    // `chisel.Stats` dataclass rather than a pyclass so users get the
    // standard dataclass ergonomics (repr, eq, frozen).
    pub(crate) fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let s = self.with_inner_io(|c| c.stats())?;
        let module = py.import("chisel")?;
        let cls = module.getattr("Stats")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("handle_count", s.handle_count)?;
        kwargs.set_item("total_pages", s.total_pages)?;
        kwargs.set_item("file_size_bytes", s.file_size_bytes)?;
        // I74 (ISSUES.md, 2026-05-22): forward the two spillway
        // capacity fields. Option<u64> auto-converts to Python
        // Optional[int] via pyo3's IntoPyObject impl — None on the
        // Rust side becomes Python None.
        kwargs.set_item("spillway_logical_bytes", s.spillway_logical_bytes)?;
        kwargs.set_item("spillway_max_bytes", s.spillway_max_bytes)?;
        Ok(cls.call((), Some(&kwargs))?.unbind())
    }

    // counters() is read-only on the engine side (`&self`), so the usual
    // immutable borrow path is sufficient. We materialize a
    // `chisel.Counters` dataclass — same shape as stats().
    pub(crate) fn counters(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let c = self.with_inner_io(|c| c.counters())?;
        let module = py.import("chisel")?;
        let cls = module.getattr("Counters")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("cache_hits", c.cache_hits)?;
        kwargs.set_item("cache_misses", c.cache_misses)?;
        kwargs.set_item("pages_allocated", c.pages_allocated)?;
        kwargs.set_item("fsync_calls", c.fsync_calls)?;
        Ok(cls.call((), Some(&kwargs))?.unbind())
    }

    // defrag() runs inside the caller's active transaction — see
    // src/defrag.rs "Transactionality" note. If no transaction is
    // active, the engine surfaces NoActiveTransactionError, which
    // propagates unchanged. We accept either a `chisel.DefragOptions`
    // dataclass or None (defaults).
    //
    // Duck-typed on attribute names rather than checking the exact
    // class, so a user-defined options-shaped object will also work
    // — the dataclass is just the nice ergonomic default.
    #[pyo3(signature = (options=None))]
    pub(crate) fn defrag(
        &self,
        py: Python<'_>,
        options: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let rust_opts = match options {
            None => chisel::DefragOptions::default(),
            Some(obj) => {
                let sparse_threshold: f64 = obj.getattr("sparse_threshold")?.extract()?;
                let max_values: usize = obj.getattr("max_values")?.extract()?;
                // I36: DefragOptions is #[non_exhaustive]; build via the
                // chained-setter builder.
                chisel::DefragOptions::default()
                    .sparse_threshold(sparse_threshold)
                    .max_values(max_values)
            }
        };

        let stats = self.with_inner_mut_io(|c| c.defrag(rust_opts))?;

        let module = py.import("chisel")?;
        let cls = module.getattr("DefragStats")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("pages_examined", stats.pages_examined)?;
        kwargs.set_item("pages_freed", stats.pages_freed)?;
        kwargs.set_item("values_moved", stats.values_moved)?;
        Ok(cls.call((), Some(&kwargs))?.unbind())
    }

    // ── Key-rotation methods ─────────────────────────────────────────
    //
    // These three methods mirror `Chisel::{add_key, rotate_key, remove_key}`
    // and use `py_key` (the module-level helper) to coerce each Python key
    // argument. Key material is scrubbed (Zeroizing) immediately on coercion,
    // before any Rust engine call, so keys never appear in error messages,
    // tracebacks, or repr output.
    //
    // Errors are routed through the standard `to_py_err` path:
    //   InvalidEncryptionKey  → InvalidEncryptionKeyError  (wrong/unknown key)
    //   NoFreeKeySlot         → NoFreeKeySlotError         (8-slot table full)
    //   LastKeySlot           → LastKeySlotError           (would leave DB keyless)
    //
    // All three are between-transaction operations at the engine level;
    // the engine enforces this and raises TransactionInProgress if violated.

    pub(crate) fn add_key(
        &self,
        existing: &Bound<'_, PyAny>,
        new: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let existing = py_key(existing)?;
        let new = py_key(new)?;
        self.with_inner_mut_io(|c| c.add_key(&existing, &new))
    }

    pub(crate) fn rotate_key(
        &self,
        old: &Bound<'_, PyAny>,
        new: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let old = py_key(old)?;
        let new = py_key(new)?;
        self.with_inner_mut_io(|c| c.rotate_key(&old, &new))
    }

    pub(crate) fn remove_key(&self, key: &Bound<'_, PyAny>) -> PyResult<()> {
        let key = py_key(key)?;
        self.with_inner_mut_io(|c| c.remove_key(&key))
    }

    // ── Between-transaction configuration mutators ──────────────────
    //
    // The three setters below mirror the same-named methods on
    // chisel::Chisel. They operate ONLY on between-transactions state:
    // calling any of them while a transaction is active raises
    // TransactionInProgressError, because shrinking the cache or
    // spillway mid-transaction would either reject pinned dirty pages
    // or silently spill them — neither is a clean story (see
    // ARCHITECTURE.md + ISSUES.md for the spillway design).
    //
    // All three flow through with_inner_mut_io, so a closed handle
    // raises ClosedError uniformly with every other mutating method.

    fn set_cache_max_bytes(&self, bytes: u64) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_cache_max_bytes(bytes))
    }

    fn set_spillway_max_bytes(&self, bytes: u64) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_spillway_max_bytes(bytes))
    }

    fn set_drain_insertion(&self, policy: PyDrainInsertion) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.set_drain_insertion(policy.into()))
    }
}

// Internal helpers shared with PyTransaction / PySavepoint. The public
// pymethods on PyChisel are also ordinary inherent Rust methods, so the
// transaction and savepoint wrappers call them directly (e.g.
// `db.borrow().commit()`). Only the three savepoint operations below have
// no public PyChisel pymethod — they are reached through the PySavepoint
// object, not `db.<op>()` — so they keep a pub(crate) entry point here.
// The with_inner_io / with_inner_mut_io wrappers are the ONE place the
// closed/poisoned check lives.
//
// Rationale: every method that mutates or reads the engine must go
// through one of these two helpers, so there is exactly one site where
// "closed" collapses into PoisonedError. Bypassing them would create a
// path where `self.inner.lock().unwrap().as_ref().unwrap()` panics a closed db.
impl PyChisel {
    pub(crate) fn savepoint_internal(&self, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.savepoint(name))
    }

    pub(crate) fn release_internal(&self, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.release(name))
    }

    pub(crate) fn rollback_to_internal(&self, name: &str) -> PyResult<()> {
        self.with_inner_mut_io(|c| c.rollback_to(name))
    }

    // These two helpers are the ONLY place the closed/poisoned
    // distinction collapses to its typed errors — callers above never
    // see a bare None.
    //
    // I156 (#144): every engine call releases the GIL. Commit (three
    // fsyncs), defrag, and the key-slot operations (a passphrase
    // add_key / rotate_key / remove_key runs one or two deliberately
    // slow Argon2id derivations, 19 MiB, t=2) would otherwise freeze
    // every Python thread in the process, not just threads using this
    // handle.
    // The engine Mutex, not the GIL, is what serializes calls on one
    // handle; Chisel only has to be Send for that (Mutex<T: Send> is
    // Sync), which open() already relies on when it moves a fresh
    // engine across its own detach.
    //
    // Lock order — the invariant that keeps this deadlock-free: detach
    // FIRST, then lock, call, and unlock entirely inside the detached
    // closure (see `locked`). A thread therefore never waits for the
    // Mutex while holding the GIL, and never waits for the GIL while
    // holding the Mutex. The tempting alternative — lock under the GIL,
    // then detach around just the engine call — inverts that: thread A
    // holds the Mutex and blocks re-attaching while thread B holds the
    // GIL and blocks on the Mutex. For the same reason nothing inside
    // the closure may call `Python::attach`. The `Send` bounds keep
    // `Bound`/`Python` out of it, but a captured `Py<T>` is `Send`, so
    // this is a convention, not a compile-time guarantee. In particular
    // `to_py_err` attaches, which is why errors are converted to Python
    // exceptions only after `locked` returns.
    //
    // The helpers take no `Python` token so that the ~55 call sites
    // (PyChisel, PyTransaction, PySavepoint) keep their signatures;
    // `locked` obtains one with `Python::attach`, which is a cheap
    // thread-local check because the GIL is already held at every call
    // site.
    fn with_inner_io<R: Send>(
        &self,
        f: impl FnOnce(&Chisel) -> chisel::Result<R> + Send,
    ) -> PyResult<R> {
        self.locked(|slot| slot.as_ref().map(f))
            .ok_or_else(closed_err)?
            .map_err(to_py_err)
    }

    fn with_inner_mut_io<R: Send>(
        &self,
        f: impl FnOnce(&mut Chisel) -> chisel::Result<R> + Send,
    ) -> PyResult<R> {
        self.locked(|slot| slot.as_mut().map(f))
            .ok_or_else(closed_err)?
            .map_err(to_py_err)
    }

    /// Release the GIL, then run `f` on the engine slot under the Mutex.
    /// The guard is dropped before `detach` returns, so the GIL is
    /// re-acquired only after the Mutex is free (lock order above).
    fn locked<T: Send>(&self, f: impl FnOnce(&mut Option<Chisel>) -> T + Send) -> T {
        let inner = &self.inner;
        Python::attach(|py| {
            py.detach(|| {
                // Recover from a poisoned Mutex rather than panicking. The
                // engine never panics while holding the lock (fatal conditions
                // return ChiselError::Poisoned, surfaced to Python as
                // PoisonedError), so the Mutex should never poison; but if some
                // future engine debug_assert or panic ever did fire under the
                // lock, `into_inner()` degrades to the normal poisoned-engine
                // path instead of bricking every subsequent call (including
                // close() and is_poisoned()) with a PanicException.
                let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                f(&mut guard)
            })
        })
    }
}

// Small helper so the ClosedError construction is not duplicated
// between the two `with_inner_*` helpers. Message matches the
// exception class description in errors.rs — the user-visible
// distinction between "closed" and "poisoned" is the class name,
// not the text.
fn closed_err() -> PyErr {
    crate::errors::ClosedError::new_err("database handle has been closed")
}

/// Bulk DEK rotation — re-encrypt an entire database under a fresh data key.
///
/// A module-level function rather than a `Chisel` method because it operates on
/// a PATH, not an open handle: it rewrites the whole file and replaces it by
/// rename, so any handle opened beforehand would afterwards refer to the
/// original, now-unlinked inode. Mirrors `chisel.open()` in shape for that same
/// reason — both name a database by path rather than assuming one is open.
///
/// I156: the GIL is released for the duration. This is O(total_pages) I/O plus
/// an Argon2id derivation for a passphrase credential; holding the GIL across
/// it would freeze every other Python thread for the length of a whole-file
/// rewrite, which is the worst case of exactly the problem I156 describes.
#[pyfunction]
#[pyo3(signature = (path, key))]
pub fn rekey(py: Python<'_>, path: Py<PyAny>, key: Py<PyAny>) -> PyResult<()> {
    // Coerce both arguments under the GIL, before detaching, so a bad type
    // raises a synchronous Python TypeError — same discipline as open().
    let path_buf: PathBuf = {
        let bound = path.bind(py);
        let s: String = if let Ok(py_str) = bound.cast::<PyString>() {
            py_str.to_str()?.to_owned()
        } else {
            let os = py.import("os")?;
            os.call_method1("fspath", (path,))?.extract()?
        };
        PathBuf::from(s)
    };
    // Zeroizing on coercion, before any engine call, so key material never
    // reaches an error message, traceback, or repr.
    let key = py_key(key.bind(py))?;

    py.detach(|| chisel::Chisel::rekey(&path_buf, &key, None))
        .map_err(to_py_err)
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChisel>()?;
    m.add_class::<PyDrainInsertion>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(rekey, m)?)?;
    Ok(())
}
