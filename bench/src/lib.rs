// Bench harness for Chisel.
//
// This crate provides the layered architecture described in the
// benchmark-suite design spec (`docs/superpowers/specs/2026-04-25-
// chisel-benchmark-suite-design.md`):
//
//   Engine trait  ── uniform façade over chisel / redb / sqlite
//   Workload      ── seeded operation-sequence generators
//   Runner        ── pre-population, cache state control, Criterion glue
//   Reporter      ── Markdown + JSON output post-processing
//
// PRs 1–2 + PR-A + PR 3 landed the Engine trait and all three engine
// impls. PR 4a (this PR) lands the Workload data layer. PR 4b adds
// the Runner + 270-cell registration. PRs 5–7 follow.

pub mod chisel_engine;
pub mod engine;
pub mod redb_engine;
pub mod sqlite_engine;
pub mod workload;

pub use chisel_engine::ChiselEngine;
pub use engine::{DurabilityMode, Engine, EngineResult, Identifier};
pub use redb_engine::RedbEngine;
pub use sqlite_engine::SqliteEngine;
pub use workload::{Operation, Workload};
