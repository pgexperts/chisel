// freemap_churn.rs — Report-only benchmark for the multi-page freemap's
// reclamation behavior under sustained delete/realloc churn.
//
// Role in the system: tracks two properties that the multi-page freemap must
// preserve:
//
//   1. Flat high-water. Under constant live-data size, the file does not
//      grow per-commit. The freemap's structural COW recycle (one-commit-
//      deferred pool of dead freemap pages) is what keeps the freemap's own
//      churn from marching the high-water up one page per commit.
//
//   2. Reclamation throughput. Pages freed in one commit are reusable in the
//      next. This bench measures the page count before and after a churn
//      cycle.
//
// What is reported:
//   - Criterion wall-clock timings and throughput annotations, for all three
//     groups. Units differ: group 1 reports round-trips/sec (Elements(1));
//     groups 2 and 3 report records/sec (Elements(live_count)).
//   - One JSONL line per case of groups 1 and 2 in
//     `bench/results/freemap_churn_aux.jsonl` (the same `CellAuxMetrics`
//     shape micro_grid writes: `row` = group name, `mode` = benchmark-id
//     function, `size` = case label). `file_size_delta_bytes` is the file
//     high-water change across one steady-state churn cycle (property 1:
//     should be ~0); `counters.pages_allocated` is the page allocations
//     across it (property 2; see group 2's doc for how to read it).
//   These come from a SEPARATE UNTIMED pass per case (`capture_churn_aux`),
//   not from the timed closures: the timed closures read nothing back, so
//   the aux capture cannot perturb the measurement.
//   The file is this bench's own rather than micro_grid's `aux_metrics.jsonl`
//   because `AuxMetricsWriter::create` truncates; sharing one file would
//   clobber whichever bench ran first. Nothing reads it yet (no summary
//   renderer or CI wiring); it exists so the two properties above can be
//   trend-tracked.
//   Group 3 is not recorded: `CellAuxMetrics` has no field for
//   `freemap_orphans_reclaimed`, and the orphan sweep is not one of the two
//   properties this bench exists to track.
//
// Why public-API rather than freemap_tree internals:
//   `FreeMapTree`, `PageCache`, and the structural allocator are all
//   `pub(crate)`. Exposing them through a bench seam would require making
//   non-trivial engine internals public, widening the surface well beyond
//   what one bench warrants. The flat-high-water and throughput properties
//   are directly observable through `Chisel::file_size_bytes()` and
//   `ChiselCounters::pages_allocated`, which this bench measures.
//
// Depth->=1 regime note:
//   Freemap depth grows only when the highest freed page id exceeds the
//   current tree's capacity (65,344 pages × 1021^depth = ~512 MB at depth 0).
//   This bench targets the depth-0 steady-state, which is realistic for the
//   workloads that run on developer hardware. Exercising depth>=1 requires
//   either a >512 MB workload (65,344+ page file) or a synthetic internal
//   test that frees a high id directly — the integration tests in
//   tests/freemap_multipage.rs cover both. On-demand depth>=1 bench runs
//   should use dedicated hardware with a multi-gigabyte dataset and a larger
//   cache budget; those are deferred to explicit runs, not the CI grid.
//
// Run this bench:
//   cargo bench --bench freemap_churn
//
// The bench is REPORT-ONLY. No assertion gates on the numbers; CI marks
// bench steps continue-on-error / non-blocking per project conventions
// (CLAUDE.md: perf regressions are trend-tracked, not pass/fail).

use chisel::{Chisel, DefragOptions, Options, PAGE_SIZE};
use chisel_bench::runner::{counter_delta, AuxMetricsWriter, CellAuxMetrics, CellId};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use tempfile::NamedTempFile;

// I93 (from micro_grid.rs): pin mimalloc as this bench binary's process
// allocator. Chisel is allocation-heavy; a fixed fast allocator removes
// allocator variance from the measured numbers and reports Chisel's
// realistic best case.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Cache budget in pages for the churn bench. 512 pages = 4 MB. Large enough
/// to keep most of the working set hot; small enough to exercise eviction on
/// bigger churn counts.
const CACHE_PAGES: usize = 512;

/// Populate `count` records of `size` bytes each in `db`.
/// Returns the allocated handles.
fn populate(db: &mut Chisel, count: usize, size: usize) -> Vec<chisel::Handle> {
    let payload = vec![0u8; size];
    let chunk = 200.min(count);
    let mut handles = Vec::with_capacity(count);
    let mut remaining = count;
    while remaining > 0 {
        let this = remaining.min(chunk);
        db.begin().expect("begin populate");
        for _ in 0..this {
            let h = db.allocate(&payload).expect("allocate");
            handles.push(h);
        }
        db.commit().expect("commit populate");
        remaining -= this;
    }
    handles
}

/// One churn cycle: delete all `handles`, commit, reallocate the same count at
/// `size` bytes each, commit. Returns the new handle list.
/// Each cycle is ONE delete-commit + ONE allocate-commit = 2 commits = 6 fsyncs.
/// Whether those fsyncs cost anything depends on the CALLER's engine, not on
/// this function: group 1 runs against `open_in_memory_with_options`, whose
/// backing `Vec<u8>` makes each fsync a counted no-op (see
/// `page_io::tests::fsync_count_in_memory_backing_also_increments`); groups 2
/// and 3 run against a real file and pay a real fsync (`F_FULLFSYNC` on macOS,
/// plain `fsync` on Linux — see `PageIo::fsync`; the dedicated bench machine
/// and CI both run Linux). The freemap tree
/// sees `count` frees in the first commit and `count` reuses in the second.
fn churn_cycle(db: &mut Chisel, handles: Vec<chisel::Handle>, size: usize) -> Vec<chisel::Handle> {
    let count = handles.len();
    let payload = vec![0u8; size];

    // Delete pass: one transaction, frees `count` pages into txn_freed_pages.
    // persist_freemap marks them free in the COW freemap tree at commit.
    db.begin().expect("begin delete");
    for h in handles {
        db.delete(h).expect("delete");
    }
    db.commit().expect("commit delete");

    // Reallocate pass: one transaction, allocate_data_page draws from the
    // freemap (reusing the just-freed pages) before extending the file.
    let chunk = 200.min(count);
    let mut new_handles = Vec::with_capacity(count);
    let mut remaining = count;
    while remaining > 0 {
        let this = remaining.min(chunk);
        db.begin().expect("begin alloc");
        for _ in 0..this {
            let h = db.allocate(&payload).expect("reallocate");
            new_handles.push(h);
        }
        db.commit().expect("commit alloc");
        remaining -= this;
    }
    new_handles
}

/// Untimed aux capture for one case: bring `db` to the same steady state the
/// timed setup does (populate, one warm-up churn cycle), then run ONE more
/// churn cycle bracketed by `file_size_bytes` / `counters` reads. The caller
/// opens `db` exactly as its timed setup does, so the recorded cycle is the
/// one the timed rows measure.
fn capture_churn_aux(
    mut db: Chisel,
    live_count: usize,
    size_bytes: usize,
    cell_id: CellId,
) -> CellAuxMetrics {
    let handles = populate(&mut db, live_count, size_bytes);
    let handles = churn_cycle(&mut db, handles, size_bytes);

    let counters_before = db.counters().expect("counters before");
    let size_before = db.file_size_bytes().expect("size before");
    let _ = churn_cycle(&mut db, handles, size_bytes);
    let counters_after = db.counters().expect("counters after");
    let size_after = db.file_size_bytes().expect("size after");

    CellAuxMetrics {
        cell_id,
        file_size_delta_bytes: size_after as i64 - size_before as i64,
        counters: counter_delta(Some(counters_before), Some(counters_after)),
    }
}

/// Bench group 1: DELETE-CHURN-FLAT (in-memory engine).
///
/// Measures the cost of `cycles` delete/realloc commit pairs at a fixed live
/// size. The throughput unit is "delete+realloc round-trips per second"
/// (one round-trip = 2 commits). The bench body is inside `iter_batched` so
/// each Criterion sample gets a freshly populated, warmed engine and the
/// setup cost stays outside the measured segment.
///
/// The flat-high-water property: `file_size_bytes` at the end of the last
/// cycle should be <= `file_size_bytes` at the end of the warm-up cycle.
/// This bench does NOT assert that numerically — assertions on raw bench
/// numbers are explicitly forbidden by project conventions. It records it
/// instead, as `file_size_delta_bytes` in the aux file (see the module note).
fn bench_delete_churn_flat(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    // (live_count, size_bytes, label)
    let cases: &[(usize, usize, &str)] = &[
        (500, 256, "500x256B"),
        (200, 4096, "200x4KB"),
        (100, 16384, "100x16KB"),
    ];

    let group_name = "freemap-churn-flat";
    let mut group = c.benchmark_group(group_name);

    for &(live_count, size_bytes, label) in cases {
        let cache_bytes = CACHE_PAGES as u64 * PAGE_SIZE as u64;
        let db = Chisel::open_in_memory_with_options(
            Options::default()
                .cache_max_bytes(cache_bytes)
                .spillway_max_bytes(cache_bytes * 1024),
        )
        .expect("open chisel aux");
        aux.append(&capture_churn_aux(
            db,
            live_count,
            size_bytes,
            CellId {
                row: group_name,
                mode: "in-memory",
                size: label,
            },
        ))
        .expect("append freemap_churn aux");

        // Throughput: one unit = one (delete + realloc) round-trip, and each
        // timed iteration runs exactly one `churn_cycle`, so Elements(1) makes
        // Criterion's reported rate literally round-trips/sec. This used to be
        // Elements(live_count), i.e. records churned/sec: trend data recorded
        // before this change is normalised by live_count and must be divided
        // by live_count to compare with data after it.
        group.throughput(criterion::Throughput::Elements(1));

        group.bench_with_input(BenchmarkId::new("in-memory", label), &(), |b, _| {
            // Setup: open a fresh in-memory engine and populate `live_count`
            // records. The engine is rebuilt per Criterion sample so each sample
            // starts from a clean warm state.
            b.iter_batched(
                || {
                    // Genuinely in-memory, matching the `in-memory` benchmark
                    // id above. This used to open a NamedTempFile on the stated
                    // grounds that "in-memory / Vec<u8> opens need special
                    // handling in Chisel" — which was never true;
                    // `open_in_memory_with_options` is public and is what
                    // bench/src/chisel_engine.rs's own in-memory constructor
                    // calls. The consequence was that the Criterion row named
                    // `freemap-churn-flat/in-memory/...` carried real page
                    // writes and real fsync cost, i.e. exactly the fsync tax
                    // the in-memory mode exists to exclude, while group 2
                    // below (which IS file-backed) looked like the only
                    // file-touching group.
                    let cache_bytes = CACHE_PAGES as u64 * PAGE_SIZE as u64;
                    let mut db = Chisel::open_in_memory_with_options(
                        Options::default()
                            .cache_max_bytes(cache_bytes)
                            .spillway_max_bytes(cache_bytes * 1024),
                    )
                    .expect("open chisel");
                    let handles = populate(&mut db, live_count, size_bytes);
                    // Warm-up: one churn cycle BEFORE the timed segment so the
                    // freemap structural recycle pool is primed (steady state).
                    let handles = churn_cycle(&mut db, handles, size_bytes);
                    (db, handles)
                },
                |(mut db, handles)| {
                    // Timed: one delete+realloc round-trip. Nothing is read
                    // back here; the untimed aux pass records the high-water.
                    let new_handles = churn_cycle(&mut db, handles, size_bytes);
                    black_box(new_handles.len());
                    // Return the engine rather than dropping it here.
                    // `iter_batched` drops the routine's output AFTER
                    // `measurement.end`, so this moves engine teardown (and
                    // freeing the in-memory backing store) out of the timed
                    // span; it is not the round-trip this row measures.
                    (db, new_handles)
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

/// Bench group 2: RECLAMATION-PAGES-ALLOCATED (file-backed engine).
///
/// Measures the `pages_allocated` counter delta across a churn cycle (delete
/// then reallocate the same `live_count` records). When freemap reclamation
/// is working, the second (realloc) commit draws from freed ids and allocates
/// few or zero NEW pages beyond what the freemap already tracks.
///
/// `pages_allocated` here is `ChiselCounters::pages_allocated`, which counts
/// BOTH file extensions (`PageCache::new_page`) AND freemap reuses
/// (`PageCache::claim_page`) — see `ChiselCounters` in src/stats.rs. It is
/// therefore NOT ~0 under healthy reclamation: a realloc commit that reuses
/// every freed page still counts each claim. Reclamation shows up as this
/// counter paired with a ~0 `file_size_delta_bytes` (pages handed out, file
/// not extended); a broken freemap shows a similar counter with the file
/// growing by the extension count.
///
/// Report-only. One aux line per case is written from an untimed pass (see
/// the module note at the top); the timed closure reads nothing back.
fn bench_reclamation_pages_allocated(c: &mut Criterion, aux: &mut AuxMetricsWriter) {
    let cases: &[(usize, usize, &str)] = &[(200, 256, "200x256B"), (100, 4096, "100x4KB")];

    let group_name = "freemap-reclamation-pages-allocated";
    let mut group = c.benchmark_group(group_name);

    for &(live_count, size_bytes, label) in cases {
        {
            let tf = NamedTempFile::new().expect("tempfile aux");
            let cache_bytes = CACHE_PAGES as u64 * PAGE_SIZE as u64;
            let db = Chisel::open(
                tf.path(),
                Options::default()
                    .cache_max_bytes(cache_bytes)
                    .spillway_max_bytes(cache_bytes * 1024),
            )
            .expect("open chisel aux");
            aux.append(&capture_churn_aux(
                db,
                live_count,
                size_bytes,
                CellId {
                    row: group_name,
                    mode: "chisel-file",
                    size: label,
                },
            ))
            .expect("append freemap_churn aux");
            // `capture_churn_aux` consumed and dropped the engine, so the
            // tempfile is unlinked (end of this block) only after close.
        }

        group.throughput(Throughput::Elements(live_count as u64));

        group.bench_with_input(BenchmarkId::new("chisel-file", label), &(), |b, _| {
            b.iter_batched(
                || {
                    let tf = NamedTempFile::new().expect("tempfile");
                    let cache_bytes = CACHE_PAGES as u64 * PAGE_SIZE as u64;
                    let mut db = Chisel::open(
                        tf.path(),
                        Options::default()
                            .cache_max_bytes(cache_bytes)
                            .spillway_max_bytes(cache_bytes * 1024),
                    )
                    .expect("open chisel");
                    let handles = populate(&mut db, live_count, size_bytes);
                    // Warm-up churn cycle: primes the structural recycle pool
                    // so the measured cycle starts in steady state.
                    let handles = churn_cycle(&mut db, handles, size_bytes);
                    (db, handles, tf)
                },
                |(mut db, handles, tf)| {
                    // Timed: one delete+realloc round-trip. The counter and
                    // file-size deltas are recorded by the untimed aux pass,
                    // so their reads stay out of this span.
                    let new_handles = churn_cycle(&mut db, handles, size_bytes);
                    black_box(new_handles.len());
                    // Return engine + tempfile so Criterion drops them after
                    // `measurement.end`. Dropping the NamedTempFile here
                    // charged this row for unlinking a multi-megabyte file,
                    // whose cost varies by filesystem and has nothing to do
                    // with reclamation throughput.
                    (db, new_handles, tf)
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

/// Bench group 3: DEFRAG-ORPHAN-SWEEP (file-backed engine, defrag pass).
///
/// Measures the cost of `defrag()` when it includes the freemap orphan sweep
/// (`reclaim_freemap_orphans` step 7 in `defrag.rs`). After normal churn the
/// in-memory structural recycle pool holds superseded freemap pages; a crash
/// would orphan them. This bench simulates the pattern: churn, drop the engine
/// (orphaning the recycle pool), reopen, and run defrag. The timed segment is
/// the reopen + defrag.
///
/// At the default (depth-0) scale, the sweep reclaims 0..2 pages per run
/// (the freemap has at most 1-2 structural supersedes per commit). The bench
/// exists to track that the sweep is cheap and does not regress linearly with
/// live-data size.
fn bench_defrag_orphan_sweep(c: &mut Criterion) {
    let cases: &[(usize, usize, &str)] = &[(300, 256, "300x256B"), (100, 4096, "100x4KB")];

    let mut group = c.benchmark_group("freemap-defrag-orphan-sweep");
    group.sample_size(20); // fewer samples: each iter is a full open+defrag+commit

    for &(live_count, size_bytes, label) in cases {
        group.throughput(Throughput::Elements(live_count as u64));

        group.bench_with_input(BenchmarkId::new("chisel-file", label), &(), |b, _| {
            b.iter_batched(
                || {
                    // Build a database, churn once (so the freemap has been
                    // COW'd at least once), and capture the file path.
                    // Drop the engine WITHOUT saving the structural recycle
                    // pool — simulating a crash that orphans those pages.
                    let tf = NamedTempFile::new().expect("tempfile");
                    let cache_bytes = CACHE_PAGES as u64 * PAGE_SIZE as u64;
                    {
                        let mut db = Chisel::open(
                            tf.path(),
                            Options::default()
                                .cache_max_bytes(cache_bytes)
                                .spillway_max_bytes(cache_bytes * 1024),
                        )
                        .expect("open chisel setup");
                        let handles = populate(&mut db, live_count, size_bytes);
                        // One churn commit to dirty the freemap (creates
                        // structural supersedes in the in-memory recycle pool).
                        let _new_handles = churn_cycle(&mut db, handles, size_bytes);
                        // Drop `db` here: the in-memory recycle pool is lost,
                        // potentially orphaning freemap pages (crash simulation).
                    }
                    (tf, cache_bytes)
                },
                |(tf, cache_bytes)| {
                    // Timed: reopen + defrag (which runs the orphan sweep).
                    let mut db = Chisel::open(
                        black_box(tf.path()),
                        Options::default()
                            .cache_max_bytes(cache_bytes)
                            .spillway_max_bytes(cache_bytes * 1024),
                    )
                    .expect("open chisel measured");

                    db.begin().expect("begin defrag");
                    let stats = db.defrag(DefragOptions::default()).expect("defrag");
                    db.commit().expect("commit defrag");

                    // Report orphans_reclaimed through black_box so the
                    // optimizer cannot elide the defrag work.
                    black_box(stats.freemap_orphans_reclaimed);
                    black_box(db.file_size_bytes().expect("file_size after defrag"));
                    // These used to be `drop(db); drop(tf);` right here, which
                    // put the engine close and the tempfile unlink INSIDE the
                    // measured span. The timed segment is reopen + defrag, not
                    // teardown; returning the pair lets `iter_batched` drop it
                    // after `measurement.end`.
                    (db, tf)
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

/// Entry point: owns the aux writer for groups 1 and 2 (one file per bench
/// run, truncated at start — see the module note) and registers all three
/// groups.
fn freemap_churn(c: &mut Criterion) {
    let mut aux = AuxMetricsWriter::create(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/results/freemap_churn_aux.jsonl"
    ))
    .expect("create freemap_churn aux file");

    bench_delete_churn_flat(c, &mut aux);
    bench_reclamation_pages_allocated(c, &mut aux);
    bench_defrag_orphan_sweep(c);
}

criterion_group!(benches, freemap_churn);
criterion_main!(benches);
