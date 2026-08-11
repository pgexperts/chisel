"""Tests for the between-transaction configuration mutators on Chisel:
set_cache_max_bytes, set_spillway_max_bytes, set_drain_insertion.

All three are wired through PyChisel's with_inner_mut_io helper, so
they share the same closed/poisoned/transaction-state error semantics
as every other mutating method. These tests confirm:

  1. Happy-path: each setter accepts a valid value between transactions.
  2. TransactionInProgressError fires when called mid-transaction. This
     also serves as the only test in the suite that exercises that
     error class, which was previously unreachable from Python.
  3. The setters don't disturb subsequent transaction behaviour (sanity
     check that they're not corrupting engine state).
  4. set_drain_insertion (and the matching open() kwarg) actually reach
     the engine — see the drain-residency section at the end of the file.
"""

import pytest
import chisel


# ── Happy path: each setter accepts a value between transactions ────


def test_set_cache_max_bytes_accepts_new_value(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        db.set_cache_max_bytes(4 * 1024 * 1024)  # halve the default 8 MiB
        # Verify the engine is still usable after the resize: open a
        # transaction, allocate, commit, and read back.
        with db.transaction() as tx:
            h = tx.allocate(b"after-resize")
        assert db.read(h) == b"after-resize"


def test_set_spillway_max_bytes_accepts_new_value(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        db.set_spillway_max_bytes(2 * 1024 * 1024)
        # Sanity: subsequent transaction still works.
        with db.transaction() as tx:
            h = tx.allocate(b"after-spillway-resize")
        assert db.read(h) == b"after-spillway-resize"


def test_set_spillway_max_bytes_zero_disables(tmp_db):
    # Resizing the spillway to 0 disables it. Confirms that the same
    # value the open() kwarg accepts is also acceptable at runtime.
    with chisel.open(str(tmp_db)) as db:
        db.set_spillway_max_bytes(0)
        with db.transaction() as tx:
            tx.allocate(b"spillway-now-disabled")


def test_set_drain_insertion_accepts_both_variants(tmp_db):
    # Both LruTail and Mru should be accepted; flip between them.
    with chisel.open(str(tmp_db)) as db:
        db.set_drain_insertion(chisel.DrainInsertion.Mru)
        with db.transaction() as tx:
            tx.allocate(b"after-mru")
        db.set_drain_insertion(chisel.DrainInsertion.LruTail)
        with db.transaction() as tx:
            tx.allocate(b"after-lru-tail")


# ── Mid-transaction calls raise TransactionInProgressError ──────────


def test_set_cache_max_bytes_mid_transaction_raises(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        with db.transaction() as tx:  # noqa: F841 (tx is used to keep txn open)
            with pytest.raises(chisel.TransactionInProgressError):
                db.set_cache_max_bytes(4 * 1024 * 1024)


def test_set_spillway_max_bytes_mid_transaction_raises(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        with db.transaction() as tx:  # noqa: F841
            with pytest.raises(chisel.TransactionInProgressError):
                db.set_spillway_max_bytes(0)


def test_set_drain_insertion_mid_transaction_raises(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        with db.transaction() as tx:  # noqa: F841
            with pytest.raises(chisel.TransactionInProgressError):
                db.set_drain_insertion(chisel.DrainInsertion.Mru)


# ── Mid-transaction failures don't poison or corrupt ────────────────


def test_failed_setter_does_not_disturb_active_transaction(tmp_db):
    # After a setter raises mid-transaction, the active transaction
    # should still be usable and committable. The setter's failure is
    # operational, not fatal — it didn't touch engine state.
    with chisel.open(str(tmp_db)) as db:
        with db.transaction() as tx:
            with pytest.raises(chisel.TransactionInProgressError):
                db.set_cache_max_bytes(4 * 1024 * 1024)
            h = tx.allocate(b"survives-failed-setter")
        # Transaction committed normally on __exit__.
        assert db.read(h) == b"survives-failed-setter"
        assert not db.is_poisoned


# ── Closed-handle handling ──────────────────────────────────────────


def test_setter_on_closed_db_raises_closed_error(tmp_db):
    db = chisel.open(str(tmp_db))
    db.close()
    with pytest.raises(chisel.ClosedError):
        db.set_cache_max_bytes(4 * 1024 * 1024)
    with pytest.raises(chisel.ClosedError):
        db.set_spillway_max_bytes(0)
    with pytest.raises(chisel.ClosedError):
        db.set_drain_insertion(chisel.DrainInsertion.Mru)


# ── drain_insertion has an OBSERVABLE effect (issue #120, PYTHON-11) ─
#
# Every other drain_insertion test in the suite — here and in
# test_open.py — only checks that a call returns. That is structurally
# incapable of catching the failure that matters: the kwarg crosses two
# conversion layers (PyDrainInsertion -> From impl -> Options /
# set_drain_insertion), and a transposed match arm would silently invert
# the policy for every Python caller.
#
# MECHANISM. drain_insertion never changes a return value or raises. It
# picks WHERE a rehydrated spillway page lands in the LRU list during the
# commit drain (src/page_cache.rs, flush() Phase 1b): LruTail pushes to
# the tail, Mru to the head. The eviction that runs after each drain batch
# (evict_clean_to_cap) takes its victims from the tail. So a spilling
# commit leaves LARGELY COMPLEMENTARY sets resident under the two policies
# — LruTail evicts the pages it just drained and keeps the never-spilled
# ones; Mru keeps the drained pages and evicts never-spilled ones instead.
# Not strictly disjoint: the cache still holds `_CACHE_PAGES - spilled`
# pages that neither policy evicts, which is why the separation tracks the
# spill size rather than being total.
# Re-reading the spilled values therefore hits under Mru and misses under
# LruTail, which counters() makes visible.
#
# DETERMINISM CONSTRAINT — the reason the probe asserts its preconditions
# instead of assuming them. Spillway::drain_batch takes `batch_size` keys
# from a HashMap, whose iteration order is randomized per process, and the
# batch size is the cache cap in pages. Within ONE batch the drained set
# is the entire resident spillway, so the outcome is order-independent;
# with two or more batches, which pages survive is nondeterministic and
# this test would pass today and fail on someone else's machine later.
#
# CALIBRATION. Constants below were measured, not guessed: at
# _CACHE_PAGES = 96 / _N_VALUES = 88 the transaction spills 81 pages (one
# batch, 84% of the cap) and the read-back yields 96 hits / 80 misses
# under LruTail against 128 / 48 under Mru — a delta of 32 in each
# direction, four times _MARGIN. The delta shrinks towards zero as the
# spill approaches _CACHE_PAGES // 2, which is what the second precondition
# guards. If a precondition fires, its message names the knob to turn.

_PAGE_SIZE = 8192                    # engine PAGE_SIZE
_CACHE_PAGES = 96                    # cache cap in pages == drain batch size
_N_VALUES = 88                       # allocations per probe
_VALUE = b"d" * 4096
# Pure safety factor, NOT noise slack: the counts are exactly reproducible
# (measured identical across 30 fresh processes). It exists so a future page-
# layout change that shaves the separation fails loudly here rather than
# silently flipping a `>` comparison.
_MARGIN = 8


def _drain_probe(*, policy_at_open=None, policy_via_setter=None, path=None):
    """Run one spilling commit under a drain policy; return (hits, misses).

    The two counts are deltas across a read-back of every allocated handle.
    `policy_at_open` exercises the open() kwarg, `policy_via_setter` the
    runtime mutator; passing neither leaves the engine at its LruTail
    default.

    `path` selects the backing. It is load-bearing, not a convenience:
    `options.drain_insertion` is read at TWO independent `PageCache::new`
    call sites in lib.rs — one on the file-backed open path, one on the
    in-memory one — so a probe that only ever runs in memory cannot notice
    the file-backed site being dropped. The open-kwarg test therefore runs
    file-backed and the setter test in memory, covering both.
    """
    kwargs = {
        "cache_max_bytes": _CACHE_PAGES * _PAGE_SIZE,
        # Generous: the spillway CAP is not what this test measures, and
        # hitting it would raise SpillwayFullError instead.
        "spillway_max_bytes": 64 * 1024 * 1024,
    }
    if policy_at_open is not None:
        kwargs["drain_insertion"] = policy_at_open

    with chisel.open(path, **kwargs) as db:
        if policy_via_setter is not None:
            db.set_drain_insertion(policy_via_setter)

        handles = []
        with db.transaction() as tx:
            for _ in range(_N_VALUES):
                handles.append(tx.allocate(_VALUE))
            # Sample INSIDE the transaction: commit drains the spillway and
            # truncates it back to zero, so this is unobservable afterwards.
            spill_bytes = db.stats().spillway_logical_bytes

        # None means the spillway was never opened — it is created lazily on
        # the first spill, so this fires when the workload fit in cache, not
        # when the spillway is disabled (that path raises CacheFullError).
        assert spill_bytes is not None, (
            "the workload never spilled, so no drain ran and the policy has "
            "nothing to act on. Raise _N_VALUES or lower _CACHE_PAGES."
        )
        spilled = spill_bytes // _PAGE_SIZE
        assert 0 < spilled <= _CACHE_PAGES, (
            f"probe must spill, and spill within ONE drain batch (batch size "
            f"= _CACHE_PAGES = {_CACHE_PAGES} pages); it spilled {spilled}. "
            f"Two or more batches make the surviving set nondeterministic. "
            f"Adjust _N_VALUES or _CACHE_PAGES."
        )
        # The separation grows as `spilled - _CACHE_PAGES // 2` (measured), so
        # the floor must clear _MARGIN, not merely the zero-crossing. Guarding
        # only at _CACHE_PAGES // 2 would admit a band that passes every
        # precondition and then fails the direction assertion below with a
        # message about Mru, sending the reader hunting in the wrong place.
        _signal_floor = _CACHE_PAGES // 2 + _MARGIN + 1
        assert spilled > _signal_floor, (
            f"only {spilled} pages spilled; the two policies separate by "
            f"about {spilled - _CACHE_PAGES // 2}, which does not clear "
            f"_MARGIN = {_MARGIN}. Need more than {_signal_floor}. "
            f"Raise _N_VALUES."
        )

        before = db.counters()
        for h in handles:
            db.read(h)
        after = db.counters()

    return (after.cache_hits - before.cache_hits,
            after.cache_misses - before.cache_misses)


def test_drain_insertion_open_kwarg_changes_cache_residency(tmp_path):
    # Deleting `.drain_insertion(...)` from the builder chain in db.rs
    # collapses both runs onto the LruTail default and fails this test.
    #
    # File-backed on purpose: this is the probe that covers the file-backed
    # `PageCache::new` site. Its sibling below covers the in-memory one.
    lru_hits, lru_misses = _drain_probe(
        policy_at_open=chisel.DrainInsertion.LruTail,
        path=str(tmp_path / "lru.db"))
    mru_hits, mru_misses = _drain_probe(
        policy_at_open=chisel.DrainInsertion.Mru,
        path=str(tmp_path / "mru.db"))
    # DIRECTION, not merely difference: `impl From<PyDrainInsertion>` is
    # symmetric in its two arms, so a transposition would still satisfy a
    # bare `!=`. Mru keeps the drained pages resident, so it must hit MORE
    # and miss FEWER — never the other way round.
    assert mru_hits > lru_hits + _MARGIN, (
        f"Mru should keep the drained pages resident and out-hit LruTail; "
        f"got Mru {mru_hits} vs LruTail {lru_hits}"
    )
    assert lru_misses > mru_misses + _MARGIN, (
        f"LruTail should evict the drained pages and out-miss Mru; "
        f"got LruTail {lru_misses} vs Mru {mru_misses}"
    )


def test_set_drain_insertion_changes_cache_residency():
    # Same observable, reached through the runtime mutator instead of the
    # open() kwarg — which is omitted entirely here, so the setter is the
    # only difference between the two runs. A no-op setter leaves both at
    # the engine's LruTail default and fails this test.
    lru_hits, lru_misses = _drain_probe(
        policy_via_setter=chisel.DrainInsertion.LruTail)
    mru_hits, mru_misses = _drain_probe(
        policy_via_setter=chisel.DrainInsertion.Mru)
    assert mru_hits > lru_hits + _MARGIN, (
        f"set_drain_insertion(Mru) should keep the drained pages resident; "
        f"got Mru {mru_hits} vs LruTail {lru_hits}"
    )
    assert lru_misses > mru_misses + _MARGIN, (
        f"set_drain_insertion(LruTail) should evict the drained pages; "
        f"got LruTail {lru_misses} vs Mru {mru_misses}"
    )
