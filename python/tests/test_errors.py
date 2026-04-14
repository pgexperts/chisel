import chisel


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
