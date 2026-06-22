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
// forbidden. The full rationale lives on the `inner` field below.
//
// Re-entry hazard (ISSUES.md I21): a SAME-THREAD re-entry — e.g. a future
// engine API that takes a Python callback, dispatches into Python while
// holding the lock, and the callback calls back into this PyChisel on the
// same thread — would DEADLOCK (std::sync::Mutex is non-reentrant), not
// panic as the old RefCell did. Both are noisy at the point of misuse. No
// such callback API exists today — every public method takes Python
// values and returns them; no engine callback escapes back into Python.
// If one is introduced, either wrap each `lock()` in `try_lock()` and
// raise an explicit reentrancy error, or release the lock before calling
// back into Python.
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
// PEP-8 `LRU_TAIL` / `MRU` so cross-referencing ARCHITECTURE.md
// and the Rust source stays mechanical. `eq` / `eq_int` give Python users
// `chisel.DrainInsertion.LruTail == chisel.DrainInsertion.LruTail` and
// efficient int-based comparison under the hood.
#[pyclass(
    eq,
    eq_int,
    from_py_object,
    module = "chisel._chisel",
    name = "DrainInsertion"
)]
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
    path: Option<Py<PyAny>>,
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
            let s: String = if let Ok(py_str) = bound.cast::<PyString>() {
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
        // Note: this runs Drop on the inner Chisel while the GIL is
        // held. That is intentional — if a Drop impl ever needs to
        // report an error (currently none do), it must go through the
        // Chisel::close method. An in-flight transaction is silently
        // discarded by shadow paging; the last committed state remains
        // durable on disk.
        let _ = self.inner.lock().unwrap_or_else(|e| e.into_inner()).take();
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
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(c) => c.is_poisoned(),
            None => true, // closed == poisoned from the caller's POV
        }
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
        Py::new(py, crate::transaction::PyTransaction::new(slf))
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
    pub(crate) fn defrag(&self, py: Python<'_>, options: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
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
    // filesystem setup — see the `py.detach` there — but per-op calls do
    // not. If future work wants to overlap Python work with Chisel I/O,
    // the fix is (a) make Chisel's interior state Sync-safe and release
    // the GIL here via `Python::detach`, or (b) move the GIL release into
    // the engine layer. The GIL is held at every call site today, so these
    // helpers take no `Python` token.
    fn with_inner_io<R>(&self, f: impl FnOnce(&Chisel) -> chisel::Result<R>) -> PyResult<R> {
        // Recover from a poisoned Mutex rather than panicking. The engine never
        // panics while holding the lock (fatal conditions return
        // ChiselError::Poisoned, surfaced to Python as PoisonedError), so the
        // Mutex should never poison; but if some future engine debug_assert or
        // panic ever did fire under the lock, `into_inner()` degrades to the
        // normal poisoned-engine path instead of bricking every subsequent call
        // (including close() and is_poisoned()) with a PanicException.
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let c = guard.as_ref().ok_or_else(closed_err)?;
        f(c).map_err(to_py_err)
    }

    fn with_inner_mut_io<R>(
        &self,
        f: impl FnOnce(&mut Chisel) -> chisel::Result<R>,
    ) -> PyResult<R> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
