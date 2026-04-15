import threading
import chisel


def test_db_can_be_moved_between_threads(tmp_db):
    db = chisel.open(str(tmp_db))
    result = []

    def worker():
        with db.transaction() as tx:
            h = tx.allocate(b"from-thread")
            result.append(h)

    t = threading.Thread(target=worker)
    t.start()
    t.join()

    assert len(result) == 1
    h = result[0]
    assert db.read(h) == b"from-thread"
    db.close()


def test_sequential_access_across_threads(tmp_db):
    # Verify a Chisel can be handed from one thread to another and that
    # the second thread's commits are visible to the first. This is the
    # supported threading pattern — migration, not concurrency.
    db = chisel.open(str(tmp_db))
    handles = []

    def writer():
        with db.transaction() as tx:
            for i in range(5):
                handles.append(tx.allocate(bytes([i])))

    t = threading.Thread(target=writer)
    t.start()
    t.join()

    assert len(handles) == 5
    for i, h in enumerate(handles):
        assert db.read(h) == bytes([i])
    db.close()
