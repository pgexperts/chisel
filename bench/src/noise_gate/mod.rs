// noise_gate.rs — Setup-time noise-validation gate for the dedicated
// benchmark machine.
//
// Runs the scenario tier N times back-to-back, computes coefficient of
// variation per cell across the runs, and reports whether the cells stay
// under threshold. Used at provisioning time to qualify a candidate
// instance type before it goes into production.
//
// Deliberately does NOT run continuously in production — periodic
// re-validation is deferred to a future bench-noise-monitor.yml workflow.

pub mod cov;
pub mod report;

// `pub use cov::{compute_cov, Cov}` and `pub use report::render_report`
// are intentionally deferred until those items exist — they are added
// once the implementations land in tasks 4.3 / 4.5. Adding them here
// in the skeleton phase would block the crate from compiling.
