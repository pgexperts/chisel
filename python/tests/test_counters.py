import pytest
import chisel


def test_counters_dataclass_shape(mem_db):
    c = mem_db.counters()
    assert isinstance(c, chisel.Counters)
    assert isinstance(c.cache_hits, int)
    assert isinstance(c.cache_misses, int)
    assert isinstance(c.pages_allocated, int)
    assert isinstance(c.fsync_calls, int)


def test_counters_track_commit(mem_db):
    baseline = mem_db.counters()
    with mem_db.transaction() as tx:
        tx.allocate(b"hello")
    after = mem_db.counters()
    # Commit performs at least 2 fsyncs (data + superblock).
    assert after.fsync_calls >= baseline.fsync_calls + 2
    # At least one new page was allocated (the value's data page).
    assert after.pages_allocated > baseline.pages_allocated


def test_counters_is_frozen(mem_db):
    c = mem_db.counters()
    with pytest.raises(Exception):  # FrozenInstanceError subclasses AttributeError
        c.cache_hits = 999


def test_counters_snapshot_does_not_mutate(mem_db):
    snap = mem_db.counters()
    snap_fsync_at_capture = snap.fsync_calls
    # Do work that must produce fsyncs.
    with mem_db.transaction() as tx:
        tx.allocate(b"x")
    # Engine counters must have advanced (commit fsyncs).
    snap2 = mem_db.counters()
    assert snap2.fsync_calls > snap.fsync_calls, "commit must advance fsync_calls"
    # The original snapshot's field value is the same as captured —
    # the snapshot is a value, not a live view. (Frozen-dataclass
    # immutability is separately covered by test_counters_is_frozen.)
    assert snap.fsync_calls == snap_fsync_at_capture
