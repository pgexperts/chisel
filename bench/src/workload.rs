// Pure data-layer for benchmark workloads. No engine interaction —
// a `Workload` can be built, inspected, and unit-tested without any
// of the engine impls compiled in. The Runner (PR 4b) consumes
// `Workload` values and drives them against an `Engine`.
//
// Determinism contract: `(seed, params) -> Workload` is a pure
// function. We pin `ChaCha8Rng` rather than `StdRng` so result
// reproducibility survives `rand` minor bumps.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, LogNormal, WeightedIndex, Zipf};

/// One unit of work the Runner executes against an Engine.
///
/// `alloc_index` is a position in the Runner's "allocations seen so
/// far" vector (per the alloc-order-index identifier scheme). Each
/// generator guarantees every emitted index is live (allocated and
/// not deleted) at that point in the sequence — so the Runner does
/// not need to filter or validate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Allocate a value of `size` bytes. The Runner fills the payload
    /// with `0u8` — content compressibility is out of scope.
    Allocate { size: usize },
    /// Read the value at allocation index `alloc_index`.
    Read { alloc_index: usize },
    /// Replace the value at `alloc_index` with `size` filler bytes.
    /// Independent of the original value's size; micro-grid generators
    /// happen to keep them equal but the type does not require it.
    Update { alloc_index: usize, size: usize },
    /// Delete the value at `alloc_index`. Generators do not emit
    /// further references to that index after a Delete.
    Delete { alloc_index: usize },
    /// Bulk-delete a list of indices in a single Operation. Same
    /// liveness contract as `Delete`. The only variant that carries
    /// a heap allocation, hence Operation derives Clone but not Copy.
    DeleteMany { alloc_indices: Vec<usize> },
}

/// A deterministic sequence of Operations plus enough metadata to
/// identify which generator produced it.
///
/// `name` is the Criterion BenchmarkId stem (e.g., "read_random").
/// The Runner appends size and other parameters when constructing
/// final BenchmarkIds — keeping size out of `name` lets one workload
/// drive multiple cells if a future caller wants to reuse it.
///
/// `seed` is preserved so logs and reports can answer "exactly what
/// ran?" `prepop_count` records how many records the workload assumes
/// already exist when execution starts; the Runner uses it to size
/// the pre-population step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    pub name: String,
    pub seed: u64,
    pub prepop_count: usize,
    pub ops: Vec<Operation>,
}

/// Tag for the four mutating-or-reading operation kinds, used by
/// `mix_operations` to pick which `Operation` variant to emit.
/// `DeleteMany` is intentionally absent — scenarios use the single
/// `Delete` variant per master spec §4.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Allocate,
    Read,
    Update,
    Delete,
}

/// Zipfian-distributed random indices into [0, prepop_count). `theta`
/// controls skew: 0.0 = uniform, 0.99 = YCSB default (heavy skew,
/// ~75% of accesses to ~10% of records).
///
/// Returns Vec<usize> of length `count`. Uses `rand_distr::Zipf`,
/// which models the Zipf distribution Z(N, s). The `Zipf` constructor
/// takes (n, s) where s is the exponent: s=0 is uniform, s=1.0+ is
/// heavily skewed. We translate YCSB's `theta` to s via `s = theta`
/// (rand_distr's parameterization matches YCSB's directly).
///
/// Deterministic: same (seed, count, prepop_count, theta) → same Vec.
/// Uses ChaCha8Rng seeded from `seed` for cross-platform reproducibility.
pub fn zipfian_indices(seed: u64, count: usize, prepop_count: usize, theta: f64) -> Vec<usize> {
    assert!(
        prepop_count > 0,
        "zipfian_indices: prepop_count must be > 0"
    );
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let zipf = Zipf::new(prepop_count as u64, theta).unwrap();
    (0..count)
        .map(|_| {
            // Zipf samples in [1, n], convert to [0, n-1] by subtracting 1.
            let v = zipf.sample(&mut rng) as usize;
            v.saturating_sub(1).min(prepop_count - 1)
        })
        .collect()
}

/// Log-normal-distributed sizes in bytes. `median_bytes` and
/// `p99_bytes` parameterize the distribution: log-normal has shape
/// (mu, sigma); we solve for mu/sigma such that exp(mu) = median
/// and exp(mu + 2.326 * sigma) ≈ p99 (z-score 2.326 for the 99th
/// percentile of a standard normal).
///
/// Returns Vec<usize> of length `n`. Sizes clamped to [16, 4_194_304]
/// (16B floor, 4MB ceiling) to avoid pathological outliers from the
/// log-normal tail. ~0.001% of unbounded log-normal samples land at
/// multi-GB sizes; clamping accepts a tiny bias in exchange for stable
/// wall-clock measurements.
///
/// Deterministic: same (seed, n, median_bytes, p99_bytes) → same Vec.
pub fn lognormal_sizes(seed: u64, n: usize, median_bytes: usize, p99_bytes: usize) -> Vec<usize> {
    assert!(
        median_bytes > 0,
        "lognormal_sizes: median_bytes must be > 0"
    );
    assert!(
        p99_bytes >= median_bytes,
        "lognormal_sizes: p99_bytes must be >= median_bytes"
    );
    let mu = (median_bytes as f64).ln();
    // z-score 2.326 for the 99th percentile of a standard normal.
    let sigma = ((p99_bytes as f64).ln() - mu) / 2.326;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let dist = LogNormal::new(mu, sigma).unwrap();
    (0..n)
        .map(|_| {
            let raw = dist.sample(&mut rng);
            // Clamp to [16, 4_194_304]. NaN/inf samples shunted to the
            // floor explicitly: f64::clamp is only safe when min/max are
            // non-NaN (true here), but it propagates NaN from the input,
            // and a NaN-as-usize cast is UB-adjacent — so the
            // is_finite() gate stays.
            if raw.is_finite() {
                raw.clamp(16.0, 4_194_304.0) as usize
            } else {
                16
            }
        })
        .collect()
}

/// Compose a mixed-op sequence from per-op-kind probabilities.
/// Decouples op-kind sampling from access-pattern + size sampling —
/// each op gets a fresh sample from the appropriate input slice.
///
/// `op_specs` is a slice of (OpKind, weight) pairs; weights are
/// normalized internally by `WeightedIndex`. `access_pattern` provides
/// alloc_index values for Read/Update/Delete ops (consumed in order).
/// `sizes` provides byte counts for Allocate/Update ops (consumed in
/// order).
///
/// The function consumes one access_pattern entry and/or one sizes
/// entry per op based on op kind:
///   - Allocate → consumes one size
///   - Read → consumes one access_pattern entry
///   - Update → consumes one access_pattern entry AND one size
///   - Delete → consumes one access_pattern entry
///
/// Caller must provide enough entries; the function panics with a
/// clear message on under-provision. The scenario generators
/// precompute the right counts based on the op-mix probabilities.
///
/// Deterministic: same (seed, count, op_specs, access_pattern, sizes)
/// → same Vec.
pub fn mix_operations(
    seed: u64,
    count: usize,
    op_specs: &[(OpKind, f64)],
    access_pattern: &[usize],
    sizes: &[usize],
) -> Vec<Operation> {
    assert!(
        !op_specs.is_empty(),
        "mix_operations: op_specs must be non-empty"
    );
    let weights: Vec<f64> = op_specs.iter().map(|(_, w)| *w).collect();
    let kinds: Vec<OpKind> = op_specs.iter().map(|(k, _)| *k).collect();
    let weighted = WeightedIndex::new(&weights).unwrap();

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut access_iter = access_pattern.iter();
    let mut sizes_iter = sizes.iter();
    let mut out: Vec<Operation> = Vec::with_capacity(count);

    for _ in 0..count {
        let kind = kinds[weighted.sample(&mut rng)];
        let op = match kind {
            OpKind::Allocate => {
                let size = *sizes_iter
                    .next()
                    .expect("mix_operations: not enough sizes for Allocate ops");
                Operation::Allocate { size }
            }
            OpKind::Read => {
                let alloc_index = *access_iter
                    .next()
                    .expect("mix_operations: not enough access entries for Read ops");
                Operation::Read { alloc_index }
            }
            OpKind::Update => {
                let alloc_index = *access_iter
                    .next()
                    .expect("mix_operations: not enough access entries for Update ops");
                let size = *sizes_iter
                    .next()
                    .expect("mix_operations: not enough sizes for Update ops");
                Operation::Update { alloc_index, size }
            }
            OpKind::Delete => {
                let alloc_index = *access_iter
                    .next()
                    .expect("mix_operations: not enough access entries for Delete ops");
                Operation::Delete { alloc_index }
            }
        };
        out.push(op);
    }
    out
}

/// Pre-population: `count` Allocate ops of `size` bytes each.
/// No randomness — the Workload is fully determined by its arguments.
/// `seed` is fixed at 0 and `name` at "prepopulate" so that
/// pre-population does not appear as a varying axis in Criterion ids.
/// `prepop_count` is 0 because the workload itself does the populating
/// — it does not assume any pre-existing records.
pub fn gen_prepopulate(count: usize, size: usize) -> Workload {
    let ops = (0..count).map(|_| Operation::Allocate { size }).collect();
    Workload {
        name: "prepopulate".to_string(),
        seed: 0,
        prepop_count: 0,
        ops,
    }
}

/// Row 1/2 of the micro grid (allocate, 1 op/tx and 1000 ops/tx).
/// `count` Allocate ops of `size` bytes. No randomness; takes no
/// seed (parallels `gen_prepopulate`, where seedless and unambiguous
/// beats symmetry-with-an-unused-arg). The Workload's `seed` field
/// is set to 0; `prepop_count` is 0 because rows 1/2 measure
/// allocate against an empty database.
pub fn gen_allocate(count: usize, size: usize) -> Workload {
    let ops = (0..count).map(|_| Operation::Allocate { size }).collect();
    Workload {
        name: "allocate".to_string(),
        seed: 0,
        prepop_count: 0,
        ops,
    }
}

/// Row 3/4 of the micro grid (read warm/cold). `count` Read ops with
/// alloc_indices sampled uniformly with replacement from
/// `0..prepop_count`. The same alloc_index may appear multiple times
/// in the workload — that is intentional, mirroring real read access
/// where popular records get hit repeatedly.
pub fn gen_read_random(seed: u64, prepop_count: usize, count: usize) -> Workload {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let ops = (0..count)
        .map(|_| Operation::Read {
            alloc_index: rng.gen_range(0..prepop_count),
        })
        .collect();
    Workload {
        name: "read_random".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Row 5/6 of the micro grid (update, 1 op/tx and 1000 ops/tx).
/// Same selection scheme as `gen_read_random` — uniform random with
/// replacement — but emits Update ops with `size` bytes of filler.
pub fn gen_update_random(seed: u64, prepop_count: usize, count: usize, size: usize) -> Workload {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let ops = (0..count)
        .map(|_| Operation::Update {
            alloc_index: rng.gen_range(0..prepop_count),
            size,
        })
        .collect();
    Workload {
        name: "update_random".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Row 7/8 of the micro grid (delete, 1 op/tx and 1000 ops/tx).
/// Sampled WITHOUT replacement so no index is deleted twice within
/// the workload — uses Floyd-style sampling via `rand::seq::index::sample`,
/// O(count) time, no need to materialize the full `0..prepop_count` Vec.
///
/// Panics if `count > prepop_count`. This is a generator-author bug
/// (you cannot delete more records than exist), not a runtime
/// condition; failing loud at construction time beats silently
/// producing a malformed workload.
pub fn gen_delete_random(seed: u64, prepop_count: usize, count: usize) -> Workload {
    assert!(
        count <= prepop_count,
        "gen_delete_random: count ({}) exceeds prepop_count ({})",
        count,
        prepop_count
    );
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let indices = rand::seq::index::sample(&mut rng, prepop_count, count);
    let ops = indices
        .iter()
        .map(|alloc_index| Operation::Delete { alloc_index })
        .collect();
    Workload {
        name: "delete_random".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

/// Row 9 of the micro grid (delete_many — bulk-delete primitive).
/// Each Operation is one DeleteMany carrying `batch_size` distinct
/// indices; `batches` total. Sampled without replacement across the
/// entire workload — no index appears in two different batches —
/// via a single `rand::seq::index::sample` call chunked into batches.
///
/// Panics if `batches * batch_size > prepop_count` (same rationale
/// as `gen_delete_random`: caller bug, fail loud).
pub fn gen_delete_many(
    seed: u64,
    prepop_count: usize,
    batches: usize,
    batch_size: usize,
) -> Workload {
    let total = batches * batch_size;
    assert!(
        total <= prepop_count,
        "gen_delete_many: batches*batch_size ({}) exceeds prepop_count ({})",
        total,
        prepop_count
    );
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all = rand::seq::index::sample(&mut rng, prepop_count, total).into_vec();
    let ops = all
        .chunks(batch_size)
        .map(|chunk| Operation::DeleteMany {
            alloc_indices: chunk.to_vec(),
        })
        .collect();
    Workload {
        name: "delete_many".to_string(),
        seed,
        prepop_count,
        ops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_construct_and_compare() {
        let op_a: Operation = Operation::Read { alloc_index: 7 };
        let op_b: Operation = Operation::Read { alloc_index: 7 };
        assert_eq!(op_a, op_b);

        let w = Workload {
            name: "test".to_string(),
            seed: 42,
            prepop_count: 100,
            ops: vec![op_a],
        };
        assert_eq!(w.name, "test");
        assert_eq!(w.ops.len(), 1);
    }

    #[test]
    fn gen_prepopulate_shape() {
        let w = gen_prepopulate(10, 32);
        assert_eq!(w.name, "prepopulate");
        assert_eq!(w.seed, 0);
        assert_eq!(w.prepop_count, 0);
        assert_eq!(w.ops.len(), 10);
        for op in &w.ops {
            assert!(matches!(op, Operation::Allocate { size: 32 }));
        }
    }

    #[test]
    fn gen_allocate_shape() {
        let w = gen_allocate(10, 64);
        assert_eq!(w.name, "allocate");
        assert_eq!(w.seed, 0);
        assert_eq!(w.prepop_count, 0);
        assert_eq!(w.ops.len(), 10);
        for op in &w.ops {
            assert!(matches!(op, Operation::Allocate { size: 64 }));
        }
    }

    #[test]
    fn gen_read_random_determinism() {
        let a = gen_read_random(42, 1000, 100);
        let b = gen_read_random(42, 1000, 100);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_read_random_validity() {
        let prepop = 1000;
        let w = gen_read_random(1, prepop, 500);
        assert_eq!(w.name, "read_random");
        assert_eq!(w.seed, 1);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), 500);
        for op in &w.ops {
            match op {
                Operation::Read { alloc_index } => {
                    assert!(*alloc_index < prepop, "out-of-range index {}", alloc_index);
                }
                other => panic!("expected Read, got {:?}", other),
            }
        }
    }

    #[test]
    fn gen_read_random_cross_seed_independence() {
        let a = gen_read_random(1, 1000, 100);
        let b = gen_read_random(2, 1000, 100);
        assert_ne!(a.ops, b.ops);
    }

    #[test]
    fn gen_update_random_determinism() {
        let a = gen_update_random(42, 1000, 100, 64);
        let b = gen_update_random(42, 1000, 100, 64);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_update_random_validity() {
        let prepop = 1000;
        let w = gen_update_random(1, prepop, 500, 256);
        assert_eq!(w.name, "update_random");
        assert_eq!(w.seed, 1);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), 500);
        for op in &w.ops {
            match op {
                Operation::Update { alloc_index, size } => {
                    assert!(*alloc_index < prepop, "out-of-range index {}", alloc_index);
                    assert_eq!(*size, 256);
                }
                other => panic!("expected Update, got {:?}", other),
            }
        }
    }

    #[test]
    fn gen_delete_random_determinism() {
        let a = gen_delete_random(42, 1000, 500);
        let b = gen_delete_random(42, 1000, 500);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_delete_random_no_replacement() {
        use std::collections::HashSet;
        let prepop = 1000;
        let count = 500;
        let w = gen_delete_random(1, prepop, count);
        assert_eq!(w.name, "delete_random");
        assert_eq!(w.seed, 1);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), count);

        let mut seen: HashSet<usize> = HashSet::new();
        for op in &w.ops {
            match op {
                Operation::Delete { alloc_index } => {
                    assert!(*alloc_index < prepop, "out-of-range index {}", alloc_index);
                    assert!(seen.insert(*alloc_index), "duplicate index {}", alloc_index);
                }
                other => panic!("expected Delete, got {:?}", other),
            }
        }
        assert_eq!(seen.len(), count);
    }

    #[test]
    #[should_panic(expected = "exceeds prepop_count")]
    fn gen_delete_random_panics_on_overcount() {
        // count > prepop_count is a generator-author bug; we panic
        // rather than silently produce an invalid workload.
        let _ = gen_delete_random(0, 10, 11);
    }

    #[test]
    fn gen_delete_many_determinism() {
        let a = gen_delete_many(7, 10_000, 5, 200);
        let b = gen_delete_many(7, 10_000, 5, 200);
        assert_eq!(a, b);
    }

    #[test]
    fn gen_delete_many_cross_batch_no_replacement() {
        use std::collections::HashSet;
        let prepop = 1000;
        let batches = 5;
        let batch_size = 100;
        let w = gen_delete_many(2, prepop, batches, batch_size);
        assert_eq!(w.name, "delete_many");
        assert_eq!(w.seed, 2);
        assert_eq!(w.prepop_count, prepop);
        assert_eq!(w.ops.len(), batches);

        let mut seen: HashSet<usize> = HashSet::new();
        for op in &w.ops {
            match op {
                Operation::DeleteMany { alloc_indices } => {
                    assert_eq!(alloc_indices.len(), batch_size);
                    for &i in alloc_indices {
                        assert!(i < prepop, "out-of-range index {}", i);
                        assert!(seen.insert(i), "duplicate index {} across batches", i);
                    }
                }
                other => panic!("expected DeleteMany, got {:?}", other),
            }
        }
        assert_eq!(seen.len(), batches * batch_size);
    }

    #[test]
    #[should_panic(expected = "exceeds prepop_count")]
    fn gen_delete_many_panics_on_overcount() {
        let _ = gen_delete_many(0, 10, 3, 4); // 3 * 4 = 12 > 10
    }

    #[test]
    fn zipfian_indices_determinism() {
        let a = zipfian_indices(42, 1000, 100, 0.99);
        let b = zipfian_indices(42, 1000, 100, 0.99);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1000);
    }

    #[test]
    fn zipfian_indices_distribution_is_skewed() {
        // Theta=0.99 (YCSB default) means heavy skew. Over 10K samples
        // from [0, 100), the top decile (indices 0..10) should receive
        // ~75% of accesses per spec §4.1. Allow generous tolerance.
        let samples = zipfian_indices(7, 10_000, 100, 0.99);
        let top_decile = samples.iter().filter(|&&i| i < 10).count();
        let pct = (top_decile as f64) / 10_000.0;
        assert!(
            pct > 0.50,
            "zipfian θ=0.99 over 10K samples expected ≥50% in top decile, got {pct:.3}"
        );
    }

    #[test]
    fn lognormal_sizes_determinism() {
        let a = lognormal_sizes(42, 1000, 4096, 1_048_576);
        let b = lognormal_sizes(42, 1000, 4096, 1_048_576);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1000);
    }

    #[test]
    fn lognormal_sizes_clamps_outliers() {
        // Generate 10K samples; assert all fall inside [16, 4_194_304].
        // The lognormal tail can produce both ~0-byte and gigabyte
        // values; the clamp protects bench timing from outliers.
        let samples = lognormal_sizes(7, 10_000, 4096, 1_048_576);
        for &s in &samples {
            assert!(s >= 16, "lognormal_sizes should clamp at >= 16, got {s}");
            assert!(
                s <= 4_194_304,
                "lognormal_sizes should clamp at <= 4_194_304, got {s}"
            );
        }
    }

    #[test]
    fn mix_operations_op_proportions() {
        // 50/50 read/update mix over 10_000 ops should produce ~5000
        // of each (binomial slack: ±200 covers > 99.99% of cases).
        let access: Vec<usize> = (0..10_000).map(|i| i % 100).collect();
        let sizes: Vec<usize> = (0..10_000).map(|_| 1024).collect();
        let ops = mix_operations(
            42,
            10_000,
            &[(OpKind::Read, 0.5), (OpKind::Update, 0.5)],
            &access,
            &sizes,
        );
        assert_eq!(ops.len(), 10_000);
        let reads = ops
            .iter()
            .filter(|o| matches!(o, Operation::Read { .. }))
            .count();
        let updates = ops
            .iter()
            .filter(|o| matches!(o, Operation::Update { .. }))
            .count();
        assert!(
            (4800..=5200).contains(&reads),
            "expected ~5000 reads ±200, got {reads}"
        );
        assert!(
            (4800..=5200).contains(&updates),
            "expected ~5000 updates ±200, got {updates}"
        );
        assert_eq!(reads + updates, 10_000);
    }

    #[test]
    fn mix_operations_uses_provided_inputs() {
        // Allocate ops should use sizes; Read ops should use access_pattern.
        let access: Vec<usize> = vec![7, 13, 21, 35];
        let sizes: Vec<usize> = vec![64, 128, 256, 512];
        let ops = mix_operations(
            42,
            4,
            &[(OpKind::Allocate, 0.5), (OpKind::Read, 0.5)],
            &access,
            &sizes,
        );
        assert_eq!(ops.len(), 4);
        for op in &ops {
            match op {
                Operation::Allocate { size } => {
                    assert!(
                        sizes.contains(size),
                        "allocate size {size} not in sizes slice"
                    );
                }
                Operation::Read { alloc_index } => {
                    assert!(
                        access.contains(alloc_index),
                        "read alloc_index {alloc_index} not in access slice"
                    );
                }
                _ => panic!("expected only Allocate or Read"),
            }
        }
    }
}
