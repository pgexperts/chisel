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
    # Build a stack [sp1, sp2], then use sp1.rollback_to(), which pops
    # everything layered ON TOP of sp1 — so sp2 goes and sp1 stays (the
    # engine retains the named savepoint so it can be rolled back to again;
    # src/transaction/savepoints.rs:47-49).  The sp2 Python object still
    # exists and its guard is NOT set (we never called
    # sp2.release/rollback_to), so sp2.release() goes to the engine, which
    # no longer knows "sp2" → SavepointNotFoundError.
    with mem_db.transaction() as tx:
        sp1 = tx.savepoint("sp1")
        sp2 = tx.savepoint("sp2")
        sp1.rollback_to()  # pops sp2; sp1 itself remains on the engine stack
        # sp2's guard is still clear → this reaches the engine → not found
        with pytest.raises(chisel.SavepointNotFoundError) as exc_info:
            sp2.release()
    assert isinstance(exc_info.value, chisel.OperationalError)


def test_savepoint_not_found_via_release(mem_db):
    # Same popping trick but exercising rollback_to on the orphaned object.
    with mem_db.transaction() as tx:
        sp1 = tx.savepoint("sp1")
        sp2 = tx.savepoint("sp2")
        sp1.rollback_to()  # pops sp2; sp1 itself remains on the engine stack
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
    # I139 / I75: the binding wraps the engine in a Mutex (required for Sync in
    # PyO3 0.24+). Per-op calls do NOT release the GIL (see db.rs
    # with_inner_io / with_inner_mut_io — only open() releases it); the GIL
    # serializes every Chisel call across threads. The Mutex is a Sync-
    # satisfying wrapper, not the primary serialization point here. Drive two
    # threads concurrently hammering READS of the same handles to confirm no
    # panic, no deadlock, no data corruption — the GIL serializes their calls.
    #
    # The threads do READS, not begin/commit cycles, ON PURPOSE: the single-
    # writer transaction state is a SHARED resource. Even with GIL serialization,
    # two threads independently transacting on one Chisel would race on
    # `TransactionAlreadyActive` (the engine is single-writer by design — see the
    # single-client design note). Reads take `&self` and need no active
    # transaction, so they are the correct way to exercise GIL-serialized
    # concurrent access.
    ITERATIONS = 200
    db = chisel.open(str(tmp_db))
    # Single-threaded setup: commit a handful of handles to read concurrently.
    # Build a mapping handle -> expected payload so the reader can verify the
    # exact bytes rather than using a length-zero sentinel (b"" is a valid
    # stored value, so len == 0 would be a fragile proxy for corruption).
    handle_payloads = {}
    with db.transaction() as tx:
        for i in range(8):
            payload = b"payload-" + bytes([i])
            h = tx.allocate(payload)
            handle_payloads[h] = payload
    handles = list(handle_payloads.keys())
    errors = []

    def reader():
        try:
            for _ in range(ITERATIONS):
                for h in handles:
                    got = db.read(h)
                    if got != handle_payloads[h]:
                        errors.append(("corrupt read", h, got))
        except chisel.ChiselError as exc:
            errors.append(("error", exc))

    t_a = threading.Thread(target=reader, daemon=True)
    t_b = threading.Thread(target=reader, daemon=True)
    t_a.start()
    t_b.start()
    t_a.join(timeout=10.0)
    t_b.join(timeout=10.0)

    assert not t_a.is_alive(), "Thread A did not finish within 10 s — possible deadlock"
    assert not t_b.is_alive(), "Thread B did not finish within 10 s — possible deadlock"
    assert not errors, f"Unexpected error during concurrent reads: {errors}"
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


# ---------------------------------------------------------------------------
# 11. Fatal-arm routing: a real fatal error reaches its concrete class
# ---------------------------------------------------------------------------
# The operational arms of to_py_err have contract tests above; the fatal arms
# did not — a swapped/mis-routed fatal arm would pass the whole green suite
# (review 2026-06-22). Drive the highest-value fatal variant end-to-end through
# the binding: corrupt every superblock so open() surfaces CorruptSuperblock.
# (Per-variant coverage of all fatal arms would want a Rust-side unit test of
# to_py_err; that needs binding-crate test infrastructure tracked separately.)


def test_corrupt_superblock_routes_to_fatal_class(tmp_db):
    # superblock_count=2 puts the only superblock slots at pages 0 and 1.
    with chisel.open(str(tmp_db), superblock_count=2) as db:
        with db.transaction() as tx:
            tx.allocate(b"x")
    # Zero both superblock slots (PAGE_SIZE = 8192) WITHOUT resizing the file
    # (r+b does not truncate), so the failure is bad magic, not a size mismatch.
    with open(tmp_db, "r+b") as f:
        f.write(b"\x00" * (2 * 8192))
    with pytest.raises(chisel.CorruptSuperblockError) as exc_info:
        chisel.open(str(tmp_db))
    # It must ALSO be a FatalError: `except chisel.FatalError` must catch every
    # drop-and-reopen condition (the two-tier poison contract).
    assert isinstance(exc_info.value, chisel.FatalError)


# ---------------------------------------------------------------------------
# 12. Encryption exception contract (Phase 4, Tasks 4.4 / 4.5)
# ---------------------------------------------------------------------------
# Four ChiselError encryption variants map to typed Python classes in
# to_py_err. Pin the concrete class AND the tier base for each, matching the
# per-variant contract pattern above.
#
#   NoEncryptionKey        -> NoEncryptionKeyError        (OperationalError)
#   InvalidEncryptionKey   -> InvalidEncryptionKeyError   (OperationalError)
#   EncryptionNotSupported -> EncryptionNotSupportedError (OperationalError)
#   DecryptionFailed       -> DecryptionFailedError       (FatalError)
#
# The three operational variants are reachable end-to-end from Python at
# open() time and are TRIGGERED below. DecryptionFailed fires on a per-PAGE MAC
# verification failure during a read after a successful open, and (issue #119)
# on the sealed superblock body during open itself once a key slot has already
# unwrapped. Either way it requires tampering with ciphertext/tag bytes at a
# precise on-disk offset (the 8232-byte encrypted stride) AND restamping the
# XXH3 page checksum, which covers the sealed body -- tampering the ciphertext
# alone fails `deserialize`, so the slot is filtered out before selection and
# the open reports sibling fallback or CorruptSuperblock instead. That is the same
# "hard-to-trigger fatal, needs binding-crate test infra" class as the
# per-variant fatal coverage noted in test 11. It is therefore pinned by the
# issubclass(FatalError) hierarchy check only, not triggered here.

RAW_KEY = b"\xcd" * 32


def _make_encrypted_db(path):
    """Create an encrypted DB at `path` with one committed value, then close."""
    with chisel.open(str(path), encryption_key=RAW_KEY) as db:
        with db.transaction() as tx:
            tx.allocate(b"secret")


def test_no_encryption_key_exact_class(tmp_db):
    # Encrypted DB reopened with NO key -> NoEncryptionKeyError at open time.
    _make_encrypted_db(tmp_db)
    with pytest.raises(chisel.NoEncryptionKeyError) as exc_info:
        chisel.open(str(tmp_db), create_if_missing=False)
    assert isinstance(exc_info.value, chisel.OperationalError)


def test_invalid_encryption_key_exact_class(tmp_db):
    # Encrypted DB reopened with the WRONG key -> InvalidEncryptionKeyError.
    _make_encrypted_db(tmp_db)
    with pytest.raises(chisel.InvalidEncryptionKeyError) as exc_info:
        chisel.open(str(tmp_db), create_if_missing=False, encryption_key=b"\x00" * 32)
    assert isinstance(exc_info.value, chisel.OperationalError)


def test_encryption_not_supported_exact_class(tmp_db):
    # Plaintext DB reopened WITH a key -> EncryptionNotSupportedError.
    with chisel.open(str(tmp_db)) as db:
        with db.transaction() as tx:
            tx.allocate(b"plain")
    with pytest.raises(chisel.EncryptionNotSupportedError) as exc_info:
        chisel.open(str(tmp_db), create_if_missing=False, encryption_key=RAW_KEY)
    assert isinstance(exc_info.value, chisel.OperationalError)


def test_decryption_failed_is_fatal_hierarchy():
    # DecryptionFailed is not reachable from pure Python without precise
    # per-page ciphertext tampering (see the group comment above); pin its
    # tier via issubclass only. issubclass does not require an instance.
    assert issubclass(chisel.DecryptionFailedError, chisel.FatalError)
    assert issubclass(chisel.DecryptionFailedError, chisel.ChiselError)
    assert not issubclass(chisel.DecryptionFailedError, chisel.OperationalError)


# ---------------------------------------------------------------------------
# 13. Key-rotation exception contract (Phase 5, Task 5.5)
# ---------------------------------------------------------------------------
# Two ChiselError key-rotation variants map to typed Python classes:
#
#   NoFreeKeySlot  -> NoFreeKeySlotError  (OperationalError)
#   LastKeySlot    -> LastKeySlotError    (OperationalError)
#
# NoFreeKeySlotError is triggered end-to-end: fill all 8 key slots (add_key
# 7 times after the initial open) then attempt an 8th add_key. The engine
# has a fixed 8-slot key envelope table; the 9th call must raise exactly
# NoFreeKeySlotError.
#
# LastKeySlotError is triggered end-to-end: open with one key, call
# remove_key — the engine refuses because removing the last slot would
# leave the database permanently inaccessible.

_KS = [bytes([i]) * 32 for i in range(10)]  # 10 distinct 32-byte raw keys


def test_no_free_key_slot_exact_class(tmp_db):
    # The key envelope table holds 8 slots. Create a DB with key 0 (1 slot
    # used). Add keys 1–7 (slots 2–8 used, table full). Then add key 8 — the
    # table is full and NoFreeKeySlotError must be raised.
    with chisel.open(str(tmp_db), encryption_key=_KS[0]) as db:
        for i in range(1, 8):                # add keys 1..7 → 8 slots occupied
            db.add_key(_KS[i - 1], _KS[i])
        with pytest.raises(chisel.NoFreeKeySlotError) as exc_info:
            db.add_key(_KS[7], _KS[8])       # 9th key — no slot available
    assert isinstance(exc_info.value, chisel.OperationalError)


def test_last_key_slot_exact_class(tmp_db):
    # A DB opened with a single key has exactly one occupied slot. Removing
    # that key would leave no way to unlock the database; LastKeySlotError
    # must fire.
    with chisel.open(str(tmp_db), encryption_key=_KS[0]) as db:
        with pytest.raises(chisel.LastKeySlotError) as exc_info:
            db.remove_key(_KS[0])
    assert isinstance(exc_info.value, chisel.OperationalError)
