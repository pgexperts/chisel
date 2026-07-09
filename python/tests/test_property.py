from hypothesis import given, settings, strategies as st
import chisel


@given(data=st.binary(min_size=0, max_size=16 * 1024))
@settings(max_examples=200, deadline=None)
def test_round_trip_bytes(data):
    with chisel.open(None) as db:
        with db.transaction() as tx:
            h = tx.allocate(data)
            assert tx.read(h) == data


@given(data=st.binary(min_size=0, max_size=16 * 1024))
@settings(max_examples=100, deadline=None)
def test_round_trip_memoryview(data):
    with chisel.open(None) as db:
        with db.transaction() as tx:
            h = tx.allocate(memoryview(bytearray(data)))
            assert tx.read(h) == data


@given(values=st.lists(st.binary(max_size=256), min_size=0, max_size=50))
@settings(max_examples=100, deadline=None)
def test_round_trip_many(values):
    with chisel.open(None) as db:
        handles = []
        with db.transaction() as tx:
            for v in values:
                handles.append(tx.allocate(v))
        for h, expected in zip(handles, values):
            assert db.read(h) == expected
