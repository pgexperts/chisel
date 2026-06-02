import chisel
import pytest


def test_chunk_tags_roundtrip(mem_db):
    with mem_db.transaction() as tx:
        h = tx.allocate(b"row")

    # allocate_tagged assigns a tag at allocation time
    mem_db.begin()
    h_tagged = mem_db.allocate_tagged(b"row", 42)
    mem_db.commit()

    assert mem_db.tag(h_tagged) == 42
    assert mem_db.handles_with_tag(42) == [h_tagged]

    mem_db.begin()
    deleted, complete = mem_db.delete_with_tag(42, 100)
    mem_db.commit()

    assert deleted == [h_tagged] and complete
    assert mem_db.handles_with_tag(42) == []


def test_chunk_tags_via_transaction_context(mem_db):
    # Exercise all 5 tag ops through the with-transaction context manager.
    with mem_db.transaction() as tx:
        h_tagged = tx.allocate_tagged(b"row", 55)
        assert tx.tag(h_tagged) == 55
        assert tx.handles_with_tag(55) == [h_tagged]

    # After commit the tag is visible outside the transaction.
    assert mem_db.tag(h_tagged) == 55
    assert mem_db.handles_with_tag(55) == [h_tagged]

    with mem_db.transaction() as tx:
        deleted, complete = tx.delete_with_tag(55, 100)
        assert deleted == [h_tagged] and complete

    assert mem_db.handles_with_tag(55) == []


def test_delete_tagged_wrong_tag_raises(mem_db):
    mem_db.begin()
    h = mem_db.allocate_tagged(b"row", 7)
    # Wrong tag raises the operational TagMismatchError; nothing deleted.
    with pytest.raises(chisel.TagMismatchError):
        mem_db.delete_tagged(h, 8)
    assert mem_db.handles_with_tag(7) == [h]
    mem_db.commit()
