import pytest
import chisel


def test_stats_dataclass_shape(mem_db):
    s = mem_db.stats()
    assert isinstance(s, chisel.Stats)
    assert s.handle_count == 0
    assert s.total_pages > 0
    assert s.file_size_bytes == s.total_pages * 8192


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
        result = mem_db.defrag(chisel.DefragOptions(sparse_threshold=0.5, max_pages=10))
    assert isinstance(result, chisel.DefragStats)
