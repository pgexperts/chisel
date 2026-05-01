// Bench binary: the 270-cell micro grid. Iterates EngineMode::ALL × SIZES
// × the 9 row groups, registering each cell as a Criterion benchmark
// inside a per-row BenchmarkGroup with Throughput::Elements(N) for
// per-op normalization. Aux metrics (file-size delta + Chisel internal
// counter deltas) are captured per cell into bench/results/aux_metrics.jsonl.
//
// The three Criterion-shaped cell-runner helpers (run_*_cell) live here
// rather than in src/runner.rs because Criterion is in [dev-dependencies]
// and src/ code can't import dev-deps. The helpers are private.
//
// Run the full grid: `cargo bench --bench micro_grid`. Filter to one row:
// `cargo bench --bench micro_grid read-warm`.

#![allow(unused_imports, dead_code)] // tasks 10/11 will use these row-bench imports + SIZES/seed_for

use chisel_bench::runner::{
    apply_op, capture_aux_metrics_snapshot_restore, capture_aux_metrics_warm_read,
    drive_workload_with_tx_granularity, populate_snapshot, AuxMetricsWriter, CellId, EngineMode,
    CACHE_SIZE_PAGES,
};
use chisel_bench::workload::{
    gen_allocate, gen_delete_many, gen_delete_random, gen_read_random, gen_update_random, Workload,
};
use chisel_bench::Engine;
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BatchSize, BenchmarkGroup, BenchmarkId,
    Criterion, Throughput,
};
use tempfile::NamedTempFile;

/// Six log-spaced sizes, one per Chisel internal regime (master spec §3.2).
/// `prepop_count` calibrated to ~25 MB raw payload per cell (master spec §3.4).
const SIZES: [(usize, &str, usize); 6] = [
    (32, "32B", 800_000),
    (256, "256B", 100_000),
    (2_048, "2KB", 12_500),
    (16_384, "16KB", 1_500),
    (131_072, "128KB", 200),
    (1_048_576, "1MB", 25),
];

/// Hardcoded per-row seeds for workload determinism. Hardcoded rather
/// than derived from row names because Rust's DefaultHasher randomizes
/// per-process — derived seeds would change between invocations.
fn seed_for(row_name: &str) -> u64 {
    match row_name {
        "allocate-1pertx" => 0x4001,
        "allocate-1000pertx" => 0x4002,
        "read-warm" => 0x4003,
        "read-cold" => 0x4004,
        "update-1pertx" => 0x4005,
        "update-1000pertx" => 0x4006,
        "delete-1pertx" => 0x4007,
        "delete-1000pertx" => 0x4008,
        "delete_many" => 0x4009,
        _ => panic!("unknown row name: {row_name}"),
    }
}

/// Snapshot-restore cell-runner — used by 8 of 9 rows (allocate, cold-read,
/// update, delete, delete_many). Each iteration copies the pre-built
/// snapshot, opens a fresh engine, runs the workload's ops grouped into
/// transactions of `ops_per_tx`, then drops engine + tempfile.
fn run_snapshot_restore_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    snapshot_path: &std::path::Path,
    snapshot_ids: &[u64],
    workload: &Workload,
    ops_per_tx: usize,
) {
    group.bench_with_input(BenchmarkId::new(mode.label(), size_label), &(), |b, _| {
        b.iter_batched(
            || {
                let working = NamedTempFile::new().unwrap();
                std::fs::copy(snapshot_path, working.path()).unwrap();
                let engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                (engine, working)
            },
            |(mut engine, _working)| {
                drive_workload_with_tx_granularity(
                    &mut *engine,
                    workload,
                    ops_per_tx,
                    snapshot_ids,
                );
            },
            BatchSize::PerIteration,
        );
    });
}

/// Warm-read cell-runner — row 3 only. Engine is opened once per cell
/// and reused across all iterations; the cache warms naturally during
/// Criterion's warmup phase. Reads don't mutate engine-visible state,
/// so persistent engine is safe.
fn run_warm_read_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    engine: &mut dyn Engine,
    workload: &Workload,
    snapshot_ids: &[u64],
) {
    group.bench_with_input(BenchmarkId::new(mode.label(), size_label), &(), |b, _| {
        b.iter(|| {
            for op in &workload.ops {
                apply_op(engine, op, snapshot_ids, &mut Vec::new());
            }
        });
    });
}

/// Cold-read cell-runner — row 4 only. Engine open is INSIDE the timed
/// routine: cold means "fresh open, no values touched, first read is
/// the timed call" (master spec §5.2). File copy stays in setup.
fn run_cold_read_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    mode: EngineMode,
    size_label: &str,
    snapshot_path: &std::path::Path,
    snapshot_ids: &[u64],
    workload: &Workload,
) {
    group.bench_with_input(BenchmarkId::new(mode.label(), size_label), &(), |b, _| {
        b.iter_batched(
            || {
                let working = NamedTempFile::new().unwrap();
                std::fs::copy(snapshot_path, working.path()).unwrap();
                working
            },
            |working| {
                let mut engine = mode.open(working.path(), CACHE_SIZE_PAGES).unwrap();
                apply_op(
                    &mut *engine,
                    &workload.ops[0],
                    snapshot_ids,
                    &mut Vec::new(),
                );
            },
            BatchSize::PerIteration,
        );
    });
}

fn micro_grid(c: &mut Criterion) {
    let _aux = AuxMetricsWriter::create("bench/results/aux_metrics.jsonl").unwrap();
    // Row-bench function calls land in tasks 10 and 11.
    let _ = c;
}

criterion_group!(benches, micro_grid);
criterion_main!(benches);
