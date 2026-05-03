// Markdown summary post-processor module. Reads PR 4b's bench output
// (target/criterion/<row>/<mode>/<size>/sample.json + bench/results/aux_metrics.jsonl)
// and produces three artifacts under bench/results/<UTC-ISO8601>/:
//
//   summary.md     — human-readable per-row tables
//   results.json   — flat composite-key schema for PR 7's CI diff
//   raw/           — archival copy of estimates.json + sample.json per cell
//
// The five submodules split by responsibility, not by output artifact:
//
//   format        — magnitude-adaptive formatters + percentile-interp helper
//   discover      — Cell/TimingStats/AuxMetrics types + filesystem walk + JSONL parse
//   metadata      — Metadata/MachineInfo + git/hostname gathering
//   render_json   — Vec<Cell> + Metadata -> serde_json::Value
//   render_md     — Vec<Cell> + Metadata -> String

pub mod discover;
pub mod format;
pub mod metadata;
