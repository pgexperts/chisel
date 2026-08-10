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
        "InvalidArgon2ParamsError",
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
        "LockFailedError",
        "UnsupportedFormatVersionError",
        "UnsupportedPageSizeError",
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


# I42 (ISSUES.md, 2026-05-22): IoError instances carry the inner
# io::Error's `errno` (raw OS error code) and `kind` (string form of
# std::io::ErrorKind) so callers can branch on them without parsing
# the message. The two tests below construct a real I/O failure (open
# a directory as if it were a database file) and inspect the resulting
# exception.

def test_io_error_carries_errno_attribute(tmp_path):
    # Opening a *directory* as a database triggers EISDIR at the
    # OpenOptions::open call inside PageIo::open. The kernel error
    # surfaces as io::Error → ChiselError::IoError → chisel.IoError
    # with the OS errno stashed as `.errno`. We don't pin to a
    # specific numeric value because Linux (21) and macOS (21) agree
    # but other platforms could differ; the contract is "errno is
    # populated and is an int".
    target = tmp_path  # tmp_path is a directory by definition
    with pytest.raises(chisel.IoError) as exc_info:
        chisel.open(str(target))
    e = exc_info.value
    assert hasattr(e, "errno"), "IoError instances should carry .errno"
    assert isinstance(e.errno, int), f"errno should be an int, got {type(e.errno).__name__}"
    assert e.errno > 0, f"errno should be a positive OS error code, got {e.errno}"


def test_io_error_carries_kind_attribute(tmp_path):
    # Companion test for `.kind`. The kind string is the Debug-format
    # of std::io::ErrorKind (e.g., "IsADirectory", "PermissionDenied",
    # "NotFound"). We pin only "is a non-empty string" — the exact
    # variant text changes across Rust versions (IsADirectory was
    # stabilized in 1.83; older toolchains may say "Other") and we
    # don't want this test to fail on a Rust upgrade.
    with pytest.raises(chisel.IoError) as exc_info:
        chisel.open(str(tmp_path))
    e = exc_info.value
    assert hasattr(e, "kind"), "IoError instances should carry .kind"
    assert isinstance(e.kind, str), f"kind should be a str, got {type(e.kind).__name__}"
    assert e.kind != "", "kind should not be empty"


def test_io_error_is_oserror_subclass():
    # Idiomatic Python expects disk-level failures to be catchable as OSError
    # and to expose .errno through OSError's native slot. IoError must subclass
    # BOTH chisel.FatalError (the two-tier poison contract — see
    # test_fatal_hierarchy) AND the builtin OSError.
    assert issubclass(chisel.IoError, OSError)
    assert issubclass(chisel.IoError, chisel.FatalError)


def test_io_error_catchable_as_oserror(tmp_path):
    # The whole point of the OSError base: `except OSError as e: if e.errno ==
    # errno.ENOSPC` catches a Chisel disk-level error and reads errno natively.
    with pytest.raises(OSError) as exc_info:
        chisel.open(str(tmp_path))  # opening a directory triggers EISDIR
    e = exc_info.value
    assert isinstance(e, chisel.IoError)
    assert isinstance(e, chisel.FatalError)
    assert isinstance(e.errno, int) and e.errno > 0


def test_non_io_error_does_not_carry_errno(tmp_db):
    # Sanity: errno/kind are IoError-specific. Other ChiselError
    # subclasses do NOT get them attached — to_py_err's match is
    # arm-specific. Verifies by triggering LockFailedError (open the
    # same file twice in this process), which routes through a
    # different match arm.
    with chisel.open(str(tmp_db)):
        with pytest.raises(chisel.LockFailedError) as exc_info:
            chisel.open(str(tmp_db))
    e = exc_info.value
    assert not hasattr(e, "errno") or e.errno is None
    # `kind` may exist as a Python-defined attribute on the base
    # class in future, but today it should be absent from non-IoError.
    assert not hasattr(e, "kind"), \
        f"unexpected .kind attribute on {type(e).__name__}"
