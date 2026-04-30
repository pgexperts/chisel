// Pure data-layer for benchmark workloads. No engine interaction —
// a `Workload` can be built, inspected, and unit-tested without any
// of the engine impls compiled in. The Runner (PR 4b) consumes
// `Workload` values and drives them against an `Engine`.
//
// Determinism contract: `(seed, params) -> Workload` is a pure
// function. We pin `ChaCha8Rng` rather than `StdRng` so result
// reproducibility survives `rand` minor bumps.

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
}
