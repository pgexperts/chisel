"""Python bindings for the Chisel transactional storage engine.

Handles and tags are plain integers. Tags are integers >= 1: ``0`` is not a
valid tag — create an untagged value with ``allocate`` (not ``allocate_tagged``),
and ``tag()`` returns ``None`` for an untagged handle. Passing ``tag=0`` to a
tagged operation (``allocate_tagged``, ``delete_tagged``, ``handles_with_tag``,
``delete_with_tag``) raises ``ValueError``.
"""

from dataclasses import dataclass
from typing import Optional

from chisel._chisel import (
    __version__,
    Chisel,
    Transaction,
    Savepoint,
    DrainInsertion,
    open,
    ChiselError,
    OperationalError,
    FatalError,
    InvalidHandleError,
    NoActiveTransactionError,
    TransactionAlreadyActiveError,
    TransactionInProgressError,
    SavepointNotFoundError,
    SpillwayFullError,
    DuplicateSavepointError,
    ReadOnlyModeError,
    DatabaseFileNotFoundError,
    InvalidRootNameError,
    RootNameTableFullError,
    InvalidSuperblockCountError,
    CacheFullError,
    ClosedError,
    AlreadyFinishedError,
    TagMismatchError,
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
    # I74 (ISSUES.md, 2026-05-22): the two spillway fields are Optional
    # because the spillway is lazily opened on first overflow spill.
    # None means "no overflow has happened yet on this handle"; Some(0)
    # means "spillway exists but is currently empty (just truncated by
    # commit/rollback)." Operators predict SpillwayFullError by
    # watching spillway_logical_bytes / spillway_max_bytes climb across
    # commits. Defaults to None for backward compatibility with code
    # that constructs Stats(...) without the new kwargs.
    spillway_logical_bytes: Optional[int] = None
    spillway_max_bytes: Optional[int] = None


@dataclass(frozen=True)
class Counters:
    """Cumulative engine-activity counters since the database was opened.

    Snapshot semantics: the values are point-in-time and do not update.
    Read counters() again to observe new totals. Counters reset implicitly
    on close + reopen because the underlying engine state is rebuilt.

    Fields:
        cache_hits: PageCache.get returned a cached page without disk I/O.
        cache_misses: PageCache.get had to load from disk.
        pages_allocated: PageCache.new_page invocations.
        fsync_calls: PageIo.fsync invocations (three per commit: pre-drain
            flush + main pages flush + superblock).
    """
    cache_hits: int
    cache_misses: int
    pages_allocated: int
    fsync_calls: int


@dataclass(frozen=True)
class DefragOptions:
    """Options controlling a defragmentation pass.

    sparse_threshold: fraction in [0, 1]. A data page is considered sparse
        when live-slot-count <= threshold * max_observed. Default 0.25.
    max_values: cap on VALUES relocated in one pass; 0 means no limit.
        The current implementation caps value moves, which is the useful
        knob for bounding pass cost. Default 0.
    """
    sparse_threshold: float = 0.25
    max_values: int = 0


@dataclass(frozen=True)
class DefragStats:
    """Summary returned by defrag()."""
    pages_examined: int = 0
    pages_freed: int = 0
    values_moved: int = 0


__all__ = [
    "__version__",
    "Chisel", "Transaction", "Savepoint", "DrainInsertion", "open",
    "Stats", "Counters", "DefragOptions", "DefragStats",
    "ChiselError", "OperationalError", "FatalError",
    "InvalidHandleError", "NoActiveTransactionError",
    "TransactionAlreadyActiveError", "TransactionInProgressError",
    "SavepointNotFoundError", "SpillwayFullError",
    "DuplicateSavepointError", "ReadOnlyModeError", "DatabaseFileNotFoundError",
    "InvalidRootNameError", "RootNameTableFullError",
    "InvalidSuperblockCountError", "CacheFullError",
    "ClosedError", "AlreadyFinishedError", "TagMismatchError",
    "IoError", "ChecksumMismatchError", "CorruptSuperblockError",
    "FileSizeMismatchError", "InvalidMagicError", "LockFailedError",
    "UnsupportedFormatVersionError", "CorruptPageError", "InvalidPageIdError",
    "PoisonedError",
]
