# test_exception_contract.py — parametrized typed-exception contract tests (I139).
#
# Each test verifies that a specific error path raises the *exact* concrete
# Python exception class documented in errors.rs, not just a base class.  The
# goal is to make a swapped `to_py_err` arm fail loud rather than pass green.
#
# Where a variant is also an instance of the correct base tier
# (OperationalError / FatalError) that is checked too, pinning the two-tier
# hierarchy contract.
#
# Engine constants referenced below:
#   PAGE_SIZE         = 8192 bytes
#   NAMED_ROOT_COUNT  = 8          (fixed-size slot table)
#   NAMED_ROOT_NAME_LEN = 24 bytes (max root name length)

import threading
import chisel
import pytest

# ---------------------------------------------------------------------------
# 1. SavepointNotFoundError
# ---------------------------------------------------------------------------


def test_savepoint_not_found_via_rollback_to(mem_db):
    # Build a stack [sp1, sp2], then use sp1.rollback_to() which pops BOTH
    # sp1 and sp2 from the engine.  The sp2 Python object still exists and
    # its guard is NOT set (we never called sp2.release/rollback_to), so
    # sp2.release() goes to the engine which no longer knows "sp2" →
    # SavepointNotFoundError.
    with mem_db.transaction() as tx:
        sp1 = tx.savepoint("sp1")
        sp2 = tx.savepoint("sp2")
        sp1.rollback_to()  # pops both sp1 and sp2 from the engine stack
        # sp2's guard is still clear → this reaches the engine → not found
        with pytest.raises(chisel.SavepointNotFoundError) as exc_info:
            sp2.release()
    assert isinstance(exc_info.value, chisel.OperationalError)


def test_savepoint_not_found_via_release(mem_db):
    # Same popping trick but exercising rollback_to on the orphaned object.
    with mem_db.transaction() as tx:
        sp1 = tx.savepoint("sp1")
        sp2 = tx.savepoint("sp2")
        sp1.rollback_to()  # pops sp1 and sp2
        with pytest.raises(chisel.SavepointNotFoundError) as exc_info:
            sp2.rollback_to()
    assert isinstance(exc_info.value, chisel.OperationalError)


# ---------------------------------------------------------------------------
# 2. DuplicateSavepointError
# ---------------------------------------------------------------------------


def test_duplicate_savepoint_raises(mem_db):
    # Creating the same savepoint name twice inside one transaction must raise
    # DuplicateSavepointError on the second call.
    with mem_db.transaction() as tx:
        sp = tx.savepoint("dup")
        with pytest.raises(chisel.DuplicateSavepointError) as exc_info:
            tx.savepoint("dup")
        sp.release()
    assert isinstance(exc_info.value, chisel.OperationalError)


# ---------------------------------------------------------------------------
# 3. InvalidRootNameError
# ---------------------------------------------------------------------------
# Name rules (from src/transaction.rs encode_root_name):
#   - must be non-empty
#   - must be <= NAMED_ROOT_NAME_LEN (24) bytes
#   - must not contain NUL bytes
#   All are valid UTF-8 at the Python boundary (str → bytes via as_bytes).


@pytest.mark.parametrize("bad_name,reason", [
    ("", "empty string"),
    ("x" * 25, "exceeds 24-byte limit"),
    ("null\x00byte", "contains NUL byte"),
])
def test_invalid_root_name_raises(mem_db, bad_name, reason):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"anchor")
        with pytest.raises(chisel.InvalidRootNameError) as exc_info:
            tx.set_root_name(bad_name, h)
    assert isinstance(exc_info.value, chisel.OperationalError), \
        f"InvalidRootNameError must be OperationalError ({reason})"


# ---------------------------------------------------------------------------
# 4. RootNameTableFullError
# ---------------------------------------------------------------------------
# NAMED_ROOT_COUNT = 8; fill all 8 slots then try a ninth distinct name.


def test_root_name_table_full_raises(mem_db):
    NAMED_ROOT_COUNT = 8
    handles = []
    with mem_db.transaction() as tx:
        for i in range(NAMED_ROOT_COUNT):
            h = tx.allocate(bytes([i]))
            handles.append(h)
            # Each slot name is distinct and fits within 24 bytes.
            tx.set_root_name(f"slot{i}", h)

    # One more distinct name must fail — the table is full.
    with mem_db.transaction() as tx:
        h_extra = tx.allocate(b"overflow")
        with pytest.raises(chisel.RootNameTableFullError) as exc_info:
            tx.set_root_name("overflow", h_extra)
    assert isinstance(exc_info.value, chisel.OperationalError)


# ---------------------------------------------------------------------------
# 5. TagMismatchError
# ---------------------------------------------------------------------------
# delete_tagged(handle, wrong_tag) must raise TagMismatchError exactly.
# (Also exercised in test_tags.py; here we also pin the hierarchy.)


def test_tag_mismatch_exact_class(mem_db):
    mem_db.begin()
    h = mem_db.allocate_tagged(b"val", 7)
    with pytest.raises(chisel.TagMismatchError) as exc_info:
        mem_db.delete_tagged(h, 8)   # wrong tag
    mem_db.commit()
    assert isinstance(exc_info.value, chisel.OperationalError)


# ---------------------------------------------------------------------------
# 6. TransactionInProgressError
# ---------------------------------------------------------------------------
# Configuration mutators raise this when called inside an active transaction.
# (Also exercised in test_runtime_config.py; here we pin the hierarchy.)


def test_transaction_in_progress_exact_class(mem_db):
    mem_db.begin()
    with pytest.raises(chisel.TransactionInProgressError) as exc_info:
        mem_db.set_cache_max_bytes(4 * 1024 * 1024)
    mem_db.rollback()
    assert isinstance(exc_info.value, chisel.OperationalError)


# ---------------------------------------------------------------------------
# 7. CacheFullError
# ---------------------------------------------------------------------------
# Open with a very small cache (16 pages × 8 192 B = 128 KiB) and the
# spillway DISABLED (spillway_max_bytes=0).  Allocate large values inside
# one transaction until the engine cannot evict any more clean pages and
# CacheFull fires.
#
# Each value slightly larger than one page forces multi-page overflow chains,
# so each allocation consumes 2+ dirty pages and the small cap is saturated
# quickly.  We iterate up to 200 times — far more than the ~12 allocations
# needed to saturate 16 pages — to be insensitive to minor engine overhead.


def test_cache_full_exact_class(tmp_db):
    PAGE_SIZE = 8192
    cache_bytes = 16 * PAGE_SIZE  # 131 072 bytes, 16 pages
    large_value = bytes(PAGE_SIZE + 32)  # slightly larger than one page

    with chisel.open(str(tmp_db), cache_max_bytes=cache_bytes,
                     spillway_max_bytes=0) as db:
        db.begin()
        cache_full_raised = False
        for _ in range(200):
            try:
                db.allocate(large_value)
            except chisel.CacheFullError as exc:
                assert isinstance(exc, chisel.OperationalError)
                cache_full_raised = True
                break
        assert cache_full_raised, (
            "Expected CacheFullError within 200 large allocations "
            f"(cache={cache_bytes} bytes, spillway disabled)"
        )
        db.rollback()


# ---------------------------------------------------------------------------
# 8. SpillwayFullError
# ---------------------------------------------------------------------------
# Open with a small cache AND a tiny non-zero spillway cap.  Once the cache
# is full the spillway accepts overflow; once the spillway cap is also hit,
# SpillwayFullError fires.  The spillway cap must be larger than the cache so
# the engine actually tries to use it before hitting it.
#
# Strategy: cache = 16 pages (128 KiB), spillway = 256 KiB (just 2× the
# cache).  Allocate large values; once CacheFullError would have fired without
# a spillway, the spillway absorbs them until it too fills up.


def test_spillway_full_exact_class(tmp_db):
    PAGE_SIZE = 8192
    cache_bytes = 16 * PAGE_SIZE          # 128 KiB
    spillway_bytes = 32 * PAGE_SIZE       # 256 KiB (small enough to saturate)
    large_value = bytes(PAGE_SIZE + 32)

    with chisel.open(str(tmp_db), cache_max_bytes=cache_bytes,
                     spillway_max_bytes=spillway_bytes) as db:
        db.begin()
        overflow_raised = False
        for _ in range(500):
            try:
                db.allocate(large_value)
            except (chisel.SpillwayFullError, chisel.CacheFullError) as exc:
                # With a spillway enabled we expect SpillwayFullError once both
                # the cache and the spillway are exhausted.  CacheFullError
                # would be a mis-routing; assert the concrete class explicitly.
                assert isinstance(exc, chisel.SpillwayFullError), (
                    f"Expected SpillwayFullError, got {type(exc).__name__}"
                )
                assert isinstance(exc, chisel.OperationalError)
                overflow_raised = True
                break
        assert overflow_raised, (
            "Expected SpillwayFullError within 500 large allocations "
            f"(cache={cache_bytes}, spillway={spillway_bytes} bytes)"
        )
        db.rollback()


# ---------------------------------------------------------------------------
# 9. Two-thread contention test
# ---------------------------------------------------------------------------
# Background: PyChisel wraps chisel::Chisel in a Mutex (I75, required for
# Sync in PyO3 0.24+).  The existing test_threading.py tests only the
# "migration" pattern (join before re-touching), never exercising the Mutex
# under true concurrent lock contention.
#
# This test runs TWO threads that each try to begin/allocate/commit in a
# tight loop while the other is also running.  The GIL plus the Mutex together
# serialize all Chisel calls — no panic, no data corruption, and the
# committed handles are all readable after both threads finish.
#
# We do NOT try to provoke a poisoned Mutex here.  The engine never panics
# while holding the lock (fatal conditions return ChiselError::Poisoned as a
# normal Err, not an unwind), so the Mutex should never be left poisoned in
# practice.  The contention test asserts:
#
#   a) Concurrent access does not panic or deadlock (both threads finish).
#   b) All committed values are readable after both threads join.
#   c) The db is not reported as poisoned.
#
# Determinism: 20 iterations each, bounded join timeout (5 s), shared list
# protected by the GIL (appending Python ints is GIL-safe).


def test_two_thread_mutex_contention(tmp_db):
    ITERATIONS = 20
    db = chisel.open(str(tmp_db))
    handles_a = []
    handles_b = []
    errors = []

    def worker(label, payload, out_list):
        for i in range(ITERATIONS):
            try:
                with db.transaction() as tx:
                    h = tx.allocate(payload + bytes([i % 256]))
                    out_list.append(h)
            except chisel.ChiselError as exc:
                errors.append((label, i, exc))
                break

    t_a = threading.Thread(target=worker, args=(
        "A", b"thread-a-", handles_a), daemon=True)
    t_b = threading.Thread(target=worker, args=(
        "B", b"thread-b-", handles_b), daemon=True)

    t_a.start()
    t_b.start()
    t_a.join(timeout=5.0)
    t_b.join(timeout=5.0)

    assert not t_a.is_alive(), "Thread A did not finish within 5 s — possible deadlock"
    assert not t_b.is_alive(), "Thread B did not finish within 5 s — possible deadlock"
    assert not errors, f"Unexpected ChiselError during concurrent access: {errors}"

    # All committed handles must be readable now that both threads are done.
    all_handles = handles_a + handles_b
    assert len(all_handles) == ITERATIONS * 2, (
        f"Expected {ITERATIONS * 2} committed handles, got {len(all_handles)}"
    )
    for h in all_handles:
        data = db.read(h)
        assert len(data) > 0

    assert not db.is_poisoned
    db.close()


# ---------------------------------------------------------------------------
# 10. Extend test_operational_hierarchy to cover the missing classes
# ---------------------------------------------------------------------------
# test_errors.py::test_operational_hierarchy was missing SpillwayFullError,
# TransactionInProgressError, and TagMismatchError (noted in I139).


def test_operational_hierarchy_missing_classes():
    """Pin the three OperationalError subclasses omitted from test_errors.py."""
    for cls_name in ("SpillwayFullError", "TransactionInProgressError", "TagMismatchError"):
        cls = getattr(chisel, cls_name)
        assert issubclass(cls, chisel.OperationalError), \
            f"{cls_name} must be a subclass of OperationalError"
        assert issubclass(cls, chisel.ChiselError), \
            f"{cls_name} must be a subclass of ChiselError"
        assert not issubclass(cls, chisel.FatalError), \
            f"{cls_name} must NOT be a subclass of FatalError"
