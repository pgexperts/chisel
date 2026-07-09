import pytest
import chisel


def test_transaction_commits_on_clean_exit(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"persisted")
    # After commit, value survives
    assert mem_db.read(h) == b"persisted"


def test_transaction_rolls_back_on_exception(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        with pytest.raises(RuntimeError):
            with db.transaction() as tx:
                tx.allocate(b"discarded")
                raise RuntimeError("boom")
        assert db.handles() == []


def test_explicit_begin_commit(mem_db):
    mem_db.begin()
    h = mem_db.allocate(b"x")
    mem_db.commit()
    assert mem_db.read(h) == b"x"


def test_explicit_rollback(mem_db):
    mem_db.begin()
    mem_db.allocate(b"gone")
    mem_db.rollback()
    assert mem_db.handles() == []


def test_nested_transactions_raise(mem_db):
    with mem_db.transaction():
        with pytest.raises(chisel.ChiselError):
            with mem_db.transaction():
                pass


def test_mutators_outside_transaction_raise(mem_db):
    with pytest.raises(chisel.NoActiveTransactionError):
        mem_db.allocate(b"orphan")


def test_tx_explicit_commit(mem_db):
    # I24: explicit commit() on PyTransaction drives the engine and
    # sets the `finished` guard so __exit__ becomes a silent no-op.
    tx = mem_db.transaction()
    h = tx.allocate(b"explicit")
    tx.commit()
    assert mem_db.read(h) == b"explicit"


def test_tx_explicit_rollback(mem_db):
    # I24: explicit rollback() matches commit()'s shape.
    tx = mem_db.transaction()
    tx.allocate(b"discarded")
    tx.rollback()
    assert mem_db.handles() == []


def test_tx_second_commit_raises(mem_db):
    # I24: mirrors PySavepoint's idempotency-as-error policy — a second
    # explicit drive raises AlreadyFinishedError.
    tx = mem_db.transaction()
    tx.allocate(b"once")
    tx.commit()
    with pytest.raises(chisel.AlreadyFinishedError):
        tx.commit()


def test_tx_commit_then_with_exit_is_silent(mem_db):
    # Like savepoints: explicit commit inside a `with` block must not
    # make __exit__ raise on its way out. The `finished` guard short-
    # circuits __exit__ silently.
    with mem_db.transaction() as tx:
        tx.allocate(b"keep")
        tx.commit()
        # __exit__ here should NOT raise AlreadyFinishedError
