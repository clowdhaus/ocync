//! Markdown and JSON report generation from benchmark results.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bench::proxy::ProxyMetrics;

/// Compact run record for the per-registry JSON archive.
///
/// One record per benchmark execution. Designed for source control:
/// small enough to accumulate, rich enough to cite in docs/performance.md.
///
/// When adding new fields, use `#[serde(default)]` so that old records
/// (which lack the field) still deserialize.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    /// UTC timestamp when the run started (`YYYY-MM-DD-HHMMSS`).
    pub(crate) timestamp: String,
    /// Short git commit hash at the time of the run.
    pub(crate) git_ref: String,
    /// Machine and cloud provider metadata.
    pub(crate) machine: MachineInfo,
    /// Corpus size context.
    pub(crate) corpus: CorpusInfo,
    /// Per-scenario results.
    pub(crate) scenarios: Vec<ScenarioRecord>,
}

/// Machine metadata for a benchmark run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MachineInfo {
    /// Cloud provider: `"aws"`, `"gcp"`, `"azure"`, or `"other"`.
    pub(crate) provider: String,
    /// Instance type (e.g. `c6in.4xlarge`), or `"unknown"`.
    pub(crate) instance_type: String,
    /// CPU architecture (e.g. `x86_64`, `aarch64`).
    pub(crate) arch: String,
    /// Number of vCPUs.
    pub(crate) vcpus: usize,
    /// Memory in GiB (converted from MiB at write time).
    pub(crate) memory_gib: f64,
    /// Network performance description.
    pub(crate) network: String,
    /// Cloud region (e.g. `us-east-1`).
    pub(crate) region: String,
}

/// Corpus size metadata.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CorpusInfo {
    /// Number of distinct images.
    pub(crate) images: usize,
    /// Total tags across all images.
    pub(crate) tags: usize,
}

/// One scenario's results in the compact run record.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ScenarioRecord {
    /// Scenario name (e.g. `"Cold sync"`).
    pub(crate) name: String,
    /// Per-tool results.
    pub(crate) tools: Vec<ToolRecord>,
}

/// One tool's metrics in the compact run record.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ToolRecord {
    /// Tool name (e.g. `"ocync"`).
    pub(crate) tool: String,
    /// Tool version string.
    pub(crate) version: String,
    /// Wall-clock time in seconds.
    pub(crate) wall_clock_secs: f64,
    /// Peak resident set size in KB.
    pub(crate) peak_rss_kb: Option<u64>,
    /// Total HTTP requests.
    pub(crate) requests: u64,
    /// Total HTTP response bytes.
    pub(crate) response_bytes: u64,
    /// Blob GETs to source (non-target) registries.
    pub(crate) source_blob_gets: u64,
    /// Bytes from source blob GETs.
    pub(crate) source_blob_bytes: u64,
    /// Successful cross-repo mounts.
    pub(crate) mount_successes: u64,
    /// Total mount attempts.
    pub(crate) mount_attempts: u64,
    /// Duplicate blob GET requests.
    pub(crate) duplicate_blob_gets: u64,
    /// HTTP 429 rate-limit responses.
    pub(crate) rate_limit_429s: u64,
}

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
}

/// One scenario across all tools.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ScenarioResult {
    /// Human-readable scenario name (e.g. "Cold sync").
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

        render_metric_table(&columns, &by_tool, &mut out);
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

/// Rank function: extracts a comparable u64 for sorting.
type RankFn = Box<dyn Fn(&ToolRun) -> Option<u64>>;

/// Display function: formats the metric value as a human-readable string.
type DisplayFn = Box<dyn Fn(&ToolRun) -> Option<String>>;

/// A single row in a benchmark comparison table.
struct MetricRow {
    label: &'static str,
    rank: RankFn,
    display: DisplayFn,
    lower_is_better: bool,
}

/// The standard set of metric rows for benchmark comparison tables.
fn metric_rows() -> Vec<MetricRow> {
    vec![
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
    ]
}

/// Renders a metric comparison table into `out`.
///
/// Columns are tool names, rows are metrics. The best value per row
/// is bolded when more than one tool has data for that metric.
fn render_metric_table(columns: &[String], by_tool: &BTreeMap<&str, &ToolRun>, out: &mut String) {
    out.push_str("| Metric |");
    for col in columns {
        out.push_str(&format!(" {col} |"));
    }
    out.push('\n');
    out.push_str("|---|");
    for _ in columns {
        out.push_str("---:|");
    }
    out.push('\n');

    for row in &metric_rows() {
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
                    let competing = values.iter().filter(|v| v.is_some()).count() > 1;
                    let is_winner = winner.is_some() && values[i] == winner && competing;
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
}

/// Writes the benchmark summary to `output_dir`.
///
/// Creates `summary.md` -- the human-readable comparison table with
/// per-scenario metrics and winners bolded. Also updates the docs
/// performance page (`docs/src/content/performance.md`) between
/// scenario-specific marker pairs:
/// - `<!-- BENCH:START -->` / `<!-- BENCH:END -->` for cold sync
/// - `<!-- BENCH-WARM:START -->` / `<!-- BENCH-WARM:END -->` for warm sync
///
/// The compact per-registry JSON archive is written separately by
/// `append_record()`.
pub(crate) fn write_report(output_dir: &Path, report: &BenchReport) -> Result<(), String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("create output dir {}: {e}", output_dir.display()))?;

    // summary.md
    let md = summary_markdown(report);
    let md_path = output_dir.join("summary.md");
    std::fs::write(&md_path, md).map_err(|e| format!("write {}: {e}", md_path.display()))?;

    // Update docs performance page (best-effort; missing file is not fatal).
    let perf_page = Path::new("docs/src/content/performance.md");
    if perf_page.exists() {
        if let Err(e) = update_performance_page(perf_page, report) {
            eprintln!("WARNING: failed to update {}: {e}", perf_page.display());
        }
    }

    Ok(())
}

/// Generates a benchmark results snippet for one scenario.
///
/// Produces a context header with `description` and the scenario's
/// comparison table (metrics as rows, tools as columns, winners bolded).
fn scenario_markdown(report: &BenchReport, scenario: &ScenarioResult, description: &str) -> String {
    let inst = &report.instance;
    let mut out = String::new();

    let memory_gib = inst.memory_mib as f64 / 1024.0;
    out.push_str(&format!(
        "Measured {date} on {inst_type} ({arch}, {vcpus} vCPUs, {mem:.0} GiB, {net}). \
         Full corpus: {images} images, {tags} tags. {description} to ECR {region}. \
         All traffic routed through bench-proxy for byte-accurate measurement.\n\n",
        date = report.timestamp.get(..10).unwrap_or(&report.timestamp),
        inst_type = inst.instance_type,
        arch = inst.arch,
        vcpus = inst.vcpus,
        mem = memory_gib,
        net = inst.network_performance,
        images = report.corpus_size,
        tags = report.total_tags,
        region = inst.region,
    ));

    let columns = tool_columns(report);
    let by_tool: BTreeMap<&str, &ToolRun> =
        scenario.runs.iter().map(|r| (r.tool.as_str(), r)).collect();

    render_metric_table(&columns, &by_tool, &mut out);

    out
}

/// Finds a scenario whose name starts with `prefix` (case-insensitive).
fn find_scenario<'a>(report: &'a BenchReport, prefix: &str) -> Option<&'a ScenarioResult> {
    let prefix_lower = prefix.to_lowercase();
    report
        .scenarios
        .iter()
        .find(|s| s.scenario.to_lowercase().starts_with(&prefix_lower))
}

const BENCH_COLD_START: &str = "<!-- BENCH:START -->";
const BENCH_COLD_END: &str = "<!-- BENCH:END -->";
const BENCH_WARM_START: &str = "<!-- BENCH-WARM:START -->";
const BENCH_WARM_END: &str = "<!-- BENCH-WARM:END -->";

/// Replaces content between `start_marker` and `end_marker` with `snippet`.
///
/// Returns `Ok(true)` if markers were found and replaced, `Ok(false)` if
/// the markers were not present (caller decides whether that is an error).
fn replace_between_markers(
    content: &str,
    start_marker: &str,
    end_marker: &str,
    snippet: &str,
) -> Result<Option<String>, String> {
    let start = match content.find(start_marker) {
        Some(pos) => pos,
        None => return Ok(None),
    };
    let end = match content.find(end_marker) {
        Some(pos) => pos,
        None => {
            return Err(format!(
                "{end_marker} not found but {start_marker} is present"
            ));
        }
    };
    if start >= end {
        return Err(format!("{start_marker} must appear before {end_marker}"));
    }

    let mut updated = String::with_capacity(content.len());
    updated.push_str(&content[..start + start_marker.len()]);
    updated.push('\n');
    updated.push_str(snippet);
    updated.push_str(&content[end..]);
    Ok(Some(updated))
}

/// Updates the docs performance page with benchmark results.
///
/// Replaces content between marker pairs for each scenario present:
/// - `<!-- BENCH:START -->` / `<!-- BENCH:END -->` for the cold scenario
/// - `<!-- BENCH-WARM:START -->` / `<!-- BENCH-WARM:END -->` for the warm scenario
fn update_performance_page(path: &Path, report: &BenchReport) -> Result<(), String> {
    let mut content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;

    let mut updated = false;

    // Cold scenario.
    if let Some(scenario) = find_scenario(report, "cold") {
        let snippet = scenario_markdown(report, scenario, "Cold sync");
        match replace_between_markers(&content, BENCH_COLD_START, BENCH_COLD_END, &snippet)? {
            Some(new_content) => {
                content = new_content;
                updated = true;
            }
            None => {
                eprintln!(
                    "WARNING: {BENCH_COLD_START} marker not found in {}; skipping cold update",
                    path.display()
                );
            }
        }
    }

    // Warm scenario.
    if let Some(scenario) = find_scenario(report, "warm") {
        let snippet = scenario_markdown(report, scenario, "Warm sync (no changes)");
        match replace_between_markers(&content, BENCH_WARM_START, BENCH_WARM_END, &snippet)? {
            Some(new_content) => {
                content = new_content;
                updated = true;
            }
            None => {
                eprintln!(
                    "WARNING: {BENCH_WARM_START} marker not found in {}; skipping warm update",
                    path.display()
                );
            }
        }
    }

    if updated {
        std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    }

    Ok(())
}

/// Appends a compact run record to the per-registry JSON archive.
///
/// The file at `results_dir/{registry_key}.json` is a JSON array of
/// `RunRecord` values. If the file does not exist, it is created with
/// `[record]`. If it exists, the record is appended. If the file
/// contains invalid JSON, an error is returned (never silently
/// overwrite corrupt data).
pub(crate) fn append_record(
    results_dir: &Path,
    registry_key: &str,
    record: RunRecord,
) -> Result<(), String> {
    std::fs::create_dir_all(results_dir)
        .map_err(|e| format!("create results dir {}: {e}", results_dir.display()))?;

    let path = results_dir.join(format!("{registry_key}.json"));

    let mut records: Vec<RunRecord> = if path.exists() {
        let contents =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&contents).map_err(|e| format!("parse {}: {e}", path.display()))?
    } else {
        Vec::new()
    };

    records.push(record);

    let json = serde_json::to_string_pretty(&records)
        .map_err(|e| format!("serialize run records: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;

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
                scenario: "Cold sync".to_string(),
                runs: vec![
                    ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 47.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(65536),
                        proxy_metrics: None,
                    },
                    ToolRun {
                        tool: "dregsy".to_string(),
                        wall_clock_secs: 521.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(131072),
                        proxy_metrics: None,
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
        assert!(md.contains("## Cold sync\n"));
        assert!(md.contains("| Metric | ocync | dregsy |"));
        assert!(md.contains("| Wall clock | **47s** | 8m 41s |"));

        // Both tools have proxy_metrics: None, so proxy rows show "--" for
        // both columns (no bolding since neither has data).
        assert!(md.contains("| Requests | -- | -- |"), "md:\n{md}");
        assert!(md.contains("| Source blob GETs | -- | -- |"), "md:\n{md}");
    }

    #[test]
    fn write_report_creates_summary() {
        let report = sample_report();
        let dir = tempfile::tempdir().unwrap();
        write_report(dir.path(), &report).unwrap();

        // summary.md matches the in-memory markdown exactly.
        let md_content = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
        assert_eq!(md_content, summary_markdown(&report));

        // Per-scenario JSON files are no longer generated.
        assert!(!dir.path().join("cold_sync__median_.json").exists());
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
                }],
            }],
            scale: vec![],
        };
        let md = summary_markdown(&report);
        // dregsy column shows "--"; ocync is NOT bolded because there's
        // no competing tool with data for this metric.
        assert!(md.contains("| Wall clock | 47s | -- |"), "md:\n{md}");
    }

    #[test]
    fn run_record_roundtrip() {
        let record = RunRecord {
            timestamp: "2026-04-18-021040".into(),
            git_ref: "d121864".into(),
            machine: MachineInfo {
                provider: "aws".into(),
                instance_type: "c6in.4xlarge".into(),
                arch: "x86_64".into(),
                vcpus: 16,
                memory_gib: 32.0,
                network: "Up to 50 Gigabit".into(),
                region: "us-east-1".into(),
            },
            corpus: CorpusInfo {
                images: 42,
                tags: 55,
            },
            scenarios: vec![ScenarioRecord {
                name: "Cold sync".into(),
                tools: vec![ToolRecord {
                    tool: "ocync".into(),
                    version: "0.1.0".into(),
                    wall_clock_secs: 273.0,
                    peak_rss_kb: Some(59468),
                    requests: 4131,
                    response_bytes: 16_900_000_000,
                    source_blob_gets: 726,
                    source_blob_bytes: 77200,
                    mount_successes: 362,
                    mount_attempts: 379,
                    duplicate_blob_gets: 0,
                    rate_limit_429s: 0,
                }],
            }],
        };
        let json = serde_json::to_string_pretty(&record).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.timestamp, "2026-04-18-021040");
        assert_eq!(parsed.git_ref, "d121864");
        assert_eq!(parsed.machine.provider, "aws");
        assert_eq!(parsed.machine.memory_gib, 32.0);
        assert_eq!(parsed.corpus.images, 42);
        assert_eq!(parsed.scenarios[0].tools[0].requests, 4131);
    }

    #[test]
    fn append_record_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord {
            timestamp: "2026-04-18-100000".into(),
            git_ref: "abc1234".into(),
            machine: MachineInfo {
                provider: "aws".into(),
                instance_type: "c6in.4xlarge".into(),
                arch: "x86_64".into(),
                vcpus: 16,
                memory_gib: 32.0,
                network: "Up to 50 Gigabit".into(),
                region: "us-east-1".into(),
            },
            corpus: CorpusInfo {
                images: 10,
                tags: 15,
            },
            scenarios: vec![],
        };
        append_record(dir.path(), "ecr", record).unwrap();

        let path = dir.path().join("ecr.json");
        assert!(path.exists());
        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, "2026-04-18-100000");
    }

    #[test]
    fn append_record_appends_to_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecr.json");
        std::fs::write(&path, "[]").unwrap();

        let record = RunRecord {
            timestamp: "2026-04-18-110000".into(),
            git_ref: "def5678".into(),
            machine: MachineInfo {
                provider: "aws".into(),
                instance_type: "c7g.xlarge".into(),
                arch: "aarch64".into(),
                vcpus: 4,
                memory_gib: 8.0,
                network: "Up to 12.5 Gigabit".into(),
                region: "us-east-1".into(),
            },
            corpus: CorpusInfo { images: 5, tags: 8 },
            scenarios: vec![],
        };
        append_record(dir.path(), "ecr", record).unwrap();

        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].git_ref, "def5678");
    }

    #[test]
    fn append_record_rejects_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecr.json");
        std::fs::write(&path, "not json").unwrap();

        let record = RunRecord {
            timestamp: "2026-04-18-120000".into(),
            git_ref: "aaa1111".into(),
            machine: MachineInfo {
                provider: "aws".into(),
                instance_type: "unknown".into(),
                arch: "unknown".into(),
                vcpus: 0,
                memory_gib: 0.0,
                network: "unknown".into(),
                region: "unknown".into(),
            },
            corpus: CorpusInfo { images: 0, tags: 0 },
            scenarios: vec![],
        };
        assert!(append_record(dir.path(), "ecr", record).is_err());
    }

    #[test]
    fn summary_markdown_single_tool_no_bold() {
        let report = BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([("ocync".to_string(), "0.1.0".to_string())]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync".to_string(),
                runs: vec![ToolRun {
                    tool: "ocync".to_string(),
                    wall_clock_secs: 47.0,
                    exit_code: Some(0),
                    peak_rss_kb: Some(65536),
                    proxy_metrics: None,
                }],
            }],
            scale: vec![],
        };
        let md = summary_markdown(&report);
        // Single tool -- nothing should be bolded.
        assert!(md.contains("| Wall clock | 47s |"), "md:\n{md}");
        assert!(!md.contains("**47s**"));
    }

    #[test]
    fn scenario_markdown_context_line() {
        let report = sample_report();
        let scenario = find_scenario(&report, "cold").unwrap();
        let md = scenario_markdown(&report, scenario, "Cold sync");
        assert!(md.contains("Measured 2026-04-15"));
        assert!(md.contains("c7g.xlarge"));
        assert!(md.contains("28 images, 40 tags"));
        assert!(md.contains("Cold sync to ECR"));
        assert!(md.contains("bench-proxy"));
        // Contains the metric table.
        assert!(md.contains("| Metric | ocync | dregsy |"));
        assert!(md.contains("| Wall clock |"));
    }

    #[test]
    fn scenario_markdown_warm_description() {
        let report = BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: BTreeMap::from([("ocync".to_string(), "0.1.0".to_string())]),
            scenarios: vec![ScenarioResult {
                scenario: "Warm sync (no-op)".to_string(),
                runs: vec![ToolRun {
                    tool: "ocync".to_string(),
                    wall_clock_secs: 2.5,
                    exit_code: Some(0),
                    peak_rss_kb: Some(51200),
                    proxy_metrics: None,
                }],
            }],
            scale: vec![],
        };
        let scenario = find_scenario(&report, "warm").unwrap();
        let md = scenario_markdown(&report, scenario, "Warm sync (no changes)");
        assert!(md.contains("Warm sync (no changes) to ECR"));
        assert!(md.contains("| Wall clock | 2s |"), "md:\n{md}");
    }

    #[test]
    fn find_scenario_case_insensitive() {
        let report = sample_report();
        assert!(find_scenario(&report, "cold").is_some());
        assert!(find_scenario(&report, "Cold").is_some());
        assert!(find_scenario(&report, "COLD").is_some());
        assert!(find_scenario(&report, "warm").is_none());
    }

    #[test]
    fn update_performance_page_replaces_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        std::fs::write(
            &path,
            "# Performance\n\n<!-- BENCH:START -->\nold content\n<!-- BENCH:END -->\n\nFooter\n",
        )
        .unwrap();

        let report = sample_report();
        update_performance_page(&path, &report).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<!-- BENCH:START -->"));
        assert!(content.contains("<!-- BENCH:END -->"));
        assert!(!content.contains("old content"));
        assert!(content.contains("Measured 2026-04-15"));
        assert!(content.contains("Footer"));
    }

    #[test]
    fn update_performance_page_missing_markers_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        let original = "# Performance\n\nNo markers here.\n";
        std::fs::write(&path, original).unwrap();

        let report = sample_report();
        // Missing markers now warn instead of failing (both are optional).
        update_performance_page(&path, &report).unwrap();
        // File should be unchanged.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// Report with populated proxy metrics for full table coverage.
    fn sample_report_with_metrics() -> BenchReport {
        BenchReport {
            timestamp: "2026-04-18-100000".to_string(),
            instance: test_instance(),
            corpus_size: 42,
            total_tags: 55,
            tool_versions: BTreeMap::from([
                ("ocync".to_string(), "0.1.0".to_string()),
                ("dregsy".to_string(), "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync".to_string(),
                runs: vec![
                    ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 273.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(59468),
                        proxy_metrics: Some(ProxyMetrics {
                            total_requests: 4131,
                            requests_by_method: BTreeMap::new(),
                            status_429_count: 0,
                            total_request_bytes: 0,
                            total_response_bytes: 16_900_000_000,
                            duplicate_blob_gets: 0,
                            mount_attempts: 379,
                            mount_successes: 362,
                            existence_check_posts: 0,
                            source_blob_gets: 726,
                            source_blob_bytes: 77_200,
                        }),
                    },
                    ToolRun {
                        tool: "dregsy".to_string(),
                        wall_clock_secs: 2182.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(304500),
                        proxy_metrics: Some(ProxyMetrics {
                            total_requests: 11190,
                            requests_by_method: BTreeMap::new(),
                            status_429_count: 49,
                            total_request_bytes: 0,
                            total_response_bytes: 43_400_000_000,
                            duplicate_blob_gets: 1,
                            mount_attempts: 293,
                            mount_successes: 293,
                            existence_check_posts: 0,
                            source_blob_gets: 1324,
                            source_blob_bytes: 120_100,
                        }),
                    },
                ],
            }],
            scale: vec![],
        }
    }

    #[test]
    fn summary_with_proxy_metrics_bold_winners() {
        let report = sample_report_with_metrics();
        let md = summary_markdown(&report);

        // ocync wins wall clock (4m 33s vs 36m 22s).
        assert!(
            md.contains("| Wall clock | **4m 33s** | 36m 22s |"),
            "md:\n{md}"
        );
        // ocync wins requests.
        assert!(md.contains("| Requests | **4131** | 11190 |"), "md:\n{md}");
        // ocync wins response bytes.
        assert!(
            md.contains("| Response bytes | **16.9 GB** | 43.4 GB |"),
            "md:\n{md}"
        );
        // ocync wins source blob GETs.
        assert!(
            md.contains("| Source blob GETs | **726** | 1324 |"),
            "md:\n{md}"
        );
        // Mounts: higher is better -- dregsy wins (293/293 vs 362/379).
        // dregsy has 293 successes, ocync has 362. 362 > 293, so ocync wins.
        assert!(
            md.contains("| Mounts (success/attempt) | **362/379** | 293/293 |"),
            "md:\n{md}"
        );
        // ocync wins duplicate blob GETs (0 vs 1).
        assert!(
            md.contains("| Duplicate blob GETs | **0** | 1 |"),
            "md:\n{md}"
        );
        // ocync wins rate-limit 429s (0 vs 49).
        assert!(md.contains("| Rate-limit 429s | **0** | 49 |"), "md:\n{md}");
    }

    #[test]
    fn winner_bold_requires_competing_data() {
        // When only one tool has proxy metrics, that tool's values should
        // NOT be bolded -- there's no competition.
        let report = BenchReport {
            timestamp: "2026-04-18-100000".to_string(),
            instance: test_instance(),
            corpus_size: 42,
            total_tags: 55,
            tool_versions: BTreeMap::from([
                ("ocync".to_string(), "0.1.0".to_string()),
                ("dregsy".to_string(), "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync".to_string(),
                runs: vec![
                    ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 273.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(59468),
                        proxy_metrics: Some(ProxyMetrics {
                            total_requests: 4131,
                            requests_by_method: BTreeMap::new(),
                            status_429_count: 0,
                            total_request_bytes: 0,
                            total_response_bytes: 16_900_000_000,
                            duplicate_blob_gets: 0,
                            mount_attempts: 379,
                            mount_successes: 362,
                            existence_check_posts: 0,
                            source_blob_gets: 726,
                            source_blob_bytes: 77_200,
                        }),
                    },
                    ToolRun {
                        tool: "dregsy".to_string(),
                        wall_clock_secs: 2182.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(304500),
                        // dregsy has no proxy metrics in this test.
                        proxy_metrics: None,
                    },
                ],
            }],
            scale: vec![],
        };
        let md = summary_markdown(&report);

        // Wall clock: both have data, so winner is bolded.
        assert!(
            md.contains("| Wall clock | **4m 33s** | 36m 22s |"),
            "md:\n{md}"
        );
        // Requests: only ocync has data, so it should NOT be bolded.
        assert!(md.contains("| Requests | 4131 | -- |"), "md:\n{md}");
        // Source blob GETs: only ocync, no bold.
        assert!(md.contains("| Source blob GETs | 726 | -- |"), "md:\n{md}");
    }

    #[test]
    fn winner_bold_tied_values_bold_both() {
        // When two tools tie on a metric, both should be bolded.
        let report = BenchReport {
            timestamp: "2026-04-18-100000".to_string(),
            instance: test_instance(),
            corpus_size: 42,
            total_tags: 55,
            tool_versions: BTreeMap::from([
                ("ocync".to_string(), "0.1.0".to_string()),
                ("dregsy".to_string(), "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult {
                scenario: "Cold sync".to_string(),
                runs: vec![
                    ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 120.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(65536),
                        proxy_metrics: Some(ProxyMetrics {
                            total_requests: 1000,
                            requests_by_method: BTreeMap::new(),
                            status_429_count: 0,
                            total_request_bytes: 0,
                            total_response_bytes: 0,
                            duplicate_blob_gets: 0,
                            mount_attempts: 0,
                            mount_successes: 0,
                            existence_check_posts: 0,
                            source_blob_gets: 0,
                            source_blob_bytes: 0,
                        }),
                    },
                    ToolRun {
                        tool: "dregsy".to_string(),
                        wall_clock_secs: 120.0, // same wall clock
                        exit_code: Some(0),
                        peak_rss_kb: Some(65536), // same RSS
                        proxy_metrics: Some(ProxyMetrics {
                            total_requests: 2000, // different requests
                            requests_by_method: BTreeMap::new(),
                            status_429_count: 0,
                            total_request_bytes: 0,
                            total_response_bytes: 0,
                            duplicate_blob_gets: 0,
                            mount_attempts: 0,
                            mount_successes: 0,
                            existence_check_posts: 0,
                            source_blob_gets: 0,
                            source_blob_bytes: 0,
                        }),
                    },
                ],
            }],
            scale: vec![],
        };
        let md = summary_markdown(&report);

        // Wall clock tied at 2m -- both should be bolded.
        assert!(md.contains("| Wall clock | **2m** | **2m** |"), "md:\n{md}");
        // Peak RSS tied -- both bolded.
        assert!(
            md.contains("| Peak RSS | **67.1 MB** | **67.1 MB** |"),
            "md:\n{md}"
        );
        // Requests differ -- only ocync (1000 < 2000) is bolded.
        assert!(md.contains("| Requests | **1000** | 2000 |"), "md:\n{md}");
    }

    #[test]
    fn update_performance_page_reversed_markers_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        std::fs::write(
            &path,
            "# Performance\n\n<!-- BENCH:END -->\nold\n<!-- BENCH:START -->\n",
        )
        .unwrap();

        let report = sample_report();
        let err = update_performance_page(&path, &report).unwrap_err();
        assert!(err.contains("must appear before"), "err: {err}");
    }

    #[test]
    fn update_performance_page_both_cold_and_warm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        std::fs::write(
            &path,
            "# Performance\n\n\
             <!-- BENCH:START -->\nold cold\n<!-- BENCH:END -->\n\n\
             ## Warm\n\n\
             <!-- BENCH-WARM:START -->\nold warm\n<!-- BENCH-WARM:END -->\n\n\
             Footer\n",
        )
        .unwrap();

        let report = BenchReport {
            timestamp: "2026-04-19-100000".to_string(),
            instance: test_instance(),
            corpus_size: 39,
            total_tags: 51,
            tool_versions: BTreeMap::from([("ocync".to_string(), "0.1.0".to_string())]),
            scenarios: vec![
                ScenarioResult {
                    scenario: "Cold sync".to_string(),
                    runs: vec![ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 429.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(51200),
                        proxy_metrics: None,
                    }],
                },
                ScenarioResult {
                    scenario: "Warm sync (no-op)".to_string(),
                    runs: vec![ToolRun {
                        tool: "ocync".to_string(),
                        wall_clock_secs: 2.0,
                        exit_code: Some(0),
                        peak_rss_kb: Some(40960),
                        proxy_metrics: None,
                    }],
                },
            ],
            scale: vec![],
        };
        update_performance_page(&path, &report).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old cold"));
        assert!(!content.contains("old warm"));
        assert!(content.contains("Cold sync to ECR"));
        assert!(content.contains("Warm sync (no changes) to ECR"));
        assert!(content.contains("Footer"));
    }

    #[test]
    fn update_performance_page_warm_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        std::fs::write(
            &path,
            "# Performance\n\n\
             <!-- BENCH:START -->\nexisting cold data\n<!-- BENCH:END -->\n\n\
             <!-- BENCH-WARM:START -->\nold warm\n<!-- BENCH-WARM:END -->\n",
        )
        .unwrap();

        // Report with only a warm scenario.
        let report = BenchReport {
            timestamp: "2026-04-19-100000".to_string(),
            instance: test_instance(),
            corpus_size: 39,
            total_tags: 51,
            tool_versions: BTreeMap::from([("ocync".to_string(), "0.1.0".to_string())]),
            scenarios: vec![ScenarioResult {
                scenario: "Warm sync (no-op)".to_string(),
                runs: vec![ToolRun {
                    tool: "ocync".to_string(),
                    wall_clock_secs: 3.0,
                    exit_code: Some(0),
                    peak_rss_kb: Some(40960),
                    proxy_metrics: None,
                }],
            }],
            scale: vec![],
        };
        update_performance_page(&path, &report).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        // Cold section should be untouched.
        assert!(content.contains("existing cold data"));
        // Warm section should be replaced.
        assert!(!content.contains("old warm"));
        assert!(content.contains("Warm sync (no changes) to ECR"));
    }
}
