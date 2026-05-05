// Markdown rendering for the noise-gate report. Consumed by the
// noise_gate CLI binary; tested as a pure string-rendering function.

use crate::noise_gate::cov::Cov;

#[derive(Debug, Clone)]
pub struct CellResult {
    pub scenario: String,
    pub engine: String,
    pub mode: String,
    pub throughput: Cov,
    pub p99_latency_ns: Cov,
    /// True if BOTH throughput.cov ≤ throughput_threshold AND
    /// p99_latency_ns.cov ≤ p99_threshold.
    pub passes: bool,
}

#[derive(Debug, Clone)]
pub struct GateResult {
    pub provider: String,          // e.g., "hetzner"
    pub instance_type: String,     // e.g., "ccx23"
    pub run_count: usize,          // e.g., 5
    pub throughput_threshold: f64, // e.g., 0.02 = 2%
    pub p99_threshold: f64,        // e.g., 0.05 = 5%
    pub cells: Vec<CellResult>,
}

impl GateResult {
    pub fn all_pass(&self) -> bool {
        self.cells.iter().all(|c| c.passes)
    }
}

pub fn render_report(result: &GateResult) -> String {
    let pass_count = result.cells.iter().filter(|c| c.passes).count();
    let total = result.cells.len();
    let verdict = if result.all_pass() { "PASS" } else { "FAIL" };

    let mut out = String::new();
    out.push_str("# Chisel Bench Noise-Gate Report\n\n");
    out.push_str(&format!(
        "**Verdict:** {verdict} ({pass_count} / {total} cells under threshold)\n\n"
    ));
    out.push_str(&format!("- Provider: `{}`\n", result.provider));
    out.push_str(&format!("- Instance type: `{}`\n", result.instance_type));
    out.push_str(&format!("- Run count: {}\n", result.run_count));
    out.push_str(&format!(
        "- Throughput threshold: ≤ {:.1}% COV\n",
        result.throughput_threshold * 100.0
    ));
    out.push_str(&format!(
        "- p99 latency threshold: ≤ {:.1}% COV\n\n",
        result.p99_threshold * 100.0
    ));

    out.push_str("## Per-cell results\n\n");
    out.push_str("| Scenario | Engine | Mode | Throughput COV | p99 COV | Verdict |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for cell in &result.cells {
        // Plain ASCII PASS/FAIL — matches the verdict word at the top
        // of the report and avoids Unicode-display dependencies in
        // terminal/email/CI viewers.
        let verdict_marker = if cell.passes { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {:.1}% | {} |\n",
            cell.scenario,
            cell.engine,
            cell.mode,
            cell.throughput.cov * 100.0,
            cell.p99_latency_ns.cov * 100.0,
            verdict_marker,
        ));
    }
    out.push('\n');

    if !result.all_pass() {
        out.push_str("## Failing cells\n\n");
        for cell in result.cells.iter().filter(|c| !c.passes) {
            out.push_str(&format!(
                "- `{}` / `{}` / `{}`: throughput COV {:.1}% (threshold {:.1}%), p99 COV {:.1}% (threshold {:.1}%)\n",
                cell.scenario,
                cell.engine,
                cell.mode,
                cell.throughput.cov * 100.0,
                result.throughput_threshold * 100.0,
                cell.p99_latency_ns.cov * 100.0,
                result.p99_threshold * 100.0,
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell(
        scenario: &str,
        engine: &str,
        throughput_cov: f64,
        p99_cov: f64,
        passes: bool,
    ) -> CellResult {
        CellResult {
            scenario: scenario.to_string(),
            engine: engine.to_string(),
            mode: "strict".to_string(),
            throughput: Cov {
                mean: 1000.0,
                stddev: 1000.0 * throughput_cov,
                cov: throughput_cov,
            },
            p99_latency_ns: Cov {
                mean: 1_000_000.0,
                stddev: 1_000_000.0 * p99_cov,
                cov: p99_cov,
            },
            passes,
        }
    }

    fn sample_result_pass() -> GateResult {
        GateResult {
            provider: "hetzner".to_string(),
            instance_type: "ccx23".to_string(),
            run_count: 5,
            throughput_threshold: 0.02,
            p99_threshold: 0.05,
            cells: vec![
                sample_cell("ycsb-a", "chisel", 0.005, 0.02, true),
                sample_cell("ycsb-a", "redb", 0.008, 0.03, true),
                sample_cell("ycsb-a", "sqlite", 0.012, 0.04, true),
            ],
        }
    }

    #[test]
    fn report_includes_provider_and_instance_in_header() {
        let r = render_report(&sample_result_pass());
        assert!(r.contains("hetzner"), "missing provider: {r}");
        assert!(r.contains("ccx23"), "missing instance type: {r}");
    }

    #[test]
    fn report_includes_pass_summary_when_all_pass() {
        let r = render_report(&sample_result_pass());
        assert!(r.contains("PASS"), "missing PASS marker: {r}");
        assert!(r.contains("3 / 3 cells"), "missing pass count: {r}");
    }

    #[test]
    fn report_includes_fail_summary_when_any_fails() {
        let mut result = sample_result_pass();
        result.cells[1].throughput.cov = 0.05; // exceeds 2% threshold
        result.cells[1].passes = false;
        let r = render_report(&result);
        assert!(r.contains("FAIL"), "missing FAIL marker: {r}");
        assert!(r.contains("2 / 3 cells"), "missing pass count: {r}");
    }

    #[test]
    fn report_includes_per_cell_table_with_cov_percentages() {
        let r = render_report(&sample_result_pass());
        assert!(r.contains("ycsb-a"), "missing scenario: {r}");
        assert!(r.contains("chisel"), "missing engine: {r}");
        // Throughput cov = 0.005 = 0.5%; should render as percentage with one decimal place.
        assert!(
            r.contains("0.5%"),
            "missing throughput cov as percentage: {r}"
        );
    }

    #[test]
    fn report_failing_section_lists_failing_cells_with_thresholds() {
        // Force a cell over the throughput threshold to exercise the
        // ## Failing cells branch — covers the per-failure detail
        // line that the all-pass-path tests above never touch.
        let mut result = sample_result_pass();
        result.cells[0].throughput.cov = 0.05;
        result.cells[0].passes = false;
        let r = render_report(&result);
        assert!(
            r.contains("## Failing cells"),
            "missing failing cells section: {r}"
        );
        assert!(r.contains("threshold 2.0%"), "missing threshold value: {r}");
    }
}
