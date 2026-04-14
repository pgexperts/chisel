import pytest
import chisel


@pytest.fixture
def tmp_db(tmp_path):
    return tmp_path / "test.chisel"


@pytest.fixture
def mem_db():
    # Task 3 will implement chisel.open(); this fixture will be used then.
    # Safe to define now — pytest only resolves fixtures when used.
    with chisel.open(None) as db:
        yield db
