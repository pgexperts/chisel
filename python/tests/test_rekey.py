"""Tests for chisel.rekey() — bulk DEK rotation through the binding.

`rekey` is the heavy sibling of rotate_key: rotate_key re-wraps the SAME data
key under a different credential (O(1), superblock only), while rekey generates
a fresh data key and re-encrypts every page.

It is a module-level function rather than a Chisel method because it operates
on a PATH: it rewrites the whole file and replaces it by rename, so a handle
opened beforehand would afterwards refer to the original, now-unlinked inode.

Covers:
  - values, named roots and large (overflow-chained) values survive
  - the on-disk bytes actually change (a silent no-op would pass everything else)
  - the same credential still opens the database afterwards
  - credentials NOT supplied to rekey are revoked, and can be re-added
  - a wrong key is refused without modifying the file
  - a plaintext database is refused
  - a missing file is refused
  - the database is still writable afterwards
  - no scratch file is left beside the database
  - bad argument types raise TypeError before anything is touched
"""

import pathlib

import chisel
import pytest

KEY = bytes([0xA1]) * 32
OTHER = bytes([0xB2]) * 32
BIG = b"\xc7" * (8192 * 3)


def _seed(path: pathlib.Path, key: bytes | str) -> tuple[int, int]:
    """Create a database with enough structure that a rotation has real work:
    many small values, one overflow-chained value, and a named root."""
    with chisel.open(path, encryption_key=key) as db:
        with db.transaction() as txn:
            small = txn.allocate(b"small value")
            big = txn.allocate(BIG)
            db.set_root_name("primary", small)
        return small, big


def test_rekey_preserves_data_and_changes_the_ciphertext(tmp_path):
    path = tmp_path / "rk.db"
    small, big = _seed(path, KEY)
    before = path.read_bytes()

    chisel.rekey(path, KEY)

    after = path.read_bytes()
    assert len(before) == len(after), "the page count must not change"
    assert before != after, "every page is sealed under a new data key"

    # Same credential, same data.
    with chisel.open(path, encryption_key=KEY) as db:
        assert db.read(small) == b"small value"
        assert db.read(big) == BIG
        assert db.get_root_name("primary") == small


def test_rekey_revokes_credentials_it_was_not_given(tmp_path):
    # The documented consequence: each key slot's wrapping key is derived from
    # its own credential, so a credential that was not supplied cannot have the
    # new data key wrapped for it. Asserting it keeps the doc honest.
    path = tmp_path / "rk.db"
    small, _ = _seed(path, KEY)
    with chisel.open(path, encryption_key=KEY) as db:
        db.add_key(KEY, OTHER)

    # Both work beforehand.
    with chisel.open(path, encryption_key=OTHER) as db:
        assert db.read(small) == b"small value"

    chisel.rekey(path, KEY)

    with chisel.open(path, encryption_key=KEY) as db:
        assert db.read(small) == b"small value"
    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.open(path, encryption_key=OTHER)

    # The documented remedy.
    with chisel.open(path, encryption_key=KEY) as db:
        db.add_key(KEY, OTHER)
    with chisel.open(path, encryption_key=OTHER) as db:
        assert db.read(small) == b"small value"


def test_rekey_with_a_passphrase_credential(tmp_path):
    path = tmp_path / "rk.db"
    small, _ = _seed(path, "correct horse battery staple")
    chisel.rekey(path, "correct horse battery staple")
    with chisel.open(path, encryption_key="correct horse battery staple") as db:
        assert db.read(small) == b"small value"


def test_rekey_refuses_a_wrong_key_without_touching_the_file(tmp_path):
    path = tmp_path / "rk.db"
    small, _ = _seed(path, KEY)
    before = path.read_bytes()

    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.rekey(path, OTHER)

    assert path.read_bytes() == before, "a refused rekey must not modify a byte"
    with chisel.open(path, encryption_key=KEY) as db:
        assert db.read(small) == b"small value"


def test_rekey_refuses_a_plaintext_database(tmp_path):
    path = tmp_path / "plain.db"
    with chisel.open(path) as db:
        with db.transaction() as txn:
            txn.allocate(b"cleartext")
    with pytest.raises(chisel.EncryptionNotSupportedError):
        chisel.rekey(path, KEY)


def test_rekey_refuses_a_missing_file(tmp_path):
    with pytest.raises(chisel.DatabaseFileNotFoundError):
        chisel.rekey(tmp_path / "nope.db", KEY)


def test_database_is_writable_after_a_rotation(tmp_path):
    # The freemap, handle table and next_handle all come through the superblock
    # body that was re-sealed under the new key, so a rotated database that
    # reads but cannot write would be a real (and quiet) failure.
    path = tmp_path / "rk.db"
    small, _ = _seed(path, KEY)

    chisel.rekey(path, KEY)

    with chisel.open(path, encryption_key=KEY) as db:
        with db.transaction() as txn:
            fresh = txn.allocate(b"written after the rotation")
    with chisel.open(path, encryption_key=KEY) as db:
        assert db.read(fresh) == b"written after the rotation"
        assert db.read(small) == b"small value"


def test_rekey_leaves_no_scratch_file(tmp_path):
    path = tmp_path / "rk.db"
    _seed(path, KEY)
    chisel.rekey(path, KEY)
    assert not (tmp_path / "rk.db.rekey-tmp").exists()


def test_rekey_rejects_bad_argument_types(tmp_path):
    path = tmp_path / "rk.db"
    _seed(path, KEY)
    # Coercion happens under the GIL before any engine call, so this is a
    # synchronous TypeError and the file is never opened.
    with pytest.raises(TypeError):
        chisel.rekey(path, 12345)
