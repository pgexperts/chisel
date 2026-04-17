from dataclasses import dataclass

from chisel._chisel import (
    __version__,
    Chisel,
    Transaction,
    Savepoint,
    open,
    ChiselError,
    OperationalError,
    FatalError,
    InvalidHandleError,
    NoActiveTransactionError,
    TransactionAlreadyActiveError,
    SavepointNotFoundError,
    DuplicateSavepointError,
    ReadOnlyModeError,
    DatabaseFileNotFoundError,
    InvalidRootNameError,
    RootNameTableFullError,
    InvalidSuperblockCountError,
    CacheFullError,
    ClosedError,
    AlreadyFinishedError,
    IoError,
    ChecksumMismatchError,
    CorruptSuperblockError,
    FileSizeMismatchError,
    InvalidMagicError,
    LockFailedError,
    UnsupportedFormatVersionError,
    CorruptPageError,
    InvalidPageIdError,
    PoisonedError,
)


# Structured return types for stats() / defrag().
#
# These live on the Python side (not constructed in Rust) so that users get
# real @dataclass instances with the usual repr/equality/immutability
# guarantees. The Rust binding imports this module and calls the class by
# name to build instances — see python/src/db.rs stats()/defrag().
#
# `frozen=True` matches the read-only nature of the values: a Stats snapshot
# is a point-in-time observation, and DefragOptions/DefragStats are
# request/response records that should not mutate after construction.


@dataclass(frozen=True)
class Stats:
    """Read-only summary of database size/usage at the time stats() was called."""
    handle_count: int
    total_pages: int
    file_size_bytes: int


@dataclass(frozen=True)
class DefragOptions:
    """Options controlling a defragmentation pass.

    sparse_threshold: fraction in [0, 1]. A data page is considered sparse
        when live-slot-count <= threshold * max_observed. Default 0.25.
    max_pages: cap on pages examined in one pass; 0 means no limit.
        Default 0.
    """
    sparse_threshold: float = 0.25
    max_pages: int = 0


@dataclass(frozen=True)
class DefragStats:
    """Summary returned by defrag()."""
    pages_examined: int = 0
    pages_freed: int = 0
    values_moved: int = 0


__all__ = [
    "__version__",
    "Chisel", "Transaction", "Savepoint", "open",
    "Stats", "DefragOptions", "DefragStats",
    "ChiselError", "OperationalError", "FatalError",
    "InvalidHandleError", "NoActiveTransactionError",
    "TransactionAlreadyActiveError", "SavepointNotFoundError",
    "DuplicateSavepointError", "ReadOnlyModeError", "DatabaseFileNotFoundError",
    "InvalidRootNameError", "RootNameTableFullError",
    "InvalidSuperblockCountError", "CacheFullError",
    "ClosedError", "AlreadyFinishedError",
    "IoError", "ChecksumMismatchError", "CorruptSuperblockError",
    "FileSizeMismatchError", "InvalidMagicError", "LockFailedError",
    "UnsupportedFormatVersionError", "CorruptPageError", "InvalidPageIdError",
    "PoisonedError",
]
