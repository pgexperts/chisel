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
}
