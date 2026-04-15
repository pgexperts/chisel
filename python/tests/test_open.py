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
