"""Tests for the encryption_key kwarg on chisel.open().

Covers: bytes (raw key) roundtrip, str (passphrase) roundtrip, wrong-key
rejection, missing-key rejection, and a bad-type TypeError.
"""

import pathlib

import chisel
import pytest


def test_encrypted_roundtrip_with_bytes_key(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    key = b"\xab" * 32

    with chisel.open(path, encryption_key=key) as db:
        db.begin()
        h = db.allocate(b"secret-payload")
        db.commit()

    with chisel.open(path, create_if_missing=False, encryption_key=key) as db:
        assert db.read(h) == b"secret-payload"


def test_wrong_key_raises_invalid_encryption_key(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    with chisel.open(path, encryption_key=b"\xab" * 32) as db:
        db.begin()
        db.allocate(b"x")
        db.commit()

    with pytest.raises(chisel.InvalidEncryptionKeyError):
        chisel.open(path, create_if_missing=False, encryption_key=b"\x00" * 32)


def test_missing_key_raises_no_encryption_key(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    with chisel.open(path, encryption_key=b"\xab" * 32) as db:
        db.begin()
        db.allocate(b"x")
        db.commit()

    with pytest.raises(chisel.NoEncryptionKeyError):
        chisel.open(path, create_if_missing=False)


def test_passphrase_key_roundtrip(tmp_path: pathlib.Path):
    path = tmp_path / "pass.db"
    with chisel.open(path, encryption_key="correct horse battery staple") as db:
        db.begin()
        h = db.allocate(b"v")
        db.commit()

    with chisel.open(
        path, create_if_missing=False, encryption_key="correct horse battery staple"
    ) as db:
        assert db.read(h) == b"v"


def test_bad_key_type_raises_type_error(tmp_path: pathlib.Path):
    path = tmp_path / "enc.db"
    with pytest.raises(TypeError):
        chisel.open(path, encryption_key=12345)
