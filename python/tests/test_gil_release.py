"""GIL release around engine calls (I156, issue #144).

Every engine call on a `Chisel` releases the GIL and serializes on an internal
Mutex instead (db.rs `with_inner_io` / `with_inner_mut_io`). These tests pin
the three properties that design promises:

  1. Another Python thread keeps running while a slow engine call (a
     passphrase rotate_key: two Argon2id derivations) is in flight.
  2. Several threads hammering one handle with writes, reads, and commits
     neither deadlock nor corrupt data. The lock order (detach the GIL, then
     take the Mutex; never the reverse) is what makes this hold.
  3. close() and is_poisoned from other threads while a slow call holds the
     Mutex wait for it and return, rather than deadlocking.

Every thread join is bounded so a deadlock fails the test instead of hanging
the suite.
"""

from __future__ import annotations

import pathlib
import threading
import time

import chisel

JOIN_TIMEOUT = 10.0


class _Ticker:
    """Background thread that timestamps itself roughly every half millisecond.

    Each sample needs the GIL (calling perf_counter and appending are Python
    bytecode, and waking from sleep re-acquires it), so a stretch with no
    samples is a stretch during which some other thread held the GIL. The
    sleep keeps the sample list small; a bare spin loop would record millions
    of floats per second.
    """

    def __init__(self) -> None:
        self.samples: list[float] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            self.samples.append(time.perf_counter())
            time.sleep(0.0005)

    def __enter__(self) -> _Ticker:
        self._thread.start()
        # Wait for the first sample so the measured window never includes
        # thread start-up.
        deadline = time.monotonic() + JOIN_TIMEOUT
        while not self.samples and time.monotonic() < deadline:
            time.sleep(0.001)
        assert self.samples, "ticker thread never started"
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(JOIN_TIMEOUT)
        assert not self._thread.is_alive(), "ticker thread did not stop"

    def count_between(self, lo: float, hi: float) -> int:
        return sum(1 for s in self.samples if lo < s < hi)


def test_other_threads_run_during_passphrase_rotate_key(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "gil.chisel"
    with chisel.open(path, encryption_key="old passphrase") as db:
        with db.transaction() as tx:
            h = tx.allocate(b"payload")

        with _Ticker() as ticker:
            t0 = time.perf_counter()
            db.rotate_key("old passphrase", "new passphrase")
            t1 = time.perf_counter()

        # Only the middle half of the call is examined. A GIL-holding binding
        # can still let the ticker run for up to one switch interval (5 ms)
        # after t0, before the main thread re-takes the GIL and enters Rust;
        # trimming a quarter from each end keeps that slack out of the window
        # for any plausible call duration (two Argon2id derivations at
        # 19 MiB, t=2 take tens of milliseconds even on fast hardware).
        duration = t1 - t0
        lo, hi = t0 + duration / 4, t1 - duration / 4
        during = ticker.count_between(lo, hi)
        # With the GIL released the ticker samples about every millisecond,
        # so the window holds many samples; with it held, exactly zero. A
        # floor of 3 is far from both, which keeps this stable on slow CI.
        assert during >= 3, (
            f"only {during} ticker samples in the middle {duration / 2 * 1000:.1f} ms "
            f"of a {duration * 1000:.1f} ms rotate_key — the GIL was not released"
        )
        assert db.read(h) == b"payload"

    with chisel.open(path, encryption_key="new passphrase") as db:
        assert db.read(h) == b"payload"


def test_concurrent_writers_and_readers_on_one_handle(tmp_path: pathlib.Path) -> None:
    # The engine has a single active transaction per handle, so the writers
    # take turns via a Python lock; what this exercises is the binding's own
    # serialization. While a writer is inside commit() (three fsyncs, GIL
    # released) the readers, the stats poller, and the other writer's Python
    # code all run and contend for the engine Mutex.
    path = tmp_path / "stress.chisel"
    db = chisel.open(path)
    tx_lock = threading.Lock()
    committed: dict[int, bytes] = {}
    errors: list[object] = []
    reads = [0]
    # Work-bounded, not time-boxed: each commit is three fsyncs (F_FULLFSYNC on
    # macOS), whose latency on a virtualized CI disk is unpredictable, so a
    # fixed run time could finish with almost no commits. The readers and
    # poller run until both writers are done; the bounded joins below are
    # the only clock.
    commits_per_writer = 5
    writers_done = threading.Event()

    # Seed one committed value so readers have something to check from the
    # start.
    with db.transaction() as tx:
        committed[tx.allocate(b"seed")] = b"seed"

    def writer(wid: int) -> None:
        try:
            for i in range(commits_per_writer):
                payload = f"w{wid}-{i}".encode() * 8
                with tx_lock:
                    with db.transaction() as tx:
                        h = tx.allocate(payload)
                        if tx.read(h) != payload:
                            errors.append(("read-own-write", wid, i))
                # Publish only after the commit returned, so readers never
                # look for a value that might still be rolled back.
                committed[h] = payload
        except BaseException as exc:  # pragma: no cover - surfaced below
            errors.append(("writer", wid, exc))

    def reader() -> None:
        try:
            while not writers_done.is_set():
                for h, payload in list(committed.items()):
                    got = db.read(h)
                    reads[0] += 1
                    if got != payload:
                        errors.append(("corrupt", h, got))
        except BaseException as exc:  # pragma: no cover - surfaced below
            errors.append(("reader", exc))

    def poller() -> None:
        try:
            while not writers_done.is_set():
                if db.is_poisoned:
                    errors.append(("poisoned",))
                db.stats()
                db.counters()
        except BaseException as exc:  # pragma: no cover - surfaced below
            errors.append(("poller", exc))

    writers = [threading.Thread(target=writer, args=(w,), daemon=True) for w in range(2)]
    others = [threading.Thread(target=reader, daemon=True) for _ in range(3)]
    others.append(threading.Thread(target=poller, daemon=True))
    # Readers first, so they are already calling in when the first commit
    # starts.
    for t in others + writers:
        t.start()
    for t in writers:
        t.join(JOIN_TIMEOUT)
    writers_done.set()
    for t in others:
        t.join(JOIN_TIMEOUT)
    hung = [t.name for t in writers + others if t.is_alive()]
    assert not hung, f"threads still running after {JOIN_TIMEOUT}s — deadlock? {hung}"
    assert not errors, errors
    assert len(committed) == 1 + 2 * commits_per_writer
    assert reads[0] > 0

    assert not db.is_poisoned
    assert set(db.handles()) == set(committed)
    db.close()

    with chisel.open(path) as db:
        for h, payload in committed.items():
            assert db.read(h) == payload


def test_close_and_is_poisoned_wait_for_an_in_flight_call(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "close.chisel"
    db = chisel.open(path, encryption_key="old passphrase")
    started = threading.Event()
    outcome: list[object] = []
    poisoned_seen: list[bool] = []

    def rotate() -> None:
        started.set()
        try:
            db.rotate_key("old passphrase", "new passphrase")
            outcome.append("rotated")
        except chisel.ClosedError:
            # close() won the race for the Mutex; also a valid ordering.
            outcome.append("closed")
        except BaseException as exc:  # pragma: no cover - surfaced below
            outcome.append(exc)

    worker = threading.Thread(target=rotate, daemon=True)
    worker.start()
    assert started.wait(JOIN_TIMEOUT)
    # Give the worker a moment to get into the KDF and hold the Mutex, so the
    # common case really is close() and is_poisoned blocking on it.
    time.sleep(0.005)
    prober = threading.Thread(target=lambda: poisoned_seen.append(db.is_poisoned), daemon=True)
    closer = threading.Thread(target=db.close, daemon=True)
    prober.start()
    closer.start()

    for t in (worker, prober, closer):
        t.join(JOIN_TIMEOUT)
        assert not t.is_alive(), f"{t.name} still running — deadlock between close() and an engine call"

    assert outcome in (["rotated"], ["closed"]), outcome
    assert len(poisoned_seen) == 1
    assert db.is_poisoned  # closed reads as poisoned
    try:
        db.handles()
    except chisel.ClosedError:
        pass
    else:  # pragma: no cover
        raise AssertionError("handle still usable after close()")

    if outcome == ["rotated"]:
        with chisel.open(path, encryption_key="new passphrase") as reopened:
            assert not reopened.is_poisoned
