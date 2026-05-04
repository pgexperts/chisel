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
    if has_missing_cell(report) {
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

fn has_missing_cell(report: &DiffReport) -> bool {
    report.scenarios.iter().any(|s| {
        s.metrics.iter().any(|m| {
            matches!(
                m.status,
                DeltaStatus::BaselineMissing | DeltaStatus::PrMissing
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
    let any_attention = report.regression_count > 0 || has_missing_cell(report);
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

// Higher = sorts earlier. Missing-cell rows get +infinity; regressed
// rows get their delta_pct; everything else gets f64::NEG_INFINITY.
fn sort_key_attention(s: &ScenarioDiff) -> f64 {
    if s.metrics.iter().any(|m| {
        matches!(
            m.status,
            DeltaStatus::BaselineMissing | DeltaStatus::PrMissing
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
        (_, Some(bad_pct)) => {
            // Throughput display sign is opposite of bad-direction-positive.
            let display_pct = -bad_pct;
            format!("{display_pct:+.1}%")
        }
        (_, None) => "—".to_string(),
    };

    let worst_str = match (&s.worst_regression, missing_marker(s)) {
        (_, Some(marker)) => marker,
        (Some(md), None) => format!("{} {} ⚠️", md.metric.label(), format_delta_display(md),),
        (None, None) => "—".to_string(),
    };

    format!(
        "| {:<15} | {:<13} | {:>12} | {:<14} |",
        s.scenario, s.mode, throughput_str, worst_str
    )
}

fn missing_marker(s: &ScenarioDiff) -> Option<String> {
    let pr_missing = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::PrMissing));
    let baseline_missing = s
        .metrics
        .iter()
        .any(|m| matches!(m.status, DeltaStatus::BaselineMissing));
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
    } else {
        None
    }
}

// Display percentage with raw-direction sign (spec §4.3).
// For latency metrics the bad-direction is the same as raw direction,
// so display_pct == delta_pct. For throughput, flip the sign.
fn format_delta_display(md: &MetricDelta) -> String {
    let display_pct = match md.metric {
        Metric::Throughput => -md.delta_pct.unwrap_or(0.0),
        _ => md.delta_pct.unwrap_or(0.0),
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
        (_, Some(b), Some(p)) => {
            let flag = if matches!(md.status, DeltaStatus::Regressed { .. }) {
                " ⚠️"
            } else {
                ""
            };
            let delta = format_delta_display(md);
            match md.metric {
                Metric::Throughput => format!(
                    "{} → {} ({}){}",
                    format_throughput(b),
                    format_throughput(p),
                    delta,
                    flag
                ),
                _ => format!(
                    "{} → {} ({}){}",
                    format_duration_ns(b),
                    format_duration_ns(p),
                    delta,
                    flag
                ),
            }
        }
        _ => "—".to_string(),
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
    }
}
