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


# --- PYTHON-1 (issue #105): a finished Transaction is dead ------------------
#
# The one-shot `finished` guard was checked by commit()/rollback() but by none
# of the ~20 data methods, which delegated straight to the Chisel object. A
# PyTransaction carries no transaction identity — only a handle on the database
# — so a finished wrapper silently re-bound to whatever transaction happened to
# be open.


def test_finished_tx_cannot_write_into_a_later_transaction(mem_db):
    """The reproducer: a stale tx object injecting a write into tx2's work."""
    tx1 = mem_db.transaction()
    tx1.commit()

    with mem_db.transaction() as tx2:
        with pytest.raises(chisel.AlreadyFinishedError):
            tx1.allocate(b"leaked-into-tx2")
        tx2.allocate(b"legitimate")

    # Exactly one value, and it is tx2's. Before the fix the leaked value was
    # committed by tx2 and this list had two entries.
    assert len(mem_db.handles()) == 1
    assert mem_db.read(mem_db.handles()[0]) == b"legitimate"


# Every method that must refuse to run on a finished transaction. Mutators AND
# reads: a post-finish read does not see the finished transaction's snapshot,
# it sees whatever the engine currently holds, including another transaction's
# uncommitted writes. `defrag` is on this list because it relocates values and
# frees pages — the issue's own enumeration omitted it.
_GUARDED_TX_METHODS = [
    ("allocate", (b"x",)),
    ("read", (1,)),
    ("update", (1, b"x")),
    ("delete", (1,)),
    ("delete_many", ([1],)),
    ("allocate_tagged", (b"x", 1)),
    ("tag", (1,)),
    ("client_byte", (1,)),
    ("set_client_byte", (1, 0)),
    ("handles_with_tag", (1,)),
    ("delete_tagged", (1, 1)),
    ("delete_with_tag", (1, 10)),
    ("set_root_name", ("n", 1)),
    ("get_root_name", ("n",)),
    ("clear_root_name", ("n",)),
    ("handles", ()),
    ("stats", ()),
    ("counters", ()),
    ("defrag", ()),
    ("savepoint", ("s",)),
]


@pytest.mark.parametrize("name,args", _GUARDED_TX_METHODS, ids=lambda v: v if isinstance(v, str) else "")
def test_finished_tx_methods_all_raise(mem_db, name, args):
    """Coverage sweep. This is the test that catches a method being missed.

    AlreadyFinishedError must be raised BEFORE any argument validation, so the
    deliberately bogus handles/names below never reach the engine — if a guard
    is missing, the call fails with some other error (or succeeds) and this
    fails loudly rather than passing by accident.
    """
    tx = mem_db.transaction()
    tx.commit()
    with pytest.raises(chisel.AlreadyFinishedError):
        getattr(tx, name)(*args)
