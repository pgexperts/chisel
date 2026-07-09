import pytest
import chisel


def test_stats_dataclass_shape(mem_db):
    s = mem_db.stats()
    assert isinstance(s, chisel.Stats)
    assert s.handle_count == 0
    assert s.total_pages > 0
    assert s.file_size_bytes == s.total_pages * 8192


# I74 (ISSUES.md, 2026-05-22): Stats exposes spillway capacity. The
# spillway is lazily constructed on the first overflow, so on a fresh
# handle both fields are None (no spillway has ever been opened).
def test_stats_spillway_fields_are_none_on_fresh_handle(mem_db):
    s = mem_db.stats()
    assert s.spillway_logical_bytes is None
    assert s.spillway_max_bytes is None


# I74: drive an overflow workload that lazily opens the spillway, then
# verify the fields transition to Some. Uses a tiny cache so the
# overflow is guaranteed.
def test_stats_spillway_fields_populate_after_overflow():
    spillway_max = 1024 * 16 * 8192  # 1024 × (16 pages × 8 KiB)
    db = chisel.open(
        None,
        cache_max_bytes=16 * 8192,
        spillway_max_bytes=spillway_max,
    )
    # Fresh: no spill yet.
    assert db.stats().spillway_logical_bytes is None
    # Force an overflow.
    with db.transaction() as tx:
        for i in range(200):
            tx.allocate(bytes([i % 256]) * 1024)
        # Mid-transaction, while the spillway is non-empty.
        s = db.stats()
        assert s.spillway_logical_bytes is not None, "spillway must be open after overflow"
        assert s.spillway_logical_bytes > 0
        assert s.spillway_max_bytes == spillway_max
    # Post-commit: spillway is truncated; field is still Some but 0.
    s = db.stats()
    assert s.spillway_logical_bytes == 0
    assert s.spillway_max_bytes == spillway_max
    db.close()


def test_stats_after_allocations(mem_db):
    with mem_db.transaction() as tx:
        for i in range(10):
            tx.allocate(bytes([i]))
    s = mem_db.stats()
    assert s.handle_count == 10


def test_stats_is_frozen(mem_db):
    s = mem_db.stats()
    with pytest.raises(Exception):  # FrozenInstanceError subclasses AttributeError
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


def test_defrag_options_accepted(mem_db):
    with mem_db.transaction():
        result = mem_db.defrag(chisel.DefragOptions(sparse_threshold=0.5, max_values=10))
    assert isinstance(result, chisel.DefragStats)


# Transaction.handles / .stats / .counters / .defrag — the four read-side
# methods that were missing from PyTransaction (mirrored from PyChisel).
def test_transaction_handles_visible_mid_tx(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"a")
        h2 = tx.allocate(b"b")
        # handles() must be reachable on the transaction object itself.
        live = tx.handles()
        assert h1 in live
        assert h2 in live


def test_transaction_stats_mid_tx(mem_db):
    with mem_db.transaction() as tx:
        tx.allocate(b"payload")
        s = tx.stats()
    assert isinstance(s, chisel.Stats)
    assert s.handle_count >= 1


def test_transaction_counters_mid_tx(mem_db):
    with mem_db.transaction() as tx:
        tx.allocate(b"x")
        c = tx.counters()
    assert isinstance(c, chisel.Counters)
    assert c.pages_allocated >= 1


def test_transaction_defrag_mid_tx(mem_db):
    # defrag() is documented as running inside the active transaction;
    # verify it is accessible on the Transaction object.
    with mem_db.transaction() as tx:
        for i in range(10):
            tx.allocate(bytes([i]) * 50)
    with mem_db.transaction() as tx:
        result = tx.defrag()
    assert isinstance(result, chisel.DefragStats)
