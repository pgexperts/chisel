import array
import pytest
import chisel


def test_allocate_bytes(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"hello")
        assert tx.read(h) == b"hello"


def test_allocate_bytearray(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(bytearray(b"world"))
        assert tx.read(h) == b"world"


def test_allocate_memoryview(mem_db):
    buf = bytearray(b"memoryview")
    with mem_db.transaction() as tx:
        h = tx.allocate(memoryview(buf))
        assert tx.read(h) == b"memoryview"


def test_allocate_array(mem_db):
    a = array.array("b", [1, 2, 3, 4])
    with mem_db.transaction() as tx:
        h = tx.allocate(a)
        assert tx.read(h) == bytes([1, 2, 3, 4])


def test_allocate_rejects_str(mem_db):
    with mem_db.transaction() as tx:
        with pytest.raises(TypeError, match="bytes-like"):
            tx.allocate("not bytes")


def test_allocate_rejects_int(mem_db):
    with mem_db.transaction() as tx:
        with pytest.raises(TypeError):
            tx.allocate(42)


def test_empty_bytes(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"")
        assert tx.read(h) == b""


def test_large_value(mem_db):
    data = b"x" * (1024 * 1024)
    with mem_db.transaction() as tx:
        h = tx.allocate(data)
        assert tx.read(h) == data


def test_read_returns_bytes_type(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"abc")
        assert type(tx.read(h)) is bytes
