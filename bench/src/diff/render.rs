// Markdown renderer for a DiffReport. Produces the PR-comment body
// with status line, summary table, collapsible per-scenario detail,
// and footer. Always-emitted marker `<!-- chisel-bench-diff -->`
// on first line lets peter-evans/find-comment update existing
// comments rather than appending new ones.

use crate::diff::compare::{DeltaStatus, DiffReport, Metric, MetricDelta, ScenarioDiff};
use std::collections::BTreeSet;

/// Render the full markdown comment body. See spec §4 for structure.
pub fn render_markdown(report: &DiffReport) -> String {
    let mut out = String::new();
    out.push_str("<!-- chisel-bench-diff -->\n");
    out.push_str("## 🚦 Bench results: PR vs main\n\n");
    out.push_str(&render_status_line(report));
    out.push_str("\n\n");

    // Empty-both-inputs early exit (spec §4.1 first variant).
    if report.scenarios.is_empty() {
        out.push_str(&render_footer(report));
        return out;
    }

    out.push_str(&render_summary_table(report));
    out.push('\n');

    out.push_str("<details>\n");
    out.push_str("<summary>Per-scenario detail (4 metrics × cells)</summary>\n\n");
    for scenario in unique_scenarios(report) {
        out.push_str(&render_scenario_detail(report, &scenario));
    }
    out.push_str("</details>\n\n");

    out.push_str(&render_footer(report));
    out
}

fn render_status_line(report: &DiffReport) -> String {
    // Priority order per spec §4.1.
    if report.scenarios.is_empty() {
        return "❗ No scenarios to compare — both inputs have empty scenario data".to_string();
    }
    if has_unusable_cell(report) {
        return "❗ Diff incomplete — see details below".to_string();
    }
    if report.regression_count > 0 {
        let pair_count = report
            .scenarios
            .iter()
            .filter(|s| s.worst_regression.is_some())
            .count();
        return format!(
            "⚠️ {} regression(s) detected across {} scenario/mode pair(s)",
            report.regression_count, pair_count,
        );
    }
    "✅ No regressions detected".to_string()
}

// True when any cell cannot be compared: absent on one side, or present on
// both but uncomputable (`ZeroBaseline`). Named for the property rather than
// for "missing" because a zero-baseline cell is present and still unusable —
// and it MUST be included here. Without it the status line reads
// "✅ No regressions detected" over a table containing a degenerate cell,
// which is the failure the zero guard exists to prevent.
fn has_unusable_cell(report: &DiffReport) -> bool {
    report.scenarios.iter().any(|s| {
        s.metrics.iter().any(|m| {
            matches!(
                m.status,
                DeltaStatus::BaselineMissing | DeltaStatus::PrMissing | DeltaStatus::ZeroBaseline
            )
        })
    })
}

fn render_summary_table(report: &DiffReport) -> String {
    let mut out = String::new();
    out.push_str("| Scenario        | Mode          | Δ throughput | Worst Δ        |\n");
    out.push_str("| --------------- | ------------- | ------------ | -------------- |\n");
    let rows = sort_summary_rows(report);
    for s in rows {
        out.push_str(&render_summary_row(s));
        out.push('\n');
    }
    out
}

fn sort_summary_rows(report: &DiffReport) -> Vec<&ScenarioDiff> {
    let mut rows: Vec<&ScenarioDiff> = report.scenarios.iter().collect();
    let any_attention = report.regression_count > 0 || has_unusable_cell(report);
    if any_attention {
        // Worst-regression first; missing-cell rows sort to the top.
        rows.sort_by(|a, b| {
            let ka = sort_key_attention(a);
            let kb = sort_key_attention(b);
            kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        rows.sort_by(|a, b| (&a.scenario, &a.mode).cmp(&(&b.scenario, &b.mode)));
    }
    rows
}

// Higher = sorts earlier. Unusable-cell rows (missing on either side, or
// zero-baseline) get +infinity; regressed rows get their delta_pct;
// everything else gets f64::NEG_INFINITY. Zero-baseline rows belong in the
// +infinity group: they have no delta_pct, so leaving them out would sort
// them to the BOTTOM with the clean rows.
fn sort_key_attention(s: &ScenarioDiff) -> f64 {
    if s.metrics.iter().any(|m| {
        matches!(
            m.status,
            DeltaStatus::BaselineMissing | DeltaStatus::PrMissing | DeltaStatus::ZeroBaseline
        )
    }) {
        return f64::INFINITY;
    }
    s.worst_regression
        .as_ref()
        .and_then(|m| m.delta_pct)
        .unwrap_or(f64::NEG_INFINITY)
}

fn render_summary_row(s: &ScenarioDiff) -> String {
    // Δ throughput column: display sign convention (raw direction),
    // not bad-direction-positive. See spec §4.3. Throughput is metrics[0].
    let throughput = &s.metrics[0];
    let throughput_str = match (&throughput.status, throughput.delta_pct) {
        (DeltaStatus::PrMissing, _) => "—".to_string(),
        (DeltaStatus::BaselineMissing, _) => "—".to_string(),
        // Spelled out rather than left to the `(_, None)` fallthrough: a
        // zero-baseline cell has no percentage, and the reason it has none is
        // worth naming next to the other two uncomparable statuses.
        (DeltaStatus::ZeroBaseline, _) => "—".to_string(),
        (_, Some(bad_pct)) => {
            // Throughput display sign is opposite of bad-direction-positive,
            // but guard exact zero to avoid rendering "-0.0%".
            let display_pct = if bad_pct != 0.0 { -bad_pct } else { bad_pct };
            format!("{display_pct:+.1}%")
        }
        (_, None) => "—".to_string(),
    };

    let worst_str = match (&s.worst_regression, attention_marker(s)) {
        (_, Some(marker)) => marker,
        (Some(md), None) => format!("{} {} ⚠️", md.metric.label(), format_delta_display(md),),
        (None, None) => "—".to_string(),
    };

    format!(
        "| {:<15} | {:<13} | {:>12} | {:<14} |",
        s.scenario, s.mode, throughput_str, worst_str
    )
}

// Marker for the "Worst Δ" column when a row has something more important to
// say than its worst regression. Checked BEFORE `worst_regression` in
// `render_summary_row`, so the ordering here is a priority ordering: a wholly
// absent cell outranks a new scenario, which outranks a degenerate one.
fn attention_marker(s: &ScenarioDiff) -> Option<String> {
    let pr_missing = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::PrMissing));
    let baseline_missing = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::BaselineMissing));
    let zero_baseline = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::ZeroBaseline));
    if pr_missing {
        Some(format!(
            "❌ {} / {} — missing on PR side",
            s.scenario, s.mode
        ))
    } else if baseline_missing {
        Some(format!(
            "❓ {} / {} — new scenario, no baseline",
            s.scenario, s.mode
        ))
    } else if zero_baseline {
        Some(format!(
            // Deliberately not "zero baseline": the status also covers a
            // non-finite or negative value on EITHER side, so naming the
            // baseline would point a reader at the wrong column.
            "❗ {} / {} — degenerate value, no % comparison",
            s.scenario, s.mode
        ))
    } else {
        None
    }
}

// Display percentage with raw-direction sign (spec §4.3).
// For latency metrics the bad-direction is the same as raw direction,
// so display_pct == delta_pct. For throughput, flip the sign — but
// avoid producing -0.0 when the raw delta is exactly zero (would
// render as "-0.0%" and look like a tiny regression).
//
// The `unwrap_or(0.0)` below is why every caller must screen out statuses
// that carry no percentage: a `None` delta silently prints as "+0.0%".
// Both callers do — `render_summary_row` only reaches here for a
// `Regressed` worst-Δ, and `render_detail_cell` handles `ZeroBaseline` and
// the missing statuses in earlier arms.
fn format_delta_display(md: &MetricDelta) -> String {
    let raw = md.delta_pct.unwrap_or(0.0);
    let display_pct = match md.metric {
        Metric::Throughput if raw != 0.0 => -raw,
        _ => raw,
    };
    format!("{display_pct:+.1}%")
}

fn unique_scenarios(report: &DiffReport) -> Vec<String> {
    let mut s = BTreeSet::new();
    for sd in &report.scenarios {
        s.insert(sd.scenario.clone());
    }
    s.into_iter().collect()
}

fn render_scenario_detail(report: &DiffReport, scenario_name: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("### {scenario_name}\n"));
    out.push_str("| Mode          | Throughput        | p50               | p95               | p99               |\n");
    out.push_str("| ------------- | ----------------- | ----------------- | ----------------- | ----------------- |\n");
    for s in report
        .scenarios
        .iter()
        .filter(|s| s.scenario == scenario_name)
    {
        out.push_str(&render_detail_row(s));
        out.push('\n');
    }
    out.push('\n');
    out
}

fn render_detail_row(s: &ScenarioDiff) -> String {
    let cells: Vec<String> = s.metrics.iter().map(render_detail_cell).collect();
    format!(
        "| {:<13} | {} | {} | {} | {} |",
        s.mode, cells[0], cells[1], cells[2], cells[3]
    )
}

fn render_detail_cell(md: &MetricDelta) -> String {
    match (&md.status, md.baseline, md.pr) {
        (DeltaStatus::PrMissing, _, _) | (DeltaStatus::BaselineMissing, _, _) => "—".to_string(),
        // Both values exist; the percentage does not. Show the raw pair — the
        // reader needs to see WHICH side was degenerate — and print "n/a"
        // where the percentage would go. Letting this fall through to the arm
        // below would render `0 ops/s → 900 ops/s (+0.0%)`, because
        // `format_delta_display` turns a `None` delta into 0.0: a degenerate
        // cell dressed up as an unchanged one.
        (DeltaStatus::ZeroBaseline, Some(b), Some(p)) => format!(
            "{} → {} (n/a) ❗",
            format_value(md.metric, b),
            format_value(md.metric, p)
        ),
        (_, Some(b), Some(p)) => {
            let flag = if matches!(md.status, DeltaStatus::Regressed { .. }) {
                " ⚠️"
            } else {
                ""
            };
            format!(
                "{} → {} ({}){}",
                format_value(md.metric, b),
                format_value(md.metric, p),
                format_delta_display(md),
                flag
            )
        }
        _ => "—".to_string(),
    }
}

fn format_value(metric: Metric, v: f64) -> String {
    match metric {
        Metric::Throughput => format_throughput(v),
        _ => format_duration_ns(v),
    }
}

fn format_throughput(ops: f64) -> String {
    format!("{} ops/s", ops.round() as u64)
}

fn format_duration_ns(ns: f64) -> String {
    if ns >= 1_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1} µs", ns / 1_000.0)
    } else {
        format!("{:.0} ns", ns)
    }
}

fn render_footer(report: &DiffReport) -> String {
    format!(
        "<sub>\nGenerated by chisel-bench-diff at {}.\nCompares PR HEAD against main. Never blocks merge — signal, not gate.\nThresholds: throughput 5%, p50 5%, p95 10%, p99 10%.\n</sub>\n",
        report.generated_at.format("%Y-%m-%dT%H:%M:%SZ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::compare::compare;
    use crate::diff::parse::{ParsedResults, ScenarioMetrics};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn fixed_metrics() -> ScenarioMetrics {
        ScenarioMetrics {
            throughput_ops_per_sec: 1000.0,
            p50_ns: 100_000.0,
            p95_ns: 200_000.0,
            p99_ns: 500_000.0,
        }
    }

    fn one_scenario(key: &str, m: ScenarioMetrics) -> ParsedResults {
        let mut s = BTreeMap::new();
        s.insert(key.to_string(), m);
        ParsedResults { scenarios: s }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-04T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn no_regression_renders_green_check() {
        let m = fixed_metrics();
        let baseline = one_scenario("ycsb-a/chisel-strict", m);
        let pr = one_scenario("ycsb-a/chisel-strict", m);
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(
            out.starts_with("<!-- chisel-bench-diff -->\n"),
            "marker first line missing"
        );
        assert!(out.contains("## 🚦 Bench results: PR vs main"));
        assert!(out.contains("✅ No regressions detected"));
        assert!(out.contains("| ycsb-a"));
        assert!(out.contains("chisel-strict"));
        assert!(out.contains("<details>"));
        assert!(out.contains("Generated by chisel-bench-diff at 2026-05-04T12:00:00Z"));
        assert!(out.contains("Thresholds: throughput 5%, p50 5%, p95 10%, p99 10%"));

        // Bug guard: zero-delta throughput should not render as -0.0%.
        // (Caught in code review on initial Task 6 commit; sign-flip on
        // exact zero produces negative zero which formats as "-0.0%".)
        assert!(
            !out.contains("-0.0%"),
            "no-regression output contains -0.0% — sign-flip-on-zero bug:\n{out}"
        );
    }

    #[test]
    fn regression_renders_warning_with_worst_column_populated() {
        // Set up two scenarios; one with a 12% p99 regression, one clean.
        let mut bs = BTreeMap::new();
        bs.insert("ycsb-a/chisel-strict".to_string(), fixed_metrics());
        bs.insert("ycsb-b/chisel-strict".to_string(), fixed_metrics());
        let baseline = ParsedResults { scenarios: bs };

        let mut ps = BTreeMap::new();
        ps.insert(
            "ycsb-a/chisel-strict".to_string(),
            ScenarioMetrics {
                p99_ns: 560_000.0, // 12% over 500_000
                ..fixed_metrics()
            },
        );
        ps.insert("ycsb-b/chisel-strict".to_string(), fixed_metrics());
        let pr = ParsedResults { scenarios: ps };

        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        assert_eq!(report.regression_count, 1);
        let out = render_markdown(&report);

        assert!(
            out.contains("⚠️ 1 regression(s) detected across 1 scenario/mode pair(s)"),
            "warning header missing or wrong:\n{out}"
        );
        assert!(
            out.contains("p99 +12.0% ⚠️"),
            "worst-Δ column wrong:\n{out}"
        );
        // ycsb-a (the regressed row) should appear before ycsb-b in the
        // summary table when sort-by-worst-first is applied.
        let ya_pos = out.find("| ycsb-a").unwrap();
        let yb_pos = out.find("| ycsb-b").unwrap();
        assert!(
            ya_pos < yb_pos,
            "ycsb-a (regressed) should sort before ycsb-b (clean):\nya_pos={ya_pos} yb_pos={yb_pos}\n{out}"
        );
    }

    #[test]
    fn missing_cell_renders_with_red_x_and_diff_incomplete_header() {
        // Baseline has a scenario that PR doesn't (e.g. PR removed it).
        let baseline = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let pr = ParsedResults::default();
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(
            out.contains("❗ Diff incomplete — see details below"),
            "diff-incomplete header missing:\n{out}"
        );
        assert!(
            out.contains("❌ ycsb-a / chisel-strict — missing on PR side"),
            "missing-row marker missing:\n{out}"
        );
    }

    #[test]
    fn new_scenario_renders_with_question_mark_marker() {
        // PR adds a scenario that baseline doesn't have.
        let baseline = ParsedResults::default();
        let pr = one_scenario("ycsb-c/chisel-strict", fixed_metrics());
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(
            out.contains("❗ Diff incomplete — see details below"),
            "diff-incomplete header missing:\n{out}"
        );
        assert!(
            out.contains("❓ ycsb-c / chisel-strict — new scenario, no baseline"),
            "new-scenario marker missing:\n{out}"
        );
    }

    #[test]
    fn zero_baseline_does_not_render_as_no_regressions() {
        // `DeltaStatus::ZeroBaseline` has no exhaustive match anywhere, so the
        // variant could be added and every render site would still compile
        // while quietly treating it as an ordinary comparison. This pins the
        // two places that would be silently wrong: the status line (which
        // would read "✅ No regressions detected" over a degenerate table) and
        // the detail cell (which would print "0 ops/s → 1000 ops/s (+0.0%)").
        let baseline = one_scenario(
            "ycsb-a/chisel-strict",
            ScenarioMetrics {
                throughput_ops_per_sec: 0.0,
                ..fixed_metrics()
            },
        );
        let pr = one_scenario("ycsb-a/chisel-strict", fixed_metrics());
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(
            !out.contains("✅ No regressions detected"),
            "degenerate cell reported as clean:\n{out}"
        );
        assert!(
            out.contains("❗ Diff incomplete — see details below"),
            "diff-incomplete header missing:\n{out}"
        );
        assert!(
            out.contains("❗ ycsb-a / chisel-strict — degenerate value, no % comparison"),
            "degenerate-value marker missing:\n{out}"
        );
        assert!(
            out.contains("0 ops/s → 1000 ops/s (n/a)"),
            "detail cell should show both raw values and no percentage:\n{out}"
        );
        assert!(
            !out.contains("inf"),
            "no infinity should reach the rendered output:\n{out}"
        );
    }

    #[test]
    fn empty_both_inputs_renders_no_scenarios_message() {
        let baseline = ParsedResults::default();
        let pr = ParsedResults::default();
        let report = compare(
            &baseline,
            &pr,
            PathBuf::from("b"),
            PathBuf::from("p"),
            now(),
        );
        let out = render_markdown(&report);

        assert!(out.starts_with("<!-- chisel-bench-diff -->\n"));
        assert!(
            out.contains("❗ No scenarios to compare — both inputs have empty scenario data"),
            "empty-inputs header missing:\n{out}"
        );
        // Footer is still present.
        assert!(out.contains("Generated by chisel-bench-diff"));
        // No summary table or per-scenario detail in the output.
        assert!(
            !out.contains("| Scenario"),
            "summary table should be absent for empty input:\n{out}"
        );
        assert!(
            !out.contains("<details>"),
            "details block should be absent for empty input:\n{out}"
        );
    }
}
