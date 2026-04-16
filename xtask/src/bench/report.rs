//! Markdown and JSON report generation from benchmark results.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bench::proxy::ProxyMetrics;

/// Results for one tool in one scenario.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ToolRun {
    /// Name of the tool (e.g. "ocync", "dregsy").
    pub(crate) tool: String,
    /// Wall-clock time in seconds.
    pub(crate) wall_clock_secs: f64,
    /// Process exit code, if captured.
    pub(crate) exit_code: Option<i32>,
    /// HTTP proxy metrics, if a proxy was attached.
    pub(crate) proxy_metrics: Option<ProxyMetrics>,
    /// Parsed JSON from ocync's `--json` output, if available.
    pub(crate) ocync_json: Option<serde_json::Value>,
}

/// One scenario across all tools.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ScenarioResult {
    /// Human-readable scenario name (e.g. "Cold sync (median)").
    pub(crate) scenario: String,
    /// Per-tool run results.
    pub(crate) runs: Vec<ToolRun>,
}

/// A single point on the scaling curve.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ScalePoint {
    /// Number of images at this scale point.
    pub(crate) images: usize,
    /// Wall-clock seconds keyed by tool name.
    pub(crate) results: BTreeMap<String, f64>,
}

/// Full benchmark report.
#[derive(Debug)]
pub(crate) struct BenchReport {
    /// UTC timestamp when the run started (`YYYY-MM-DD-HHMMSS`).
    pub(crate) timestamp: String,
    /// Number of distinct images in the corpus.
    pub(crate) corpus_size: usize,
    /// Total number of tags across all images.
    pub(crate) total_tags: usize,
    /// Tool version strings keyed by tool name.
    pub(crate) tool_versions: BTreeMap<String, String>,
    /// Per-scenario results.
    pub(crate) scenarios: Vec<ScenarioResult>,
    /// Scaling curve data points (empty when the scale scenario was not run).
    pub(crate) scale: Vec<ScalePoint>,
}

/// Formats a [`Duration`] as a human-readable string.
///
/// Examples: `"47s"`, `"3m 12s"`, `"2m"` (exact minutes omit the seconds component).
fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    match (mins, secs) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

/// Formats a byte count using SI decimal prefixes (1 KB = 1,000).
///
/// Examples: `"2.1 GB"`, `"450.0 MB"`, `"1.5 KB"`, `"123 B"`.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    const KB: u64 = 1_000;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Generates a Markdown summary of the benchmark report.
///
/// Produces a `# Benchmark Results` heading, a corpus size line, a
/// `## Tool Versions` section, and a `## Summary` table with tools as
/// columns (ocync first) and scenarios as rows.
fn summary_markdown(report: &BenchReport) -> String {
    let mut out = String::new();

    out.push_str("# Benchmark Results\n\n");
    out.push_str(&format!("Run: {}\n\n", report.timestamp));
    out.push_str(&format!(
        "Corpus: {} images, {} total tags\n\n",
        report.corpus_size, report.total_tags
    ));

    // Tool versions section.
    out.push_str("## Tool Versions\n\n");
    for (tool, version) in &report.tool_versions {
        out.push_str(&format!("- **{tool}**: {version}\n"));
    }
    out.push('\n');

    // Summary table.
    if !report.scenarios.is_empty() {
        // Determine column order: ocync first, then remaining tools in sorted order.
        let mut tools: Vec<String> = report
            .tool_versions
            .keys()
            .filter(|&t| t != "ocync")
            .cloned()
            .collect();
        tools.sort();
        let mut columns: Vec<String> = Vec::new();
        if report.tool_versions.contains_key("ocync") {
            columns.push("ocync".to_string());
        }
        columns.extend(tools);

        // Fallback: collect tool names from scenario runs if tool_versions is empty.
        if columns.is_empty() {
            let mut seen: HashSet<String> = HashSet::new();
            for scenario in &report.scenarios {
                for run in &scenario.runs {
                    if seen.insert(run.tool.clone()) {
                        if run.tool == "ocync" {
                            columns.insert(0, run.tool.clone());
                        } else {
                            columns.push(run.tool.clone());
                        }
                    }
                }
            }
        }

        out.push_str("## Summary\n\n");

        // Header row.
        out.push_str("| Scenario |");
        for col in &columns {
            out.push_str(&format!(" {col} |"));
        }
        out.push('\n');

        // Separator row.
        out.push_str("|---|");
        for _ in &columns {
            out.push_str("---|");
        }
        out.push('\n');

        // Data rows.
        for scenario in &report.scenarios {
            let by_tool: BTreeMap<&str, f64> = scenario
                .runs
                .iter()
                .map(|r| (r.tool.as_str(), r.wall_clock_secs))
                .collect();

            out.push_str(&format!("| {} |", scenario.scenario));
            for col in &columns {
                match by_tool.get(col.as_str()) {
                    Some(&secs) => {
                        let d = Duration::from_secs_f64(secs);
                        out.push_str(&format!(" {} |", format_duration(d)));
                    }
                    None => out.push_str(" — |"),
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }

    out
}

/// Writes the benchmark report to `output_dir`.
///
/// Creates the following files:
/// - `summary.md` — the Markdown summary table
/// - `{scenario_name}.json` — per-scenario JSON (one file per scenario)
/// - `scale.json` — scaling curve data (only when `report.scale` is non-empty)
pub(crate) fn write_report(output_dir: &Path, report: &BenchReport) -> Result<(), String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("create output dir {}: {e}", output_dir.display()))?;

    // summary.md
    let md = summary_markdown(report);
    let md_path = output_dir.join("summary.md");
    std::fs::write(&md_path, md).map_err(|e| format!("write {}: {e}", md_path.display()))?;

    // Per-scenario JSON files.
    for scenario in &report.scenarios {
        let filename = scenario
            .scenario
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), "_")
            + ".json";
        let path = output_dir.join(&filename);
        let json = serde_json::to_string_pretty(scenario)
            .map_err(|e| format!("serialize scenario {}: {e}", scenario.scenario))?;
        std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;
    }

    // scale.json (only when data is present).
    if !report.scale.is_empty() {
        let scale_path = output_dir.join("scale.json");
        let json = serde_json::to_string_pretty(&report.scale)
            .map_err(|e| format!("serialize scale data: {e}"))?;
        std::fs::write(&scale_path, json)
            .map_err(|e| format!("write {}: {e}", scale_path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> BenchReport {
        BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([
                ("ocync".to_string(), "0.1.0".to_string()),
                ("dregsy".to_string(), "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync (median)".to_string(),
                runs: vec![
                    ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 47.0,
                        exit_code: Some(0),
                        proxy_metrics: None,
                        ocync_json: None,
                    },
                    ToolRun {
                        tool: "dregsy".to_string(),
                        wall_clock_secs: 521.0,
                        exit_code: Some(0),
                        proxy_metrics: None,
                        ocync_json: None,
                    },
                ],
            }],
            scale: vec![],
        }
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(47)), "47s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(192)), "3m 12s");
    }

    #[test]
    fn format_duration_exact_minutes() {
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn format_bytes_gb() {
        assert_eq!(format_bytes(2_100_000_000), "2.1 GB");
    }

    #[test]
    fn format_bytes_mb() {
        assert_eq!(format_bytes(450_000_000), "450.0 MB");
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn format_bytes_exact_boundaries() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(format_bytes(999_999), "1000.0 KB");
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
        assert_eq!(format_bytes(999_999_999), "1000.0 MB");
        assert_eq!(format_bytes(1_000_000_000), "1.0 GB");
    }

    #[test]
    fn format_bytes_kb() {
        assert_eq!(format_bytes(1_500), "1.5 KB");
    }

    #[test]
    fn format_bytes_small() {
        assert_eq!(format_bytes(42), "42 B");
    }

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn summary_markdown_structure() {
        let report = sample_report();
        let md = summary_markdown(&report);

        // Verify exact structure of key lines.
        let lines: Vec<&str> = md.lines().collect();
        assert_eq!(lines[0], "# Benchmark Results");
        assert_eq!(lines[2], "Run: 2026-04-15-100000");
        assert_eq!(lines[4], "Corpus: 28 images, 40 total tags");
        assert_eq!(lines[6], "## Tool Versions");

        // Tool versions (BTreeMap order: dregsy before ocync).
        assert_eq!(lines[8], "- **dregsy**: 0.5.0");
        assert_eq!(lines[9], "- **ocync**: 0.1.0");

        // Table header: ocync first, then dregsy (alphabetical among non-ocync).
        assert_eq!(lines[11], "## Summary");
        assert_eq!(lines[13], "| Scenario | ocync | dregsy |");
        assert_eq!(lines[14], "|---|---|---|");
        assert_eq!(lines[15], "| Cold sync (median) | 47s | 8m 41s |");
    }

    #[test]
    fn write_report_creates_files() {
        let report = sample_report();
        let dir = tempfile::tempdir().unwrap();
        write_report(dir.path(), &report).unwrap();

        // summary.md matches the in-memory markdown exactly.
        let md_content = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
        assert_eq!(md_content, summary_markdown(&report));

        // Per-scenario JSON file exists and is valid JSON.
        let json_path = dir.path().join("cold_sync__median_.json");
        let json_content = std::fs::read_to_string(&json_path).unwrap();
        let parsed: ScenarioResult = serde_json::from_str(&json_content).unwrap();
        assert_eq!(parsed.scenario, "Cold sync (median)");
        assert_eq!(parsed.runs.len(), 2);
    }

    #[test]
    fn write_report_writes_scale_json_when_present() {
        let mut report = sample_report();
        report.scale = vec![ScalePoint {
            images: 10,
            results: BTreeMap::from([("ocync".to_string(), 8.5)]),
        }];

        let dir = tempfile::tempdir().unwrap();
        write_report(dir.path(), &report).unwrap();

        let scale_content = std::fs::read_to_string(dir.path().join("scale.json")).unwrap();
        let parsed: Vec<ScalePoint> = serde_json::from_str(&scale_content).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].images, 10);
    }

    #[test]
    fn write_report_roundtrips_tool_run_with_proxy_metrics() {
        use crate::bench::proxy::ProxyMetrics;

        let mut report = sample_report();
        report.scenarios[0].runs[0].proxy_metrics = Some(ProxyMetrics {
            total_requests: 847,
            requests_by_method: BTreeMap::from([
                ("GET".to_string(), 400),
                ("HEAD".to_string(), 200),
                ("PUT".to_string(), 247),
            ]),
            status_429_count: 3,
            total_request_bytes: 50_000,
            total_response_bytes: 2_100_000_000,
            duplicate_blob_gets: 0,
        });

        let dir = tempfile::tempdir().unwrap();
        write_report(dir.path(), &report).unwrap();

        let json_path = dir.path().join("cold_sync__median_.json");
        let json_content = std::fs::read_to_string(&json_path).unwrap();
        let parsed: ScenarioResult = serde_json::from_str(&json_content).unwrap();

        let metrics = parsed.runs[0].proxy_metrics.as_ref().unwrap();
        assert_eq!(metrics.total_requests, 847);
        assert_eq!(metrics.status_429_count, 3);
        assert_eq!(metrics.total_response_bytes, 2_100_000_000);
        assert_eq!(metrics.duplicate_blob_gets, 0);
    }

    #[test]
    fn write_report_omits_scale_json_when_empty() {
        let report = sample_report();
        let dir = tempfile::tempdir().unwrap();
        write_report(dir.path(), &report).unwrap();

        assert!(!dir.path().join("scale.json").exists());
    }

    #[test]
    fn summary_markdown_empty_scenarios_omits_table() {
        let report = BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([("ocync".to_string(), "0.1.0".to_string())]),
            scenarios: vec![],
            scale: vec![],
        };
        let md = summary_markdown(&report);
        assert!(!md.contains("## Summary"));
        assert!(!md.contains("| Scenario"));
    }

    #[test]
    fn summary_markdown_missing_tool_shows_em_dash() {
        let report = BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([
                ("ocync".to_string(), "0.1.0".to_string()),
                ("dregsy".to_string(), "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync".to_string(),
                // Only ocync ran — dregsy is missing.
                runs: vec![ToolRun {
                    tool: "ocync".to_string(),
                    wall_clock_secs: 47.0,
                    exit_code: Some(0),
                    proxy_metrics: None,
                    ocync_json: None,
                }],
            }],
            scale: vec![],
        };
        let md = summary_markdown(&report);
        assert!(md.contains("| Cold sync | 47s | \u{2014} |"));
    }
}
