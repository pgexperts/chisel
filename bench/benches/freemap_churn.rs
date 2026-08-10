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
//      cycle, reported via Criterion throughput annotations.
//
// NOTE ON WHAT IS ACTUALLY REPORTED: only Criterion's wall-clock timings and
// throughput annotations. This file writes NO aux metrics — it constructs no
// `AuxMetricsWriter` (only micro_grid.rs does), and the `file_size_bytes` /
// `pages_allocated` values it reads are `black_box`'d and discarded. Three
// comments here used to claim otherwise; they are corrected in place below.
// Wiring up an aux writer so the flat-high-water and reclamation numbers are
// genuinely trend-tracked is real work, and worth doing — but until it is
// done, this file must not say it has been.
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
/// numbers are explicitly forbidden by project conventions. It does not
/// record it either: the value is `black_box`'d to keep the optimizer honest
/// and then dropped. Observing the property today means reading the timings
/// and running the engine by hand; see the module note above.
fn bench_delete_churn_flat(c: &mut Criterion) {
    // (live_count, size_bytes, label)
    let cases: &[(usize, usize, &str)] = &[
        (500, 256, "500x256B"),
        (200, 4096, "200x4KB"),
        (100, 16384, "100x16KB"),
    ];

    let mut group = c.benchmark_group("freemap-churn-flat");

    for &(live_count, size_bytes, label) in cases {
        // Throughput: one unit = one (delete + realloc) round-trip.
        group.throughput(criterion::Throughput::Elements(live_count as u64));

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
                    // Timed: one delete+realloc round-trip.
                    let new_handles = churn_cycle(&mut db, handles, size_bytes);
                    // black_box both engine and handles to prevent the
                    // optimizer from eliding the work.
                    black_box(db.file_size_bytes().expect("file_size_bytes"));
                    black_box(new_handles.len())
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
/// `PageCache::new_page` calls (= file extensions). After the warm-up, the
/// second-commit delta should be close to 0 (reusing freed pages) rather than
/// `live_count` (extending for every new record). This is observable through
/// the Chisel public counters API.
///
/// Report-only, and — like group 1 — NOT currently trend-tracked: the
/// `pages_allocated` delta is `black_box`'d, not written anywhere. This used
/// to claim results go to bench/results/aux_metrics.jsonl "via the
/// AuxMetricsWriter pattern"; no `AuxMetricsWriter` is constructed in this
/// file at all. See the module note at the top.
fn bench_reclamation_pages_allocated(c: &mut Criterion) {
    let cases: &[(usize, usize, &str)] = &[(200, 256, "200x256B"), (100, 4096, "100x4KB")];

    let mut group = c.benchmark_group("freemap-reclamation-pages-allocated");

    for &(live_count, size_bytes, label) in cases {
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
                |(mut db, handles, _tf)| {
                    let counters_before = db.counters().expect("counters before");
                    let size_before = db.file_size_bytes().expect("size before");

                    let new_handles = churn_cycle(&mut db, handles, size_bytes);

                    let counters_after = db.counters().expect("counters after");
                    let size_after = db.file_size_bytes().expect("size after");

                    // Report both metrics through black_box so the optimizer
                    // cannot eliminate the measurement calls.
                    black_box(
                        counters_after
                            .pages_allocated
                            .saturating_sub(counters_before.pages_allocated),
                    );
                    black_box(size_after as i64 - size_before as i64);
                    black_box(new_handles.len())
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
                    drop(db);
                    drop(tf);
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_delete_churn_flat,
    bench_reclamation_pages_allocated,
    bench_defrag_orphan_sweep,
);
criterion_main!(benches);
