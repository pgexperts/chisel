// db.rs — PyChisel wraps chisel::Chisel and provides the top-level
// `open()` constructor. The engine is held in a RefCell<Option<Chisel>>
// so that close() can deterministically drop it; subsequent mutating
// or reading calls raise `ClosedError` (ISSUES.md I25, distinct from
// `PoisonedError` so the user can tell "I closed this" from "Rust-side
// corruption"). The `is_poisoned` getter treats closed as poisoned
// because from a "can this handle still do work?" perspective the two
// are equivalent — the distinct exception class lets code that DOES
// care about the cause tell them apart.
//
// Why RefCell (and not Mutex): Chisel is explicitly a single-client
// embedded engine (see MEMORY: project_chisel_single_client_design),
// and the Python GIL already serializes CROSS-THREAD access to this
// object from Python code. RefCell gives us `&self` pymethods with
// interior mutability at near-zero cost.
//
// What the GIL does NOT protect against (ISSUES.md I21): a
// SAME-THREAD re-entry — e.g., some future engine API that takes a
// Python callback, dispatches into Python while holding a borrow_mut
// on `inner`, and then the callback turns around and calls another
// PyChisel method on the same thread. The GIL is held throughout
// (same thread, no yield), but the second borrow_mut hits an already-
// borrowed RefCell and panics. No such callback API exists in this
// binding today — every public method takes Python values and returns
// them; no engine callback escapes back into Python code. If/when a
// callback API is introduced, either (a) convert `inner` to
// `Option<Chisel>` behind a `try_borrow_mut` wrapper that raises an
// explicit reentrancy error, or (b) reshape the engine call so the
// mutable borrow is released before the Python callback fires.
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

#[pyclass(name = "Chisel", module = "chisel._chisel")]
pub struct PyChisel {
    // Option<> so close() can take the inner engine and drop it
    // deterministically. After close(), is_poisoned reports true and
    // (future) mutating methods will raise PoisonedError.
    //
    // I75 (ISSUES.md, 2026-05-22): Mutex (not RefCell) because PyO3
    // 0.24+ requires `#[pyclass]` types to be Sync. RefCell is !Sync;
    // Mutex provides Sync at near-zero uncontended cost. The
    // GIL+Python-level serialization argument from the original
    // pre-I75 design is still true at the Python level, but PyO3 0.24
    // enforces Sync as a *compile-time* property the GIL cannot
    // satisfy. Mutex also preserves the cross-thread-handoff use case
    // (test_threading.py asserts a Chisel handle can migrate between
    // threads — single-writer migration, not concurrent access),
    // which the `unsendable` annotation would have forbidden.
    //
    // I21 (still relevant): same-thread re-entry on the lock would
    // deadlock (std::sync::Mutex is non-reentrant) rather than
    // panicking as RefCell did. The bug-surfacing property is
    // preserved — both forms are noisy at the point of misuse. No
    // current API surface invites such re-entry; if a future callback
    // API is added, either wrap each `lock()` in `try_lock()` and
    // raise an explicit reentrancy error, or release the lock before
    // calling back into Python.
    inner: Mutex<Option<Chisel>>,
}

// `DrainInsertion` mirrors `chisel::DrainInsertion` one-to-one. We use a
// dedicated PyO3 pyclass enum rather than re-using the Rust enum so the
// Python type is owned by this binding (its module path, repr, and any
// future Python-side methods are ours to evolve) and so we don't depend
// on the Rust enum implementing the PyO3 conversion traits.
//
// Variant names match the Rust spelling (`LruTail`, `Mru`) rather than
// PEP-8 `LRU_TAIL` / `MRU` so cross-referencing ARCHITECTURE.md ADR-5
// and the Rust source stays mechanical. `eq` / `eq_int` give Python users
// `chisel.DrainInsertion.LruTail == chisel.DrainInsertion.LruTail` and
// efficient int-based comparison under the hood.
#[pyclass(eq, eq_int, module = "chisel._chisel", name = "DrainInsertion")]
#[derive(Clone, Copy, PartialEq)]
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
    superblock_count = 2
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
    path: Option<PyObject>,
    cache_max_bytes: u64,
    spillway_max_bytes: Option<u64>,
    drain_insertion: PyDrainInsertion,
    create_if_missing: bool,
    read_only: bool,
    superblock_count: u32,
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
            let s: String = if let Ok(py_str) = bound.downcast::<PyString>() {
                py_str.to_str()?.to_owned()
            } else {
                let os = py.import("os")?;
                os.call_method1("fspath", (obj,))?.extract()?
            };
            Some(PathBuf::from(s))
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
    let options = chisel::Options::default()
        .cache_max_bytes(cache_max_bytes)
        .spillway_max_bytes(resolved_spillway_max_bytes)
        .drain_insertion(drain_insertion.into())
        .create_if_missing(create_if_missing)
        .read_only(read_only)
        .superblock_count(superblock_count);

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
        inner: Mutex::new(Some(engine)),
    })
}

#[pymethods]
impl PyChisel {
    fn close(&self) -> PyResult<()> {
        // Drop the engine; this releases the flock and closes the file.
        // Safe to call repeatedly — second call is a no-op.
        //
        // Note: this runs Drop on the inner Chisel while the GIL is
        // held. That is intentional — if a Drop impl ever needs to
        // report an error (currently none do), it must go through the
        // Chisel::close method. An in-flight transaction is silently
        // discarded by shadow paging; the last committed state remains
        // durable on disk.
        let _ = self.inner.lock().unwrap().take();
        Ok(())
    }

    // Context manager support: `with chisel.open(...) as db:` closes on
    // exit. __exit__ returns False so exceptions are never suppressed.
    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyRef<'py, Self> {
        slf
    }

    fn __exit__(&self, _exc_type: PyObject, _exc: PyObject, _tb: PyObject) -> PyResult<bool> {
        self.close()?;
        Ok(false) // do not suppress exceptions
    }

    #[getter]
    fn is_poisoned(&self) -> bool {
        let guard = self.inner.lock().unwrap();
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

    // stats() is read-only on the engine side (`&self`), so the usual
    // immutable borrow path is sufficient. We materialize a
    // `chisel.Stats` dataclass rather than a pyclass so users get the
    // standard dataclass ergonomics (repr, eq, frozen).
    fn stats(&self, py: Python<'_>) -> PyResult<PyObject> {
        let s = self.with_inner_io(py, |c| c.stats())?;
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
    fn counters(&self, py: Python<'_>) -> PyResult<PyObject> {
        let c = self.with_inner_io(py, |c| c.counters())?;
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
    fn defrag(&self, py: Python<'_>, options: Option<&Bound<'_, PyAny>>) -> PyResult<PyObject> {
        let rust_opts = match options {
            None => chisel::DefragOptions::default(),
            Some(obj) => {
                let sparse_threshold: f64 = obj.getattr("sparse_threshold")?.extract()?;
                let max_pages: usize = obj.getattr("max_pages")?.extract()?;
                // I36: DefragOptions is #[non_exhaustive]; build via the
                // chained-setter builder.
                chisel::DefragOptions::default()
                    .sparse_threshold(sparse_threshold)
                    .max_pages(max_pages)
            }
        };

        let stats = self.with_inner_mut_io(py, |c| c.defrag(rust_opts.clone()))?;

        let module = py.import("chisel")?;
        let cls = module.getattr("DefragStats")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("pages_examined", stats.pages_examined)?;
        kwargs.set_item("pages_freed", stats.pages_freed)?;
        kwargs.set_item("values_moved", stats.values_moved)?;
        Ok(cls.call((), Some(&kwargs))?.unbind())
    }

    // ── Between-transaction configuration mutators ──────────────────
    //
    // The three setters below mirror the same-named methods on
    // chisel::Chisel. They operate ONLY on between-transactions state:
    // calling any of them while a transaction is active raises
    // TransactionInProgressError, because shrinking the cache or
    // spillway mid-transaction would either reject pinned dirty pages
    // or silently spill them — neither is a clean story (see
    // ARCHITECTURE.md ADR-5 + ISSUES.md for the spillway design).
    //
    // All three flow through with_inner_mut_io, so a closed handle
    // raises ClosedError uniformly with every other mutating method.

    fn set_cache_max_bytes(&self, py: Python<'_>, bytes: u64) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.set_cache_max_bytes(bytes))
    }

    fn set_spillway_max_bytes(&self, py: Python<'_>, bytes: u64) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.set_spillway_max_bytes(bytes))
    }

    fn set_drain_insertion(&self, py: Python<'_>, policy: PyDrainInsertion) -> PyResult<()> {
        self.with_inner_mut_io(py, |c| c.set_drain_insertion(policy.into()))
    }
}

// Internal helpers — NOT exposed to Python. PyTransaction reaches into
// these pub(crate) methods to share the same engine-access code path as
// the direct PyChisel pymethods. The with_inner_io / with_inner_mut_io
// wrappers are the ONE place the closed/poisoned check lives.
//
// Rationale: every method that mutates or reads the engine must go
// through one of these two helpers, so there is exactly one site where
// "closed" collapses into PoisonedError. Bypassing them would create a
// path where `self.inner.lock().unwrap().as_ref().unwrap()` panics a closed db.
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
        Ok(pyo3::types::PyBytes::new(py, &data))
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
    // distinction collapses to its typed errors — callers above never
    // see a bare None. We intentionally do NOT release the GIL around
    // the engine call: Chisel owns `Cell<bool>` internally so `&Chisel`
    // is not `Sync`, and the single-client embedded design means
    // there is no concurrent Python work to overlap with anyway (see
    // MEMORY: project_chisel_single_client_design). The GIL prevents
    // CROSS-THREAD re-entry into the RefCell, but not same-thread
    // re-entry through a hypothetical future callback API — see the
    // module-level I21 note at the top of this file.
    //
    // Consequence: long-running engine calls (e.g. a large commit's
    // fsync, a big defrag) will block ALL other Python threads in this
    // process. `open()` itself does release the GIL around the initial
    // filesystem setup — see the `allow_threads` there — but per-op
    // calls do not. If future work wants to overlap Python work with
    // Chisel I/O, the fix is (a) make Chisel's interior state Sync-
    // safe, or (b) move the GIL release into the engine layer.
    //
    // The `_py: Python<'_>` parameter on both helpers is an unused
    // token; it exists to make the GIL-held precondition explicit at
    // the call sites and to leave room for a future `allow_threads`
    // insertion without changing every caller signature.
    fn with_inner_io<R>(
        &self,
        _py: Python<'_>,
        f: impl FnOnce(&Chisel) -> chisel::Result<R>,
    ) -> PyResult<R> {
        let guard = self.inner.lock().unwrap();
        let c = guard.as_ref().ok_or_else(closed_err)?;
        f(c).map_err(to_py_err)
    }

    fn with_inner_mut_io<R>(
        &self,
        _py: Python<'_>,
        f: impl FnOnce(&mut Chisel) -> chisel::Result<R>,
    ) -> PyResult<R> {
        let mut guard = self.inner.lock().unwrap();
        let c = guard.as_mut().ok_or_else(closed_err)?;
        f(c).map_err(to_py_err)
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

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChisel>()?;
    m.add_class::<PyDrainInsertion>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    Ok(())
}
