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
    /// Peak resident set size in KB (from `/usr/bin/time -v`).
    pub(crate) peak_rss_kb: Option<u64>,
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

/// Instance metadata captured at benchmark runtime.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InstanceInfo {
    /// EC2 instance type (e.g. `c7g.xlarge`), or `"unknown"` outside EC2.
    pub(crate) instance_type: String,
    /// CPU architecture (e.g. `aarch64`, `x86_64`).
    pub(crate) arch: String,
    /// CPU manufacturer and model from EC2 `DescribeInstanceTypes`
    /// (e.g. `AWS Graviton3`, `Intel Xeon Platinum 8488C`).
    pub(crate) cpu_model: String,
    /// Number of default vCPUs for this instance type.
    pub(crate) vcpus: usize,
    /// Total memory in MiB (from `DescribeInstanceTypes`).
    pub(crate) memory_mib: u64,
    /// Network performance description from EC2 (e.g. `Up to 12.5 Gigabit`).
    pub(crate) network_performance: String,
    /// AWS region (e.g. `us-east-1`), or `"unknown"` outside EC2.
    pub(crate) region: String,
}

impl InstanceInfo {
    /// Collect instance metadata from EC2 IMDS + `DescribeInstanceTypes`.
    ///
    /// IMDS provides the instance type and region. The AWS CLI
    /// `describe-instance-types` provides authoritative CPU, memory,
    /// and network specs. Falls back to `"unknown"` / 0 outside EC2.
    pub(crate) fn collect() -> Self {
        let instance_type = read_imds("instance-type").unwrap_or_else(|| "unknown".into());
        let region = read_imds("placement/region").unwrap_or_else(|| "unknown".into());

        // Query DescribeInstanceTypes for authoritative hardware specs.
        let (arch, cpu_model, vcpus, memory_mib, network_performance) =
            if instance_type != "unknown" {
                match describe_instance_type(&instance_type) {
                    Some(info) => info,
                    None => {
                        eprintln!(
                            "WARNING: ec2:DescribeInstanceTypes failed for {instance_type}. \
                             Instance metadata will be incomplete. Check IAM permissions \
                             (ec2:DescribeInstanceTypes) or AWS CLI availability."
                        );
                        Default::default()
                    }
                }
            } else {
                Default::default()
            };

        Self {
            instance_type,
            arch: if arch.is_empty() {
                "unknown".into()
            } else {
                arch
            },
            cpu_model: if cpu_model.is_empty() {
                "unknown".into()
            } else {
                cpu_model
            },
            vcpus,
            memory_mib,
            network_performance: if network_performance.is_empty() {
                "unknown".into()
            } else {
                network_performance
            },
            region,
        }
    }
}

/// Query `aws ec2 describe-instance-types` for hardware specs.
///
/// Uses the AWS CLI (guaranteed on the bench instance) rather than
/// `aws-sdk-ec2` which compiles every EC2 API and adds ~220 crates.
///
/// Returns `(arch, cpu_model, vcpus, memory_mib, network_performance)`.
fn describe_instance_type(instance_type: &str) -> Option<(String, String, usize, u64, String)> {
    let output = std::process::Command::new("aws")
        .args([
            "ec2",
            "describe-instance-types",
            "--instance-types",
            instance_type,
            "--query",
            "InstanceTypes[0].{Arch:ProcessorInfo.SupportedArchitectures[0],Cpu:ProcessorInfo.Manufacturer,Vcpus:VCpuInfo.DefaultVCpus,Mem:MemoryInfo.SizeInMiB,Net:NetworkInfo.NetworkPerformance}",
            "--output",
            "json",
            "--region",
            "us-east-1",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;

    let arch = json["Arch"].as_str().unwrap_or_default().to_string();
    let cpu_model = json["Cpu"].as_str().unwrap_or_default().to_string();
    let vcpus = json["Vcpus"].as_u64().unwrap_or(0) as usize;
    let memory_mib = json["Mem"].as_u64().unwrap_or(0);
    let network_performance = json["Net"].as_str().unwrap_or_default().to_string();

    Some((arch, cpu_model, vcpus, memory_mib, network_performance))
}

/// Read a value from the EC2 Instance Metadata Service (`IMDSv2`).
fn read_imds(path: &str) -> Option<String> {
    // IMDSv2: get a session token first, then use it.
    let token = std::process::Command::new("curl")
        .args([
            "-s",
            "-f",
            "--connect-timeout",
            "1",
            "-X",
            "PUT",
            "http://169.254.169.254/latest/api/token",
            "-H",
            "X-aws-ec2-metadata-token-ttl-seconds: 10",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())?;

    let url = format!("http://169.254.169.254/latest/meta-data/{path}");
    std::process::Command::new("curl")
        .args([
            "-s",
            "-f",
            "--connect-timeout",
            "1",
            "-H",
            &format!("X-aws-ec2-metadata-token: {}", token.trim()),
            &url,
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Full benchmark report.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BenchReport {
    /// UTC timestamp when the run started (`YYYY-MM-DD-HHMMSS`).
    pub(crate) timestamp: String,
    /// Instance metadata (hardware, region).
    pub(crate) instance: InstanceInfo,
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

    // Instance metadata.
    let inst = &report.instance;
    out.push_str("## Instance\n\n");
    out.push_str(&format!("- **Type**: {}\n", inst.instance_type));
    out.push_str(&format!(
        "- **CPU**: {} ({}, {} vCPUs)\n",
        inst.cpu_model, inst.arch, inst.vcpus
    ));
    if inst.memory_mib > 0 {
        let gb = inst.memory_mib as f64 / 1024.0;
        out.push_str(&format!("- **Memory**: {gb:.1} GiB\n"));
    }
    out.push_str(&format!("- **Network**: {}\n", inst.network_performance));
    out.push_str(&format!("- **Region**: {}\n", inst.region));
    out.push('\n');

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

    // Determine column order: ocync first, then remaining tools in sorted order.
    let columns = tool_columns(report);

    // Per-scenario detailed tables.
    for scenario in &report.scenarios {
        out.push_str(&format!("## {}\n\n", scenario.scenario));

        let by_tool: BTreeMap<&str, &ToolRun> =
            scenario.runs.iter().map(|r| (r.tool.as_str(), r)).collect();

        out.push_str("| Metric |");
        for col in &columns {
            out.push_str(&format!(" {col} |"));
        }
        out.push('\n');
        out.push_str("|---|");
        for _ in &columns {
            out.push_str("---:|");
        }
        out.push('\n');

        // Metric rows with winner highlighting.
        // Each row: (label, extract sortable value, format display, lower_is_better).
        type RankFn = Box<dyn Fn(&ToolRun) -> Option<u64>>;
        type DisplayFn = Box<dyn Fn(&ToolRun) -> Option<String>>;
        struct MetricRow {
            label: &'static str,
            /// Extract a comparable value for ranking (None = no data).
            rank: RankFn,
            /// Format the display string.
            display: DisplayFn,
            /// When true, lowest value wins; when false, highest wins.
            lower_is_better: bool,
        }

        let rows: Vec<MetricRow> = vec![
            MetricRow {
                label: "Wall clock",
                rank: Box::new(|r| Some((r.wall_clock_secs * 1000.0) as u64)),
                display: Box::new(|r| {
                    Some(format_duration(Duration::from_secs_f64(r.wall_clock_secs)))
                }),
                lower_is_better: true,
            },
            MetricRow {
                label: "Peak RSS",
                rank: Box::new(|r| r.peak_rss_kb),
                display: Box::new(|r| r.peak_rss_kb.map(|kb| format_bytes(kb * 1024))),
                lower_is_better: true,
            },
            MetricRow {
                label: "Requests",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.total_requests)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| m.total_requests.to_string())
                }),
                lower_is_better: true,
            },
            MetricRow {
                label: "Response bytes",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.total_response_bytes)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| format_bytes(m.total_response_bytes))
                }),
                lower_is_better: true,
            },
            MetricRow {
                label: "Source blob GETs",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.source_blob_gets)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| m.source_blob_gets.to_string())
                }),
                lower_is_better: true,
            },
            MetricRow {
                label: "Source blob bytes",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.source_blob_bytes)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| format_bytes(m.source_blob_bytes))
                }),
                lower_is_better: true,
            },
            MetricRow {
                label: "Mounts (success/attempt)",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.mount_successes)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| format!("{}/{}", m.mount_successes, m.mount_attempts))
                }),
                lower_is_better: false,
            },
            MetricRow {
                label: "Duplicate blob GETs",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.duplicate_blob_gets)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| m.duplicate_blob_gets.to_string())
                }),
                lower_is_better: true,
            },
            MetricRow {
                label: "Rate-limit 429s",
                rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.status_429_count)),
                display: Box::new(|r| {
                    r.proxy_metrics
                        .as_ref()
                        .map(|m| m.status_429_count.to_string())
                }),
                lower_is_better: true,
            },
        ];

        for row in &rows {
            // Find the winning value across all tools for this metric.
            let values: Vec<Option<u64>> = columns
                .iter()
                .map(|col| by_tool.get(col.as_str()).and_then(|r| (row.rank)(r)))
                .collect();
            let winner = if row.lower_is_better {
                values.iter().filter_map(|v| *v).min()
            } else {
                values.iter().filter_map(|v| *v).max()
            };

            out.push_str(&format!("| {} |", row.label));
            for (i, col) in columns.iter().enumerate() {
                let display = by_tool.get(col.as_str()).and_then(|r| (row.display)(r));
                match display {
                    Some(text) => {
                        let is_winner = winner.is_some() && values[i] == winner && values.len() > 1;
                        if is_winner {
                            out.push_str(&format!(" **{text}** |"));
                        } else {
                            out.push_str(&format!(" {text} |"));
                        }
                    }
                    None => out.push_str(" -- |"),
                }
            }
            out.push('\n');
        }

        out.push('\n');
    }

    out
}

/// Determine tool column order: ocync first, then remaining sorted.
fn tool_columns(report: &BenchReport) -> Vec<String> {
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
    columns
}

/// Writes the benchmark report to `output_dir`.
///
/// Creates the following files:
/// - `summary.md` -- Markdown summary with per-scenario metric tables
/// - `{scenario_name}.json` -- per-scenario JSON (one file per scenario)
/// - `scale.json` -- scaling curve data (only when `report.scale` is non-empty)
/// - `bench-results/runs/{timestamp}.json` -- full report archive for
///   historical comparison across benchmark runs
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

    // Historical archive: full report as a single JSON file keyed by timestamp.
    // Lives under `bench-results/runs/` so all runs accumulate in one directory
    // regardless of per-run output_dir.
    let runs_dir = Path::new("bench-results/runs");
    std::fs::create_dir_all(runs_dir)
        .map_err(|e| format!("create runs dir {}: {e}", runs_dir.display()))?;
    let archive_path = runs_dir.join(format!("{}.json", report.timestamp));
    let archive_json = serde_json::to_string_pretty(report)
        .map_err(|e| format!("serialize report archive: {e}"))?;
    std::fs::write(&archive_path, &archive_json)
        .map_err(|e| format!("write {}: {e}", archive_path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_instance() -> InstanceInfo {
        InstanceInfo {
            instance_type: "c7g.xlarge".into(),
            arch: "arm64".into(),
            cpu_model: "AWS".into(),
            vcpus: 4,
            memory_mib: 8192,
            network_performance: "Up to 12.5 Gigabit".into(),
            region: "us-east-1".into(),
        }
    }

    fn sample_report() -> BenchReport {
        BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
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
                        peak_rss_kb: Some(65536),
                        proxy_metrics: None,
                        ocync_json: None,
                    },
                    ToolRun {
                        tool: "dregsy".to_string(),
                        wall_clock_secs: 521.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(131072),
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

        // Verify key sections are present and ordered correctly.
        assert!(md.starts_with("# Benchmark Results\n"));
        assert!(md.contains("Run: 2026-04-15-100000"));
        assert!(md.contains("## Instance\n"));
        assert!(md.contains("- **Type**: c7g.xlarge"));
        assert!(md.contains("- **CPU**: AWS (arm64, 4 vCPUs)"));
        assert!(md.contains("- **Network**: Up to 12.5 Gigabit"));
        assert!(md.contains("Corpus: 28 images, 40 total tags"));
        assert!(md.contains("## Tool Versions\n"));
        assert!(md.contains("- **dregsy**: 0.5.0"));
        assert!(md.contains("- **ocync**: 0.1.0"));

        // Per-scenario detailed table (metrics as rows, tools as columns).
        // Winner is bolded: ocync (47s) beats dregsy (8m 41s).
        assert!(md.contains("## Cold sync (median)\n"));
        assert!(md.contains("| Metric | ocync | dregsy |"));
        assert!(md.contains("| Wall clock | **47s** | 8m 41s |"));
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
            mount_attempts: 0,
            mount_successes: 0,
            source_blob_gets: 200,
            source_blob_bytes: 4_000_000_000,
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
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([("ocync".to_string(), "0.1.0".to_string())]),
            scenarios: vec![],
            scale: vec![],
        };
        let md = summary_markdown(&report);
        assert!(!md.contains("## Cold"));
    }

    #[test]
    fn summary_markdown_missing_tool_shows_dash() {
        let report = BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([
                ("ocync".to_string(), "0.1.0".to_string()),
                ("dregsy".to_string(), "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync".to_string(),
                // Only ocync ran -- dregsy is missing.
                runs: vec![ToolRun {
                    tool: "ocync".to_string(),
                    wall_clock_secs: 47.0,
                    exit_code: Some(0),
                    peak_rss_kb: Some(65536),
                    proxy_metrics: None,
                    ocync_json: None,
                }],
            }],
            scale: vec![],
        };
        let md = summary_markdown(&report);
        // dregsy column shows "--", ocync is bolded as only tool with data.
        assert!(md.contains("| Wall clock | **47s** | -- |"), "md:\n{md}");
    }
}
