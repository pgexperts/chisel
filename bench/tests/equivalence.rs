// Cross-engine equivalence tests. Five scenarios × three engines =
// fifteen named tests. Each scenario asserts that an engine round-
// trips its own identifiers — read(allocate(v).id) returns v. We do
// not assert across engines (identifier values don't align by design,
// per the Engine::allocate doc comment).

use chisel_bench::{ChiselEngine, DurabilityMode, Engine, RedbEngine, SqliteEngine};
use tempfile::NamedTempFile;

// === Scenarios — generic over Engine ===

fn scenario_empty_value<E: Engine>(engine: &mut E) {
    engine.begin().expect("begin");
    let id = engine.allocate(b"").expect("allocate empty");
    engine.commit().expect("commit");
    assert_eq!(
        engine.read(id).expect("read empty"),
        b"",
        "empty-value round-trip failed",
    );
}

fn scenario_inline_range<E: Engine>(engine: &mut E) {
    let sizes = [32usize, 256, 2048];
    let values: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&n| (0..n).map(|i| (i % 251) as u8).collect())
        .collect();

    engine.begin().expect("begin");
    let ids: Vec<_> = values
        .iter()
        .map(|v| engine.allocate(v).expect("allocate inline"))
        .collect();
    engine.commit().expect("commit");

    for (id, expected) in ids.iter().zip(values.iter()) {
        assert_eq!(
            &engine.read(*id).expect("read inline"),
            expected,
            "inline-range round-trip failed for size {}",
            expected.len(),
        );
    }
}

fn scenario_just_overflow_boundary<E: Engine>(engine: &mut E) {
    // Sizes bracket Chisel's MAX_INLINE_VALUE = 8162. 8160 and 8161 fit
    // inline; 8163, 8200, and 9000 spill to overflow. The 8161/8163 pair
    // probes the exact edge — the off-by-one most likely to regress.
    // For redb / SQLite this is just storage of the same byte ranges.
    let sizes = [8160usize, 8161, 8163, 8200, 9000];
    let values: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&n| (0..n).map(|i| (i % 251) as u8).collect())
        .collect();

    engine.begin().expect("begin");
    let ids: Vec<_> = values
        .iter()
        .map(|v| engine.allocate(v).expect("allocate boundary"))
        .collect();
    engine.commit().expect("commit");

    for (id, expected) in ids.iter().zip(values.iter()) {
        assert_eq!(
            &engine.read(*id).expect("read boundary"),
            expected,
            "just-overflow-boundary round-trip failed for size {}",
            expected.len(),
        );
    }
}

fn scenario_large_overflow<E: Engine>(engine: &mut E) {
    let value: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();

    engine.begin().expect("begin");
    let id = engine.allocate(&value).expect("allocate 1 MB");
    engine.commit().expect("commit");

    assert_eq!(
        engine.read(id).expect("read 1 MB"),
        value,
        "large-overflow round-trip failed",
    );
}

fn scenario_delete_and_allocate<E: Engine>(engine: &mut E) {
    // Allocate 5 in one tx.
    engine.begin().expect("begin");
    let initial: Vec<_> = (0..5)
        .map(|i| {
            engine
                .allocate(format!("v{i}").as_bytes())
                .expect("allocate initial")
        })
        .collect();
    engine.commit().expect("commit");

    // Delete 3 of them, allocate 5 more.
    engine.begin().expect("begin");
    engine.delete(initial[1]).expect("delete 1");
    engine.delete(initial[2]).expect("delete 2");
    engine.delete(initial[3]).expect("delete 3");
    let added: Vec<_> = (5..10)
        .map(|i| {
            engine
                .allocate(format!("v{i}").as_bytes())
                .expect("allocate added")
        })
        .collect();
    engine.commit().expect("commit");

    // Surviving from initial: 0 and 4.
    assert_eq!(engine.read(initial[0]).expect("read survivor 0"), b"v0");
    assert_eq!(engine.read(initial[4]).expect("read survivor 4"), b"v4");

    // All added values readable.
    for (id, i) in added.iter().zip(5..10) {
        assert_eq!(
            engine.read(*id).expect("read added"),
            format!("v{i}").as_bytes(),
        );
    }

    // Deleted identifiers must error on read.
    assert!(
        engine.read(initial[1]).is_err(),
        "deleted identifier 1 must not be readable",
    );
    assert!(
        engine.read(initial[2]).is_err(),
        "deleted identifier 2 must not be readable",
    );
    assert!(
        engine.read(initial[3]).is_err(),
        "deleted identifier 3 must not be readable",
    );
}

// === Per-engine constructors ===

fn make_chisel() -> ChiselEngine {
    ChiselEngine::open_in_memory(64).expect("open chisel")
}

fn make_redb() -> (RedbEngine, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("tempfile");
    // redb's Database::create wants a non-existent or empty file; the
    // tempfile is created empty, redb treats that as "create new".
    let engine = RedbEngine::open_file(tmp.path(), 64, DurabilityMode::Strict).expect("open redb");
    (engine, tmp)
}

fn make_sqlite() -> (SqliteEngine, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let engine =
        SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Strict).expect("open sqlite");
    (engine, tmp)
}

// === Per-engine, per-scenario named tests (5 × 3 = 15) ===

#[test]
fn equivalence_empty_value_chisel() {
    let mut e = make_chisel();
    scenario_empty_value(&mut e);
}

#[test]
fn equivalence_empty_value_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_empty_value(&mut e);
}

#[test]
fn equivalence_empty_value_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_empty_value(&mut e);
}

#[test]
fn equivalence_inline_range_chisel() {
    let mut e = make_chisel();
    scenario_inline_range(&mut e);
}

#[test]
fn equivalence_inline_range_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_inline_range(&mut e);
}

#[test]
fn equivalence_inline_range_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_inline_range(&mut e);
}

#[test]
fn equivalence_just_overflow_boundary_chisel() {
    let mut e = make_chisel();
    scenario_just_overflow_boundary(&mut e);
}

#[test]
fn equivalence_just_overflow_boundary_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_just_overflow_boundary(&mut e);
}

#[test]
fn equivalence_just_overflow_boundary_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_just_overflow_boundary(&mut e);
}

#[test]
fn equivalence_large_overflow_chisel() {
    let mut e = make_chisel();
    scenario_large_overflow(&mut e);
}

#[test]
fn equivalence_large_overflow_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_large_overflow(&mut e);
}

#[test]
fn equivalence_large_overflow_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_large_overflow(&mut e);
}

#[test]
fn equivalence_delete_and_allocate_chisel() {
    let mut e = make_chisel();
    scenario_delete_and_allocate(&mut e);
}

#[test]
fn equivalence_delete_and_allocate_redb() {
    let (mut e, _tmp) = make_redb();
    scenario_delete_and_allocate(&mut e);
}

#[test]
fn equivalence_delete_and_allocate_sqlite() {
    let (mut e, _tmp) = make_sqlite();
    scenario_delete_and_allocate(&mut e);
}
