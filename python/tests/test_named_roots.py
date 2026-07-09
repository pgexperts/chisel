import pytest
import chisel


def test_set_and_get_root_name(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"meta")
        tx.set_root_name("meta", h)
    assert mem_db.get_root_name("meta") == h


def test_get_missing_root_returns_none(mem_db):
    assert mem_db.get_root_name("missing") is None


def test_clear_root_name(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"x")
        tx.set_root_name("foo", h)
    assert mem_db.get_root_name("foo") == h
    with mem_db.transaction() as tx:
        tx.clear_root_name("foo")
    assert mem_db.get_root_name("foo") is None


def test_root_name_rolled_back_with_transaction(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"pre")
        tx.set_root_name("stable", h)
    with pytest.raises(RuntimeError):
        with mem_db.transaction() as tx:
            tx.clear_root_name("stable")
            raise RuntimeError("nope")
    assert mem_db.get_root_name("stable") == h


def test_handles_enumerates_live_values(mem_db):
    handles = []
    with mem_db.transaction() as tx:
        for i in range(5):
            handles.append(tx.allocate(bytes([i])))
    assert sorted(mem_db.handles()) == sorted(handles)


def test_handles_excludes_deleted(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"keep")
        h2 = tx.allocate(b"drop")
        tx.delete(h2)
    assert mem_db.handles() == [h1]
