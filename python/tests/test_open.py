import pytest
import chisel


def test_open_in_memory_with_none():
    db = chisel.open(None)
    assert db is not None
    db.close()


def test_open_creates_file(tmp_db):
    assert not tmp_db.exists()
    db = chisel.open(str(tmp_db))
    db.close()
    assert tmp_db.exists()


def test_open_context_manager(tmp_db):
    with chisel.open(str(tmp_db)) as db:
        assert db is not None
    # second open verifies file is closed / flock released
    with chisel.open(str(tmp_db)) as db:
        assert db is not None


def test_open_rejects_missing_when_create_false(tmp_db):
    with pytest.raises(chisel.DatabaseFileNotFoundError):
        chisel.open(str(tmp_db), create_if_missing=False)


def test_open_rejects_bad_superblock_count(tmp_db):
    with pytest.raises(chisel.InvalidSuperblockCountError):
        chisel.open(str(tmp_db), superblock_count=1)
    with pytest.raises(chisel.InvalidSuperblockCountError):
        chisel.open(str(tmp_db), superblock_count=17)


def test_open_in_memory_rejects_read_only():
    with pytest.raises(chisel.ReadOnlyModeError):
        chisel.open(None, read_only=True)


def test_open_accepts_pathlib(tmp_db):
    with chisel.open(tmp_db) as db:
        assert db is not None


def test_double_open_same_path_fails(tmp_db):
    with chisel.open(str(tmp_db)):
        with pytest.raises(chisel.LockFailedError):
            chisel.open(str(tmp_db))


# ── Spillway and drain_insertion kwargs ─────────────────────────────


def test_drain_insertion_has_both_variants():
    # Smoke check that the pyclass enum is exposed and both variants
    # are accessible as class attributes.
    assert hasattr(chisel, "DrainInsertion")
    assert hasattr(chisel.DrainInsertion, "LruTail")
    assert hasattr(chisel.DrainInsertion, "Mru")


def test_drain_insertion_variants_compare_equal_to_themselves():
    # The pyclass enum was declared with `eq, eq_int` so identity AND
    # equality must hold. Catches accidental future regressions where
    # a variant becomes a wrapper that breaks equality.
    assert chisel.DrainInsertion.LruTail == chisel.DrainInsertion.LruTail
    assert chisel.DrainInsertion.Mru == chisel.DrainInsertion.Mru
    assert chisel.DrainInsertion.LruTail != chisel.DrainInsertion.Mru


def test_open_with_default_spillway(tmp_db):
    # spillway_max_bytes=None (default) resolves to 1024 × cache_max_bytes
    # inside the Rust binding. Just verifying the open path accepts the
    # default and succeeds.
    with chisel.open(str(tmp_db)) as db:
        assert db is not None


def test_open_with_spillway_disabled(tmp_db):
    # Explicit 0 disables the spillway and restores CacheFull-at-cap.
    with chisel.open(str(tmp_db), spillway_max_bytes=0) as db:
        assert db is not None


def test_open_with_custom_spillway_size(tmp_db):
    # A small explicit cap. Engine should accept any non-negative integer.
    with chisel.open(str(tmp_db), spillway_max_bytes=1024 * 1024) as db:
        assert db is not None


def test_open_with_drain_insertion_lru_tail(tmp_db):
    # Default value explicit-passed. Mainly verifies that a
    # DrainInsertion variant survives the round-trip through pyo3 into
    # chisel::DrainInsertion without panicking or rejecting.
    with chisel.open(str(tmp_db), drain_insertion=chisel.DrainInsertion.LruTail) as db:
        assert db is not None


def test_open_with_drain_insertion_mru(tmp_db):
    # The non-default variant. Reaching here means the From<PyDrainInsertion>
    # impl is wired for both arms.
    with chisel.open(str(tmp_db), drain_insertion=chisel.DrainInsertion.Mru) as db:
        assert db is not None


def test_open_in_memory_with_spillway_and_drain(tmp_db):
    # In-memory mode uses an in-memory spillway buffer instead of a
    # sidecar file; same kwargs should still be accepted.
    with chisel.open(
        None,
        spillway_max_bytes=1024 * 1024,
        drain_insertion=chisel.DrainInsertion.Mru,
    ) as db:
        assert db is not None
