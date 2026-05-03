// Markdown renderer for the summary post-processor. Produces a single
// String from a Vec<Cell> + Metadata. Document structure follows
// master spec §7.1 + spec §4.1 of this PR.
//
// The output document is composed in this order:
//   1. Header (timestamp, commit, machine, cell count)
//   2. Durability mode legend (5 modes; "-strict" is durable, "-unsafe" is diagnostic)
//   3. Method/disclaimer (explains p50/p99 and Criterion HTML report path)
//   4. Micro grid (one H3 subsection per row; modes as rows; sizes as numerically
//      sorted columns; cell value is "p50 (p99)" via magnitude-adaptive units)
//   5. File-size delta table (rectangular table; rows are (row, mode); columns are sizes)
//   6. Chisel internals appendix (counter deltas for chisel-strict cells only)
//   7. Footer (post-processor version + chisel commit again)
//
// Modes are iterated in MODE_ORDER (canonical) rather than alphabetical so
// "chisel-strict" is always the leftmost reference point and the *-strict /
// *-unsafe pairs stay adjacent. Sizes are sorted numerically via
// parse_size_to_bytes() so "32B" precedes "1MB" rather than the other way
// around. Cells with missing timing render as the U+2014 em-dash ("—").
//
// All writes go through `let _ = writeln!(out, ...)` because writing to a
// String is infallible — the io::Result is uniformly Ok and we don't want
// to clutter the call sites with .unwrap() noise.

use crate::summary::discover::Cell;
use crate::summary::format::{format_bytes, format_duration_ns, parse_size_to_bytes};
use crate::summary::metadata::Metadata;
use std::collections::BTreeSet;
use std::fmt::Write;

/// Canonical ordering of durability modes. Drives row order in the micro
/// grid and the file-size delta table. The pairing of `-strict` / `-unsafe`
/// for redb and SQLite is intentionally adjacent so reviewers can spot
/// the durability-cost gap at a glance.
const MODE_ORDER: &[&str] = &[
    "chisel-strict",
    "redb-strict",
    "redb-unsafe",
    "sqlite-strict",
    "sqlite-unsafe",
];

/// Render a Vec<Cell> + Metadata into a complete markdown summary
/// (header + per-row tables + file-size delta table + Chisel internals
/// appendix + footer).
pub fn render_markdown(cells: &[Cell], metadata: &Metadata) -> String {
    let mut out = String::new();
    render_header(&mut out, metadata);
    render_durability_legend(&mut out);
    render_disclaimer(&mut out);
    render_micro_grid(&mut out, cells);
    render_file_size_table(&mut out, cells);
    render_chisel_internals_appendix(&mut out, cells);
    render_footer(&mut out, metadata);
    out
}

fn render_header(out: &mut String, m: &Metadata) {
    let _ = writeln!(out, "# Chisel Benchmark Summary\n");
    let _ = writeln!(out, "**Generated:** {}", m.timestamp);
    let _ = writeln!(out, "**Chisel commit:** {}", m.chisel_commit);
    let _ = writeln!(
        out,
        "**Machine:** {} {} — hostname: {}",
        m.machine.os, m.machine.arch, m.machine.hostname
    );
    let _ = writeln!(out, "**Cells:** {}", m.cell_count);
    let _ = writeln!(out);
}

fn render_durability_legend(out: &mut String) {
    let _ = writeln!(out, "## Durability mode legend\n");
    let _ = writeln!(
        out,
        "- `chisel-strict` — Chisel native (always fsync; no unsafe mode by design)"
    );
    let _ = writeln!(out, "- `redb-strict` — redb with `Durability::Immediate`");
    let _ = writeln!(
        out,
        "- `redb-unsafe` — redb with `Durability::Eventual` (diagnostic only — not durable)"
    );
    let _ = writeln!(
        out,
        "- `sqlite-strict` — SQLite with `synchronous=FULL` and WAL journaling"
    );
    let _ = writeln!(
        out,
        "- `sqlite-unsafe` — SQLite with `synchronous=OFF` (diagnostic only — not durable)"
    );
    let _ = writeln!(out);
}

fn render_disclaimer(out: &mut String) {
    let _ = writeln!(out, "## Method\n");
    let _ = writeln!(out, "Wall-clock cells show `p50 (p99)` in magnitude-adaptive units. Percentiles are computed directly from Criterion's raw `sample.json` per-iteration times via numpy-style linear interpolation; all three percentiles share the same sample distribution. With Criterion's default ~100 samples per cell, p99 has appreciable statistical uncertainty — for tight tail bounds, consult Criterion's per-cell HTML report under `target/criterion/<row>/<mode>/<size>/report/`.");
    let _ = writeln!(out);
}

fn render_micro_grid(out: &mut String, cells: &[Cell]) {
    let _ = writeln!(out, "## Micro grid\n");

    // Distinct rows in alphabetical order — BTreeSet gives us dedup +
    // sort in one step. (Spec calls for stable ordering; alphabetical
    // is fine since row names like "allocate-1pertx" sort sensibly.)
    let rows: Vec<String> = {
        let mut seen = BTreeSet::new();
        for c in cells {
            seen.insert(c.row.clone());
        }
        seen.into_iter().collect()
    };

    for row in &rows {
        let row_cells: Vec<&Cell> = cells.iter().filter(|c| &c.row == row).collect();
        if row_cells.is_empty() {
            continue;
        }

        // Size columns: distinct + numerically ascending (NOT lexical —
        // "1MB" must come after "128KB", not before).
        let mut sizes: Vec<String> = {
            let mut seen = BTreeSet::new();
            for c in &row_cells {
                seen.insert(c.size.clone());
            }
            seen.into_iter().collect()
        };
        sizes.sort_by_key(|s| parse_size_to_bytes(s).unwrap_or(u64::MAX));

        let _ = writeln!(out, "### `{}`\n", row);
        // Header row.
        let _ = write!(out, "| mode |");
        for size in &sizes {
            let _ = write!(out, " {} |", size);
        }
        let _ = writeln!(out);
        // Separator row.
        let _ = write!(out, "|------|");
        for _ in &sizes {
            let _ = write!(out, "-----|");
        }
        let _ = writeln!(out);
        // Data rows — one per mode in canonical MODE_ORDER. Modes
        // missing for this row render as a full row of em-dashes
        // (rather than being skipped) so the table shape stays stable
        // across rows for visual diffing.
        for mode in MODE_ORDER {
            let _ = write!(out, "| {} |", mode);
            for size in &sizes {
                let cell_opt = row_cells
                    .iter()
                    .find(|c| c.mode == *mode && &c.size == size);
                let cell_str = match cell_opt.and_then(|c| c.timing.as_ref()) {
                    Some(t) => format!(
                        "{} ({})",
                        format_duration_ns(t.p50_ns),
                        format_duration_ns(t.p99_ns)
                    ),
                    None => "—".to_string(),
                };
                let _ = write!(out, " {} |", cell_str);
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out);
    }
}

fn render_file_size_table(out: &mut String, cells: &[Cell]) {
    let _ = writeln!(out, "## File-size delta\n");
    let _ = writeln!(out, "Captured per-cell from one calibration iteration (post-Criterion measurement). Negative values indicate the operation shrunk the file. Magnitude-adaptive units (1024-base).\n");

    // Distinct sizes from all cells, numerically ascending.
    let mut sizes: Vec<String> = {
        let mut seen = BTreeSet::new();
        for c in cells {
            seen.insert(c.size.clone());
        }
        seen.into_iter().collect()
    };
    sizes.sort_by_key(|s| parse_size_to_bytes(s).unwrap_or(u64::MAX));

    // Header.
    let _ = write!(out, "| row | mode |");
    for size in &sizes {
        let _ = write!(out, " {} |", size);
    }
    let _ = writeln!(out);
    let _ = write!(out, "|-----|------|");
    for _ in &sizes {
        let _ = write!(out, "-----|");
    }
    let _ = writeln!(out);

    // Group by (row, mode) — row alphabetical (BTreeSet), mode by
    // canonical MODE_ORDER. Skip (row, mode) pairs where no cell
    // exists at all (avoids blank rows for modes that aren't run
    // for that row).
    let mut row_set: BTreeSet<String> = BTreeSet::new();
    for c in cells {
        row_set.insert(c.row.clone());
    }

    for row in &row_set {
        for mode in MODE_ORDER {
            let has_any = cells.iter().any(|c| &c.row == row && c.mode == *mode);
            if !has_any {
                continue;
            }
            let _ = write!(out, "| {} | {} |", row, mode);
            for size in &sizes {
                let cell_opt = cells
                    .iter()
                    .find(|c| &c.row == row && c.mode == *mode && &c.size == size);
                let s = match cell_opt.and_then(|c| c.aux.as_ref()) {
                    Some(a) => format_bytes(a.file_size_delta_bytes),
                    None => "—".to_string(),
                };
                let _ = write!(out, " {} |", s);
            }
            let _ = writeln!(out);
        }
    }
    let _ = writeln!(out);
}

fn render_chisel_internals_appendix(out: &mut String, cells: &[Cell]) {
    let _ = writeln!(out, "## Chisel internals appendix\n");
    let _ = writeln!(out, "Counter deltas for cells where `engine_mode = chisel-strict`. One row per cell (row × size); columns are the four counters from `Chisel::counters()`.\n");
    let _ = writeln!(
        out,
        "| row | size | cache_hits | cache_misses | fsync_calls | pages_allocated |"
    );
    let _ = writeln!(
        out,
        "|-----|------|------------|--------------|-------------|-----------------|"
    );

    // Filter to chisel-strict only, then sort by (row, numeric-size).
    // Using numeric size keeps the appendix readable when sizes span
    // multiple orders of magnitude (32B / 256B / 2KB / ... / 1MB).
    let mut chisel_cells: Vec<&Cell> = cells.iter().filter(|c| c.mode == "chisel-strict").collect();
    chisel_cells.sort_by(|a, b| {
        a.row.cmp(&b.row).then_with(|| {
            let asize = parse_size_to_bytes(&a.size).unwrap_or(u64::MAX);
            let bsize = parse_size_to_bytes(&b.size).unwrap_or(u64::MAX);
            asize.cmp(&bsize)
        })
    });

    for cell in chisel_cells {
        let counters = cell.aux.and_then(|a| a.counters);
        let (h, m, f, p) = match counters {
            Some(c) => (
                c.cache_hits.to_string(),
                c.cache_misses.to_string(),
                c.fsync_calls.to_string(),
                c.pages_allocated.to_string(),
            ),
            None => (
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
            ),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            cell.row, cell.size, h, m, f, p
        );
    }
    let _ = writeln!(out);
}

fn render_footer(out: &mut String, m: &Metadata) {
    let _ = writeln!(out, "---\n");
    let _ = writeln!(
        out,
        "*Generated by `chisel-bench-summarize` {} against Chisel commit `{}`.*",
        m.post_processor_version, m.chisel_commit
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ChiselCountersDelta;
    use crate::summary::discover::{AuxMetrics, TimingStats};
    use crate::summary::metadata::MachineInfo;

    fn fixture_metadata() -> Metadata {
        Metadata {
            timestamp: "2026-05-03T13:22:15Z".to_string(),
            chisel_commit: "abc123".to_string(),
            machine: MachineInfo {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                hostname: "test".to_string(),
            },
            post_processor_version: "0.1.0",
            criterion_dir: "x".to_string(),
            aux_metrics_path: "y".to_string(),
            cell_count: 2,
        }
    }

    fn fixture_cells() -> Vec<Cell> {
        vec![
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "chisel-strict".to_string(),
                size: "32B".to_string(),
                timing: Some(TimingStats {
                    p50_ns: 3000.0,
                    p95_ns: 4800.0,
                    p99_ns: 4960.0,
                }),
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 8192,
                    counters: Some(ChiselCountersDelta {
                        cache_hits: 12,
                        cache_misses: 3,
                        fsync_calls: 2,
                        pages_allocated: 4,
                    }),
                }),
            },
            Cell {
                row: "allocate-1pertx".to_string(),
                mode: "redb-strict".to_string(),
                size: "32B".to_string(),
                timing: None,
                aux: Some(AuxMetrics {
                    file_size_delta_bytes: 4096,
                    counters: None,
                }),
            },
        ]
    }

    #[test]
    fn render_markdown_includes_required_sections() {
        let md = render_markdown(&fixture_cells(), &fixture_metadata());
        assert!(md.contains("# Chisel Benchmark Summary"), "missing H1");
        assert!(md.contains("## Durability mode legend"), "missing legend");
        assert!(
            md.contains("## Method"),
            "missing method/disclaimer section"
        );
        assert!(md.contains("## Micro grid"), "missing micro grid header");
        assert!(
            md.contains("### `allocate-1pertx`"),
            "missing allocate-1pertx subsection"
        );
        assert!(
            md.contains("## File-size delta"),
            "missing file-size delta header"
        );
        assert!(
            md.contains("## Chisel internals appendix"),
            "missing appendix header"
        );
        assert!(md.contains("chisel-strict"), "missing chisel-strict mode");
        assert!(md.contains("redb-strict"), "missing redb-strict mode");
        // 3000 ns formats as "3.00 µs" per format_duration_ns (≥ 1000 ns
        // crosses into the µs unit).
        assert!(md.contains("3.00 µs"), "missing chisel p50 cell value");
        assert!(
            md.contains("+8.0 KB") || md.contains("+8192 B"),
            "missing chisel file-size delta"
        );
        assert!(md.contains("abc123"), "missing chisel commit reference");
    }

    #[test]
    fn render_markdown_skipped_cells_render_as_dash() {
        let md = render_markdown(&fixture_cells(), &fixture_metadata());
        // The redb-strict cell has timing: None → must show as —
        // (the em-dash is U+2014).
        assert!(md.contains("—"), "missing em-dash for skipped cell");
        // Specifically, the redb-strict micro-grid row should contain
        // an em-dash for the missing timing cell.
        let redb_line = md
            .lines()
            .find(|l| l.starts_with("| redb-strict |"))
            .expect("redb-strict line should exist in micro grid");
        assert!(
            redb_line.contains("—"),
            "redb-strict row should contain em-dash for missing timing"
        );
    }
}
