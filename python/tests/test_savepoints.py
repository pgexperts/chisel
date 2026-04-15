import pytest
import chisel


def test_savepoint_release_on_clean_exit(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"before")
        with tx.savepoint("sp1"):
            h2 = tx.allocate(b"after")
        # clean exit -> release; both values survive
    assert sorted(mem_db.handles()) == sorted([h1, h2])


def test_savepoint_rollback_on_exception(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"keep")
        with pytest.raises(RuntimeError):
            with tx.savepoint("sp1"):
                tx.allocate(b"discard")
                raise RuntimeError("boom")
        # outer transaction still valid; h1 preserved
    assert mem_db.handles() == [h1]


def test_nested_savepoints(mem_db):
    with mem_db.transaction() as tx:
        h_outer = tx.allocate(b"outer")
        with tx.savepoint("outer"):
            h_mid = tx.allocate(b"mid")
            with pytest.raises(RuntimeError):
                with tx.savepoint("inner"):
                    tx.allocate(b"innermost")
                    raise RuntimeError("boom")
            # inner rolled back; outer + mid preserved
    assert sorted(mem_db.handles()) == sorted([h_outer, h_mid])


def test_explicit_savepoint_methods(mem_db):
    with mem_db.transaction() as tx:
        h1 = tx.allocate(b"a")
        sp = tx.savepoint("manual")
        tx.allocate(b"b")
        sp.rollback_to()
        # After rollback_to, the savepoint is consumed
    assert mem_db.handles() == [h1]


def test_savepoint_exposes_name(mem_db):
    with mem_db.transaction() as tx:
        sp = tx.savepoint("mark")
        assert sp.name == "mark"
        sp.release()
