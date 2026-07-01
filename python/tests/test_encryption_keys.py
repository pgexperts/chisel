"""Tests for add_key / rotate_key / remove_key on the Chisel binding.

Covers:
  - add_key: both keys unlock the DB after adding a second key
  - add_key with a passphrase key works alongside a raw key
  - rotate_key: old key is revoked, new key unlocks the same data
  - remove_key: a removed key can no longer unlock the DB; the other can
  - remove last key: LastKeySlotError (database must stay accessible)
  - add_key to full table: NoFreeKeySlotError
  - bad key type: TypeError from py_key coercion
"""

import pathlib

import chisel
import pytest

K1 = bytes([0x11]) * 32
K2 = bytes([0x22]) * 32
K3 = bytes([0x33]) * 32
# 10 distinct keys for the slot-full test (key envelope table holds 8).
_KEYS = [bytes([i]) * 32 for i in range(10)]


def _open(path: pathlib.Path, key: bytes | str) -> chisel.Chisel:
    return chisel.open(path, encryption_key=key)


# ---------------------------------------------------------------------------
# add_key
# ---------------------------------------------------------------------------

def test_add_key_either_key_opens(tmp_path: pathlib.Path):
    """After add_key(K1, K2), the DB is openable with either K1 or K2."""
    path = tmp_path / "db"

    db = _open(path, K1)
    db.begin()
    h = db.allocate(b"secret-payload")
    db.commit()
    db.add_key(K1, K2)
    db.close()

    with _open(path, K1) as db1:
        assert db1.read(h) == b"secret-payload"
    with _open(path, K2) as db2:
        assert db2.read(h) == b"secret-payload"


def test_add_key_passphrase_alongside_raw(tmp_path: pathlib.Path):
    """A passphrase key can be added alongside an existing raw key."""
    path = tmp_path / "db"
    passphrase = "correct horse battery staple"

    with _open(path, K1) as db:
        db.begin()
        h = db.allocate(b"v")
        db.commit()
        db.add_key(K1, passphrase)

    with _open(path, passphrase) as db:
        assert db.read(h) == b"v"

    with _open(path, K1) as db:
        assert db.read(h) == b"v"


# ---------------------------------------------------------------------------
# rotate_key
# ---------------------------------------------------------------------------

def test_rotate_key_old_revoked(tmp_path: pathlib.Path):
    """After rotate_key(K1, K2), K1 is revoked and K2 opens the same data."""
    path = tmp_path / "db"

    with _open(path, K1) as db:
        db.begin()
        h = db.allocate(b"rotated-data")
        db.commit()
        db.rotate_key(K1, K2)

    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.open(str(path), create_if_missing=False, encryption_key=K1)

    with _open(path, K2) as db:
        assert db.read(h) == b"rotated-data"


def test_rotate_key_with_two_existing_slots(tmp_path: pathlib.Path):
    """rotate_key revokes the named old slot even when a second slot exists."""
    path = tmp_path / "db"

    with _open(path, K1) as db:
        db.begin()
        h = db.allocate(b"multi-slot")
        db.commit()
        db.add_key(K1, K2)   # two slots: K1 and K2
        db.rotate_key(K1, K3)  # replace K1 slot with K3; K2 slot unchanged

    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.open(str(path), create_if_missing=False, encryption_key=K1)

    with _open(path, K2) as db:
        assert db.read(h) == b"multi-slot"

    with _open(path, K3) as db:
        assert db.read(h) == b"multi-slot"


# ---------------------------------------------------------------------------
# remove_key
# ---------------------------------------------------------------------------

def test_remove_key_revokes_that_slot(tmp_path: pathlib.Path):
    """After remove_key(K1) with K2 still active, K1 is rejected."""
    path = tmp_path / "db"

    with _open(path, K1) as db:
        db.begin()
        h = db.allocate(b"remove-test")
        db.commit()
        db.add_key(K1, K2)
        db.remove_key(K1)

    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.open(str(path), create_if_missing=False, encryption_key=K1)

    with _open(path, K2) as db:
        assert db.read(h) == b"remove-test"


def test_remove_last_key_raises_last_key_slot(tmp_path: pathlib.Path):
    """remove_key on the only active key must raise LastKeySlotError."""
    path = tmp_path / "db"

    with _open(path, K1) as db:
        db.begin()
        db.allocate(b"x")
        db.commit()
        with pytest.raises(chisel.LastKeySlotError):
            db.remove_key(K1)


# ---------------------------------------------------------------------------
# add_key to a full key table
# ---------------------------------------------------------------------------

def test_add_key_full_table_raises_no_free_key_slot(tmp_path: pathlib.Path):
    """add_key when all 8 key slots are occupied must raise NoFreeKeySlotError.

    The key envelope table has 8 fixed slots. Open with _KEYS[0] (1 slot),
    add keys 1–7 (fills the remaining 7 slots → all 8 occupied), then
    attempt one more add_key.
    """
    path = tmp_path / "db"

    with chisel.open(str(path), encryption_key=_KEYS[0]) as db:
        for i in range(1, 8):
            db.add_key(_KEYS[i - 1], _KEYS[i])   # fill slots 2..8
        with pytest.raises(chisel.NoFreeKeySlotError):
            db.add_key(_KEYS[7], _KEYS[8])        # 9th key — table full


# ---------------------------------------------------------------------------
# bad key type
# ---------------------------------------------------------------------------

def test_bad_key_type_raises_type_error(tmp_path: pathlib.Path):
    """Passing a non-bytes, non-str key raises TypeError immediately."""
    path = tmp_path / "db"
    with _open(path, K1) as db:
        db.begin()
        db.allocate(b"x")
        db.commit()
        db.add_key(K1, K2)  # ensure two slots exist so remove_key is safe
        with pytest.raises(TypeError):
            db.add_key(K1, 12345)
        with pytest.raises(TypeError):
            db.rotate_key(K1, 12345)
        with pytest.raises(TypeError):
            db.remove_key(12345)
