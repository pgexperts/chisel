// Integration smoke test for the runner machinery. Confirms one cell
// runs end-to-end against a minimal Criterion::default() — exercises
// the snapshot-restore cell-runner, populate_snapshot, apply_op, and
// drive_workload_with_tx_granularity in a single test.
//
// This is NOT a real benchmark — it runs at sample_size = 10, in
// well under a second. Its purpose is to catch dumb mistakes
// (off-by-one in chunking, wrong snapshot path handling, panics on
// engine error, etc.) without paying full bench-grid cost.

use chisel_bench::runner::{
    capture_aux_metrics_snapshot_restore, drive_workload_with_tx_granularity, populate_snapshot,
    AuxMetricsWriter, CellId, EngineMode, CACHE_SIZE_PAGES,
};
use chisel_bench::workload::gen_allocate;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use tempfile::NamedTempFile;

#[test]
fn smoke_run_one_snapshot_restore_cell() {
    // Build a Criterion configured for the smallest practical run.
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(std::time::Duration::from_millis(50))
        .measurement_time(std::time::Duration::from_millis(200));

    // Empty snapshot for an allocate workload.
    let snap = populate_snapshot(EngineMode::ChiselStrict, 256, 0).unwrap();
    let workload = gen_allocate(/*count*/ 4, /*size*/ 256);

    {
        let mut group = c.benchmark_group("smoke");
        group.throughput(Throughput::Elements(4));
        let snapshot_path = snap.path().to_path_buf();
        let snapshot_ids = snap.ids().to_vec();
        let workload_ref = &workload;
        group.bench_with_input(BenchmarkId::new("chisel-strict", "256B"), &(), |b, _| {
            b.iter_batched(
                || {
                    let working = NamedTempFile::new().unwrap();
                    std::fs::copy(&snapshot_path, working.path()).unwrap();
                    let engine = EngineMode::ChiselStrict
                        .open(working.path(), CACHE_SIZE_PAGES)
                        .unwrap();
                    (engine, working)
                },
                |(mut engine, _working)| {
                    drive_workload_with_tx_granularity(
                        &mut *engine,
                        workload_ref,
                        /*ops_per_tx*/ 1,
                        &snapshot_ids,
                    );
                },
                BatchSize::PerIteration,
            );
        });
        group.finish();
    }

    // Aux-metrics path: write to a tempfile, capture one cell, verify
    // we got a parseable line.
    let dir = tempfile::tempdir().unwrap();
    let aux_path = dir.path().join("aux.jsonl");
    let mut aux = AuxMetricsWriter::create(&aux_path).unwrap();
    aux.append(&capture_aux_metrics_snapshot_restore(
        CellId {
            row: "smoke",
            mode: "chisel-strict",
            size: "256B",
        },
        EngineMode::ChiselStrict,
        snap.path(),
        snap.ids(),
        &workload,
        /*ops_per_tx*/ 1,
    ))
    .unwrap();
    drop(aux); // flush

    let contents = std::fs::read_to_string(&aux_path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["row"], "smoke");
    assert_eq!(v["mode"], "chisel-strict");
    assert!(
        v["counters"].is_object(),
        "ChiselStrict should produce non-null counters"
    );
}
