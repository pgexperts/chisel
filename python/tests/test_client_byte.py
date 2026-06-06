import chisel
import pytest


def test_client_byte_roundtrip_via_database(mem_db):
    mem_db.begin()
    h = mem_db.allocate(b"row")
    assert mem_db.client_byte(h) == 0  # default
    mem_db.set_client_byte(h, 0xAB)
    mem_db.commit()
    assert mem_db.client_byte(h) == 0xAB


def test_client_byte_via_transaction_context(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"row")
        tx.set_client_byte(h, 7)
        assert tx.client_byte(h) == 7
    assert mem_db.client_byte(h) == 7  # visible after commit


def test_client_byte_out_of_range_raises(mem_db):
    mem_db.begin()
    h = mem_db.allocate(b"row")
    with pytest.raises(OverflowError):
        mem_db.set_client_byte(h, 256)  # u8 overflow
    mem_db.rollback()


def test_client_byte_durable_across_reopen(tmp_db):
    # Open file-backed, set client byte, close, reopen, verify persistence.
    db = chisel.open(str(tmp_db))
    db.begin()
    h = db.allocate(b"row")
    db.set_client_byte(h, 0xC9)
    db.commit()
    db.close()

    db2 = chisel.open(str(tmp_db))
    assert db2.client_byte(h) == 0xC9
    db2.close()
