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


def test_close_then_call_raises_poisoned():
    db = chisel.open(None)
    db.close()
    with pytest.raises(chisel.PoisonedError):
        db.begin()
    with pytest.raises(chisel.PoisonedError):
        db.read(0)


def test_closed_db_reports_poisoned():
    db = chisel.open(None)
    db.close()
    assert db.is_poisoned is True
