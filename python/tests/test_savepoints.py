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


def test_savepoint_second_release_raises(mem_db):
    # I22: a second explicit release() must raise AlreadyFinishedError
    # rather than silently succeeding. The silent no-op masked
    # "called release() on the wrong savepoint" bugs.
    with mem_db.transaction() as tx:
        sp = tx.savepoint("once")
        sp.release()
        with pytest.raises(chisel.AlreadyFinishedError):
            sp.release()


def test_savepoint_second_rollback_to_raises(mem_db):
    # I22: same idempotency-as-error rule as release().
    with mem_db.transaction() as tx:
        tx.allocate(b"before")
        sp = tx.savepoint("once")
        tx.allocate(b"after")
        sp.rollback_to()
        with pytest.raises(chisel.AlreadyFinishedError):
            sp.rollback_to()


def test_savepoint_explicit_then_with_exit_is_silent(mem_db):
    # The __exit__ path stays idempotent — the guard short-circuits
    # without raising, matching normal context-manager semantics. A
    # user who called release() inside the `with` block should not
    # see AlreadyFinishedError bubble out of the block exit.
    with mem_db.transaction() as tx:
        with tx.savepoint("ok") as sp:
            sp.release()  # explicit finish
        # __exit__ here should NOT raise AlreadyFinishedError
