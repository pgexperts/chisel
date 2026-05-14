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
