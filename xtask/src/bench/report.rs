//! Markdown and JSON report generation from benchmark results.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bench::proxy::ProxyMetrics;
use crate::bench::runner::Tool;
use crate::bench::{Provider, command_stdout};

/// Serde helper: returns `true` when a `u64` is zero (for `skip_serializing_if`).
fn is_zero(v: &u64) -> bool {
    *v == 0
}

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
    /// Versions of binaries the benchmarked tools relay their transfers
    /// through, keyed by binary name.
    ///
    /// dregsy transfers nothing itself, so skopeo's version moves dregsy's
    /// numbers. Recorded here rather than only in the per-run summary, which
    /// is not tracked in git and dies with the instance.
    #[serde(default)]
    pub(crate) relays: BTreeMap<String, String>,
    /// Corpus size context.
    pub(crate) corpus: CorpusInfo,
    /// Per-scenario results.
    pub(crate) scenarios: Vec<ScenarioRecord>,
}

/// Machine metadata for a benchmark run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MachineInfo {
    /// Cloud provider, absent when the target registry is none of the three
    /// clouds' own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<Provider>,
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
    /// Machine image the instance booted (e.g. `ami-0742fcde83639b335`), or
    /// `"unknown"` outside a cloud instance.
    ///
    /// The image carries the kernel, glibc and NIC driver, so two runs on
    /// different images are not directly comparable however identical the rest
    /// of the metadata looks.
    #[serde(default = "unknown_string")]
    pub(crate) image_id: String,
}

/// Default for record fields added after runs were already archived.
fn unknown_string() -> String {
    "unknown".to_string()
}

/// Reads an instance metadata field, warning when it cannot be resolved.
///
/// `read_imds` returns `None` for an absent curl, an unreachable metadata
/// service and a non-UTF-8 body alike, and the run log is never persisted, so
/// the warning is the operator's only signal that a comparability anchor is
/// missing from the record.
fn imds_or_unknown(path: &str) -> String {
    read_imds(path).unwrap_or_else(|| {
        eprintln!(
            "WARNING: instance metadata {path} unavailable, recording \"unknown\". \
             Runs cannot be compared on it."
        );
        unknown_string()
    })
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
    /// Shuffle seed for tool order randomization (for reproducibility).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) shuffle_seed: u64,
    /// Tool execution order for this scenario.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_order: Vec<String>,
    /// Per-tool results.
    pub(crate) tools: Vec<ToolRecord>,
}

/// One tool's metrics in the compact run record.
///
/// Proxy metrics are embedded via `#[serde(flatten)]` so that adding a
/// new metric to [`ProxyMetrics`] automatically appears in the JSON archive
/// -- no manual field mapping needed.
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
    /// All HTTP proxy metrics (requests, bytes, mounts, CDN, auth, 429s, etc.).
    #[serde(flatten)]
    pub(crate) metrics: ProxyMetrics,
    /// Phase timing from ocync's `OCYNC_TIMING_FILE` (milliseconds).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) phase_timing_ms: BTreeMap<String, u64>,
}

/// Results for one tool in one scenario.
#[derive(Debug)]
pub(crate) struct ToolRun {
    /// The tool that ran.
    pub(crate) tool: Tool,
    /// Wall-clock time in seconds.
    pub(crate) wall_clock_secs: f64,
    /// Peak resident set size in KB (from `/usr/bin/time -v`).
    pub(crate) peak_rss_kb: Option<u64>,
    /// HTTP proxy metrics, if a proxy was attached.
    pub(crate) proxy_metrics: Option<ProxyMetrics>,
    /// Phase timing from ocync's `OCYNC_TIMING_FILE` (milliseconds).
    pub(crate) phase_timing_ms: BTreeMap<String, u64>,
}

/// Which scenario a [`ScenarioResult`] measured. Code that selects a result
/// matches this, never the display label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScenarioKind {
    /// Full sync into an empty target.
    Cold,
    /// Immediate re-sync of a populated target.
    Warm,
    /// Re-sync after a share of the tags changed.
    Partial,
    /// Full sync of a corpus subset of this many images.
    Scale {
        /// Number of images in the subset.
        images: usize,
    },
    /// Resolution of every mapping with no transfer.
    Prepare,
}

impl ScenarioKind {
    /// Human-readable scenario name, the heading of its report section and
    /// its name in the JSON archive (e.g. `"Cold sync"`).
    pub(crate) fn label(self) -> String {
        match self {
            Self::Cold => "Cold sync".into(),
            Self::Warm => "Warm sync (no-op)".into(),
            Self::Partial => "Partial sync (~5% changed)".into(),
            Self::Scale { images } => format!("Scale ({images} images)"),
            Self::Prepare => "Prepare (dry-run)".into(),
        }
    }

    /// File-name stem for this scenario's measured runs (e.g. `"scale-10"`).
    pub(crate) fn slug(self) -> String {
        match self {
            Self::Cold => "cold".into(),
            Self::Warm => "warm".into(),
            Self::Partial => "partial".into(),
            Self::Scale { images } => format!("scale-{images}"),
            Self::Prepare => "prepare".into(),
        }
    }
}

/// One scenario across all tools.
#[derive(Debug)]
pub(crate) struct ScenarioResult {
    /// Which scenario this is.
    pub(crate) kind: ScenarioKind,
    /// Per-tool run results.
    pub(crate) runs: Vec<ToolRun>,
    /// Shuffle seed for tool order randomization (for the JSON archive).
    pub(crate) shuffle_seed: u64,
    /// Tool execution order.
    pub(crate) tool_order: Vec<Tool>,
}

impl ScenarioResult {
    /// A scenario whose tool order was not shuffled, so the record carries no
    /// seed and no order.
    pub(crate) fn unshuffled(kind: ScenarioKind, runs: Vec<ToolRun>) -> Self {
        Self {
            kind,
            runs,
            shuffle_seed: 0,
            tool_order: Vec::new(),
        }
    }
}

/// Instance metadata captured at benchmark runtime.
#[derive(Debug)]
pub(crate) struct InstanceInfo {
    /// EC2 instance type (e.g. `c7g.xlarge`), or `"unknown"` outside EC2.
    pub(crate) instance_type: String,
    /// CPU architecture (e.g. `aarch64`, `x86_64`).
    pub(crate) arch: String,
    /// CPU manufacturer from EC2 `DescribeInstanceTypes`
    /// (e.g. `AWS`, `Intel`).
    pub(crate) cpu_manufacturer: String,
    /// Number of default vCPUs for this instance type.
    pub(crate) vcpus: usize,
    /// Total memory in MiB (from `DescribeInstanceTypes`).
    pub(crate) memory_mib: u64,
    /// Network performance description from EC2 (e.g. `Up to 12.5 Gigabit`).
    pub(crate) network_performance: String,
    /// AWS region (e.g. `us-east-1`), or `"unknown"` outside EC2.
    pub(crate) region: String,
    /// AMI the instance booted, or `"unknown"` outside EC2.
    pub(crate) image_id: String,
}

impl InstanceInfo {
    /// Collect instance metadata from EC2 IMDS + `DescribeInstanceTypes`.
    ///
    /// IMDS provides the instance type and region. The AWS CLI
    /// `describe-instance-types` provides authoritative CPU, memory,
    /// and network specs. Falls back to `"unknown"` / 0 outside EC2.
    pub(crate) fn collect() -> Self {
        // Announced rather than silently recorded: the image and instance type
        // are what decide whether two runs are comparable, and a bare
        // "unknown" in the archive is indistinguishable from an older record
        // that predates the field.
        let instance_type = imds_or_unknown("instance-type");
        let region = imds_or_unknown("placement/region");
        let image_id = imds_or_unknown("ami-id");

        // Query DescribeInstanceTypes for authoritative hardware specs.
        let (arch, cpu_manufacturer, vcpus, memory_mib, network_performance) =
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
            cpu_manufacturer: if cpu_manufacturer.is_empty() {
                "unknown".into()
            } else {
                cpu_manufacturer
            },
            vcpus,
            memory_mib,
            network_performance: if network_performance.is_empty() {
                "unknown".into()
            } else {
                network_performance
            },
            region,
            image_id,
        }
    }
}

/// Query `aws ec2 describe-instance-types` for hardware specs.
///
/// Uses the AWS CLI (guaranteed on the bench instance) rather than
/// `aws-sdk-ec2` which compiles every EC2 API and adds ~220 crates.
///
/// Returns `(arch, cpu_manufacturer, vcpus, memory_mib, network_performance)`.
#[expect(
    clippy::disallowed_methods,
    reason = "reads the AWS CLI's instance type answer, which holds no credential"
)]
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
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;

    let arch = json["Arch"].as_str().unwrap_or_default().to_string();
    let cpu_manufacturer = json["Cpu"].as_str().unwrap_or_default().to_string();
    let vcpus = json["Vcpus"].as_u64().unwrap_or(0) as usize;
    let memory_mib = json["Mem"].as_u64().unwrap_or(0);
    let network_performance = json["Net"].as_str().unwrap_or_default().to_string();

    Some((
        arch,
        cpu_manufacturer,
        vcpus,
        memory_mib,
        network_performance,
    ))
}

/// Read a value from the EC2 Instance Metadata Service (`IMDSv2`).
fn read_imds(path: &str) -> Option<String> {
    let token = command_stdout(std::process::Command::new("curl").args([
        "-s",
        "-f",
        "--connect-timeout",
        "1",
        "-X",
        "PUT",
        "http://169.254.169.254/latest/api/token",
        "-H",
        "X-aws-ec2-metadata-token-ttl-seconds: 10",
    ]))?;

    let url = format!("http://169.254.169.254/latest/meta-data/{path}");
    command_stdout(std::process::Command::new("curl").args([
        "-s",
        "-f",
        "--connect-timeout",
        "1",
        "-H",
        &format!("X-aws-ec2-metadata-token: {token}"),
        &url,
    ]))
}

/// Full benchmark report.
#[derive(Debug)]
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
    pub(crate) tool_versions: BTreeMap<Tool, String>,
    /// Versions of binaries a tool relays its transfers through, keyed by
    /// binary name.
    ///
    /// Kept apart from [`Self::tool_versions`] because these are not
    /// benchmarked tools and must not become columns in the metric tables.
    pub(crate) relay_versions: BTreeMap<String, String>,
    /// Per-scenario results.
    pub(crate) scenarios: Vec<ScenarioResult>,
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

/// Render the OCI 1.1 referrers footnote for a scenario, or `None` if no
/// tool issued referrer calls.
///
/// ocync issues `GET /v2/<repo>/referrers/...` to discover artifact
/// attestations (SBOM, SLSA) and transfers any referenced artifacts.
/// Comparable tools do not implement this OCI 1.1 endpoint. The footnote
/// surfaces the per-tool count so the Source blob GETs / Source blob
/// bytes columns are not read as a fairness violation.
///
/// Returned string starts with a leading newline so it can be appended
/// directly after a metric table without manual spacing.
fn referrer_footnote(scenario: &ScenarioResult) -> Option<String> {
    let calls: Vec<(Tool, u64)> = scenario
        .runs
        .iter()
        .filter_map(|r| r.proxy_metrics.as_ref().map(|m| (r.tool, m.referrer_calls)))
        .collect();
    if !calls.iter().any(|(_, n)| *n > 0) {
        return None;
    }
    let mut out = String::new();
    out.push('\n');
    out.push_str("> **Artifact sync (OCI 1.1 referrers).** ");
    out.push_str("ocync issues `GET /v2/<repo>/referrers/...` to discover ");
    out.push_str("attestations (SBOM, SLSA) and transfers any referenced ");
    out.push_str("artifacts. Comparable tools do not implement this. The ");
    out.push_str("Source blob GETs and Source blob bytes columns above include ");
    out.push_str("this artifact content for ocync.\n>\n> ");
    let parts: Vec<String> = calls
        .iter()
        .map(|(tool, n)| format!("{tool}: {n}"))
        .collect();
    out.push_str(&format!("Referrers calls -- {}.\n", parts.join(", ")));
    Some(out)
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

    let inst = &report.instance;
    out.push_str("## Instance\n\n");
    out.push_str(&format!("- **Type**: {}\n", inst.instance_type));
    out.push_str(&format!(
        "- **CPU**: {} ({}, {} vCPUs)\n",
        inst.cpu_manufacturer, inst.arch, inst.vcpus
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

    out.push_str("## Tool Versions\n\n");
    for (tool, version) in &report.tool_versions {
        out.push_str(&format!("- **{tool}**: {version}\n"));
    }
    // The marker leads, since a version can now carry a parenthesised reason
    // and a trailing "(relay)" would read as part of it.
    for (binary, version) in &report.relay_versions {
        out.push_str(&format!("- **{binary}** (relay): {version}\n"));
    }
    out.push('\n');

    // Column order: ocync first, then the remaining tools in sorted order. Each
    // scenario keeps only the tools that ran in it.
    let columns = tool_columns(report);

    for scenario in &report.scenarios {
        out.push_str(&format!("## {}\n\n", scenario.kind.label()));

        let by_tool = runs_by_tool(scenario);
        let ran: Vec<Tool> = columns
            .iter()
            .copied()
            .filter(|tool| by_tool.contains_key(tool))
            .collect();
        render_metric_table(&ran, &by_tool, &mut out);
        if let Some(footnote) = referrer_footnote(scenario) {
            out.push_str(&footnote);
        }
        out.push('\n');
    }

    out
}

/// A scenario's runs keyed by tool.
fn runs_by_tool(scenario: &ScenarioResult) -> BTreeMap<Tool, &ToolRun> {
    scenario.runs.iter().map(|r| (r.tool, r)).collect()
}

/// Determine tool column order: the benchmarked tools in [`Tool`] order, which
/// leads with ocync. Falls back to the tools that have runs if `tool_versions`
/// is empty.
fn tool_columns(report: &BenchReport) -> Vec<Tool> {
    if !report.tool_versions.is_empty() {
        return report.tool_versions.keys().copied().collect();
    }
    report
        .scenarios
        .iter()
        .flat_map(|scenario| scenario.runs.iter().map(|run| run.tool))
        .collect::<BTreeSet<Tool>>()
        .into_iter()
        .collect()
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
            rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.requests)),
            display: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.requests.to_string())),
            lower_is_better: true,
        },
        MetricRow {
            label: "Response bytes",
            rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.response_bytes)),
            display: Box::new(|r| {
                r.proxy_metrics
                    .as_ref()
                    .map(|m| format_bytes(m.response_bytes))
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
            label: "CDN hits/misses",
            rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.cdn_hits)),
            display: Box::new(|r| {
                r.proxy_metrics
                    .as_ref()
                    .map(|m| format!("{}/{}", m.cdn_hits, m.cdn_misses))
            }),
            lower_is_better: false,
        },
        MetricRow {
            label: "Rate-limit 429s",
            rank: Box::new(|r| r.proxy_metrics.as_ref().map(|m| m.rate_limit_429s)),
            display: Box::new(|r| {
                r.proxy_metrics
                    .as_ref()
                    .map(|m| m.rate_limit_429s.to_string())
            }),
            lower_is_better: true,
        },
    ]
}

/// Renders a metric comparison table into `out`.
///
/// Columns are tool names, rows are metrics. The best value per row
/// is bolded when more than one tool has data for that metric.
fn render_metric_table(columns: &[Tool], by_tool: &BTreeMap<Tool, &ToolRun>, out: &mut String) {
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
            .map(|col| by_tool.get(col).and_then(|r| (row.rank)(r)))
            .collect();
        let winner = if row.lower_is_better {
            values.iter().filter_map(|v| *v).min()
        } else {
            values.iter().filter_map(|v| *v).max()
        };

        out.push_str(&format!("| {} |", row.label));
        for (i, col) in columns.iter().enumerate() {
            let display = by_tool.get(col).and_then(|r| (row.display)(r));
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

    let proxy_note = if scenario.runs.iter().all(|r| r.proxy_metrics.is_none()) {
        "Direct connections (proxy disabled -- no traffic metrics)."
    } else {
        "All traffic routed through bench-proxy for byte-accurate measurement."
    };

    let memory_gib = inst.memory_mib as f64 / 1024.0;
    out.push_str(&format!(
        "Measured {date} on {inst_type} ({arch}, {vcpus} vCPUs, {mem:.0} GiB, {net}). \
         Full corpus: {images} images, {tags} tags. {description} to ECR {region}. \
         {proxy_note}\n\n",
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
    render_metric_table(&columns, &runs_by_tool(scenario), &mut out);
    if let Some(footnote) = referrer_footnote(scenario) {
        out.push_str(&footnote);
    }

    out
}

/// Finds the result of the scenario of this kind.
pub(crate) fn find_scenario(
    scenarios: &[ScenarioResult],
    kind: ScenarioKind,
) -> Option<&ScenarioResult> {
    scenarios.iter().find(|s| s.kind == kind)
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

    if let Some(scenario) = find_scenario(&report.scenarios, ScenarioKind::Cold) {
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

    if let Some(scenario) = find_scenario(&report.scenarios, ScenarioKind::Warm) {
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
#[expect(
    clippy::disallowed_methods,
    reason = "reads a record file the bench wrote"
)]
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
            cpu_manufacturer: "AWS".into(),
            vcpus: 4,
            memory_mib: 8192,
            network_performance: "Up to 12.5 Gigabit".into(),
            region: "us-east-1".into(),
            image_id: "unknown".into(),
        }
    }

    fn sample_report() -> BenchReport {
        BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([
                (Tool::Ocync, "0.1.0".to_string()),
                (Tool::Dregsy, "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![
                    ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 47.0,
                        peak_rss_kb: Some(65536),
                        proxy_metrics: None,
                        phase_timing_ms: BTreeMap::new(),
                    },
                    ToolRun {
                        tool: Tool::Dregsy,
                        wall_clock_secs: 521.0,
                        peak_rss_kb: Some(131072),
                        proxy_metrics: None,
                        phase_timing_ms: BTreeMap::new(),
                    },
                ],
            )],
        }
    }

    #[test]
    fn format_duration_shows_minutes_only_when_there_are_some() {
        for (secs, expected) in [(0, "0s"), (47, "47s"), (192, "3m 12s"), (120, "2m")] {
            assert_eq!(
                format_duration(Duration::from_secs(secs)),
                expected,
                "{secs}s"
            );
        }
    }

    #[test]
    fn format_bytes_scales_to_the_decimal_unit() {
        for (bytes, expected) in [
            (0, "0 B"),
            (42, "42 B"),
            (999, "999 B"),
            (1_000, "1.0 KB"),
            (1_500, "1.5 KB"),
            (999_999, "1000.0 KB"),
            (1_000_000, "1.0 MB"),
            (450_000_000, "450.0 MB"),
            (999_999_999, "1000.0 MB"),
            (1_000_000_000, "1.0 GB"),
            (2_100_000_000, "2.1 GB"),
        ] {
            assert_eq!(format_bytes(bytes), expected, "{bytes}");
        }
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
    fn relay_versions_are_reported_but_never_become_columns() {
        let mut report = sample_report();
        report
            .relay_versions
            .insert("skopeo".to_string(), "skopeo version 1.24.0".to_string());

        // A relay is not benchmarked, so it has no run data. Letting it reach
        // the column list renders an empty column in every metric table.
        assert!(
            tool_columns(&report) == [Tool::Ocync, Tool::Dregsy],
            "relay binary leaked into metric table columns: {:?}",
            tool_columns(&report)
        );

        // The marker leads, so a version carrying a parenthesised reason does
        // not read as though "(relay)" were part of that reason.
        let md = summary_markdown(&report);
        assert!(
            md.contains("**skopeo** (relay): skopeo version 1.24.0"),
            "relay version missing from the versions section"
        );
    }

    #[test]
    fn summary_markdown_empty_scenarios_omits_table() {
        let report = BenchReport {
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Ocync, "0.1.0".to_string())]),
            scenarios: vec![],
        };
        let md = summary_markdown(&report);
        assert!(!md.contains("## Cold"));
    }

    #[test]
    fn a_scenario_table_has_a_column_per_tool_that_ran_in_it() {
        let run = |tool| ToolRun {
            tool,
            wall_clock_secs: 47.0,
            peak_rss_kb: Some(65536),
            proxy_metrics: None,
            phase_timing_ms: BTreeMap::new(),
        };
        let mut report = sample_report();
        report.tool_versions = BTreeMap::from([
            (Tool::Ocync, "0.1.0".to_string()),
            (Tool::Dregsy, "0.5.0".to_string()),
            (Tool::Regsync, "0.9.0".to_string()),
        ]);
        report.scenarios = vec![
            ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![run(Tool::Regsync), run(Tool::Ocync), run(Tool::Dregsy)],
            ),
            ScenarioResult::unshuffled(ScenarioKind::Scale { images: 10 }, vec![run(Tool::Ocync)]),
        ];

        let md = summary_markdown(&report);

        let cold = md.split("## Cold sync\n").nth(1).unwrap();
        let scale = md.split("## Scale (10 images)\n").nth(1).unwrap();
        assert!(
            cold.contains("| Metric | ocync | dregsy | regsync |"),
            "md:\n{md}"
        );
        assert!(scale.contains("| Metric | ocync |\n"), "md:\n{md}");
        assert!(!scale.contains("dregsy") && !scale.contains("regsync"));
    }

    #[test]
    fn run_record_roundtrip() {
        let record = RunRecord {
            timestamp: "2026-04-18-021040".into(),
            git_ref: "d121864".into(),
            machine: MachineInfo {
                provider: Some(Provider::Aws),
                instance_type: "c6in.4xlarge".into(),
                arch: "x86_64".into(),
                vcpus: 16,
                memory_gib: 32.0,
                network: "Up to 50 Gigabit".into(),
                region: "us-east-1".into(),
                image_id: "unknown".into(),
            },
            relays: BTreeMap::new(),
            corpus: CorpusInfo {
                images: 42,
                tags: 55,
            },
            scenarios: vec![ScenarioRecord {
                name: "Cold sync".into(),
                shuffle_seed: 0,
                tool_order: Vec::new(),
                tools: vec![ToolRecord {
                    tool: "ocync".into(),
                    version: "0.1.0".into(),
                    wall_clock_secs: 273.0,
                    peak_rss_kb: Some(59468),
                    metrics: ProxyMetrics {
                        requests: 4131,
                        response_bytes: 16_900_000_000,
                        source_blob_gets: 726,
                        source_blob_bytes: 77200,
                        mount_successes: 362,
                        mount_attempts: 379,
                        ..Default::default()
                    },
                    phase_timing_ms: BTreeMap::new(),
                }],
            }],
        };
        let json = serde_json::to_string_pretty(&record).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.timestamp, "2026-04-18-021040");
        assert_eq!(parsed.git_ref, "d121864");
        assert_eq!(parsed.machine.provider, Some(Provider::Aws));
        assert_eq!(parsed.machine.memory_gib, 32.0);
        assert_eq!(parsed.corpus.images, 42);
        assert_eq!(parsed.scenarios[0].tools[0].metrics.requests, 4131);
    }

    #[test]
    fn append_record_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord {
            timestamp: "2026-04-18-100000".into(),
            git_ref: "abc1234".into(),
            machine: MachineInfo {
                provider: Some(Provider::Aws),
                instance_type: "c6in.4xlarge".into(),
                arch: "x86_64".into(),
                vcpus: 16,
                memory_gib: 32.0,
                network: "Up to 50 Gigabit".into(),
                region: "us-east-1".into(),
                image_id: "unknown".into(),
            },
            relays: BTreeMap::new(),
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
                provider: Some(Provider::Aws),
                instance_type: "c7g.xlarge".into(),
                arch: "aarch64".into(),
                vcpus: 4,
                memory_gib: 8.0,
                network: "Up to 12.5 Gigabit".into(),
                region: "us-east-1".into(),
                image_id: "unknown".into(),
            },
            relays: BTreeMap::new(),
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
                provider: Some(Provider::Aws),
                instance_type: "unknown".into(),
                arch: "unknown".into(),
                vcpus: 0,
                memory_gib: 0.0,
                network: "unknown".into(),
                region: "unknown".into(),
                image_id: "unknown".into(),
            },
            relays: BTreeMap::new(),
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Ocync, "0.1.0".to_string())]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![ToolRun {
                    tool: Tool::Ocync,
                    wall_clock_secs: 47.0,
                    peak_rss_kb: Some(65536),
                    proxy_metrics: None,
                    phase_timing_ms: BTreeMap::new(),
                }],
            )],
        };
        let md = summary_markdown(&report);
        // Single tool -- nothing should be bolded.
        assert!(md.contains("| Wall clock | 47s |"), "md:\n{md}");
        assert!(!md.contains("**47s**"));
    }

    #[test]
    fn scenario_markdown_context_line() {
        let report = sample_report();
        let scenario = find_scenario(&report.scenarios, ScenarioKind::Cold).unwrap();
        let md = scenario_markdown(&report, scenario, "Cold sync");
        assert!(md.contains("Measured 2026-04-15"));
        assert!(md.contains("c7g.xlarge"));
        assert!(md.contains("28 images, 40 tags"));
        assert!(md.contains("Cold sync to ECR"));
        // sample_report() has no proxy metrics, so the no-proxy note is shown.
        assert!(md.contains("Direct connections (proxy disabled"));
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Ocync, "0.1.0".to_string())]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Warm,
                vec![ToolRun {
                    tool: Tool::Ocync,
                    wall_clock_secs: 2.5,
                    peak_rss_kb: Some(51200),
                    proxy_metrics: None,
                    phase_timing_ms: BTreeMap::new(),
                }],
            )],
        };
        let scenario = find_scenario(&report.scenarios, ScenarioKind::Warm).unwrap();
        let md = scenario_markdown(&report, scenario, "Warm sync (no changes)");
        assert!(md.contains("Warm sync (no changes) to ECR"));
        assert!(md.contains("| Wall clock | 2s |"), "md:\n{md}");
    }

    #[test]
    fn find_scenario_selects_by_kind() {
        let mut report = sample_report();
        report.scenarios.insert(
            0,
            ScenarioResult::unshuffled(ScenarioKind::Scale { images: 10 }, vec![]),
        );

        let cold = find_scenario(&report.scenarios, ScenarioKind::Cold).unwrap();

        assert_eq!(cold.kind.label().to_string(), "Cold sync");
        assert!(find_scenario(&report.scenarios, ScenarioKind::Warm).is_none());
        assert!(find_scenario(&report.scenarios, ScenarioKind::Scale { images: 25 }).is_none());
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([
                (Tool::Ocync, "0.1.0".to_string()),
                (Tool::Dregsy, "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![
                    ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 273.0,
                        peak_rss_kb: Some(59468),
                        proxy_metrics: Some(ProxyMetrics {
                            requests: 4131,
                            response_bytes: 16_900_000_000,
                            mount_attempts: 379,
                            mount_successes: 362,
                            source_blob_gets: 726,
                            source_blob_bytes: 77_200,
                            ..Default::default()
                        }),
                        phase_timing_ms: BTreeMap::new(),
                    },
                    ToolRun {
                        tool: Tool::Dregsy,
                        wall_clock_secs: 2182.0,
                        peak_rss_kb: Some(304500),
                        proxy_metrics: Some(ProxyMetrics {
                            requests: 11190,
                            rate_limit_429s: 49,
                            response_bytes: 43_400_000_000,
                            duplicate_blob_gets: 1,
                            mount_attempts: 293,
                            mount_successes: 293,
                            source_blob_gets: 1324,
                            source_blob_bytes: 120_100,
                            ..Default::default()
                        }),
                        phase_timing_ms: BTreeMap::new(),
                    },
                ],
            )],
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([
                (Tool::Ocync, "0.1.0".to_string()),
                (Tool::Dregsy, "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![
                    ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 273.0,
                        peak_rss_kb: Some(59468),
                        proxy_metrics: Some(ProxyMetrics {
                            requests: 4131,
                            response_bytes: 16_900_000_000,
                            mount_attempts: 379,
                            mount_successes: 362,
                            source_blob_gets: 726,
                            source_blob_bytes: 77_200,
                            ..Default::default()
                        }),
                        phase_timing_ms: BTreeMap::new(),
                    },
                    ToolRun {
                        tool: Tool::Dregsy,
                        wall_clock_secs: 2182.0,
                        peak_rss_kb: Some(304500),
                        // dregsy has no proxy metrics in this test.
                        proxy_metrics: None,
                        phase_timing_ms: BTreeMap::new(),
                    },
                ],
            )],
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([
                (Tool::Ocync, "0.1.0".to_string()),
                (Tool::Dregsy, "0.5.0".to_string()),
            ]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![
                    ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 120.0,
                        peak_rss_kb: Some(65536),
                        proxy_metrics: Some(ProxyMetrics {
                            requests: 1000,
                            ..Default::default()
                        }),
                        phase_timing_ms: BTreeMap::new(),
                    },
                    ToolRun {
                        tool: Tool::Dregsy,
                        wall_clock_secs: 120.0,   // same wall clock
                        peak_rss_kb: Some(65536), // same RSS
                        proxy_metrics: Some(ProxyMetrics {
                            requests: 2000, // different requests
                            ..Default::default()
                        }),
                        phase_timing_ms: BTreeMap::new(),
                    },
                ],
            )],
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Ocync, "0.1.0".to_string())]),
            scenarios: vec![
                ScenarioResult::unshuffled(
                    ScenarioKind::Cold,
                    vec![ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 429.0,
                        peak_rss_kb: Some(51200),
                        proxy_metrics: None,
                        phase_timing_ms: BTreeMap::new(),
                    }],
                ),
                ScenarioResult::unshuffled(
                    ScenarioKind::Warm,
                    vec![ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 2.0,
                        peak_rss_kb: Some(40960),
                        proxy_metrics: None,
                        phase_timing_ms: BTreeMap::new(),
                    }],
                ),
            ],
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
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Ocync, "0.1.0".to_string())]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Warm,
                vec![ToolRun {
                    tool: Tool::Ocync,
                    wall_clock_secs: 3.0,
                    peak_rss_kb: Some(40960),
                    proxy_metrics: None,
                    phase_timing_ms: BTreeMap::new(),
                }],
            )],
        };
        update_performance_page(&path, &report).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        // Cold section should be untouched.
        assert!(content.contains("existing cold data"));
        // Warm section should be replaced.
        assert!(!content.contains("old warm"));
        assert!(content.contains("Warm sync (no changes) to ECR"));
    }

    #[test]
    fn summary_markdown_includes_referrer_footnote_when_present() {
        let report = BenchReport {
            timestamp: "2026-04-26-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([
                (Tool::Ocync, "0.1.0".into()),
                (Tool::Dregsy, "0.5.0".into()),
                (Tool::Regsync, "0.7.0".into()),
            ]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![
                    ToolRun {
                        tool: Tool::Ocync,
                        wall_clock_secs: 250.0,
                        peak_rss_kb: Some(60_000),
                        proxy_metrics: Some(ProxyMetrics {
                            referrer_calls: 79,
                            ..Default::default()
                        }),
                        phase_timing_ms: BTreeMap::new(),
                    },
                    ToolRun {
                        tool: Tool::Dregsy,
                        wall_clock_secs: 2000.0,
                        peak_rss_kb: Some(300_000),
                        proxy_metrics: Some(ProxyMetrics::default()),
                        phase_timing_ms: BTreeMap::new(),
                    },
                    ToolRun {
                        tool: Tool::Regsync,
                        wall_clock_secs: 2200.0,
                        peak_rss_kb: Some(280_000),
                        proxy_metrics: Some(ProxyMetrics::default()),
                        phase_timing_ms: BTreeMap::new(),
                    },
                ],
            )],
        };
        let md = summary_markdown(&report);
        assert!(
            md.contains("OCI 1.1 referrers"),
            "footnote missing from summary.md:\n{md}"
        );
        assert!(md.contains("ocync: 79"), "ocync count missing:\n{md}");
        assert!(md.contains("dregsy: 0"), "dregsy count missing:\n{md}");
        assert!(md.contains("regsync: 0"), "regsync count missing:\n{md}");
    }

    #[test]
    fn summary_markdown_omits_referrer_footnote_when_no_calls() {
        let report = BenchReport {
            timestamp: "2026-04-26-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Dregsy, "0.5.0".into())]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![ToolRun {
                    tool: Tool::Dregsy,
                    wall_clock_secs: 2000.0,
                    peak_rss_kb: Some(300_000),
                    proxy_metrics: Some(ProxyMetrics::default()),
                    phase_timing_ms: BTreeMap::new(),
                }],
            )],
        };
        let md = summary_markdown(&report);
        assert!(
            !md.contains("OCI 1.1 referrers"),
            "footnote should be absent when no tool has referrer_calls > 0:\n{md}"
        );
    }

    #[test]
    fn scenario_markdown_includes_referrer_footnote_when_present() {
        // Parity with summary.md: the docs performance.md snippet (via
        // update_performance_page -> scenario_markdown) must also surface
        // referrers when ocync is in the run.
        let report = BenchReport {
            timestamp: "2026-04-26-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            tool_versions: BTreeMap::from([(Tool::Ocync, "0.1.0".into())]),
            scenarios: vec![ScenarioResult::unshuffled(
                ScenarioKind::Cold,
                vec![ToolRun {
                    tool: Tool::Ocync,
                    wall_clock_secs: 250.0,
                    peak_rss_kb: Some(60_000),
                    proxy_metrics: Some(ProxyMetrics {
                        referrer_calls: 79,
                        ..Default::default()
                    }),
                    phase_timing_ms: BTreeMap::new(),
                }],
            )],
        };
        let scenario = &report.scenarios[0];
        let md = scenario_markdown(&report, scenario, "Cold sync");
        assert!(
            md.contains("OCI 1.1 referrers"),
            "footnote missing from scenario_markdown:\n{md}"
        );
        assert!(md.contains("ocync: 79"), "count missing:\n{md}");
    }
}
