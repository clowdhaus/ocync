//! Markdown and JSON report generation from benchmark results.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use clap::ValueEnum;
use ocync_sync::engine::DEFAULT_MAX_CONCURRENT_TRANSFERS;
use serde::{Deserialize, Serialize};

use crate::bench::lock::{LockedAt, SyncFrom};
use crate::bench::proxy::ProxyMetrics;
use crate::bench::remote::CommitSha;
use crate::bench::runner::Tool;
use crate::bench::stats::{self, Pair, Ratio, Verdict};
use crate::bench::{Provider, Rounds, command_stdout};

/// The docs performance page, relative to the repository root.
pub(crate) const PERFORMANCE_PAGE: &str = "docs/src/content/performance.md";

/// The directory of the per-registry run archives, relative to the repository
/// root.
pub(crate) const ARCHIVE_DIR: &str = "bench/results";

/// The file in a run's output directory that carries what a deferred run did
/// not write into the tracked files.
const DEFERRED_FILE: &str = "deferred.json";

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
    /// When the corpus lock the run synced against was written. Runs under
    /// different locks synced different content and are different series;
    /// absent from records written before the lock existed and from runs that
    /// sync no locked corpus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) corpus_locked_at: Option<LockedAt>,
    /// What the tools synced each tag from. Runs that synced from source tags,
    /// as every record written before this field did, are another series than
    /// runs that synced from the locked digests; absent from those records and
    /// from runs that sync no locked corpus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sync_from: Option<SyncFrom>,
    /// Machine and cloud provider metadata.
    pub(crate) machine: MachineInfo,
    /// Versions of binaries the benchmarked tools relay their transfers
    /// through, keyed by binary name.
    ///
    /// dregsy transfers nothing itself, so skopeo's version moves dregsy's
    /// numbers. Recorded here rather than only in the per-run summary, which
    /// is not tracked in git and dies with the instance. Empty for a run that
    /// used no relay; `None` when the record does not carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) relays: Option<BTreeMap<String, String>>,
    /// How the CDN pre-warm went. A source that was not warmed changes what the
    /// numbers mean. `None` when no pre-warm was decided or the record does
    /// not carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prewarm: Option<Prewarm>,
    /// Corpus size context.
    pub(crate) corpus: CorpusInfo,
    /// The cap on concurrent image transfers the current ocync ran with, the
    /// default's value when none was given. Runs under different caps are
    /// different series; absent when no ocync ran and from records written
    /// before the cap was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_concurrent_transfers: Option<NonZeroUsize>,
    /// The cap the baseline build ran with; absent without a baseline build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) baseline_max_concurrent_transfers: Option<BaselineCap>,
    /// How many rounds were asked for and finished; absent for a run with no
    /// repeated scenario and for records written before rounds existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rounds: Option<RoundCount>,
    /// Per-scenario results, one per round of a repeated scenario.
    pub(crate) scenarios: Vec<ScenarioRecord>,
}

/// The cap the baseline build ran with. Its default is unknown: the build is an
/// older commit, whose own default may differ from the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BaselineCap {
    /// The cap the run gave it.
    Given(NonZeroUsize),
    /// None was given, so it ran at its own default.
    OwnDefault,
}

/// How many rounds of the repeated scenarios a run was asked for and finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RoundCount {
    /// Rounds asked for.
    pub(crate) requested: u32,
    /// Rounds the scenario that finished the fewest got to; fewer than
    /// requested when the run stopped.
    pub(crate) finished: u32,
}

impl RunRecord {
    /// The record of a finished live run, `report` as measured on the machine
    /// it names, synced against the lock written at `corpus_locked_at`.
    pub(crate) fn of(
        report: &BenchReport,
        git_ref: String,
        corpus_locked_at: Option<LockedAt>,
    ) -> Self {
        Self {
            timestamp: report.timestamp.clone(),
            git_ref,
            sync_from: SyncFrom::of(corpus_locked_at.as_ref()),
            corpus_locked_at,
            machine: MachineInfo {
                provider: report.instance.provider(),
                instance_type: report.instance.instance_type.clone(),
                arch: report.instance.arch.clone(),
                vcpus: report.instance.vcpus,
                memory_gib: report.instance.memory_mib as f64 / 1024.0,
                network: report.instance.network_performance.clone(),
                region: report.instance.region.clone(),
                image_id: Some(report.instance.image_id.clone()),
            },
            relays: Some(report.relay_versions.clone()),
            prewarm: report.prewarm.clone(),
            corpus: CorpusInfo {
                images: report.corpus_size,
                tags: report.total_tags,
            },
            max_concurrent_transfers: report.ocync_transfer_cap(),
            baseline_max_concurrent_transfers: report.baseline_transfer_cap(),
            rounds: report.round_count(),
            scenarios: report
                .scenarios
                .iter()
                .map(|s| ScenarioRecord {
                    name: s.kind.label(),
                    round: s.round,
                    shuffle_seed: s.shuffle_seed,
                    tool_order: s.tool_order.iter().map(|&t| report.label(t)).collect(),
                    tools: s
                        .runs
                        .iter()
                        .map(|r| ToolRecord {
                            tool: report.label(r.tool),
                            version: report
                                .tool_versions
                                .get(&r.tool)
                                .cloned()
                                .unwrap_or_default(),
                            wall_clock_secs: r.wall_clock_secs,
                            peak_rss_kb: r.peak_rss_kb,
                            metrics: r.proxy_metrics.clone().unwrap_or_default(),
                            phase_timing_ms: r.phase_timing_ms.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// What happened to the CDN pre-warm of a run that transfers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Prewarm {
    /// Skipped with `--skip-prewarm`.
    Skipped,
    /// Ran.
    Done {
        /// For each registry with a blob that failed or was not finished by
        /// the deadline, started or not, how many; empty when every blob was
        /// warmed.
        unwarmed: BTreeMap<String, u64>,
    },
}

impl Prewarm {
    /// The line the summary carries, when the pre-warm was skipped or left a
    /// registry unwarmed.
    pub(crate) fn note(&self) -> Option<String> {
        match self {
            Self::Skipped => Some("Pre-warm: skipped (--skip-prewarm)".to_owned()),
            Self::Done { unwarmed } if unwarmed.is_empty() => None,
            Self::Done { unwarmed } => {
                let counts: Vec<String> = unwarmed
                    .iter()
                    .map(|(registry, blobs)| format!("{registry} ({blobs} blobs)"))
                    .collect();
                Some(format!("Pre-warm: not warmed for {}", counts.join(", ")))
            }
        }
    }
}

/// Machine metadata for a benchmark run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MachineInfo {
    /// Cloud the benchmark ran in, whichever cloud the target registry is in;
    /// absent when the machine could not be identified.
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
    /// `"unknown"` when a live run could not read it. Absent when the
    /// record does not carry it.
    ///
    /// The image carries the kernel, glibc and NIC driver, so two runs on
    /// different images are not directly comparable however identical the rest
    /// of the metadata looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) image_id: Option<String>,
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
        "unknown".to_owned()
    })
}

/// Corpus size metadata.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Which round of a repeated scenario this is, from 1; absent for a
    /// scenario run once and for records written before rounds existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) round: Option<usize>,
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

/// One scenario across all tools, in one round when it is repeated.
#[derive(Debug)]
pub(crate) struct ScenarioResult {
    /// Which scenario this is.
    pub(crate) kind: ScenarioKind,
    /// Which round of a repeated scenario this is, from 1; `None` for a
    /// scenario run once.
    pub(crate) round: Option<usize>,
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
            round: None,
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
    /// The cloud the benchmark ran in: AWS when EC2's metadata service
    /// answered. Only EC2 is read, so no other cloud is named.
    pub(crate) fn provider(&self) -> Option<Provider> {
        (self.instance_type != "unknown").then_some(Provider::Aws)
    }

    /// Collect instance metadata from EC2 IMDS + `DescribeInstanceTypes`.
    ///
    /// IMDS provides the instance type and region. The AWS CLI
    /// `describe-instance-types` provides authoritative CPU, memory,
    /// and network specs. Falls back to `"unknown"` / 0 outside EC2.
    pub(crate) fn collect() -> Self {
        // Announced rather than silently recorded: the image and instance type
        // are what decide whether two runs are comparable.
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
    reason = "parses the AWS CLI's instance type answer, which holds no credential"
)]
fn describe_instance_type(instance_type: &str) -> Option<(String, String, usize, u64, String)> {
    let output = command_stdout(std::process::Command::new("aws").args([
            "ec2",
            "describe-instance-types",
            "--instance-types",
            instance_type,
            "--query",
            "InstanceTypes[0].{Arch:ProcessorInfo.SupportedArchitectures[0],Cpu:ProcessorInfo.Manufacturer,Vcpus:VCpuInfo.DefaultVCpus,Mem:MemoryInfo.SizeInMiB,Net:NetworkInfo.NetworkPerformance}",
            "--output",
            "json",
        ]))?;

    let json: serde_json::Value = serde_json::from_str(&output).ok()?;

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
    /// How the CDN pre-warm went; `None` when no scenario transfers.
    pub(crate) prewarm: Option<Prewarm>,
    /// The commit the baseline build was made from; `None` without one.
    pub(crate) baseline: Option<CommitSha>,
    /// The cap on concurrent image transfers the run gave ocync, the current
    /// build and the baseline alike; `None` when it kept their defaults.
    pub(crate) max_concurrent_transfers: Option<NonZeroUsize>,
    /// The rounds the run was asked for, of which a stopped run finished fewer.
    pub(crate) rounds: Rounds,
    /// The scenarios the run planned to repeat, so that one which finished no
    /// round still counts as zero rounds, not as absent.
    pub(crate) repeated: Vec<ScenarioKind>,
    /// Per-scenario results, one per round of a repeated scenario.
    pub(crate) scenarios: Vec<ScenarioResult>,
}

/// A tool's name where a person reads it. The baseline carries its commit,
/// since "ocync-baseline" names no code; that name is for file names.
pub(crate) fn tool_label(tool: Tool, baseline: Option<&CommitSha>) -> String {
    match (tool, baseline) {
        (Tool::OcyncBaseline, Some(commit)) => format!("ocync@{}", commit.short()),
        _ => tool.to_string(),
    }
}

/// The cap the current ocync runs with: the one `given`, else the engine's
/// default.
pub(crate) fn effective_transfer_cap(given: Option<NonZeroUsize>) -> NonZeroUsize {
    given.unwrap_or(DEFAULT_MAX_CONCURRENT_TRANSFERS)
}

impl BenchReport {
    /// The cap the current ocync ran with. `None` when no ocync ran.
    pub(crate) fn ocync_transfer_cap(&self) -> Option<NonZeroUsize> {
        self.tool_versions
            .contains_key(&Tool::Ocync)
            .then(|| effective_transfer_cap(self.max_concurrent_transfers))
    }

    /// The cap the baseline build ran with. `None` without a baseline build.
    fn baseline_transfer_cap(&self) -> Option<BaselineCap> {
        self.baseline.as_ref().map(|_| {
            self.max_concurrent_transfers
                .map_or(BaselineCap::OwnDefault, BaselineCap::Given)
        })
    }

    /// A tool's name in the reports.
    fn label(&self, tool: Tool) -> String {
        tool_label(tool, self.baseline.as_ref())
    }

    /// How many rounds a repeated scenario finished, in words: `5 rounds`, or
    /// `3 of 5 rounds (stopped)` when the run stopped short.
    fn rounds_note(&self, finished: usize) -> String {
        let requested = self.rounds.get();
        if finished == requested {
            format!("{requested} rounds")
        } else {
            format!("{finished} of {requested} rounds (stopped)")
        }
    }

    /// The rounds asked for and the fewest any planned repeated scenario
    /// finished; `None` when no scenario was repeated.
    fn round_count(&self) -> Option<RoundCount> {
        let finished = self
            .repeated
            .iter()
            .map(|&kind| rounds_of(&self.scenarios, kind).len())
            .min()?;
        let count = |n: usize| u32::try_from(n).expect("a run has fewer than 2^32 rounds");
        Some(RoundCount {
            requested: count(self.rounds.get()),
            finished: count(finished),
        })
    }
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
/// tool issued referrer calls in any round.
///
/// ocync issues `GET /v2/<repo>/referrers/...` to discover artifact
/// attestations (SBOM, SLSA) and transfers any referenced artifacts.
/// Comparable tools do not implement this OCI 1.1 endpoint. The footnote
/// surfaces the per-tool count, the median over the rounds, so the Source blob
/// GETs / Source blob bytes columns are not read as a fairness violation.
///
/// Returned string starts with a leading newline so it can be appended
/// directly after a metric table without manual spacing.
fn referrer_footnote(report: &BenchReport, rounds: &[&ScenarioResult]) -> Option<String> {
    let calls: Vec<(Tool, u64)> = rounds_by_tool(rounds)
        .into_iter()
        .filter_map(|(tool, runs)| {
            let median = spread_of(
                runs.iter()
                    .filter_map(|r| r.proxy_metrics.as_ref())
                    .map(|m| m.referrer_calls),
            )?;
            Some((tool, median.median.round() as u64))
        })
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
        .map(|&(tool, n)| format!("{}: {n}", report.label(tool)))
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
    if let Some(cap) = report.ocync_transfer_cap() {
        out.push_str(&format!("ocync ran with `max_concurrent_transfers: {cap}`"));
        if report.baseline_transfer_cap() == Some(BaselineCap::OwnDefault) {
            out.push_str("; the baseline build ran at its own default, which may differ");
        }
        out.push_str("\n\n");
    }
    if let Some(note) = report.prewarm.as_ref().and_then(Prewarm::note) {
        out.push_str(&format!("{note}\n\n"));
    }

    out.push_str("## Tool Versions\n\n");
    for (&tool, version) in &report.tool_versions {
        out.push_str(&format!("- **{}**: {version}\n", report.label(tool)));
    }
    // The marker leads, since a version can carry a parenthesised reason
    // and a trailing "(relay)" would read as part of it.
    for (binary, version) in &report.relay_versions {
        out.push_str(&format!("- **{binary}** (relay): {version}\n"));
    }
    out.push('\n');

    // Column order: ocync first, then the remaining tools in sorted order. Each
    // scenario keeps only the tools that ran in it.
    let columns = tool_columns(report);

    for kind in kinds(&report.scenarios) {
        let rounds = rounds_of(&report.scenarios, kind);
        out.push_str(&format!("## {}", kind.label()));
        if rounds[0].round.is_some() {
            out.push_str(&format!(" ({})", report.rounds_note(rounds.len())));
        }
        out.push_str("\n\n");

        let by_tool = rounds_by_tool(&rounds);
        let ran: Vec<Tool> = columns
            .iter()
            .copied()
            .filter(|tool| by_tool.contains_key(tool))
            .collect();
        render_metric_table(report, &ran, &by_tool, &mut out);
        render_comparison(report, &rounds, &mut out);
        if let Some(footnote) = referrer_footnote(report, &rounds) {
            out.push_str(&footnote);
        }
        out.push('\n');
    }

    out
}

/// The runs of a scenario's rounds, keyed by tool.
fn rounds_by_tool<'a>(rounds: &[&'a ScenarioResult]) -> BTreeMap<Tool, Vec<&'a ToolRun>> {
    let mut by_tool: BTreeMap<Tool, Vec<&ToolRun>> = BTreeMap::new();
    for run in rounds.iter().flat_map(|round| &round.runs) {
        by_tool.entry(run.tool).or_default().push(run);
    }
    by_tool
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

/// A single row in a benchmark comparison table.
struct MetricRow {
    label: &'static str,
    /// The value that decides the row's winner and that the spread is taken of.
    rank: fn(&ToolRun) -> Option<u64>,
    /// The second number of an `a/b` cell, when the row has one.
    of: Option<fn(&ToolRun) -> Option<u64>>,
    format: fn(u64) -> String,
    lower_is_better: bool,
    /// The words for ocync being lower and higher than another tool in every
    /// round, for a row that is compared round by round.
    verdict_words: Option<[&'static str; 2]>,
}

fn count(n: u64) -> String {
    n.to_string()
}

/// The standard set of metric rows for benchmark comparison tables.
fn metric_rows() -> [MetricRow; 10] {
    fn proxy(r: &ToolRun, value: fn(&ProxyMetrics) -> u64) -> Option<u64> {
        r.proxy_metrics.as_ref().map(value)
    }
    [
        MetricRow {
            label: "Wall clock",
            rank: |r| Some((r.wall_clock_secs * 1000.0) as u64),
            of: None,
            format: |ms| format_duration(Duration::from_millis(ms)),
            lower_is_better: true,
            verdict_words: Some(["faster", "slower"]),
        },
        MetricRow {
            label: "Peak RSS",
            rank: |r| r.peak_rss_kb,
            of: None,
            format: |kb| format_bytes(kb * 1024),
            lower_is_better: true,
            verdict_words: None,
        },
        MetricRow {
            label: "Requests",
            rank: |r| proxy(r, |m| m.requests),
            of: None,
            format: count,
            lower_is_better: true,
            verdict_words: Some(["fewer", "more"]),
        },
        MetricRow {
            label: "Response bytes",
            rank: |r| proxy(r, |m| m.response_bytes),
            of: None,
            format: format_bytes,
            lower_is_better: true,
            verdict_words: Some(["fewer", "more"]),
        },
        MetricRow {
            label: "Source blob GETs",
            rank: |r| proxy(r, |m| m.source_blob_gets),
            of: None,
            format: count,
            lower_is_better: true,
            verdict_words: None,
        },
        MetricRow {
            label: "Source blob bytes",
            rank: |r| proxy(r, |m| m.source_blob_bytes),
            of: None,
            format: format_bytes,
            lower_is_better: true,
            verdict_words: None,
        },
        MetricRow {
            label: "Mounts (success/attempt)",
            rank: |r| proxy(r, |m| m.mount_successes),
            of: Some(|r| proxy(r, |m| m.mount_attempts)),
            format: count,
            lower_is_better: false,
            verdict_words: None,
        },
        MetricRow {
            label: "Duplicate blob GETs",
            rank: |r| proxy(r, |m| m.duplicate_blob_gets),
            of: None,
            format: count,
            lower_is_better: true,
            verdict_words: None,
        },
        MetricRow {
            label: "CDN hits/misses",
            rank: |r| proxy(r, |m| m.cdn_hits),
            of: Some(|r| proxy(r, |m| m.cdn_misses)),
            format: count,
            lower_is_better: false,
            verdict_words: None,
        },
        MetricRow {
            label: "Rate-limit 429s",
            rank: |r| proxy(r, |m| m.rate_limit_429s),
            of: None,
            format: count,
            lower_is_better: true,
            verdict_words: None,
        },
    ]
}

/// The spread of integer values, as the statistics module takes them.
fn spread_of(values: impl Iterator<Item = u64>) -> Option<stats::Spread> {
    stats::spread(&values.map(|v| v as f64).collect::<Vec<_>>())
}

/// A tool's cell in a row: its spread over the runs and how it reads.
struct Cell {
    spread: stats::Spread,
    /// How many runs the spread is over.
    runs: usize,
    /// The median, followed by the lowest and highest in brackets when there
    /// is more than one run. For an `a/b` row the brackets hold both ranges,
    /// `(a_low..a_high / b_low..b_high)`.
    text: String,
}

/// A tool's cell in a row, `None` when a run has no value for it. A run with
/// no value for the row is left out of the spread, and of the count the
/// brackets and the bold rule depend on.
fn cell(row: &MetricRow, runs: &[&ToolRun]) -> Option<Cell> {
    let values: Vec<f64> = runs
        .iter()
        .filter_map(|r| (row.rank)(r))
        .map(|v| v as f64)
        .collect();
    let spread = stats::spread(&values)?;
    let shown = |value: f64| (row.format)(value.round() as u64);
    let range = |s: &stats::Spread| format!("{}..{}", shown(s.min), shown(s.max));
    let denominator = match row.of {
        Some(of) => Some(spread_of(runs.iter().filter_map(|r| of(r)))?),
        None => None,
    };
    let mut text = shown(spread.median);
    if let Some(d) = &denominator {
        text = format!("{text}/{}", shown(d.median));
    }
    if values.len() > 1 {
        let ranges = match &denominator {
            Some(d) => format!("{} / {}", range(&spread), range(d)),
            None => range(&spread),
        };
        text = format!("{text} ({ranges})");
    }
    Some(Cell {
        spread,
        runs: values.len(),
        text,
    })
}

/// Whether a cell is bolded as the best of its row: the best median, and with
/// rounds, only when its range overlaps no other tool's, since a median that
/// another tool's rounds reach does not separate them.
fn is_bold(row: &MetricRow, cell: &Cell, cells: &[Option<Cell>]) -> bool {
    let competitors = || cells.iter().flatten().filter(|c| !std::ptr::eq(*c, cell));
    let best = competitors().all(|other| {
        if row.lower_is_better {
            cell.spread.median <= other.spread.median
        } else {
            cell.spread.median >= other.spread.median
        }
    });
    let separate =
        |other: &Cell| cell.spread.min > other.spread.max || other.spread.min > cell.spread.max;
    best && competitors().next().is_some() && (cell.runs == 1 || competitors().all(separate))
}

/// Renders a metric comparison table into `out`.
///
/// Columns are tool names, rows are metrics. The best value per row is bolded
/// when more than one tool has data for that metric, see [`is_bold`].
fn render_metric_table(
    report: &BenchReport,
    columns: &[Tool],
    by_tool: &BTreeMap<Tool, Vec<&ToolRun>>,
    out: &mut String,
) {
    out.push_str("| Metric |");
    for &col in columns {
        out.push_str(&format!(" {} |", report.label(col)));
    }
    out.push('\n');
    out.push_str("|---|");
    for _ in columns {
        out.push_str("---:|");
    }
    out.push('\n');

    for row in &metric_rows() {
        let cells: Vec<Option<Cell>> = columns
            .iter()
            .map(|col| by_tool.get(col).and_then(|runs| cell(row, runs)))
            .collect();

        out.push_str(&format!("| {} |", row.label));
        for cell in &cells {
            match cell {
                Some(cell) if is_bold(row, cell, &cells) => {
                    out.push_str(&format!(" **{}** |", cell.text));
                }
                Some(cell) => out.push_str(&format!(" {} |", cell.text)),
                None => out.push_str(" -- |"),
            }
        }
        out.push('\n');
    }
}

/// The ratio `other / ocync` of a row's metric in each round, `None` unless
/// both tools have a value in every round.
fn round_ratio(rounds: &[&ScenarioResult], other: Tool, row: &MetricRow) -> Option<Ratio> {
    let pairs: Vec<Pair> = rounds
        .iter()
        .map(|round| {
            let value = |tool| {
                let run = round.runs.iter().find(|r| r.tool == tool)?;
                (row.rank)(run).map(|v| v as f64)
            };
            Some(Pair {
                ocync: value(Tool::Ocync)?,
                other: value(other)?,
            })
        })
        .collect::<Option<_>>()?;
    stats::ratio(&pairs)
}

/// Renders how ocync compares with each other tool round by round: the median
/// and range of the per-round ratio, the chance the range holds the true
/// median, and, from [`stats::VERDICT_ROUNDS`] rounds, what every round agrees
/// on with the chance that identical builds would agree this way. Pairing
/// within a round cancels drift that moved every tool in it.
fn render_comparison(report: &BenchReport, rounds: &[&ScenarioResult], out: &mut String) {
    // Fewer rounds than the range can be stated for, as when a run stopped
    // early, or a single pass with nothing to pair.
    if rounds.len() < Rounds::MIN {
        return;
    }
    let count = u32::try_from(rounds.len()).expect("a run has fewer than 2^32 rounds");
    let identical = (1.0 - stats::confidence(count)) * 100.0;
    let verdicts = rounds.len() >= stats::VERDICT_ROUNDS;
    let others: BTreeSet<Tool> = rounds
        .iter()
        .flat_map(|round| round.runs.iter().map(|r| r.tool))
        .filter(|&tool| tool != Tool::Ocync)
        .collect();
    let mut table = String::new();
    for other in others {
        for row in metric_rows()
            .iter()
            .filter(|row| row.verdict_words.is_some())
        {
            let Some(Ratio { spread, verdict }) = round_ratio(rounds, other, row) else {
                continue;
            };
            table.push_str(&format!(
                "| {} | {} | {:.2} | {:.2}..{:.2} |",
                report.label(other),
                row.label,
                spread.median,
                spread.min,
                spread.max,
            ));
            if verdicts {
                let [lower, higher] = row.verdict_words.expect("filtered above");
                let (words, chance) = match verdict {
                    Some(Verdict::OcyncLower) => (lower, format!("{identical:.2}%")),
                    Some(Verdict::OcyncHigher) => (higher, format!("{identical:.2}%")),
                    _ => ("no measured difference", "--".to_owned()),
                };
                table.push_str(&format!(" {words} | {chance} |"));
            }
            table.push('\n');
        }
    }
    if table.is_empty() {
        return;
    }
    out.push_str(&format!(
        "\nPer-round ratio of the other tool to ocync (above 1: ocync lower). The range \
         is the lowest to the highest of {count} rounds and holds the true median ratio \
         with {:.2}% confidence.",
        stats::confidence(count) * 100.0,
    ));
    if verdicts {
        out.push_str(
            " A difference is claimed only when every round agrees, and the last column is \
             the chance that identical builds would agree this way in every round.\n\n\
             | Tool | Metric | Median ratio | Range | ocync is | Chance if identical |\n\
             |---|---|---:|---:|---|---:|\n",
        );
    } else {
        out.push_str(&format!(
            " No verdict is given below {} rounds.\n\n\
             | Tool | Metric | Median ratio | Range |\n|---|---|---:|---:|\n",
            stats::VERDICT_ROUNDS
        ));
    }
    out.push_str(&table);
}

/// Writes the benchmark summary to `output_dir`.
///
/// Creates `summary.md` -- the human-readable comparison table with
/// per-scenario metrics and winners bolded. The performance page and the
/// compact per-registry JSON archive are written separately, see
/// [`ArchiveMode`].
pub(crate) fn write_report(output_dir: &Path, report: &BenchReport) -> Result<(), String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("create output dir {}: {e}", output_dir.display()))?;

    let md = summary_markdown(report);
    let md_path = output_dir.join("summary.md");
    std::fs::write(&md_path, md).map_err(|e| format!("write {}: {e}", md_path.display()))?;

    Ok(())
}

/// Generates a benchmark results snippet for one scenario.
///
/// Produces a context header with `description`, naming the number of rounds
/// when there is more than one, and the scenario's comparison table (metrics
/// as rows, tools as columns, winners bolded).
fn scenario_markdown(
    report: &BenchReport,
    rounds: &[&ScenarioResult],
    description: &str,
) -> String {
    let inst = &report.instance;
    let mut out = String::new();

    let proxy_note = if rounds
        .iter()
        .flat_map(|round| &round.runs)
        .all(|r| r.proxy_metrics.is_none())
    {
        "Direct connections (proxy disabled -- no traffic metrics)."
    } else {
        "All traffic routed through bench-proxy for byte-accurate measurement."
    };

    let memory_gib = inst.memory_mib as f64 / 1024.0;
    out.push_str(&format!(
        "Measured {date} on {inst_type} ({arch}, {vcpus} vCPUs, {mem:.0} GiB, {net}). \
         Full corpus: {images} images, {tags} tags. {description} to ECR {region}. \
         {proxy_note}{round_note}\n\n",
        date = report.timestamp.get(..10).unwrap_or(&report.timestamp),
        inst_type = inst.instance_type,
        arch = inst.arch,
        vcpus = inst.vcpus,
        mem = memory_gib,
        net = inst.network_performance,
        images = report.corpus_size,
        tags = report.total_tags,
        region = inst.region,
        round_note = if rounds[0].round.is_some() {
            format!(
                " {}, the tool order shuffled in each. A cell is the median over \
                 the rounds, with the lowest and highest in brackets; a paired cell (a/b) brackets \
                 the range of each value, in the order shown.",
                report.rounds_note(rounds.len())
            )
        } else {
            String::new()
        },
    ));

    let columns = tool_columns(report);
    render_metric_table(report, &columns, &rounds_by_tool(rounds), &mut out);
    render_comparison(report, rounds, &mut out);
    if let Some(footnote) = referrer_footnote(report, rounds) {
        out.push_str(&footnote);
    }

    out
}

/// The results of the scenario of this kind, one per round, in the order they
/// ran. Empty when the scenario did not run.
pub(crate) fn rounds_of(scenarios: &[ScenarioResult], kind: ScenarioKind) -> Vec<&ScenarioResult> {
    scenarios.iter().filter(|s| s.kind == kind).collect()
}

/// The scenarios that ran, each once, in the order they first appeared.
fn kinds(scenarios: &[ScenarioResult]) -> Vec<ScenarioKind> {
    let mut kinds = Vec::new();
    for scenario in scenarios {
        if !kinds.contains(&scenario.kind) {
            kinds.push(scenario.kind);
        }
    }
    kinds
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

/// The performance-page blocks of a run: the text that replaces the content
/// between each scenario's marker pair.
///
/// - `<!-- BENCH:START -->` / `<!-- BENCH:END -->` for the cold scenario
/// - `<!-- BENCH-WARM:START -->` / `<!-- BENCH-WARM:END -->` for the warm one
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PageBlocks {
    /// The cold-sync block; `None` when the run has no cold scenario.
    cold: Option<String>,
    /// The warm-sync block; `None` when the run has no warm scenario.
    warm: Option<String>,
}

impl PageBlocks {
    /// The blocks `report` produces. A run whose transfer cap is not ocync's
    /// default produces none: the page shows ocync as it runs out of the box.
    pub(crate) fn of(report: &BenchReport) -> Self {
        if effective_transfer_cap(report.max_concurrent_transfers)
            != DEFAULT_MAX_CONCURRENT_TRANSFERS
        {
            return Self::default();
        }
        let block = |kind, description| {
            let rounds = rounds_of(&report.scenarios, kind);
            (!rounds.is_empty()).then(|| scenario_markdown(report, &rounds, description))
        };
        Self {
            cold: block(ScenarioKind::Cold, "Cold sync"),
            warm: block(ScenarioKind::Warm, "Warm sync (no changes)"),
        }
    }

    /// Writes the blocks into the page at `path`, replacing what is between
    /// each marker pair, so applying the same blocks twice leaves the page as
    /// applying them once. A pair missing from the page is skipped with a
    /// warning; a page with no block to write is left untouched. Returns
    /// whether the page was written.
    pub(crate) fn apply(&self, path: &Path) -> Result<bool, String> {
        let mut content =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;

        let mut updated = false;
        for (block, start, end, name) in [
            (&self.cold, BENCH_COLD_START, BENCH_COLD_END, "cold"),
            (&self.warm, BENCH_WARM_START, BENCH_WARM_END, "warm"),
        ] {
            let Some(snippet) = block else { continue };
            match replace_between_markers(&content, start, end, snippet)? {
                Some(new_content) => {
                    content = new_content;
                    updated = true;
                }
                None => {
                    eprintln!(
                        "WARNING: {start} marker not found in {}; skipping {name} update",
                        path.display()
                    );
                }
            }
        }

        if updated {
            std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
        }

        Ok(updated)
    }
}

/// Where a run's record and performance-page blocks go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ArchiveMode {
    /// Append the record to the tracked archive and update the tracked page.
    Write,
    /// Touch neither: write both into the run's output directory
    /// ([`Deferred`]) for whoever fetches the run to apply.
    Defer,
}

/// What a deferred run leaves in its output directory in place of writing the
/// tracked archive and page.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Deferred {
    /// The archive the record belongs to: `{archive_key}.json`.
    archive_key: String,
    /// The run's record; `None` for a run that is not comparable to the
    /// archived ones, which the archive never takes.
    record: Option<RunRecord>,
    /// The page blocks.
    page: PageBlocks,
}

impl Deferred {
    /// The deferred output of a run: its `record` going to the archive
    /// `archive_key`, and the `page` blocks.
    pub(crate) fn new(archive_key: &str, record: Option<RunRecord>, page: PageBlocks) -> Self {
        Self {
            archive_key: archive_key.to_string(),
            record,
            page,
        }
    }

    /// Writes this into `output_dir`.
    pub(crate) fn write(&self, output_dir: &Path) -> Result<(), String> {
        let path = output_dir.join(DEFERRED_FILE);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize {}: {e}", path.display()))?;
        std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// What a run left in `output_dir`; `None` for a directory with none (a
    /// run that was not deferred).
    #[expect(clippy::disallowed_methods, reason = "reads a file the bench wrote")]
    pub(crate) fn read(output_dir: &Path) -> Result<Option<Self>, String> {
        let path = output_dir.join(DEFERRED_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let contents =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&contents)
            .map(Some)
            .map_err(|e| format!("parse {}: {e}", path.display()))
    }

    /// Appends the record to the archive in `archive_dir` and writes the
    /// blocks into `page`; the one step a local run and a fetched run share.
    /// Safe to repeat, see [`append_record`]. A record that cannot be appended
    /// is an error. A `page` that does not exist is left alone, and one that
    /// cannot be updated (broken markers, say) is a warning, never an error.
    pub(crate) fn apply(self, archive_dir: &Path, page: &Path) -> Result<(), String> {
        if let Some(record) = self.record {
            append_record(archive_dir, &self.archive_key, record)?;
            eprintln!(
                "bench: run record is in {}",
                archive_dir
                    .join(format!("{}.json", self.archive_key))
                    .display()
            );
        }
        if page.exists() {
            match self.page.apply(page) {
                Ok(true) => eprintln!("bench: updated {}", page.display()),
                Ok(false) => {}
                Err(e) => eprintln!("WARNING: failed to update {}: {e}", page.display()),
            }
        }
        Ok(())
    }
}

/// The part of an archived record the duplicate check reads.
#[derive(Deserialize)]
struct Stamped {
    timestamp: String,
}

/// Appends a compact run record to the per-registry JSON archive.
///
/// The file at `results_dir/{registry_key}.json` is a JSON array of
/// `RunRecord` values. If the file does not exist, it is created with
/// `[record]`. If it exists, the record is appended, unless the file already
/// has a record with its timestamp: a run is recorded once, however many
/// times its record is applied. Earlier records are never parsed into
/// `RunRecord` and written back, so their bytes stay as written whatever
/// fields `RunRecord` has gained or lost since. If the file contains invalid
/// JSON, an error is returned (never silently overwrite corrupt data).
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
    let contents = if path.exists() {
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?
    } else {
        "[]".to_owned()
    };
    let archived: Vec<Stamped> =
        serde_json::from_str(&contents).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if archived.iter().any(|r| r.timestamp == record.timestamp) {
        return Ok(());
    }

    let item = serde_json::to_string_pretty(&record)
        .map_err(|e| format!("serialize run record: {e}"))?
        .replace('\n', "\n  ");
    // The array parsed, so its last non-space character is the closing `]`.
    let body = contents.trim_end().trim_end_matches(']').trim_end();
    let separator = if archived.is_empty() { "" } else { "," };
    let json = format!("{body}{separator}\n  {item}\n]");
    std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// A finished cold-sync report to build records and page blocks from.
    pub(crate) fn sample_report() -> BenchReport {
        BenchReport {
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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

        // No per-scenario JSON file is written.
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
    fn the_tracked_archive_still_parses() {
        let archive = include_str!("../../../bench/results/ecr.json");
        let records: Vec<RunRecord> = serde_json::from_str(archive).unwrap();
        assert!(!records.is_empty());
        // Catches: a record that does not rewrite as it was read, as when a
        // field an old run never had is written as an empty value. Compared
        // as values because the archive spells one float longer than Rust's
        // shortest form.
        let rewritten = serde_json::to_string_pretty(&records).unwrap();
        let as_value = |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap();
        assert_eq!(as_value(&rewritten), as_value(archive));
    }

    #[test]
    fn corpus_locked_at_is_written_when_set() {
        let mut record: RunRecord = serde_json::from_str(
            r#"{
                "timestamp": "2026-10-30-120000",
                "git_ref": "abc1234",
                "machine": {
                    "instance_type": "c6in.4xlarge", "arch": "x86_64", "vcpus": 16,
                    "memory_gib": 32.0, "network": "n", "region": "us-east-1",
                    "image_id": "unknown"
                },
                "corpus": { "images": 1, "tags": 1 },
                "scenarios": []
            }"#,
        )
        .unwrap();
        record.corpus_locked_at =
            Some(crate::bench::lock::fixtures::locked_at("2026-10-30-120000"));
        let with = serde_json::to_string(&record).unwrap();
        assert!(
            with.contains(r#""corpus_locked_at":"2026-10-30-120000""#),
            "{with}"
        );
        let reread: RunRecord = serde_json::from_str(&with).unwrap();
        assert_eq!(reread.corpus_locked_at, record.corpus_locked_at);

        // Catches: a run of locked digests recorded as indistinguishable from
        // the earlier runs that followed source tags, and a run with no lock
        // claiming digests.
        let at = || Some(crate::bench::lock::fixtures::locked_at("2026-10-30-120000"));
        let json = |at| serde_json::to_string(&RunRecord::of(&sample_report(), "abc".into(), at));
        assert!(json(at()).unwrap().contains(r#""sync_from":"digest""#));
        assert!(!json(None).unwrap().contains("sync_from"));
    }

    #[test]
    fn the_cap_ocync_ran_with_is_recorded_and_a_capped_run_writes_no_page_blocks() {
        // Catches: a run at ocync's default recorded as uncapped, so a later
        // default sits in the same series as the old one; a capped run
        // published on the page as ocync's out-of-the-box result; the baseline
        // build's default taken to be the current one's.
        let default = DEFAULT_MAX_CONCURRENT_TRANSFERS;
        let mut report = five_rounds(1);
        let record = RunRecord::of(&report, "abc1234".into(), None);
        assert_eq!(record.max_concurrent_transfers, Some(default));
        assert_eq!(
            record.baseline_max_concurrent_transfers,
            Some(BaselineCap::OwnDefault)
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains(r#""baseline_max_concurrent_transfers":"own_default""#),
            "{json}"
        );
        assert!(summary_markdown(&report).contains(&format!(
            "ocync ran with `max_concurrent_transfers: {default}`; the baseline build ran at \
             its own default, which may differ\n"
        )));
        assert_ne!(PageBlocks::of(&report), PageBlocks::default());

        report.max_concurrent_transfers = Some(default);
        assert_ne!(PageBlocks::of(&report), PageBlocks::default());

        report.max_concurrent_transfers = NonZeroUsize::new(30);
        let record = RunRecord::of(&report, "abc1234".into(), None);
        assert_eq!(record.max_concurrent_transfers, NonZeroUsize::new(30));
        assert_eq!(
            record.baseline_max_concurrent_transfers,
            NonZeroUsize::new(30).map(BaselineCap::Given)
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""max_concurrent_transfers":30"#), "{json}");
        assert!(
            json.contains(r#""baseline_max_concurrent_transfers":{"given":30}"#),
            "{json}"
        );
        let reread: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reread.baseline_max_concurrent_transfers,
            record.baseline_max_concurrent_transfers
        );
        assert!(
            summary_markdown(&report).contains("ocync ran with `max_concurrent_transfers: 30`\n")
        );
        assert_eq!(PageBlocks::of(&report), PageBlocks::default());

        let mut no_ocync = sample_report();
        no_ocync.tool_versions.remove(&Tool::Ocync);
        let record = RunRecord::of(&no_ocync, "abc1234".into(), None);
        assert_eq!(record.max_concurrent_transfers, None);
        assert_eq!(record.baseline_max_concurrent_transfers, None);
        assert!(!summary_markdown(&no_ocync).contains("max_concurrent_transfers"));
    }

    #[test]
    fn the_summary_notes_a_pre_warm_only_when_it_was_skipped_or_left_a_registry_unwarmed() {
        // Catches: an unwarmed registry recorded nowhere a reader of the
        // numbers would see it.
        let unwarmed = Prewarm::Done {
            unwarmed: BTreeMap::from([("quay.io".to_owned(), 3)]),
        };
        let cases = [
            (
                Prewarm::Done {
                    unwarmed: BTreeMap::new(),
                },
                None,
            ),
            (Prewarm::Skipped, Some("Pre-warm: skipped (--skip-prewarm)")),
            (unwarmed, Some("Pre-warm: not warmed for quay.io (3 blobs)")),
        ];
        for (outcome, line) in cases {
            let mut report = sample_report();
            report.prewarm = Some(outcome);
            let summary = summary_markdown(&report);
            assert_eq!(summary.contains("Pre-warm:"), line.is_some(), "{summary}");
            if let Some(line) = line {
                assert!(summary.contains(line), "{summary}");
            }
        }
    }

    #[test]
    fn the_machines_cloud_is_aws_only_when_ec2_answered() {
        let mut instance = test_instance();
        assert_eq!(instance.provider(), Some(Provider::Aws));
        instance.instance_type = "unknown".into();
        assert_eq!(instance.provider(), None);
    }

    #[test]
    fn a_run_record_carries_the_pre_warm_and_no_relay_as_an_empty_map() {
        // Catches: a record written with the pre-warm or the relays dropped,
        // which reads as "not recorded" instead of the run's own answer.
        let mut report = sample_report();
        report.prewarm = Some(Prewarm::Done {
            unwarmed: BTreeMap::from([("quay.io".to_owned(), 3)]),
        });
        let record = RunRecord::of(&report, "abc1234".into(), None);
        let written = serde_json::to_value(&record).unwrap();
        assert_eq!(written["relays"], serde_json::json!({}));
        assert_eq!(
            written["prewarm"],
            serde_json::json!({"done": {"unwarmed": {"quay.io": 3}}})
        );
    }

    #[test]
    fn append_record_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord::of(&sample_report(), "abc1234".into(), None);
        append_record(dir.path(), "ecr", record).unwrap();

        let path = dir.path().join("ecr.json");
        assert!(path.exists());
        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, "2026-04-15-100000");
    }

    #[test]
    fn append_record_appends_to_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecr.json");
        std::fs::write(&path, "[]").unwrap();

        let record = RunRecord::of(&sample_report(), "abc1234".into(), None);
        append_record(dir.path(), "ecr", record).unwrap();

        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].git_ref, "abc1234");
    }

    #[test]
    fn append_record_rejects_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecr.json");
        std::fs::write(&path, "not json").unwrap();

        let record = RunRecord::of(&sample_report(), "abc1234".into(), None);
        assert!(append_record(dir.path(), "ecr", record).is_err());
    }

    fn sample_record() -> RunRecord {
        RunRecord::of(&sample_report(), "abc1234".into(), None)
    }

    #[test]
    fn append_record_skips_a_timestamp_already_in_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        append_record(dir.path(), "ecr", sample_record()).unwrap();
        append_record(dir.path(), "ecr", sample_record()).unwrap();

        let path = dir.path().join("ecr.json");
        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 1);

        // A different run is still appended.
        let mut later = sample_record();
        later.timestamp = "2026-04-16-100000".into();
        append_record(dir.path(), "ecr", later).unwrap();
        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn appending_leaves_an_old_records_floats_as_written() {
        // Catches: a float that does not round-trip through `f64` parsing
        // being rewritten (`2.5753332589999998` as `2.575333259`).
        let dir = tempfile::tempdir().unwrap();
        let old = serde_json::to_string_pretty(&[sample_record()])
            .unwrap()
            .replacen(
                "\"wall_clock_secs\": 47.0",
                "\"wall_clock_secs\": 2.5753332589999998",
                1,
            );
        assert!(old.contains("2.5753332589999998"), "{old}");
        std::fs::write(dir.path().join("ecr.json"), &old).unwrap();

        let mut later = sample_record();
        later.timestamp = "2026-04-16-100000".into();
        append_record(dir.path(), "ecr", later).unwrap();

        let after = std::fs::read_to_string(dir.path().join("ecr.json")).unwrap();
        assert!(after.contains("2.5753332589999998"), "{after}");
    }

    #[test]
    fn appending_keeps_the_archive_format_and_leaves_old_records_as_written() {
        // Catches: an old record re-serialized with a field it never had, and
        // an appended record indented differently from the archive.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecr.json");
        let first = sample_record();
        let mut second = sample_record();
        second.timestamp = "2026-04-16-100000".into();
        let pretty = |records: &[&RunRecord]| serde_json::to_string_pretty(records).unwrap();
        let both = pretty(&[&first, &second]);
        let one = pretty(&[&first]);

        // An archive written before `referrer_calls` existed.
        let old = one.replace(",\n            \"referrer_calls\": 0", "");
        assert_ne!(old, one, "the field was not found to remove");
        std::fs::write(&path, &old).unwrap();
        append_record(dir.path(), "ecr", second).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let kept = old.strip_suffix("\n]").unwrap();
        assert!(after.starts_with(kept), "{after}");
        // The new record is indented as the archive writes it.
        let tail = both.strip_prefix(one.strip_suffix("\n]").unwrap()).unwrap();
        assert_eq!(&after[kept.len()..], tail, "{after}");
    }

    #[test]
    fn a_deferred_run_is_applied_once_however_often_it_is_fetched() {
        let run = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let page = local.path().join("performance.md");
        let marked = "<!-- BENCH:START -->\nold\n<!-- BENCH:END -->\n";
        std::fs::write(&page, marked).unwrap();

        // A directory with no deferred output has nothing to apply.
        assert!(Deferred::read(run.path()).unwrap().is_none());

        let report = sample_report();
        Deferred::new("ecr", Some(sample_record()), PageBlocks::of(&report))
            .write(run.path())
            .unwrap();

        for _ in 0..2 {
            Deferred::read(run.path())
                .unwrap()
                .unwrap()
                .apply(local.path(), &page)
                .unwrap();
        }

        let archive = local.path().join("ecr.json");
        let records: Vec<RunRecord> =
            serde_json::from_str(&std::fs::read_to_string(archive).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        let content = std::fs::read_to_string(&page).unwrap();
        assert!(content.contains("Measured 2026-04-15"), "{content}");
        assert!(!content.contains("\nold\n"), "{content}");
        assert_eq!(content.matches("Measured").count(), 1);
    }

    #[test]
    fn a_page_that_cannot_be_updated_is_a_warning_not_an_error() {
        let local = tempfile::tempdir().unwrap();
        let page = local.path().join("performance.md");
        // End marker before the start marker: the page cannot take the block.
        let broken = "<!-- BENCH:END -->\n<!-- BENCH:START -->\n";
        std::fs::write(&page, broken).unwrap();

        Deferred::new(
            "ecr",
            Some(sample_record()),
            PageBlocks::of(&sample_report()),
        )
        .apply(local.path(), &page)
        .unwrap();

        assert_eq!(std::fs::read_to_string(&page).unwrap(), broken);
        assert!(local.path().join("ecr.json").exists(), "the record is kept");
    }

    #[test]
    fn a_deferred_run_not_comparable_to_the_archive_appends_no_record() {
        let local = tempfile::tempdir().unwrap();
        let page = local.path().join("performance.md");
        Deferred::new("ecr", None, PageBlocks::of(&sample_report()))
            .apply(local.path(), &page)
            .unwrap();
        assert!(!local.path().join("ecr.json").exists());
        // A page that does not exist is not created.
        assert!(!page.exists());
    }

    #[test]
    fn summary_markdown_single_tool_no_bold() {
        let report = BenchReport {
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
        let cold = rounds_of(&report.scenarios, ScenarioKind::Cold);
        let md = scenario_markdown(&report, &cold, "Cold sync");
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-15-100000".to_string(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
        let warm = rounds_of(&report.scenarios, ScenarioKind::Warm);
        let md = scenario_markdown(&report, &warm, "Warm sync (no changes)");
        assert!(md.contains("Warm sync (no changes) to ECR"));
        assert!(md.contains("| Wall clock | 2s |"), "md:\n{md}");
    }

    #[test]
    fn rounds_of_selects_by_kind() {
        let mut report = sample_report();
        report.scenarios.insert(
            0,
            ScenarioResult::unshuffled(ScenarioKind::Scale { images: 10 }, vec![]),
        );

        let cold = rounds_of(&report.scenarios, ScenarioKind::Cold);

        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].kind.label(), "Cold sync");
        assert!(rounds_of(&report.scenarios, ScenarioKind::Warm).is_empty());
        assert!(rounds_of(&report.scenarios, ScenarioKind::Scale { images: 25 }).is_empty());
    }

    #[test]
    fn page_blocks_replaces_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        std::fs::write(
            &path,
            "# Performance\n\n<!-- BENCH:START -->\nold content\n<!-- BENCH:END -->\n\nFooter\n",
        )
        .unwrap();

        let report = sample_report();
        assert!(PageBlocks::of(&report).apply(&path).unwrap());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<!-- BENCH:START -->"));
        assert!(content.contains("<!-- BENCH:END -->"));
        assert!(!content.contains("old content"));
        assert!(content.contains("Measured 2026-04-15"));
        assert!(content.contains("Footer"));
    }

    #[test]
    fn page_blocks_missing_markers_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        let original = "# Performance\n\nNo markers here.\n";
        std::fs::write(&path, original).unwrap();

        let report = sample_report();
        // Missing markers warn instead of failing: both are optional.
        PageBlocks::of(&report).apply(&path).unwrap();
        // File should be unchanged.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// Report with populated proxy metrics for full table coverage.
    fn sample_report_with_metrics() -> BenchReport {
        BenchReport {
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-18-100000".to_string(),
            instance: test_instance(),
            corpus_size: 42,
            total_tags: 55,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-18-100000".to_string(),
            instance: test_instance(),
            corpus_size: 42,
            total_tags: 55,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-18-100000".to_string(),
            instance: test_instance(),
            corpus_size: 42,
            total_tags: 55,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
    fn page_blocks_reversed_markers_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("performance.md");
        std::fs::write(
            &path,
            "# Performance\n\n<!-- BENCH:END -->\nold\n<!-- BENCH:START -->\n",
        )
        .unwrap();

        let report = sample_report();
        let err = PageBlocks::of(&report).apply(&path).unwrap_err();
        assert!(err.contains("must appear before"), "err: {err}");
    }

    #[test]
    fn page_blocks_both_cold_and_warm() {
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-19-100000".to_string(),
            instance: test_instance(),
            corpus_size: 39,
            total_tags: 51,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
        PageBlocks::of(&report).apply(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old cold"));
        assert!(!content.contains("old warm"));
        assert!(content.contains("Cold sync to ECR"));
        assert!(content.contains("Warm sync (no changes) to ECR"));
        assert!(content.contains("Footer"));
    }

    #[test]
    fn page_blocks_warm_only() {
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-19-100000".to_string(),
            instance: test_instance(),
            corpus_size: 39,
            total_tags: 51,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
        PageBlocks::of(&report).apply(&path).unwrap();

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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-26-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-26-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
        // PageBlocks -> scenario_markdown) must also surface
        // referrers when ocync is in the run.
        let report = BenchReport {
            baseline: None,
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: Vec::new(),
            timestamp: "2026-04-26-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            relay_versions: BTreeMap::new(),
            prewarm: None,
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
        let md = scenario_markdown(&report, &[scenario], "Cold sync");
        assert!(
            md.contains("OCI 1.1 referrers"),
            "footnote missing from scenario_markdown:\n{md}"
        );
        assert!(md.contains("ocync: 79"), "count missing:\n{md}");
    }

    /// What each tool did per round: `(wall clock seconds, requests)`.
    type Rounds5 = [(f64, u64); 5];

    /// A report of the first `finished` of 5 requested rounds of the cold
    /// scenario, the baseline built at a fixed commit.
    fn rounds_report(finished: usize, tools: &[(Tool, Rounds5)]) -> BenchReport {
        BenchReport {
            baseline: Some(CommitSha::fixture()),
            rounds: Rounds::DEFAULT,
            max_concurrent_transfers: None,
            repeated: vec![ScenarioKind::Cold],
            timestamp: "2026-10-30-100000".into(),
            instance: test_instance(),
            corpus_size: 28,
            total_tags: 40,
            tool_versions: tools
                .iter()
                .map(|&(tool, _)| (tool, "1.0".into()))
                .collect(),
            relay_versions: BTreeMap::new(),
            prewarm: None,
            scenarios: (0..finished)
                .map(|round| ScenarioResult {
                    round: Some(round + 1),
                    shuffle_seed: 7,
                    tool_order: tools.iter().map(|&(tool, _)| tool).collect(),
                    ..ScenarioResult::unshuffled(
                        ScenarioKind::Cold,
                        tools
                            .iter()
                            .map(|&(tool, runs)| ToolRun {
                                tool,
                                wall_clock_secs: runs[round].0,
                                peak_rss_kb: None,
                                proxy_metrics: Some(ProxyMetrics {
                                    requests: runs[round].1,
                                    referrer_calls: if tool == Tool::OcyncBaseline {
                                        runs[round].1
                                    } else {
                                        0
                                    },
                                    ..Default::default()
                                }),
                                phase_timing_ms: BTreeMap::new(),
                            })
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    /// Five requested rounds of four tools. Against ocync, dregsy is 2 to 2.5
    /// times as slow in every round, regsync a half to 0.6 as fast in every
    /// round, and the baseline on both sides of equal.
    fn five_rounds(finished: usize) -> BenchReport {
        rounds_report(
            finished,
            &[
                (
                    Tool::Ocync,
                    [
                        (40.0, 100),
                        (48.0, 110),
                        (44.0, 105),
                        (46.0, 108),
                        (42.0, 102),
                    ],
                ),
                (
                    Tool::OcyncBaseline,
                    [
                        (42.0, 100),
                        (46.0, 110),
                        (43.0, 105),
                        (47.0, 108),
                        (41.0, 102),
                    ],
                ),
                (
                    Tool::Dregsy,
                    [
                        (80.0, 300),
                        (120.0, 330),
                        (88.0, 315),
                        (92.0, 324),
                        (84.0, 306),
                    ],
                ),
                (
                    Tool::Regsync,
                    [
                        (20.0, 300),
                        (29.0, 330),
                        (22.0, 315),
                        (23.0, 324),
                        (21.0, 306),
                    ],
                ),
            ],
        )
    }

    #[test]
    fn five_rounds_show_ranges_verdicts_and_the_chance_of_identical_builds() {
        let md = summary_markdown(&five_rounds(5));

        // Catches: a cell showing one round, a mean, or a median without its
        // range; bold on a median another tool's rounds reach; the baseline
        // under any name but its commit.
        assert!(md.contains("## Cold sync (5 rounds)\n"), "{md}");
        assert!(
            md.contains("| Metric | ocync | ocync@0123456 | dregsy | regsync |"),
            "{md}"
        );
        assert!(
            md.contains(
                "| Wall clock | 44s (40s..48s) | 43s (41s..47s) | 1m 28s (1m 20s..2m) | **22s (20s..29s)** |"
            ),
            "{md}"
        );
        // The ocync and baseline requests are equal and overlap, so neither is
        // bolded, and neither is the rest.
        assert!(
            md.contains(
                "| Requests | 105 (100..110) | 105 (100..110) | 315 (300..330) | 315 (300..330) |"
            ),
            "{md}"
        );
        assert!(
            md.contains("holds the true median ratio with 93.75% confidence"),
            "{md}"
        );
        for row in [
            "| dregsy | Wall clock | 2.00 | 2.00..2.50 | faster | 6.25% |",
            "| dregsy | Requests | 3.00 | 3.00..3.00 | fewer | 6.25% |",
            "| regsync | Wall clock | 0.50 | 0.50..0.60 | slower | 6.25% |",
            "| regsync | Requests | 3.00 | 3.00..3.00 | fewer | 6.25% |",
            // Both sides of 1 on time and exactly 1 on requests: no claim.
            "| ocync@0123456 | Wall clock | 0.98 | 0.96..1.05 | no measured difference | -- |",
            "| ocync@0123456 | Requests | 1.00 | 1.00..1.00 | no measured difference | -- |",
        ] {
            assert!(md.contains(&format!("{row}\n")), "{row}\n{md}");
        }
        // Response bytes are zero for ocync in every round, so no ratio exists.
        assert!(!md.contains("| dregsy | Response bytes |"), "{md}");
        // The footnote names the baseline by its commit and gives the median.
        assert!(
            md.contains("Referrers calls -- ocync: 0, ocync@0123456: 105,"),
            "{md}"
        );
    }

    #[test]
    fn a_paired_cell_shows_the_range_of_both_values() {
        // A paired cell brackets the range of each of its values, so a second
        // value that varies shows even when the first does not.
        let mut report = five_rounds(3);
        let paired = [
            (1003, 69, 320, 300),
            (1072, 0, 355, 300),
            (1072, 0, 356, 300),
        ];
        for (scenario, (hits, misses, attempts, successes)) in
            report.scenarios.iter_mut().zip(paired)
        {
            let metrics = scenario.runs[0].proxy_metrics.as_mut().unwrap();
            metrics.cdn_hits = hits;
            metrics.cdn_misses = misses;
            metrics.mount_attempts = attempts;
            metrics.mount_successes = successes;
        }
        let md = summary_markdown(&report);

        assert!(
            md.contains("| CDN hits/misses | **1072/0 (1003..1072 / 0..69)** |"),
            "{md}"
        );
        // Only the second value varies in the successes/attempts row.
        assert!(
            md.contains("| Mounts (success/attempt) | **300/355 (300..300 / 320..356)** |"),
            "{md}"
        );
    }

    #[test]
    fn a_cell_of_one_measured_value_is_plain_and_bold_when_it_wins() {
        // Rounds with no value for a row are not rounds of its cell.
        let mut report = five_rounds(3);
        for (round, scenario) in report.scenarios.iter_mut().enumerate() {
            scenario.runs[0].peak_rss_kb = (round == 0).then_some(1000);
            scenario.runs[3].peak_rss_kb = Some(2_000_000 + round as u64);
        }
        let md = summary_markdown(&report);

        assert!(md.contains("| Peak RSS | **1.0 MB** | -- | -- | "), "{md}");
    }

    #[test]
    fn three_and_four_rounds_show_ranges_and_claim_nothing() {
        // Catches: a verdict at 3 or 4 rounds, where identical builds would
        // show it one time in four or eight.
        for (finished, confidence) in [(3, "75.00%"), (4, "87.50%")] {
            let md = summary_markdown(&five_rounds(finished));

            assert!(
                md.contains(&format!(
                    "## Cold sync ({finished} of 5 rounds (stopped))\n"
                )),
                "{md}"
            );
            assert!(
                md.contains(&format!("with {confidence} confidence.")),
                "{md}"
            );
            assert!(md.contains("No verdict is given below 5 rounds."), "{md}");
            assert!(
                md.contains("| dregsy | Wall clock | 2.00 | 2.00..2.50 |\n"),
                "{md}"
            );
            assert!(
                !md.contains("faster") && !md.contains("Chance if identical"),
                "{md}"
            );
        }
    }

    #[test]
    fn a_run_stopped_short_of_the_fewest_rounds_claims_nothing() {
        // Catches: ratios printed after a failed round leaves 2 finished.
        let md = summary_markdown(&five_rounds(2));

        assert!(
            md.contains("## Cold sync (2 of 5 rounds (stopped))"),
            "{md}"
        );
        assert!(md.contains("**24s (20s..29s)**"), "{md}");
        assert!(!md.contains("Per-round ratio"), "{md}");
    }

    #[test]
    fn a_median_is_bold_only_when_its_range_overlaps_no_other() {
        let row = &metric_rows()[0];
        let cell = |spread: (f64, f64, f64), runs| Cell {
            spread: stats::Spread {
                median: spread.0,
                min: spread.1,
                max: spread.2,
            },
            runs,
            text: String::new(),
        };
        let bold = |cells: &[Option<Cell>]| {
            cells
                .iter()
                .map(|c| c.as_ref().is_some_and(|c| is_bold(row, c, cells)))
                .collect::<Vec<_>>()
        };

        // Catches: bold on the lower median when the ranges overlap, which
        // reads as a win the rounds do not show.
        let apart = [
            Some(cell((20.0, 10.0, 30.0), 5)),
            Some(cell((50.0, 40.0, 60.0), 5)),
        ];
        assert_eq!(bold(&apart), [true, false]);
        let overlapping = [
            Some(cell((20.0, 10.0, 45.0), 5)),
            Some(cell((50.0, 40.0, 60.0), 5)),
        ];
        assert_eq!(bold(&overlapping), [false, false]);
        // One round has no range to overlap, and a tie is bolded on both.
        let single = [
            Some(cell((20.0, 20.0, 20.0), 1)),
            Some(cell((50.0, 50.0, 50.0), 1)),
        ];
        assert_eq!(bold(&single), [true, false]);
        let tie = [
            Some(cell((20.0, 20.0, 20.0), 1)),
            Some(cell((20.0, 20.0, 20.0), 1)),
        ];
        assert_eq!(bold(&tie), [true, true]);
        // Nothing to compete with, and a tool with no value.
        assert_eq!(
            bold(&[Some(cell((20.0, 10.0, 30.0), 5)), None]),
            [false, false]
        );
    }

    #[test]
    fn the_performance_page_block_states_its_round_count() {
        // Catches: a published table that does not say how many rounds back it,
        // or that a run stopped short, and a run of one pass claiming some.
        let report = five_rounds(5);
        let md = scenario_markdown(
            &report,
            &rounds_of(&report.scenarios, ScenarioKind::Cold),
            "Cold sync",
        );
        assert!(
            md.contains(" 5 rounds, the tool order shuffled in each."),
            "{md}"
        );
        assert!(
            md.contains("Per-round ratio of the other tool to ocync"),
            "{md}"
        );

        let stopped = five_rounds(3);
        let md = scenario_markdown(
            &stopped,
            &rounds_of(&stopped.scenarios, ScenarioKind::Cold),
            "Cold sync",
        );
        assert!(
            md.contains(" 3 of 5 rounds (stopped), the tool order"),
            "{md}"
        );

        let once = sample_report();
        let md = scenario_markdown(
            &once,
            &rounds_of(&once.scenarios, ScenarioKind::Cold),
            "Cold sync",
        );
        assert!(!md.contains("rounds") && !md.contains("Per-round"), "{md}");
        assert!(!summary_markdown(&once).contains("rounds"));
    }

    #[test]
    fn a_record_keeps_every_round_and_names_the_baseline_by_its_commit() {
        let record = RunRecord::of(&five_rounds(3), "abc1234".into(), None);

        // Catches: rounds collapsed to one, a lost seed or order, the baseline
        // recorded under ocync's name, and a stopped run recorded as complete.
        let rounds: Vec<Option<usize>> = record.scenarios.iter().map(|s| s.round).collect();
        assert_eq!(rounds, [Some(1), Some(2), Some(3)]);
        let names: Vec<&str> = record.scenarios[1]
            .tools
            .iter()
            .map(|t| t.tool.as_str())
            .collect();
        assert_eq!(names, ["ocync", "ocync@0123456", "dregsy", "regsync"]);
        assert_eq!(record.scenarios[1].tool_order, names);
        assert_eq!(record.scenarios[1].shuffle_seed, 7);
        assert_eq!(record.scenarios[2].tools[0].wall_clock_secs, 44.0);
        assert_eq!(
            record.rounds,
            Some(RoundCount {
                requested: 5,
                finished: 3
            })
        );
    }

    #[test]
    fn a_repeated_scenario_that_finished_no_round_is_counted_as_zero() {
        // Catches: the count taken from the results alone, so a partial
        // scenario that failed in round 1 left the run recorded as complete.
        let mut report = five_rounds(5);
        report.repeated = vec![ScenarioKind::Cold, ScenarioKind::Partial];
        let record = RunRecord::of(&report, "abc1234".into(), None);
        assert_eq!(
            record.rounds,
            Some(RoundCount {
                requested: 5,
                finished: 0
            })
        );
    }

    #[test]
    fn a_record_of_a_single_pass_has_no_round_and_still_parses() {
        // Catches: a `round` or `rounds` field written for a scenario run once,
        // which would change every record already archived, and one required
        // to parse.
        let record = RunRecord::of(&sample_report(), "abc1234".into(), None);
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("\"round"), "{json}");

        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert!(parsed.scenarios.iter().all(|s| s.round.is_none()));
        assert_eq!(parsed.rounds, None);

        let rounds = RunRecord::of(&five_rounds(5), "abc1234".into(), None);
        let json = serde_json::to_string(&rounds).unwrap();
        assert!(json.contains("\"round\":3"), "{json}");
        assert!(
            json.contains("\"rounds\":{\"requested\":5,\"finished\":5}"),
            "{json}"
        );
    }
}
