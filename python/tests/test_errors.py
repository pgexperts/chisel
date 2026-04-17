import chisel
import pytest


def test_base_error_is_exception():
    assert issubclass(chisel.ChiselError, Exception)


def test_operational_hierarchy():
    assert issubclass(chisel.OperationalError, chisel.ChiselError)
    for cls_name in [
        "InvalidHandleError",
        "NoActiveTransactionError",
        "TransactionAlreadyActiveError",
        "SavepointNotFoundError",
        "DuplicateSavepointError",
        "ReadOnlyModeError",
        "DatabaseFileNotFoundError",
        "InvalidRootNameError",
        "RootNameTableFullError",
        "InvalidSuperblockCountError",
        "CacheFullError",
        "ClosedError",
        "AlreadyFinishedError",
    ]:
        cls = getattr(chisel, cls_name)
        assert issubclass(cls, chisel.OperationalError)


def test_fatal_hierarchy():
    assert issubclass(chisel.FatalError, chisel.ChiselError)
    for cls_name in [
        "IoError",
        "ChecksumMismatchError",
        "CorruptSuperblockError",
        "FileSizeMismatchError",
        "InvalidMagicError",
        "LockFailedError",
        "UnsupportedFormatVersionError",
        "CorruptPageError",
        "InvalidPageIdError",
        "PoisonedError",
    ]:
        cls = getattr(chisel, cls_name)
        assert issubclass(cls, chisel.FatalError)


def test_operational_and_fatal_are_disjoint():
    assert not issubclass(chisel.OperationalError, chisel.FatalError)
    assert not issubclass(chisel.FatalError, chisel.OperationalError)


def test_is_poisoned_false_on_fresh_db(mem_db):
    assert mem_db.is_poisoned is False


def test_close_then_call_raises_closed():
    # I25: closing the handle and then using it must raise ClosedError,
    # NOT PoisonedError — the on-disk DB is intact, the user just asked
    # for the handle to go away.
    db = chisel.open(None)
    db.close()
    with pytest.raises(chisel.ClosedError):
        db.begin()
    with pytest.raises(chisel.ClosedError):
        db.read(0)


def test_closed_error_is_not_poisoned_error():
    # Belt-and-braces: ClosedError must not be a PoisonedError subclass;
    # the distinction is the whole point of I25. Code that explicitly
    # wants to handle poison-vs-close differently relies on this.
    assert not issubclass(chisel.ClosedError, chisel.PoisonedError)


def test_close_inside_transaction_surfaces_as_closed(mem_db):
    # Regression for I25's motivating scenario: calling db.close()
    # inside a `with db.transaction()` block cancels the transaction,
    # and the __exit__'s commit must surface ClosedError (not
    # PoisonedError).
    with pytest.raises(chisel.ClosedError):
        with mem_db.transaction() as tx:
            tx.allocate(b"x")
            mem_db.close()
            # __exit__ fires here, attempts commit, sees closed db


def test_closed_db_reports_poisoned():
    # `is_poisoned` treats closed as poisoned because it answers the
    # "can this handle still produce results?" question — which a
    # closed handle can't. Users who need to distinguish close from
    # poison rely on the exception CLASS raised by operations, not on
    # the is_poisoned getter.
    db = chisel.open(None)
    db.close()
    assert db.is_poisoned is True
