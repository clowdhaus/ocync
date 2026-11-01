//! Benchmark suite for comparing ocync against dregsy and regsync.

pub(crate) mod acr;
pub(crate) mod az;
pub(crate) mod config_gen;
pub(crate) mod corpus;
pub(crate) mod ecr;
pub(crate) mod gar;
pub(crate) mod gcloud;
pub(crate) mod lock;
pub(crate) mod provision;
pub(crate) mod proxy;
pub(crate) mod registry;
pub(crate) mod regression;
pub(crate) mod remote;
pub(crate) mod report;
pub(crate) mod runner;
pub(crate) mod skopeo_shim;
pub(crate) mod stats;
pub(crate) mod target;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::rc::Rc;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use ocync_distribution::Digest;
use serde::{Deserialize, Serialize};

use corpus::Corpus;
use regression::Baseline;
use remote::CommitSha;
use report::{BenchReport, Prewarm, ScenarioKind, ScenarioResult, ToolRun, rounds_of};
use runner::Tool;
use target::{Destination, Store, Target};

/// How many rounds the sync and partial scenarios run, each round running
/// every tool once in a fresh order.
///
/// At least [`Rounds::MIN`], the fewest for which a range of per-round ratios
/// is stated at all. It holds the true median with a confidence of
/// `1 - 2 * 0.5^n`, and a verdict between tools needs
/// [`stats::VERDICT_ROUNDS`].
/// Scale and prepare run ocync alone, so they have no ratio to take and run
/// one pass whatever this is, which is why no type for a single round exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rounds(usize);

impl Rounds {
    /// The fewest rounds a range is stated for; fewer say nothing of the
    /// median, and fewer than [`stats::VERDICT_ROUNDS`] give no verdict.
    const MIN: usize = 3;
    /// Rounds when none are asked for.
    pub(crate) const DEFAULT: Self = Self(5);

    /// The number of rounds.
    fn get(self) -> usize {
        self.0
    }
}

impl std::str::FromStr for Rounds {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let rounds: usize = s
            .parse()
            .map_err(|_| format!("`{s}` is not a whole number of rounds"))?;
        if rounds < Self::MIN {
            return Err(format!(
                "{rounds} rounds are too few to state a range; the fewest is {}, and a \
                 verdict between tools needs {}",
                Self::MIN,
                stats::VERDICT_ROUNDS
            ));
        }
        Ok(Self(rounds))
    }
}

impl std::fmt::Display for Rounds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What a benchmark runs and how often, shared by `bench` and `bench-remote`.
#[derive(Args)]
pub(crate) struct ToolSelection {
    /// Comma-separated list of tools to benchmark.
    #[arg(
        long,
        value_delimiter = ',',
        ignore_case = true,
        default_value = "ocync"
    )]
    pub(crate) tools: Vec<Tool>,

    /// Also run the ocync built from this commit, a full or short hex id, in
    /// every round of the sync and partial scenarios, to tell a regression
    /// from run-to-run noise. Its checkout and build live under
    /// `bench/.baseline/`, one commit at a time.
    #[arg(long, value_parser = parse_baseline_commit)]
    pub(crate) baseline_ref: Option<CommitSha>,

    /// Rounds of the sync and partial scenarios, each running every tool once
    /// in a fresh random order. Scale and prepare run one pass. Rounds
    /// multiply the run time.
    #[arg(long, default_value_t = Rounds::DEFAULT)]
    pub(crate) rounds: Rounds,

    /// Cap on the images ocync transfers at once, written to the generated
    /// config as `global.max_concurrent_transfers` for `ocync` and the
    /// `--baseline-ref` build alike. Unset leaves ocync's own default.
    #[arg(long)]
    pub(crate) max_concurrent_transfers: Option<NonZeroUsize>,
}

impl ToolSelection {
    /// The tools a run executes: those named, and with a baseline both the
    /// baseline and the current ocync it is compared with.
    pub(crate) fn run_tools(&self) -> Vec<Tool> {
        let mut tools = self.tools.clone();
        if self.baseline_ref.is_some() {
            for tool in [Tool::Ocync, Tool::OcyncBaseline] {
                if !tools.contains(&tool) {
                    tools.push(tool);
                }
            }
        }
        tools
    }

    /// The directory a tool's binary is found under: the checkout of the
    /// baseline commit for the baseline, the workspace for every other tool.
    fn root(&self, tool: Tool) -> PathBuf {
        match &self.baseline_ref {
            Some(commit) if tool == Tool::OcyncBaseline => runner::baseline_dir(commit),
            _ => PathBuf::from("."),
        }
    }

    /// A tool's name where a person reads it.
    fn label(&self, tool: Tool) -> String {
        report::tool_label(tool, self.baseline_ref.as_ref())
    }

    /// Refuses a transfer cap when no ocync build runs to take it. Called
    /// beside [`Self::ensure_baseline_applies`], for the same two commands.
    pub(crate) fn ensure_cap_applies(&self) -> Result<(), String> {
        if self.max_concurrent_transfers.is_some() && !self.run_tools().contains(&Tool::Ocync) {
            return Err(
                "--max-concurrent-transfers applies to ocync, and this run builds none: \
                 add ocync to --tools or give --baseline-ref"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Refuses a baseline for a scenario that has no rounds to put it in. Clap
    /// cannot say this: the scenario is a subcommand of `bench` and an option
    /// of `bench-remote`, so both call this before they do anything.
    pub(crate) fn ensure_baseline_applies(&self, scenario: Scenario) -> Result<(), String> {
        match (&self.baseline_ref, scenario) {
            (Some(_), Scenario::Scale | Scenario::Prepare) => Err(format!(
                "--baseline-ref applies to the sync and partial scenarios, not {}",
                value_name(&scenario)
            )),
            _ => Ok(()),
        }
    }
}

/// Resolves a hex commit id, full or short, to the full commit it names here.
/// Anything else, a branch name included, is refused so that what a run
/// compares against cannot move.
fn parse_baseline_commit(s: &str) -> Result<CommitSha, String> {
    if !(4..=40).contains(&s.len()) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{s}` is not a hex commit id of 4 to 40 digits"));
    }
    remote::resolve_commit(Path::new("."), s).map_err(|e| e.to_string())
}

/// Arguments for the bench subcommand.
#[derive(Args)]
pub(crate) struct BenchArgs {
    #[command(subcommand)]
    pub(crate) scenario: Scenario,

    #[command(flatten)]
    pub(crate) tool_selection: ToolSelection,

    #[command(flatten)]
    pub(crate) paths: corpus::CorpusPaths,

    /// Registry the run syncs into. Artifact Registry and ACR read their
    /// registry, repositories or prefixes, and credentials from the
    /// environment, as `bench-remote` sets them.
    #[arg(long, value_enum, default_value = "ecr")]
    pub(crate) target: target::Kind,

    /// Path to the prepare-scenario corpus YAML.
    ///
    /// Separate from `--corpus` because the two measure opposite things: the
    /// main corpus is a handful of entries chosen for blob sharing and pinned
    /// to literal tags, which skips tag enumeration entirely.
    #[arg(long, default_value = "bench/corpus-prepare.yaml")]
    pub(crate) prepare_corpus: String,

    /// Use first N images only.
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Proxy listen port (default: 8080).
    #[arg(long, default_value = "8080")]
    pub(crate) proxy_port: u16,

    /// Path to the `bench-proxy` binary (produced by `cargo build
    /// --release --package bench-proxy`).
    #[arg(long, default_value = "target/release/bench-proxy")]
    pub(crate) proxy_binary: PathBuf,

    /// Path to the `bench-proxy` CA certificate PEM (must be trusted by
    /// client tools).
    #[arg(long, default_value = "/etc/bench-proxy/ca.pem")]
    pub(crate) proxy_ca: PathBuf,

    /// Path to the `bench-proxy` CA private key PEM.
    #[arg(long, default_value = "/etc/bench-proxy/ca-key.pem")]
    pub(crate) proxy_ca_key: PathBuf,

    /// Output directory for results (default: bench/results/{timestamp}).
    #[arg(long)]
    pub(crate) output: Option<String>,

    /// The run's start time, `YYYY-MM-DD-HHMMSS`: names the default output
    /// directory and is the run record's timestamp (default: now).
    /// `bench-remote` sets it so it knows the directory to fetch.
    #[arg(long)]
    pub(crate) run_timestamp: Option<RunTimestamp>,

    /// Where the run record and the performance-page blocks go: `write` puts
    /// them in the tracked archive and page, `defer` leaves both in the output
    /// directory for whoever fetches the run (`bench-remote` does).
    #[arg(long, value_enum, default_value = "write")]
    pub(crate) archive: report::ArchiveMode,

    /// Regression mode: compare against stored baseline, exit non-zero on regression.
    #[arg(long)]
    pub(crate) regression: bool,

    /// Regression threshold as a percentage (default: 20).
    #[arg(long, default_value = "20")]
    pub(crate) threshold_pct: u32,

    /// Comma-separated list of source registries to skip (e.g. `docker.io`).
    /// Images from these registries are excluded from the corpus.
    #[arg(long, value_delimiter = ',')]
    pub(crate) skip_registries: Vec<String>,

    /// Skip CDN pre-warming. Only use for single-tool runs where CDN
    /// fairness across tools is not a concern (e.g. ocync-only reruns).
    #[arg(long)]
    pub(crate) skip_prewarm: bool,

    /// Disable bench-proxy. Tools connect directly to registries.
    /// Disables traffic metrics, source error detection, and CDN stats.
    #[arg(long)]
    pub(crate) no_proxy: bool,
}

/// Benchmark scenario to run.
#[derive(Subcommand, ValueEnum, Debug, Clone, Copy)]
pub(crate) enum Scenario {
    /// Full sync (cold) then immediate re-sync (warm).
    Sync,
    /// Re-sync after ~5% of tags changed -- measures incremental sync.
    Partial,
    /// Run at increasing corpus sizes -- measures scaling behavior (ocync only).
    Scale,
    /// Resolve every mapping and stop -- measures the prepare phase (ocync only).
    Prepare,
    /// Run all scenarios in sequence.
    All,
}

/// Runs `command` and returns its stdout. A failure is why it could not run,
/// or what it printed on stderr (its exit status when that is empty).
pub(crate) fn captured(command: &mut std::process::Command) -> Result<String, String> {
    let output = command.output().map_err(|e| {
        format!(
            "could not run {}: {e}",
            command.get_program().to_string_lossy()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(match stderr.trim() {
            "" => output.status.to_string(),
            stderr => stderr.to_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|e| format!("printed non-UTF-8: {e}"))
}

/// The trimmed stdout of `command`, or `None` when it cannot be run, exits
/// non-zero, prints non-UTF-8, or prints nothing.
pub(crate) fn command_stdout(command: &mut std::process::Command) -> Option<String> {
    let text = captured(command).ok()?;
    Some(text.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Read an SSM parameter by name, returning `None` on any failure.
fn read_ssm_parameter(name: &str) -> Option<String> {
    command_stdout(std::process::Command::new("aws").args([
        "ssm",
        "get-parameter",
        "--name",
        name,
        "--with-decryption",
        "--query",
        "Parameter.Value",
        "--output",
        "text",
    ]))
}

/// Read Docker Hub credentials from environment variables.
///
/// Set by cloud-init on bench instances. Returns `None` if either
/// variable is missing or empty.
fn docker_hub_auth_from_env() -> Option<corpus::DockerHubAuth> {
    let username = std::env::var("DOCKERHUB_USERNAME")
        .ok()
        .filter(|s| !s.is_empty())?;
    let token = std::env::var("DOCKERHUB_ACCESS_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(corpus::DockerHubAuth { username, token })
}

/// Docker Hub credentials from env vars (cloud-init) or SSM (local dev), or
/// `None` for anonymous pulls.
///
/// The SSM fallback needs IAM permissions the bench instance's Terraform does
/// not grant; it serves local dev with personal credentials.
fn docker_hub_auth() -> Option<corpus::DockerHubAuth> {
    if let Some(auth) = docker_hub_auth_from_env() {
        eprintln!(
            "bench: Docker Hub auth configured (username={})",
            auth.username
        );
        return Some(auth);
    }
    if let Some(token) = read_ssm_parameter("/ocync/bench/dockerhub-access-token") {
        let username = read_ssm_parameter("/ocync/bench/dockerhub-username")
            .unwrap_or_else(|| "ocync-bench".into());
        eprintln!("bench: Docker Hub auth configured via SSM (username={username})");
        return Some(corpus::DockerHubAuth { username, token });
    }
    eprintln!("bench: Docker Hub auth not available, using anonymous pulls");
    None
}

/// Write a progress line to stderr.
fn progress(msg: &str) {
    eprintln!("{msg}");
}

/// Refuse to run benchmarks when `TMPDIR` (or the system default) is on tmpfs.
///
/// Staging writes multi-GB blobs to `$TMPDIR`. On Amazon Linux 2023, `/tmp`
/// is a tmpfs mount (RAM-backed), causing `ENOSPC` when the corpus exceeds
/// available memory. The `bench-remote` xtask sets `TMPDIR=$HOME/ocync/bench/.tmp`
/// (on EBS) before invoking `cargo xtask bench`. This guard catches manual
/// invocations that skip that setup.
fn reject_tmpfs_tmpdir() -> Result<(), Box<dyn std::error::Error>> {
    let tmpdir = std::env::temp_dir();
    let tmpdir_str = tmpdir.to_string_lossy();

    // Parse /proc/mounts to find the filesystem type for TMPDIR.
    // Only exists on Linux; skip the check on other platforms.
    let mounts = match std::fs::read_to_string("/proc/mounts") {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    // Find the mount with the longest matching prefix for TMPDIR.
    let mut best_match: Option<(&str, &str)> = None;
    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let mount_point = parts[1];
        let fs_type = parts[2];
        if tmpdir_str.starts_with(mount_point) {
            let is_longer = best_match
                .map(|(prev, _)| mount_point.len() > prev.len())
                .unwrap_or(true);
            if is_longer {
                best_match = Some((mount_point, fs_type));
            }
        }
    }

    if let Some((mount_point, "tmpfs")) = best_match {
        return Err(format!(
            "TMPDIR ({tmpdir_str}) is on tmpfs (mount: {mount_point}). \
             Staging writes multi-GB blobs and will hit ENOSPC. \
             Set TMPDIR to a path on a real filesystem, or use \
             `cargo xtask bench-remote` which handles this automatically.",
        )
        .into());
    }

    Ok(())
}

/// The spelling clap accepts for `value`.
pub(crate) fn value_name<T: ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .expect("no variant is skipped")
        .get_name()
        .to_owned()
}

/// A cloud: the one a bench stack is deployed in, or the one a registry
/// belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Provider {
    /// Amazon Web Services.
    Aws,
    /// Google Cloud.
    Gcp,
    /// Microsoft Azure.
    Azure,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&value_name(self))
    }
}

/// The registry service a run's target registry hostname belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistryKind {
    /// Amazon ECR.
    Ecr,
    /// Google Container Registry or Artifact Registry.
    Gcr,
    /// Azure Container Registry.
    Acr,
    /// Any other registry.
    Other,
}

impl RegistryKind {
    /// Classifies a target registry hostname.
    fn of(target_registry: &str) -> Self {
        let host = target_registry.to_ascii_lowercase();
        let is_ecr = [".dkr.ecr.", ".dkr.ecr-fips."]
            .iter()
            .any(|infix| host.contains(infix))
            && [".amazonaws.com", ".amazonaws.com.cn"]
                .iter()
                .any(|suffix| host.ends_with(suffix));
        if is_ecr {
            Self::Ecr
        } else if host == "gcr.io" || host.ends_with(".gcr.io") || host.ends_with("-docker.pkg.dev")
        {
            Self::Gcr
        } else if [".azurecr.io", ".azurecr.us", ".azurecr.cn"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        {
            Self::Acr
        } else {
            Self::Other
        }
    }

    /// Stem of the JSON archive file that holds this registry's runs.
    fn archive_stem(self) -> &'static str {
        match self {
            Self::Ecr => "ecr",
            Self::Gcr => "gcr",
            Self::Acr => "acr",
            Self::Other => "other",
        }
    }
}

/// Returns the short git commit hash of HEAD.
///
/// This is the record's only anchor from numbers back to the code that
/// produced them, so a failure is announced rather than silently recorded as
/// `unknown`. The run log is streamed to the operator and never persisted, so
/// this warning is the only chance to notice.
fn git_ref() -> String {
    command_stdout(std::process::Command::new("git").args(["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| {
            eprintln!(
                "WARNING: could not resolve HEAD, so this run cannot be tied to a commit. \
             Recording the git ref as \"unknown\"."
            );
            "unknown".into()
        })
}

/// Run the benchmark suite.
pub(crate) async fn run(args: BenchArgs) -> Result<(), Box<dyn std::error::Error>> {
    args.tool_selection.ensure_baseline_applies(args.scenario)?;
    args.tool_selection.ensure_cap_applies()?;
    let bench_target = Target::from_env(
        args.target,
        &|name| std::env::var(name).ok(),
        args.scenario,
        &args.tool_selection.run_tools(),
        args.regression,
        args.tool_selection.rounds,
    )?;

    // The prepare scenario is exempt because it stages nothing: `--dry-run`
    // resolves mappings and stops, so there is no blob to write anywhere.
    // Applying the guard to it would refuse the one scenario cheap enough to
    // run on a laptop, for a disk cost it does not have.
    if !matches!(args.scenario, Scenario::Prepare) {
        reject_tmpfs_tmpdir()?;
    }

    let loaded = corpus::load(args.paths.corpus())?;
    let filtered = if args.skip_registries.is_empty() {
        loaded
    } else {
        let skipped = loaded.skip_registries(&args.skip_registries);
        let removed = loaded.images.len() - skipped.images.len();
        eprintln!(
            "bench: skipping {} images from registries: {}",
            removed,
            args.skip_registries.join(", ")
        );
        skipped
    };
    let mut corpus = match args.limit {
        Some(n) => filtered.limit(n),
        None => filtered,
    };

    corpus.dockerhub_auth = docker_hub_auth();

    // The lock is read only when a scenario syncs from it: prepare reads its own
    // wildcard corpus and transfers nothing, so it needs no lock.
    let transfers = plan(
        args.scenario,
        &corpus,
        args.paths.partial_overrides(),
        args.tool_selection.rounds,
    )?;
    let prepare = matches!(args.scenario, Scenario::Prepare | Scenario::All);
    let lock = (!transfers.is_empty())
        .then(|| lock::Lock::load(&args.paths.lock_path()))
        .transpose()?;
    let corpus_locked_at = lock.as_ref().map(|lock| lock.locked_at.clone());
    ensure_baseline_comparable(
        &args,
        &transfers,
        &corpus,
        corpus_locked_at.as_ref(),
        &baseline_path(),
    )?;
    let http = registry::http_client()?;
    let auth = corpus.dockerhub_auth.as_ref();
    let refs: BTreeSet<_> = transfers
        .iter()
        .flat_map(|transfer| scenario_refs(transfer, &corpus))
        .collect();
    if let Some(lock) = &lock {
        // Before the build and the CDN pre-warm, so a digest the source no
        // longer serves costs seconds rather than a wasted run.
        lock::ensure_served(&http, lock, &refs, auth).await?;
    }

    let tools = args.tool_selection.run_tools();
    let tools = &tools;

    let labels: Vec<String> = tools
        .iter()
        .map(|&t| args.tool_selection.label(t))
        .collect();
    eprintln!(
        "bench: scenario={} tools={} images={} tags={}",
        value_name(&args.scenario),
        labels.join(","),
        corpus.images.len(),
        corpus.total_tags(),
    );

    let store = Store::open(&bench_target).await?;

    // Build ocync first, so its version probe reads the binary the run will
    // actually execute rather than whatever is on PATH from instance setup.
    let workspace_root = Path::new(".");
    if tools.contains(&Tool::Ocync) {
        eprintln!("bench: building ocync in release mode...");
        runner::build_ocync(workspace_root).await?;
    }

    // The baseline is built and tried on the config and the flags before the
    // pre-warm, so a commit that does not build, or that rejects what the
    // rounds give it, costs minutes and not a run.
    if let Some(commit) = &args.tool_selection.baseline_ref {
        eprintln!("bench: building the baseline ocync at {commit}...");
        runner::build_baseline(commit).await?;
        let config_dir = tempfile::tempdir()?;
        let config = config_dir.path().join("baseline-check.yaml");
        std::fs::write(
            &config,
            config_gen::ocync_config(
                &corpus,
                args.target,
                args.tool_selection.max_concurrent_transfers,
                config_gen::Mode::Sync(
                    lock.as_ref().expect(
                        "--baseline-ref is refused for prepare, the only run without a lock",
                    ),
                ),
            ),
        )
        .map_err(|e| format!("write config {}: {e}", config.display()))?;
        runner::check_baseline(commit, &config)?;
    }

    // Check each tool binary and collect versions, along with any binary a tool
    // relays its transfers through, whose version moves that tool's numbers.
    let mut tool_versions: BTreeMap<Tool, String> = BTreeMap::new();
    let mut relay_versions: BTreeMap<String, String> = BTreeMap::new();
    for &tool in tools {
        let mut version = runner::check_tool(tool, &args.tool_selection.root(tool)).await?;
        // Two ocync builds can report one version, so the baseline's names the
        // commit it was built from.
        if let (Tool::OcyncBaseline, Some(commit)) = (tool, &args.tool_selection.baseline_ref) {
            version = format!("{version} (commit {commit})");
        }
        eprintln!("  {}: {version}", args.tool_selection.label(tool));
        tool_versions.insert(tool, version);
    }

    // Relays are shared between tools, so they are resolved once over the whole
    // set rather than per tool.
    for (binary, version) in runner::check_relays(tools).await? {
        eprintln!("  {binary} (relay): {version}");
        relay_versions.insert(binary, version);
    }

    // One value names the results directory and stamps the record.
    let timestamp = args.run_timestamp.clone().unwrap_or_else(RunTimestamp::now);
    let output_dir = match &args.output {
        Some(dir) => PathBuf::from(dir),
        None => timestamp.results_dir(),
    };

    let instance = report::InstanceInfo::collect();
    eprintln!(
        "bench: instance={} cpu={} vcpus={} mem={}MiB net={} region={}",
        instance.instance_type,
        instance.cpu_manufacturer,
        instance.vcpus,
        instance.memory_mib,
        instance.network_performance,
        instance.region,
    );

    // Pre-warm CDN so all tools see identically cached source blobs.
    //
    // The prepare scenario needs none of it. It runs one tool, pulls no blobs,
    // and reads a different corpus than the one warmed here, so warming for it
    // is hundreds of pointless requests against registries that rate-limit.
    // A mixed run still warms, because the scenarios beside it do transfer.
    let prewarm = if args.skip_prewarm {
        eprintln!("bench: skipping CDN pre-warm (--skip-prewarm)");
        (!transfers.is_empty()).then_some(Prewarm::Skipped)
    } else if let Some(lock) = &lock {
        Some(cdn_prewarm(&http, auth, lock, &refs).await)
    } else {
        eprintln!("bench: skipping CDN pre-warm (prepare transfers nothing)");
        None
    };

    let mut repeated = Vec::new();
    for transfer in &transfers {
        let kind = match transfer {
            Transfer::Sync { .. } => ScenarioKind::Cold,
            Transfer::Partial { .. } => ScenarioKind::Partial,
            Transfer::Scale => continue,
        };
        if !repeated.contains(&kind) {
            repeated.push(kind);
        }
    }
    let mut report = BenchReport {
        timestamp: timestamp.to_string(),
        instance,
        corpus_size: corpus.images.len(),
        total_tags: corpus.total_tags(),
        tool_versions,
        relay_versions,
        prewarm,
        baseline: args.tool_selection.baseline_ref.clone(),
        max_concurrent_transfers: args.tool_selection.max_concurrent_transfers,
        rounds: args.tool_selection.rounds,
        repeated,
        scenarios: Vec::new(),
    };

    // Every tool copies the locked digests, so a source tag that moves during a
    // long run cannot put different content under later numbers. Any failure
    // stops the passes still to run; the finished ones are still reported and
    // recorded, and the run then returns the reason.
    let mut stopped: Option<String> = None;
    if let Some(lock) = &lock {
        let (finished, stop) = run_passes(
            &transfers,
            tools,
            fresh_seed,
            |tool| args.tool_selection.label(tool),
            async |transfer, order| {
                let target = Destination {
                    store: &store,
                    lock,
                };
                let outcome: Result<Finished, Box<dyn std::error::Error>> = async {
                    Ok(match transfer {
                        Transfer::Sync { round } => {
                            let logs = round_dir(&output_dir, *round);
                            let round_corpus = bench_target.round_corpus(&corpus, *round);
                            let (cold, warm) =
                                run_sync(&args, &round_corpus, &target, order, &logs).await?;
                            vec![(ScenarioKind::Cold, cold), (ScenarioKind::Warm, warm)]
                        }
                        Transfer::Partial {
                            round,
                            corpus: partial_corpus,
                        } => {
                            let logs = round_dir(&output_dir, *round);
                            let runs =
                                run_partial(&args, &corpus, partial_corpus, &target, order, &logs)
                                    .await?;
                            vec![(ScenarioKind::Partial, runs)]
                        }
                        Transfer::Scale if tools.contains(&Tool::Ocync) => {
                            // Scale scenario uses ocync only per spec.
                            run_scale(&args, &corpus, &target, &[Tool::Ocync], &output_dir).await?
                        }
                        Transfer::Scale => {
                            eprintln!("bench: scale scenario requires ocync (skipping)");
                            Vec::new()
                        }
                    })
                }
                .await;
                match outcome {
                    Ok(finished) => Ok(finished),
                    Err(error) => {
                        let refs = scenario_refs(transfer, &corpus);
                        let recheck = lock::ensure_served(&http, lock, &refs, auth).await;
                        Err(lock::explain(error.to_string(), recheck))
                    }
                }
            },
        )
        .await;
        report.scenarios.extend(finished);
        stopped = stop;
    }

    if prepare && stopped.is_none() {
        // ocync only: no competitor has a mode that resolves tags and
        // stops, so there is nothing to compare against. Same reason
        // the scale scenario runs alone.
        if tools.contains(&Tool::Ocync) {
            match run_prepare(&args, &output_dir).await {
                Ok(result) => report.scenarios.push(result),
                Err(error) => stopped = Some(error.to_string()),
            }
        } else {
            eprintln!("bench: prepare scenario requires ocync (skipping)");
        }
    }

    // Nothing finished: there is nothing to report or record.
    if report.scenarios.is_empty() {
        if let Some(reason) = stopped {
            return Err(reason.into());
        }
    }

    report::write_report(&output_dir, &report)
        .map_err(|e| with_stop_reason(e, stopped.as_deref()))?;
    eprintln!("bench: summary written to {}", output_dir.display());

    let deferred = deferred_output(
        &bench_target,
        &corpus.settings.target_registry,
        &report,
        git_ref(),
        corpus_locked_at.clone(),
    );
    match args.archive {
        // The step a fetch of a deferred run takes too.
        report::ArchiveMode::Write => deferred
            .apply(
                Path::new(report::ARCHIVE_DIR),
                Path::new(report::PERFORMANCE_PAGE),
            )
            .map_err(|e| with_stop_reason(e, stopped.as_deref()))?,
        report::ArchiveMode::Defer => {
            deferred
                .write(&output_dir)
                .map_err(|e| with_stop_reason(e, stopped.as_deref()))?;
            eprintln!(
                "bench: run record and page blocks left in {} for the fetch to apply",
                output_dir.display()
            );
        }
    }

    if let Some(reason) = stopped {
        return Err(format!(
            "stopped before the remaining scenarios; those that finished are reported and recorded: {reason}"
        )
        .into());
    }

    if args.regression {
        check_regression(&report, args.threshold_pct, corpus_locked_at.as_ref())?;
    }

    Ok(())
}

/// What a finished run leaves for the archive and the performance page.
///
/// The machine's cloud is where the benchmark ran, whichever cloud the target
/// registry is in. The page compares ECR runs, so a run into another registry
/// writes no block to it.
fn deferred_output(
    target: &Target,
    target_registry: &str,
    report: &BenchReport,
    git_ref: String,
    corpus_locked_at: Option<lock::LockedAt>,
) -> report::Deferred {
    let record = report::RunRecord::of(report, git_ref, corpus_locked_at);
    // The archive is tracked in git and read as a history of comparable runs.
    // A run whose machine could not be identified is not comparable to one on
    // a known instance, and the harness already says so with a WARNING per
    // missing field. Appending it anyway puts a laptop's numbers next to the
    // rig's under the same key, where nothing downstream can tell them apart.
    // The summary is still written; only the shared history is protected.
    let record = if record.machine.provider.is_none() {
        eprintln!(
            "bench: run record NOT appended (instance metadata unknown, so this \
             run is not comparable to the archived ones); summary only"
        );
        None
    } else {
        Some(record)
    };
    let page = match target {
        Target::Ecr => report::PageBlocks::of(report),
        Target::ArtifactRegistry(_) | Target::Acr(_) => report::PageBlocks::default(),
    };
    report::Deferred::new(
        RegistryKind::of(target_registry).archive_stem(),
        record,
        page,
    )
}

/// The scenarios a pass finished, with each one's runs.
type Finished = Vec<(ScenarioKind, Vec<ToolRun>)>;

/// Runs the passes in order, and returns those finished with why the run
/// stopped, if it did. A failed pass keeps every pass before it.
///
/// Every round gets its own tool order, drawn from `next_seed` as the round
/// starts, so no tool is systematically first or last, and each result records
/// the round, seed and order it ran with. `pass` is given the transfer and the
/// order, and returns the reason to stop when it fails.
async fn run_passes(
    transfers: &[Transfer],
    tools: &[Tool],
    mut next_seed: impl FnMut() -> u64,
    label: impl Fn(Tool) -> String,
    mut pass: impl AsyncFnMut(&Transfer, &[Tool]) -> Result<Finished, String>,
) -> (Vec<ScenarioResult>, Option<String>) {
    let mut done = Vec::new();
    for transfer in transfers {
        let mut order = tools.to_vec();
        let mut seed = 0;
        if transfer.round().is_some() && tools.len() > 1 {
            seed = next_seed();
            shuffle_tools(&mut order, seed);
            let names: Vec<String> = order.iter().map(|&t| label(t)).collect();
            eprintln!(
                "bench: tool order for {transfer}: {} (seed: {seed})",
                names.join(", ")
            );
        }
        match pass(transfer, &order).await {
            Ok(finished) => {
                done.extend(
                    finished
                        .into_iter()
                        .map(|(kind, runs)| match transfer.round() {
                            Some(round) => ScenarioResult {
                                kind,
                                round: Some(round),
                                runs,
                                shuffle_seed: seed,
                                tool_order: order.clone(),
                            },
                            None => ScenarioResult::unshuffled(kind, runs),
                        }),
                )
            }
            Err(reason) => return (done, Some(reason)),
        }
    }
    (done, None)
}

/// A pass of a scenario that syncs the main corpus, and so is checked against
/// its lock: one round of a repeated scenario, or the single pass of scale.
#[derive(Debug)]
enum Transfer {
    /// Full sync, then re-sync, in this round from 1.
    Sync {
        /// The round.
        round: usize,
    },
    /// Incremental sync after some tags changed, in this round from 1.
    Partial {
        /// The round.
        round: usize,
        /// The base corpus with the override tags applied, shared by the rounds.
        corpus: Rc<Corpus>,
    },
    /// Sync at increasing corpus sizes.
    Scale,
}

impl Transfer {
    /// The round of a repeated scenario this is; `None` for scale's one pass.
    fn round(&self) -> Option<usize> {
        match self {
            Self::Sync { round } | Self::Partial { round, .. } => Some(*round),
            Self::Scale => None,
        }
    }
}

impl std::fmt::Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sync { round } => write!(f, "Sync round {round}"),
            Self::Partial { round, .. } => write!(f, "Partial round {round}"),
            Self::Scale => f.write_str("Scale"),
        }
    }
}

/// The passes a run executes, in order, that sync from the lock: every round of
/// the sync scenario, then of the partial one, then scale once. The overrides
/// are read once, and only for a run that has a partial scenario.
fn plan(
    requested: Scenario,
    corpus: &Corpus,
    overrides_path: &str,
    rounds: Rounds,
) -> Result<Vec<Transfer>, String> {
    let rounds = 1..=rounds.get();
    let sync = || rounds.clone().map(|round| Transfer::Sync { round });
    let partial = || {
        let overrides = corpus::load_overrides(overrides_path)?;
        let changed = Rc::new(corpus.with_overrides(&overrides.overrides));
        Ok::<_, String>(rounds.clone().map(move |round| Transfer::Partial {
            round,
            corpus: Rc::clone(&changed),
        }))
    };
    Ok(match requested {
        Scenario::Sync => sync().collect(),
        Scenario::Partial => partial()?.collect(),
        Scenario::Scale => vec![Transfer::Scale],
        Scenario::Prepare => Vec::new(),
        Scenario::All => sync().chain(partial()?).chain([Transfer::Scale]).collect(),
    })
}

/// Every (source, tag) a scenario syncs.
fn scenario_refs(transfer: &Transfer, corpus: &Corpus) -> BTreeSet<lock::ImageRef> {
    let mut refs = lock::refs_of(&corpus.images);
    if let Transfer::Partial {
        corpus: partial, ..
    } = transfer
    {
        refs.extend(lock::refs_of(&partial.images));
    }
    refs
}

/// `error`, plus why the run stopped early if it did.
fn with_stop_reason(error: String, stopped: Option<&str>) -> String {
    stopped.map_or_else(
        || error.clone(),
        |reason| format!("{error}; also the run stopped early: {reason}"),
    )
}

/// Combine a pass's result with its cleanup's, keeping both messages: a repo
/// left behind is named even when the pass had already failed.
fn finish_pass(
    pass: Result<(), Box<dyn std::error::Error>>,
    cleanup: Result<(), String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match (pass, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
        (Err(error), Err(cleanup)) => Err(format!("{error}; also {cleanup}").into()),
    }
}

/// The ocync runs of the cold sync scenario, one per round, the only runs a
/// baseline is comparable against.
fn cold_ocync_runs(scenarios: &[ScenarioResult]) -> Vec<&ToolRun> {
    rounds_of(scenarios, ScenarioKind::Cold)
        .into_iter()
        .filter_map(|round| round.runs.iter().find(|r| r.tool == Tool::Ocync))
        .collect()
}

/// The figures a stored baseline is compared on: the median over the rounds of
/// the ocync run of the cold sync scenario, never a scale or prepare run.
/// `None` when the report has no such run, or a round of it has no proxy
/// metrics.
fn cold_ocync_median(
    scenarios: &[ScenarioResult],
    corpus: report::CorpusInfo,
    max_concurrent_transfers: Option<NonZeroUsize>,
    corpus_locked_at: Option<&lock::LockedAt>,
) -> Option<Baseline> {
    let runs = cold_ocync_runs(scenarios);
    if runs.is_empty() {
        return None;
    }
    // A run with no value, as when the proxy was off, leaves the whole
    // comparison out rather than counting as zero.
    let median = |value: fn(&ToolRun) -> Option<f64>| -> Option<f64> {
        let values: Vec<f64> = runs.iter().map(|run| value(run)).collect::<Option<_>>()?;
        Some(stats::spread(&values)?.median)
    };
    Some(Baseline {
        wall_clock_secs: median(|r| Some(r.wall_clock_secs))?,
        requests: median(|r| r.proxy_metrics.as_ref().map(|m| m.requests as f64))?.round() as u64,
        total_bytes: median(|r| r.proxy_metrics.as_ref().map(|m| m.response_bytes as f64))?.round()
            as u64,
        corpus_locked_at: corpus_locked_at.cloned(),
        sync_from: lock::SyncFrom::of(corpus_locked_at),
        corpus,
        max_concurrent_transfers,
        rounds: u32::try_from(runs.len()).expect("a run has fewer than 2^32 rounds"),
    })
}

/// Refuses a `--regression` whose stored baseline the run could not be compared
/// with, before any round runs rather than after the last. The comparison is
/// made over the cold sync rounds of ocync and needs the proxy's metrics, so a
/// run without any of them is left alone, as the comparison skips it.
fn ensure_baseline_comparable(
    args: &BenchArgs,
    transfers: &[Transfer],
    corpus: &Corpus,
    corpus_locked_at: Option<&lock::LockedAt>,
    baseline_path: &Path,
) -> Result<(), String> {
    let cold_rounds = transfers
        .iter()
        .filter(|transfer| matches!(transfer, Transfer::Sync { .. }))
        .count();
    if !args.regression
        || args.no_proxy
        || cold_rounds == 0
        || !args.tool_selection.run_tools().contains(&Tool::Ocync)
    {
        return Ok(());
    }
    let key = Baseline {
        wall_clock_secs: 0.0,
        requests: 0,
        total_bytes: 0,
        corpus_locked_at: corpus_locked_at.cloned(),
        sync_from: lock::SyncFrom::of(corpus_locked_at),
        corpus: report::CorpusInfo {
            images: corpus.images.len(),
            tags: corpus.total_tags(),
        },
        max_concurrent_transfers: Some(report::effective_transfer_cap(
            args.tool_selection.max_concurrent_transfers,
        )),
        rounds: u32::try_from(cold_rounds).expect("a run has fewer than 2^32 rounds"),
    };
    regression::ensure_baseline_comparable(baseline_path, &key)
}

/// Where the stored baseline of `--regression` lives.
fn baseline_path() -> PathBuf {
    PathBuf::from("bench/results/baseline.json")
}

/// Compare the current ocync run against a stored baseline.
///
/// Returns an error if regression is detected.
fn check_regression(
    report: &BenchReport,
    threshold: u32,
    corpus_locked_at: Option<&lock::LockedAt>,
) -> Result<(), Box<dyn std::error::Error>> {
    let baseline_path = baseline_path();

    let corpus = report::CorpusInfo {
        images: report.corpus_size,
        tags: report.total_tags,
    };
    if let Some(current) = cold_ocync_median(
        &report.scenarios,
        corpus,
        report.ocync_transfer_cap(),
        corpus_locked_at,
    ) {
        if baseline_path.exists() {
            let baseline = regression::load_baseline(&baseline_path)?;
            let result = regression::compare(&current, &baseline, threshold, &baseline_path)?;
            eprintln!("bench: regression check:");
            eprintln!("{}", regression::format_result(&result, threshold));
            if result.regressed {
                return Err("regression detected".into());
            }
        } else {
            eprintln!(
                "bench: no baseline found at {}, saving current as baseline",
                baseline_path.display()
            );
            if let Some(parent) = baseline_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create baseline dir {}: {e}", parent.display()))?;
            }
            regression::save_baseline(&baseline_path, &current)?;
        }
    } else {
        eprintln!("bench: no cold sync ocync run with proxy metrics; regression check skipped");
    }

    Ok(())
}

/// Which run of a scenario a tool process is, named in its proxy log file.
/// Each run needs its own, or a later one truncates the earlier one's
/// `{tool}-{pass}-proxy.jsonl`.
#[derive(Clone, Copy, Debug)]
enum Pass {
    /// The unmeasured sync that populates the target before a partial run.
    Prime,
    /// The measured run of a scenario.
    Measured(ScenarioKind),
}

impl std::fmt::Display for Pass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prime => f.write_str("prime"),
            Self::Measured(kind) => f.write_str(&kind.slug()),
        }
    }
}

/// Where a tool's proxy log for a pass goes. The directory is the round's, so
/// no round truncates another's; the tool's name, never its binary's, so the
/// baseline cannot truncate ocync's.
fn proxy_log(output_dir: &Path, tool: Tool, pass: Pass) -> PathBuf {
    output_dir.join(format!("{tool}-{pass}-proxy.jsonl"))
}

/// The directory a round's proxy logs go in.
fn round_dir(output_dir: &Path, round: usize) -> PathBuf {
    output_dir.join(format!("round-{round}"))
}

/// Run a single tool against the corpus and return a `ToolRun`.
async fn run_single_tool(
    args: &BenchArgs,
    tool: Tool,
    corpus: &Corpus,
    config_dir: &Path,
    output_dir: &Path,
    pass: Pass,
    mode: config_gen::Mode<'_>,
) -> Result<ToolRun, Box<dyn std::error::Error>> {
    let config_content = match (tool, mode) {
        (Tool::Ocync | Tool::OcyncBaseline, mode) => config_gen::ocync_config(
            corpus,
            args.target,
            args.tool_selection.max_concurrent_transfers,
            mode,
        ),
        (Tool::Dregsy, config_gen::Mode::Sync(lock)) => {
            let skopeo = skopeo_shim::write_script(config_dir)?;
            config_gen::dregsy_config(corpus, lock, &skopeo)
        }
        (Tool::Regsync, config_gen::Mode::Sync(lock)) => config_gen::regsync_config(corpus, lock),
        (Tool::Dregsy | Tool::Regsync, config_gen::Mode::Prepare) => {
            return Err(
                format!("{tool} syncs from the corpus lock and has no prepare mode").into(),
            );
        }
    };

    let config_path = config_dir.join(format!("{}.yaml", tool));
    std::fs::write(&config_path, &config_content)
        .map_err(|e| format!("write config {}: {e}", config_path.display()))?;

    // The proxy log lives under `output_dir`, not the auto-cleaned tempdir, so
    // post-run analysis (`grep mount= …proxy.jsonl`) can inspect exactly which
    // API calls the tool issued.
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("create output dir {}: {e}", output_dir.display()))?;
    let (proxy_handle, proxy_url) = if !args.no_proxy {
        let log_path = proxy_log(output_dir, tool, pass);
        let handle = proxy::start(
            &args.proxy_binary,
            &args.proxy_ca,
            &args.proxy_ca_key,
            &log_path,
            args.proxy_port,
        )
        .await?;
        let url = handle.proxy_url();
        (Some(handle), Some(url))
    } else {
        eprintln!("bench: proxy disabled -- no traffic metrics");
        (None, None)
    };

    // The bench-proxy CA must be installed in the system trust store (via
    // update-ca-trust on AL2023) so that rustls-native-certs and OpenSSL
    // both trust the MITM'd connections. HTTPS_PROXY alone is sufficient.
    let label = args.tool_selection.label(tool);
    let result = runner::run_tool(
        tool,
        &label,
        &config_path,
        proxy_url.as_deref(),
        &args.tool_selection.root(tool),
        // Prepare resolves mappings and stops, so it is the only dry run.
        matches!(mode, config_gen::Mode::Prepare),
    )
    .await?;

    let proxy_metrics = if let Some(handle) = proxy_handle {
        let entries = proxy::stop(handle).await?;
        check_source_errors(&entries, &corpus.settings.target_registry)?;
        Some(proxy::aggregate(&entries, &corpus.settings.target_registry))
    } else {
        None
    };

    let rss_display = result
        .peak_rss_kb
        .map(|kb| report::format_bytes(kb * 1024))
        .unwrap_or_else(|| "n/a".into());
    if let Some(ref metrics) = proxy_metrics {
        eprintln!(
            "  {label}: wall_clock={:.1}s status={} rss={} requests={} response_bytes={} mounts={}/{} source_pulls={}/{}",
            result.wall_clock.as_secs_f64(),
            result.exit_status,
            rss_display,
            metrics.requests,
            report::format_bytes(metrics.response_bytes),
            metrics.mount_successes,
            metrics.mount_attempts,
            metrics.source_blob_gets,
            report::format_bytes(metrics.source_blob_bytes),
        );
    } else {
        eprintln!(
            "  {label}: wall_clock={:.1}s status={} rss={}",
            result.wall_clock.as_secs_f64(),
            result.exit_status,
            rss_display,
        );
    }

    // Read phase timing from ocync's OCYNC_TIMING_FILE if present.
    let phase_timing_ms = if let Some(ref tp) = result.timing_path {
        parse_timing_file(tp)
    } else {
        BTreeMap::new()
    };

    // Checked last so a source-registry error, which names the cause, is
    // reported first.
    ensure_exited_zero(&label, pass, result.exit_status)?;

    Ok(ToolRun {
        tool,
        wall_clock_secs: result.wall_clock.as_secs_f64(),
        peak_rss_kb: result.peak_rss_kb,
        proxy_metrics,
        phase_timing_ms,
    })
}

/// Fails unless a tool pass exited 0. A pass that failed did less work, so its
/// numbers cannot be compared; for `--dry-run` that includes a mapping that
/// could not be resolved.
fn ensure_exited_zero(tool: &str, pass: Pass, status: ExitStatus) -> Result<(), String> {
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "{tool} {pass} failed ({status}); aborting, a failed run cannot be compared"
    ))
}

/// Abort on source registry errors (status >= 400) that indicate tainted
/// benchmark data. Excludes 401 (expected OCI auth challenge) and 404
/// (expected blob existence checks).
fn check_source_errors(
    entries: &[proxy::ProxyEntry],
    target_registry: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let source_errors: BTreeMap<&str, Vec<u16>> = entries
        .iter()
        .filter(|e| {
            e.host != target_registry && e.status >= 400 && e.status != 401 && e.status != 404
        })
        .fold(BTreeMap::new(), |mut acc, e| {
            acc.entry(e.host.as_str()).or_default().push(e.status);
            acc
        });
    if source_errors.is_empty() {
        return Ok(());
    }
    let summary: Vec<String> = source_errors
        .iter()
        .map(|(host, codes)| {
            let unique: Vec<u16> = codes
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let code_str: Vec<String> = unique.iter().map(|c| c.to_string()).collect();
            format!("{} ({}x: {})", host, codes.len(), code_str.join("/"))
        })
        .collect();
    let hosts: Vec<&str> = source_errors.keys().copied().collect();
    Err(format!(
        "aborting: source registry errors from: {}. \
         Use --skip-registries {} to exclude, or retry later.",
        summary.join(", "),
        hosts.join(","),
    )
    .into())
}

/// One round of the sync scenario: for each tool in order, a full sync
/// (measured), then an immediate re-sync (measured) with the same config
/// directory so that ocync's `TransferStateCache` persists between runs.
/// Returns the cold and the warm runs.
async fn run_sync(
    args: &BenchArgs,
    corpus: &Corpus,
    target: &Destination<'_>,
    tools: &[Tool],
    output_dir: &Path,
) -> Result<(Vec<ToolRun>, Vec<ToolRun>), Box<dyn std::error::Error>> {
    progress("bench: running cold+warm scenario");
    let mut cold_runs = Vec::new();
    let mut warm_runs = Vec::new();

    for &tool in tools {
        let label = args.tool_selection.label(tool);
        prepare_clean_slate();

        target.reset(corpus).await?;

        // Single config_dir for cold + warm. The cache file lives
        // under this directory, so reusing it means the warm run
        // sees a fully populated cache.
        let config_dir = tempfile::tempdir()?;

        progress(&format!("  cold: {label}"));
        let run_result: Result<(), Box<dyn std::error::Error>> = async {
            let cold = run_single_tool(
                args,
                tool,
                corpus,
                config_dir.path(),
                output_dir,
                Pass::Measured(ScenarioKind::Cold),
                config_gen::Mode::Sync(target.lock),
            )
            .await?;
            progress(&format!(
                "  cold: {label} complete ({:.1}s)",
                cold.wall_clock_secs
            ));
            cold_runs.push(cold);

            target::verify_sync(target, corpus, &label, ScenarioKind::Cold).await?;

            progress(&format!("  warm: {label}"));
            let warm = run_single_tool(
                args,
                tool,
                corpus,
                config_dir.path(),
                output_dir,
                Pass::Measured(ScenarioKind::Warm),
                config_gen::Mode::Sync(target.lock),
            )
            .await?;
            progress(&format!(
                "  warm: {label} complete ({:.1}s)",
                warm.wall_clock_secs
            ));
            warm_runs.push(warm);

            target::verify_sync(target, corpus, &label, ScenarioKind::Warm).await?;

            Ok(())
        }
        .await;

        // Always clean up, even if the tool run failed.
        let cleanup = target.release(corpus).await;
        finish_pass(run_result, cleanup)?;

        cooldown().await;
    }

    Ok((cold_runs, warm_runs))
}

/// One round of the partial scenario: for each tool in order, prime with the
/// base corpus, then sync with the partial corpus. Returns the measured runs.
async fn run_partial(
    args: &BenchArgs,
    base_corpus: &Corpus,
    partial_corpus: &Corpus,
    target: &Destination<'_>,
    tools: &[Tool],
    output_dir: &Path,
) -> Result<Vec<ToolRun>, Box<dyn std::error::Error>> {
    progress("bench: running partial scenario");
    let mut runs = Vec::new();

    for &tool in tools {
        let label = args.tool_selection.label(tool);
        prepare_clean_slate();
        target.reset(base_corpus).await?;
        progress(&format!("  partial: {label} (priming)"));

        let run_result: Result<(), Box<dyn std::error::Error>> = async {
            // Prime with base corpus (unmeasured). Reuse the same
            // config_dir so ocync's TransferStateCache persists.
            let config_dir = tempfile::tempdir()?;
            run_single_tool(
                args,
                tool,
                base_corpus,
                config_dir.path(),
                output_dir,
                Pass::Prime,
                config_gen::Mode::Sync(target.lock),
            )
            .await?;
            progress(&format!("  partial: {label} measuring"));

            let run = run_single_tool(
                args,
                tool,
                partial_corpus,
                config_dir.path(),
                output_dir,
                Pass::Measured(ScenarioKind::Partial),
                config_gen::Mode::Sync(target.lock),
            )
            .await?;
            progress(&format!(
                "  partial: {label} complete ({:.1}s)",
                run.wall_clock_secs
            ));
            runs.push(run);

            target::verify_sync(target, partial_corpus, &label, ScenarioKind::Partial).await?;
            Ok(())
        }
        .await;

        // Always clean up, even if the tool run failed.
        let cleanup = target.release(base_corpus).await;
        finish_pass(run_result, cleanup)?;

        cooldown().await;
    }

    Ok(runs)
}

/// Scale scenario: run at increasing corpus sizes [10, 25, 50, all], one
/// scenario result per size so each carries the full metric table.
async fn run_scale(
    args: &BenchArgs,
    corpus: &Corpus,
    target: &Destination<'_>,
    tools: &[Tool],
    output_dir: &Path,
) -> Result<Finished, Box<dyn std::error::Error>> {
    progress("bench: running scale scenario");
    let total = corpus.images.len();
    let mut sizes: Vec<usize> = [10, 25, 50, total]
        .iter()
        .copied()
        .filter(|&s| s <= total)
        .collect();
    sizes.dedup();

    let mut scale_results = Vec::new();

    for &size in &sizes {
        progress(&format!("  scale: {size} images"));
        let subset = corpus.limit(size);
        let mut runs = Vec::new();

        for &tool in tools {
            let label = args.tool_selection.label(tool);
            prepare_clean_slate();
            target.reset(&subset).await?;
            progress(&format!("  scale: {label} @ {size} images"));

            let run_result: Result<(), Box<dyn std::error::Error>> = async {
                let config_dir = tempfile::tempdir()?;
                let run = run_single_tool(
                    args,
                    tool,
                    &subset,
                    config_dir.path(),
                    output_dir,
                    Pass::Measured(ScenarioKind::Scale { images: size }),
                    config_gen::Mode::Sync(target.lock),
                )
                .await?;
                runs.push(run);

                let kind = ScenarioKind::Scale { images: size };
                target::verify_sync(target, &subset, &label, kind).await?;
                Ok(())
            }
            .await;

            // Always clean up, even if the tool run failed.
            let cleanup = target.release(&subset).await;
            finish_pass(run_result, cleanup)?;

            cooldown().await;
        }

        scale_results.push((ScenarioKind::Scale { images: size }, runs));
    }

    Ok(scale_results)
}

/// Measure the pre-engine phase on its own.
///
/// `--dry-run` resolves every mapping and stops, so the run is a client build
/// per registry plus a tag listing per mapping and nothing else. That is the
/// whole point: in a full sync this work is a rounding error beside gigabytes
/// of blob transfer, which is why a regression here went unnoticed long enough
/// to cost eleven minutes on a 95-mapping config.
///
/// None of the sync scenario's fairness machinery applies. Nothing is
/// transferred, so there are no blobs to fall out of page cache, no ECR repos
/// to reset, and no CDN edge to pre-warm. Skipping it is what keeps this
/// scenario cheap enough to run often.
async fn run_prepare(
    args: &BenchArgs,
    output_dir: &Path,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    progress("bench: running prepare scenario (ocync, --dry-run)");

    // Its own corpus: wildcard tags, so resolution actually enumerates. A
    // corpus of literal tags takes the exact-tag fast path and measures
    // nothing. See `bench/corpus-prepare.yaml`.
    let corpus = corpus::load(&args.prepare_corpus)?;
    let corpus = match args.limit {
        Some(n) => corpus.limit(n),
        None => corpus,
    };
    progress(&format!("  prepare: {} mappings", corpus.images.len()));

    let config_dir = tempfile::tempdir()?;
    let run = run_single_tool(
        args,
        Tool::Ocync,
        &corpus,
        config_dir.path(),
        output_dir,
        Pass::Measured(ScenarioKind::Prepare),
        config_gen::Mode::Prepare,
    )
    .await?;

    Ok(ScenarioResult::unshuffled(ScenarioKind::Prepare, vec![run]))
}

/// Drops the page cache and clears the staging tmpdir, before every tool
/// including the first, so none benefits from a previous tool's source pulls
/// cached in RAM or leftover state.
fn prepare_clean_slate() {
    drop_page_cache();
    clean_staging_tmpdir();
}

/// How one blob's pre-warm ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlobOutcome {
    /// Served from the CDN cache.
    Hit,
    /// A cache header reported a miss: served from the origin, which warms
    /// the cache.
    Miss,
    /// Downloaded, but no cache header reported a hit or a miss. Nothing
    /// says the cache needs time to settle, so this never causes the wait.
    Unreported,
    /// Refused, unreachable or still failing after the attempts.
    Failed,
    /// Not warmed by the deadline: not sent, or not finished.
    PastDeadline,
}

/// Classify the final response to a blob request. A hit is what `CloudFront`
/// (`X-Cache`) or Cloudflare (`CF-Cache-Status`: `HIT`, `REVALIDATED`,
/// `UPDATING` or `STALE`, each served from its cache) says it is. A miss is an
/// `X-Cache` that says miss or any other `CF-Cache-Status`. A success whose
/// headers say neither, an object store's or a CDN's direct answer, is
/// unreported. A status that is not 2xx, an unfollowed redirect included,
/// warmed nothing.
fn outcome_of(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> BlobOutcome {
    if !status.is_success() {
        return BlobOutcome::Failed;
    }
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let x_cache = header("x-cache").map(str::to_ascii_lowercase);
    let hit = x_cache.as_ref().is_some_and(|v| v.contains("hit"))
        || header("cf-cache-status").is_some_and(|v| {
            ["HIT", "REVALIDATED", "UPDATING", "STALE"]
                .iter()
                .any(|state| v.eq_ignore_ascii_case(state))
        });
    if hit {
        BlobOutcome::Hit
    } else if x_cache.is_some_and(|v| v.contains("miss")) || header("cf-cache-status").is_some() {
        BlobOutcome::Miss
    } else {
        BlobOutcome::Unreported
    }
}

/// Read a response body to its end and discard it, a chunk at a time so a
/// large layer is never held whole.
async fn drain(mut response: reqwest::Response) -> Result<(), reqwest::Error> {
    while response.chunk().await?.is_some() {}
    Ok(())
}

/// Warm one blob, `what` naming it in notices. `send` makes the request and
/// waits out what is transient, so an error from it is final. A body that
/// fails mid-read repeats the whole attempt, up to [`PREWARM_MAX_ATTEMPTS`].
/// All of it, retries and waits included, runs under `deadline`: a blob not
/// finished by then is [`BlobOutcome::PastDeadline`], even one that was sent.
async fn warm_blob(
    deadline: Instant,
    what: &str,
    send: impl AsyncFn() -> Result<reqwest::Response, String>,
) -> BlobOutcome {
    let attempts = async {
        for attempt in 1..=PREWARM_MAX_ATTEMPTS {
            let response = match send().await {
                Ok(response) => response,
                Err(e) => {
                    eprintln!("  {what}: {e}");
                    return BlobOutcome::Failed;
                }
            };
            let outcome = outcome_of(response.status(), response.headers());
            if outcome == BlobOutcome::Failed {
                eprintln!("  {what}: {}", response.status());
                return outcome;
            }
            match drain(response).await {
                Ok(()) => return outcome,
                Err(e) => eprintln!("  {what}: body failed (attempt {attempt}): {e}"),
            }
        }
        BlobOutcome::Failed
    };
    // The timeout polls `attempts` once even when the deadline has passed, so
    // a blob that never started is dropped before it.
    if Instant::now() > deadline {
        return BlobOutcome::PastDeadline;
    }
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), attempts)
        .await
        .unwrap_or(BlobOutcome::PastDeadline)
}

/// Per-registry CDN pre-warm statistics.
#[derive(Default)]
struct PrewarmStats {
    hits: u64,
    misses: u64,
    /// Blobs downloaded with no cache header saying hit or miss.
    unreported: u64,
    /// Blobs that could not be warmed.
    failed: u64,
    /// Blobs not finished because the deadline came first.
    past_deadline: u64,
    /// The registry is on [`NO_BLOB_CACHE_HOSTS`] and its probe confirmed it:
    /// one blob was checked and the others were not requested.
    no_cache: bool,
    /// Blobs not requested because the registry has no blob cache.
    not_requested: u64,
}

impl PrewarmStats {
    /// Count one blob.
    fn record(&mut self, outcome: BlobOutcome) {
        let count = match outcome {
            BlobOutcome::Hit => &mut self.hits,
            BlobOutcome::Miss => &mut self.misses,
            BlobOutcome::Unreported => &mut self.unreported,
            BlobOutcome::Failed => &mut self.failed,
            BlobOutcome::PastDeadline => &mut self.past_deadline,
        };
        *count += 1;
    }

    /// The registry's line in the pre-warm summary.
    fn summary(&self) -> String {
        if self.no_cache {
            return format!(
                "no blob cache, not warmed ({} checked, {} not requested)",
                self.unreported, self.not_requested
            );
        }
        format!(
            "{} blobs warmed ({} hits, {} misses, {} without a cache header), {} failed, {} past the deadline",
            self.hits + self.misses + self.unreported,
            self.hits,
            self.misses,
            self.unreported,
            self.failed,
            self.past_deadline
        )
    }
}

/// The (repository, digest) of every blob the pre-warm fetches: all those the
/// lock records for the refs the run syncs, deduplicated, so every tag is
/// covered. In a fixed order, so a pre-warm cut by the deadline leaves the
/// same blobs warm each run.
fn blobs_to_warm<'a>(
    lock: &'a lock::Lock,
    refs: &'a BTreeSet<lock::ImageRef>,
) -> Vec<(registry::Repository<'a>, &'a Digest)> {
    let mut blobs = Vec::new();
    for image in refs {
        let repo = registry::split_source(&image.source)
            .expect("ensure_served refuses a source with no registry");
        let entry = lock
            .images
            .get(image)
            .expect("ensure_served refuses a ref the lock lacks");
        blobs.extend(entry.blobs.iter().map(|blob| (repo, blob)));
    }
    blobs.sort_by_cached_key(|(repo, blob)| (repo.to_string(), blob.to_string()));
    blobs.dedup();
    blobs
}

/// Registries with a blob that was not warmed, and how many.
fn unwarmed(stats: &BTreeMap<&str, PrewarmStats>) -> BTreeMap<String, u64> {
    stats
        .iter()
        .map(|(host, s)| ((*host).to_owned(), s.failed + s.past_deadline))
        .filter(|(_, blobs)| *blobs > 0)
        .collect()
}

/// How long from the start of the pre-warm a blob has to be warmed. A blob not
/// finished by then is dropped, mid-download if need be.
const PREWARM_DEADLINE: Duration = Duration::from_secs(480);

/// Concurrent blob requests to one registry. Each registry has its own, so a
/// slow one cannot hold the slots another needs.
const PREWARM_PER_REGISTRY: usize = 8;

/// Attempts at one blob whose body fails mid-read.
const PREWARM_MAX_ATTEMPTS: u32 = 2;

/// Registries measured to serve blobs through no cache, so that warming one
/// costs its bytes for nothing. quay.io redirects blobs to `cdn01.quay.io`
/// (S3) and cgr.dev to Cloudflare R2; neither answers with `X-Cache` or
/// `CF-Cache-Status` (measured 2026-10-31 and in run `2026-11-01-154015` for
/// quay.io, 2026-10-30 for cgr.dev). Each is probed with one blob, so one that
/// gains a cache is warmed after all and reported.
const NO_BLOB_CACHE_HOSTS: [&str; 2] = ["quay.io", "cgr.dev"];

/// What a probe of a host on [`NO_BLOB_CACHE_HOSTS`] found.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// A success with neither `X-Cache` nor `CF-Cache-Status`.
    NoCache,
    /// A success carrying one of them: the host has a cache now.
    Cached,
    /// Refused, unreachable or past the deadline: nothing learned, so the
    /// host is warmed as any other.
    Unknown,
}

/// Request one blob and read its headers only, dropping the body unread.
/// `what` names it in notices.
async fn probe(
    deadline: Instant,
    what: &str,
    send: impl AsyncFn() -> Result<reqwest::Response, String>,
) -> Probe {
    let answered = async {
        match send().await {
            Ok(response) if !response.status().is_success() => {
                eprintln!("  {what}: {}", response.status());
                Probe::Unknown
            }
            Ok(response) => {
                let headers = response.headers();
                if headers.contains_key("x-cache") || headers.contains_key("cf-cache-status") {
                    Probe::Cached
                } else {
                    Probe::NoCache
                }
            }
            Err(e) => {
                eprintln!("  {what}: {e}");
                Probe::Unknown
            }
        }
    };
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), answered)
        .await
        .unwrap_or(Probe::Unknown)
}

/// What [`warm_blobs`] found.
struct Warmed<'a> {
    /// The outcomes of each registry's blobs.
    stats: BTreeMap<&'a str, PrewarmStats>,
    /// Registries on the no-cache list whose probe carried a cache header.
    cache_gained: BTreeSet<&'a str>,
}

/// Warm every blob, at most [`PREWARM_PER_REGISTRY`] at a time per registry,
/// each by `deadline`, and count the outcomes by registry. First, each
/// registry in `no_cache_hosts` is probed with its first blob; one that
/// answers with no cache header has its other blobs left unrequested, and one
/// that answers with a cache header is warmed like any other and named in
/// [`Warmed::cache_gained`]. `send` requests one blob, `what` naming it in
/// notices.
async fn warm_blobs<'a>(
    blobs: &[(registry::Repository<'a>, &'a Digest)],
    deadline: Instant,
    no_cache_hosts: &[&str],
    send: impl AsyncFn(&registry::Repository<'_>, &Digest, &str) -> Result<reqwest::Response, String>,
) -> Warmed<'a> {
    let mut stats: BTreeMap<&str, PrewarmStats> = BTreeMap::new();
    let mut cache_gained = BTreeSet::new();

    let mut firsts = BTreeMap::new();
    for blob in blobs {
        let host = blob.0.host.api_host();
        if no_cache_hosts.contains(&host) {
            firsts.entry(host).or_insert(blob);
        }
    }
    let probes = futures_util::future::join_all(firsts.values().map(|(repo, digest)| {
        let send = &send;
        async move {
            let what = format!("blob probe {repo}@{digest}");
            let found = probe(deadline, &what, async || send(repo, digest, &what).await).await;
            (repo.host.api_host(), found)
        }
    }))
    .await;
    for (host, found) in probes {
        match found {
            Probe::NoCache => {
                let blobs_of_host = blobs
                    .iter()
                    .filter(|(repo, _)| repo.host.api_host() == host)
                    .count();
                stats.insert(
                    host,
                    PrewarmStats {
                        unreported: 1,
                        no_cache: true,
                        not_requested: blobs_of_host as u64 - 1,
                        ..PrewarmStats::default()
                    },
                );
            }
            Probe::Cached => {
                cache_gained.insert(host);
            }
            Probe::Unknown => {}
        }
    }

    let to_warm: Vec<_> = blobs
        .iter()
        .filter(|(repo, _)| !stats.contains_key(repo.host.api_host()))
        .collect();
    let permits: HashMap<&str, tokio::sync::Semaphore> = to_warm
        .iter()
        .map(|(repo, _)| {
            let host = repo.host.api_host();
            (host, tokio::sync::Semaphore::new(PREWARM_PER_REGISTRY))
        })
        .collect();
    let outcomes = futures_util::future::join_all(to_warm.iter().map(|(repo, digest)| {
        let (permits, send) = (&permits, &send);
        async move {
            let host = repo.host.api_host();
            let _permit = permits[host].acquire().await.unwrap();
            let what = format!("blob pre-warm {repo}@{digest}");
            let outcome =
                warm_blob(deadline, &what, async || send(repo, digest, &what).await).await;
            (host, outcome)
        }
    }))
    .await;
    for (host, outcome) in outcomes {
        stats.entry(host).or_default().record(outcome);
    }
    Warmed {
        stats,
        cache_gained,
    }
}

/// Pull every unique source blob once to warm CDN edge caches.
///
/// Runs before any tool so all tools see identically warm CDN. Uses
/// OCI token exchange for authentication so requests succeed (2xx) and
/// `CloudFront` actually caches the responses. Unauthenticated 401s are
/// NOT cached by CDN and would warm nothing. The blobs come from the lock,
/// so every tag the run syncs is warmed and no manifest is read.
async fn cdn_prewarm(
    client: &reqwest::Client,
    auth: Option<&corpus::DockerHubAuth>,
    lock: &lock::Lock,
    refs: &BTreeSet<lock::ImageRef>,
) -> Prewarm {
    let deadline = Instant::now() + PREWARM_DEADLINE;
    let access = registry::Access::new(client, auth);
    let blobs = blobs_to_warm(lock, refs);

    eprintln!(
        "bench: warming {} unique blobs ({PREWARM_PER_REGISTRY} at a time per registry)...",
        blobs.len()
    );
    let Warmed {
        stats,
        cache_gained,
    } = warm_blobs(
        &blobs,
        deadline,
        &NO_BLOB_CACHE_HOSTS,
        async |repo, digest, what| {
            let url = repo.url(&format!("blobs/{digest}"));
            access.send(repo, what, |client| client.get(&url)).await
        },
    )
    .await;
    for host in &cache_gained {
        eprintln!(
            "WARNING: {host} is on NO_BLOB_CACHE_HOSTS but its blob answered with a cache header, so it was warmed; remove it from the list"
        );
    }
    for (registry, s) in &stats {
        eprintln!("  pre-warm: {registry}: {}", s.summary());
    }
    eprintln!("bench: blob CDN pre-warm done");
    let outcome = Prewarm::Done {
        unwarmed: unwarmed(&stats),
        no_cache: stats
            .iter()
            .filter(|(_, s)| s.no_cache)
            .map(|(host, _)| (*host).to_owned())
            .collect(),
    };
    // Only what is not warmed is a warning: a registry with no cache has
    // nothing a later tool could pay for, so it is in the summary and the
    // record alone.
    if let Prewarm::Done { unwarmed, .. } = &outcome
        && !unwarmed.is_empty()
    {
        eprintln!(
            "WARNING: Pre-warm: {}; the tool that runs first may pay the CDN misses and tool order matters",
            report::not_warmed(unwarmed)
        );
    }

    // A miss is an origin fetch, which CloudFront edges need time to settle
    // after; all hits leave nothing to wait for.
    if stats.values().any(|s| s.misses > 0) {
        let wait_secs = 360; // 6 minutes
        eprintln!("bench: waiting {wait_secs}s for CDN propagation...");
        tokio::time::sleep(Duration::from_secs(wait_secs)).await;
    } else {
        eprintln!("bench: no blob missed the CDN cache, so not waiting for it");
    }
    outcome
}

/// Parses an ocync timing JSONL file into a phase-name to milliseconds map.
///
/// Each line is `{"phase":"...","elapsed_ms":NNN}`. Malformed lines are
/// silently skipped (the timing file is best-effort telemetry, not critical data).
fn parse_timing_file(path: &Path) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            #[expect(clippy::disallowed_methods, reason = "a timing file the bench wrote")]
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let (Some(phase), Some(ms)) = (v["phase"].as_str(), v["elapsed_ms"].as_u64()) {
                    map.insert(phase.to_owned(), ms);
                }
            }
        }
    }
    map
}

/// Sleeps between benchmark runs to let rate limits recover.
async fn cooldown() {
    eprintln!("bench: cooling down for 10s...");
    tokio::time::sleep(Duration::from_secs(10)).await;
}

/// A seed for a shuffle: the clock, hashed. Not cryptographic, just enough to
/// randomize execution order across rounds and runs.
fn fresh_seed() -> u64 {
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    hasher.finish()
}

/// Shuffle a tool list in-place using Fisher-Yates driven by `seed`, so the
/// order a recorded seed produced can be reproduced.
fn shuffle_tools(tools: &mut [Tool], seed: u64) {
    let mut state = seed;
    for i in (1..tools.len()).rev() {
        // xorshift64 step for cheap pseudo-randomness.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state as usize) % (i + 1);
        tools.swap(i, j);
    }
}

/// Attempt to drop the OS page cache via `/proc/sys/vm/drop_caches`.
///
/// Requires root (or `CAP_SYS_ADMIN`). If it fails (e.g. running without
/// sudo), log a warning and continue -- the benchmark is still valid, just
/// slightly biased toward later tools.
fn drop_page_cache() {
    // Flush dirty pages, then drop all page/dentry/inode caches.
    // Uses sudo because /proc/sys/vm/drop_caches requires CAP_SYS_ADMIN.
    // ec2-user on AL2023 has passwordless sudo.
    let result = std::process::Command::new("sudo")
        .args(["sh", "-c", "sync && echo 3 > /proc/sys/vm/drop_caches"])
        .status();
    match result {
        Ok(s) if s.success() => eprintln!("bench: dropped page cache"),
        Ok(s) => eprintln!(
            "bench: warning: drop_caches exited {s}; later tools may benefit from cached data"
        ),
        Err(e) => eprintln!(
            "bench: warning: could not drop page cache ({e}); later tools may benefit from cached data"
        ),
    }
}

/// Remove all files in the staging tmpdir so the next tool starts clean.
fn clean_staging_tmpdir() {
    let tmpdir = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&tmpdir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
        eprintln!("bench: cleaned staging tmpdir ({})", tmpdir.display());
    }
}

/// When a run started: a UTC timestamp, `YYYY-MM-DD-HHMMSS`. It names the
/// run's results directory and is the timestamp of its record, so the one value
/// feeds both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunTimestamp(String);

impl RunTimestamp {
    /// The current time.
    pub(crate) fn now() -> Self {
        Self(utc_timestamp())
    }

    /// Accept the format's shape: 17 characters, digits with dashes after the
    /// year, month and day.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let shaped = raw.len() == 17
            && raw.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 | 10 => b == b'-',
                _ => b.is_ascii_digit(),
            });
        if shaped {
            Ok(Self(raw.to_owned()))
        } else {
            Err(format!("{raw:?} is not a timestamp like 2026-10-30-214340"))
        }
    }

    /// The directory a run with this timestamp writes its results into.
    pub(crate) fn results_dir(&self) -> PathBuf {
        Path::new(report::ARCHIVE_DIR).join(&self.0)
    }
}

impl std::str::FromStr for RunTimestamp {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, String> {
        Self::parse(raw)
    }
}

impl std::fmt::Display for RunTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Returns a UTC timestamp string in `YYYY-MM-DD-HHMMSS` format.
fn utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Manual UTC formatting -- avoids a chrono/time dependency for the run
    // timestamp alone.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since Unix epoch to (year, month, day) -- civil calendar algorithm.
    let (year, month, day) = days_to_civil(days);

    format!("{year:04}-{month:02}-{day:02}-{hours:02}{minutes:02}{seconds:02}")
}

/// Converts days since Unix epoch (1970-01-01) to (year, month, day).
///
/// Uses Howard Hinnant's `civil_from_days` algorithm.
fn days_to_civil(days: u64) -> (u32, u32, u32) {
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m, d)
}

/// Parses `argv` (without the program name) as the `bench` subcommand's
/// arguments, the way the command line does.
#[cfg(test)]
pub(crate) fn parse_bench(argv: &[&str]) -> Result<BenchArgs, clap::Error> {
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: BenchArgs,
    }

    Cli::try_parse_from(["xtask"].into_iter().chain(argv.iter().copied())).map(|cli| cli.args)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Serializes tests that mutate process environment variables.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn days_to_civil_known_dates() {
        // Day counts are `date -d <date> +%s` / 86400.
        for (days, expected) in [
            (0, (1970, 1, 1)),
            (20558, (2026, 4, 15)),
            // 2000 is a leap year (divisible by 400).
            (11016, (2000, 2, 29)),
            (11017, (2000, 3, 1)),
            // 2100 is not (divisible by 100 but not 400).
            (47540, (2100, 2, 28)),
            (47541, (2100, 3, 1)),
        ] {
            assert_eq!(days_to_civil(days), expected, "day {days}");
        }
    }

    #[test]
    fn a_run_timestamp_names_the_results_directory_and_refuses_other_shapes() {
        let ts: RunTimestamp = "2026-10-30-214340".parse().unwrap();
        assert_eq!(ts.to_string(), "2026-10-30-214340");
        assert_eq!(
            ts.results_dir(),
            Path::new("bench/results/2026-10-30-214340")
        );
        for bad in ["", "../etc", "2026-10-30-21434", "2026/10/30-214340"] {
            assert!(bad.parse::<RunTimestamp>().is_err(), "{bad:?}");
        }
        let args = parse_bench(&["--run-timestamp", "2026-10-30-214340", "sync"]).unwrap();
        assert_eq!(args.run_timestamp, Some(ts));
        assert!(parse_bench(&["--run-timestamp", "../x", "sync"]).is_err());
    }

    #[test]
    fn utc_timestamp_format() {
        let ts = utc_timestamp();
        // Must match YYYY-MM-DD-HHMMSS.
        assert_eq!(ts.len(), 17);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "-");
    }

    #[test]
    #[cfg(unix)]
    fn a_pass_that_does_not_exit_zero_aborts_the_run() {
        use std::os::unix::process::ExitStatusExt;

        let pass = Pass::Measured(ScenarioKind::Scale { images: 10 });
        assert_eq!(
            ensure_exited_zero("ocync", pass, ExitStatus::from_raw(0)),
            Ok(())
        );
        for (name, raw_status, expected) in [
            (
                "non-zero",
                1 << 8,
                "ocync scale-10 failed (exit status: 1); aborting, a failed run cannot be compared",
            ),
            (
                "killed by a signal",
                9,
                "ocync scale-10 failed (signal: 9 (SIGKILL)); aborting, a failed run cannot be compared",
            ),
        ] {
            assert_eq!(
                ensure_exited_zero("ocync", pass, ExitStatus::from_raw(raw_status)),
                Err(expected.to_string()),
                "{name}"
            );
        }
    }

    #[test]
    fn command_stdout_is_trimmed_text_from_a_successful_command() {
        use std::process::Command;
        let run = |program: &str, args: &[&str]| command_stdout(Command::new(program).args(args));

        assert_eq!(run("printf", &["  hi\n"]).as_deref(), Some("hi"));
        assert_eq!(run("printf", &[" \n"]), None, "blank output");
        assert_eq!(run("false", &[]), None, "non-zero exit");
        assert_eq!(run("/nonexistent/program", &[]), None, "cannot spawn");
    }

    #[test]
    fn captured_failure_is_stderr_trimmed_or_the_status_or_why_it_did_not_run() {
        use std::process::Command;
        let sh = |script: &str| captured(Command::new("sh").args(["-c", script]));

        assert_eq!(sh("printf ' out\\n'").as_deref(), Ok(" out\n"), "untrimmed");
        assert_eq!(sh("echo ' denied ' >&2; exit 3"), Err("denied".to_owned()));
        assert_eq!(sh("exit 3"), Err("exit status: 3".to_owned()), "no stderr");
        let err = captured(&mut Command::new("/nonexistent/program")).unwrap_err();
        assert!(
            err.starts_with("could not run /nonexistent/program: "),
            "{err}"
        );
    }

    #[test]
    fn value_name_is_the_spelling_the_command_line_accepts() {
        for (scenario, expected) in [
            (Scenario::Sync, "sync"),
            (Scenario::Partial, "partial"),
            (Scenario::Scale, "scale"),
            (Scenario::Prepare, "prepare"),
            (Scenario::All, "all"),
        ] {
            assert_eq!(value_name(&scenario), expected);
        }
    }

    #[test]
    fn registry_kind_names_the_archive() {
        for (host, stem) in [
            ("123456789012.dkr.ecr.us-east-1.amazonaws.com", "ecr"),
            ("111111111111.dkr.ecr.eu-west-1.amazonaws.com", "ecr"),
            ("gcr.io", "gcr"),
            ("us-docker.pkg.dev", "gcr"),
            ("europe-west1-docker.pkg.dev", "gcr"),
            ("myregistry.azurecr.io", "acr"),
            (
                "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com",
                "ecr",
            ),
            ("123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn", "ecr"),
            ("123456789012.DKR.ECR.US-EAST-1.AMAZONAWS.COM", "ecr"),
            ("myreg.azurecr.us", "acr"),
            ("myreg.azurecr.cn", "acr"),
            ("MyReg.AzureCR.io", "acr"),
            ("ecr.example.com", "other"),
            ("public.ecr.aws", "other"),
            ("ghcr.io", "other"),
            ("docker.io", "other"),
            ("quay.io", "other"),
        ] {
            assert_eq!(RegistryKind::of(host).archive_stem(), stem, "{host}");
        }
    }

    #[test]
    fn a_google_or_azure_target_runs_on_the_machine_it_ran_on_and_leaves_the_page_and_names_out() {
        use crate::bench::acr::{AcrTarget, FULL_ENV};
        use crate::bench::gar::ArtifactRegistryTarget;
        use crate::bench::report::tests::sample_report;
        use crate::bench::target::{env_of, rounds};

        let google = Target::ArtifactRegistry(ArtifactRegistryTarget {
            location: "europe-west9".parse().unwrap(),
            project: "example-project".parse().unwrap(),
            repositories: vec!["run-r1".parse().unwrap(), "run-r2".parse().unwrap()],
        });
        let azure = Target::Acr(AcrTarget::parse(&env_of(&FULL_ENV), rounds("3")).unwrap());
        let report = sample_report();
        let output = |target: &Target, host: &str| {
            let deferred = deferred_output(target, host, &report, "abc1234".into(), None);
            let text = serde_json::to_string(&deferred).unwrap();
            (
                serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                text,
            )
        };

        for (target, host, key, names) in [
            (
                &google,
                "europe-west9-docker.pkg.dev",
                "gcr",
                // Catches: the project, the repositories or the host reaching
                // a file that is tracked in git.
                ["example-project", "run-r1", "europe-west9", "pkg.dev"],
            ),
            (
                &azure,
                "ocyncbench1a2b3c.azurecr.io",
                "acr",
                // Catches: the registry, the prefixes or the Entra ids
                // reaching a file that is tracked in git.
                ["ocyncbench1a2b3c", "run-r1", "11111111-2222", "azurecr.io"],
            ),
        ] {
            let (record, text) = output(target, host);
            // Catches: the target registry's cloud written as the machine's.
            assert_eq!(record["record"]["machine"]["provider"], "aws", "{key}");
            assert_eq!(record["archive_key"], key);
            // Catches: a non-ECR run rewriting the ECR comparison page.
            assert_eq!(
                record["page"],
                serde_json::json!({"cold": null, "warm": null})
            );
            for name in names {
                assert!(!text.contains(name), "{name} in {text}");
            }
        }

        let (ecr, _) = output(&Target::Ecr, "111111111111.dkr.ecr.us-east-1.amazonaws.com");
        assert_eq!(ecr["archive_key"], "ecr");
        assert!(ecr["page"]["cold"].is_string(), "{ecr}");
    }

    #[test]
    fn bench_args_parse_closed_sets_and_refuse_anything_else() {
        use clap::error::ErrorKind;

        let ok = parse_bench(&["--tools", "Ocync,DREGSY,regsync", "prepare"]).unwrap();
        assert_eq!(
            ok.tool_selection.tools,
            [Tool::Ocync, Tool::Dregsy, Tool::Regsync]
        );
        assert!(matches!(ok.scenario, Scenario::Prepare));

        for (name, argv, kind) in [
            (
                "tool",
                &["--tools", "ocync,skopeo", "sync"][..],
                ErrorKind::InvalidValue,
            ),
            ("scenario", &["cold"][..], ErrorKind::InvalidSubcommand),
        ] {
            let err = parse_bench(argv)
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted"));
            assert_eq!(err.kind(), kind, "{name}");
        }
    }

    #[test]
    fn fewer_than_three_rounds_are_refused_at_the_command_line() {
        // Catches: a run of 1 or 2 rounds, whose range holds the median with
        // at most 50% confidence, being accepted and then reported on.
        use clap::error::ErrorKind;

        for (argv, rounds) in [
            (&["sync"][..], Some(5)),
            (&["--rounds", "3", "sync"][..], Some(3)),
            (&["--rounds", "12", "sync"][..], Some(12)),
            (&["--rounds", "2", "sync"][..], None),
            (&["--rounds", "1", "sync"][..], None),
            (&["--rounds", "0", "sync"][..], None),
            (&["--rounds=-3", "sync"][..], None),
            (&["--rounds", "many", "sync"][..], None),
        ] {
            match (parse_bench(argv), rounds) {
                (Ok(args), Some(rounds)) => {
                    assert_eq!(args.tool_selection.rounds.get(), rounds, "{argv:?}")
                }
                (Err(err), None) => assert_eq!(err.kind(), ErrorKind::ValueValidation, "{argv:?}"),
                (got, want) => panic!("{argv:?}: wanted {want:?}, got ok={}", got.is_ok()),
            }
        }
        let message = "2".parse::<Rounds>().unwrap_err();
        assert!(
            message.ends_with("the fewest is 3, and a verdict between tools needs 5"),
            "{message}"
        );
    }

    #[test]
    fn the_baseline_commit_is_a_resolved_hex_id_and_comes_with_its_tool() {
        use clap::error::ErrorKind;

        let head =
            command_stdout(std::process::Command::new("git").args(["rev-parse", "HEAD"])).unwrap();
        let short = &head[..7];

        // A short id resolves to the full one, and with it the baseline and the
        // ocync it is compared with join the run, once each.
        for (tools, run) in [
            (&[][..], vec![Tool::Ocync, Tool::OcyncBaseline]),
            (
                &["--tools", "dregsy"][..],
                vec![Tool::Dregsy, Tool::Ocync, Tool::OcyncBaseline],
            ),
            (
                &["--tools", "ocync,dregsy"][..],
                vec![Tool::Ocync, Tool::Dregsy, Tool::OcyncBaseline],
            ),
        ] {
            let argv: Vec<&str> = ["--baseline-ref", short]
                .into_iter()
                .chain(tools.iter().copied())
                .chain(["sync"])
                .collect();
            let args = parse_bench(&argv).unwrap();
            assert_eq!(
                args.tool_selection
                    .baseline_ref
                    .as_ref()
                    .unwrap()
                    .to_string(),
                head
            );
            assert_eq!(args.tool_selection.run_tools(), run, "{tools:?}");
        }
        assert_eq!(
            parse_bench(&["sync"]).unwrap().tool_selection.run_tools(),
            [Tool::Ocync]
        );

        // Catches: the baseline named as a tool, which would run with no commit
        // to build, in any spelling, and a commit given in a form that can move
        // or is not one.
        for (name, argv, kind) in [
            (
                "tool alone",
                &["--tools", "ocync-baseline", "sync"][..],
                ErrorKind::InvalidValue,
            ),
            (
                "tool in capitals",
                &["--tools", "OCYNC-BASELINE", "sync"][..],
                ErrorKind::InvalidValue,
            ),
            (
                "a branch name",
                &["--baseline-ref", "main", "sync"][..],
                ErrorKind::ValueValidation,
            ),
            (
                "a flag",
                &["--baseline-ref=-abcd", "sync"][..],
                ErrorKind::ValueValidation,
            ),
            (
                "too short",
                &["--baseline-ref", "abc", "sync"][..],
                ErrorKind::ValueValidation,
            ),
            (
                "no such commit",
                &["--baseline-ref", "0000000", "sync"][..],
                ErrorKind::ValueValidation,
            ),
        ] {
            let err = parse_bench(argv)
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted"));
            assert_eq!(err.kind(), kind, "{name}");
        }
    }

    #[test]
    fn a_baseline_is_refused_for_scenarios_with_no_rounds() {
        // Catches: building a baseline for a scale or prepare run, which has no
        // round to put it in, and refusing it for the scenarios that do.
        let head =
            command_stdout(std::process::Command::new("git").args(["rev-parse", "HEAD"])).unwrap();
        let selection = |scenario: &str| {
            parse_bench(&["--baseline-ref", &head, scenario])
                .unwrap()
                .tool_selection
        };
        for scenario in [Scenario::Sync, Scenario::Partial, Scenario::All] {
            let selection = selection(&value_name(&scenario));
            assert_eq!(selection.ensure_baseline_applies(scenario), Ok(()));
        }
        for scenario in [Scenario::Scale, Scenario::Prepare] {
            let selection = selection(&value_name(&scenario));
            assert_eq!(
                selection.ensure_baseline_applies(scenario),
                Err(format!(
                    "--baseline-ref applies to the sync and partial scenarios, not {}",
                    value_name(&scenario)
                ))
            );
        }
        let none = parse_bench(&["scale"]).unwrap().tool_selection;
        assert_eq!(none.ensure_baseline_applies(Scenario::Scale), Ok(()));
    }

    #[test]
    fn a_transfer_cap_is_refused_when_no_ocync_build_runs() {
        // Catches: a cap accepted by a run that has no ocync to apply it to,
        // which then labels dregsy's numbers with a setting it never had.
        let head =
            command_stdout(std::process::Command::new("git").args(["rev-parse", "HEAD"])).unwrap();
        let refusal = "--max-concurrent-transfers applies to ocync, and this run builds none: \
                       add ocync to --tools or give --baseline-ref";
        let cap = ["--max-concurrent-transfers", "20"];
        for (argv, refused) in [
            (vec!["--tools", "dregsy", "sync"], None),
            (
                [&["--tools", "dregsy"][..], &cap, &["sync"]].concat(),
                Some(refusal),
            ),
            (
                [&["--tools", "ocync,dregsy"][..], &cap, &["sync"]].concat(),
                None,
            ),
            (
                [
                    &["--tools", "dregsy", "--baseline-ref", &head][..],
                    &cap,
                    &["sync"],
                ]
                .concat(),
                None,
            ),
        ] {
            let selection = parse_bench(&argv).unwrap().tool_selection;
            assert_eq!(
                selection.ensure_cap_applies(),
                refused.map_or(Ok(()), |text| Err(text.to_owned())),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_regression_run_is_refused_before_its_rounds_only_when_its_comparison_would_be() {
        // Catches: the refusal coming after the rounds, and a refusal for a run
        // whose comparison is skipped (proxy off, no ocync, no cold sync).
        let corpus = two_image_corpus();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        let cold = |rounds: usize| -> Vec<Transfer> {
            (1..=rounds).map(|round| Transfer::Sync { round }).collect()
        };
        let saved = Baseline {
            wall_clock_secs: 1.0,
            requests: 1,
            total_bytes: 1,
            corpus_locked_at: None,
            sync_from: None,
            corpus: report::CorpusInfo {
                images: corpus.images.len(),
                tags: corpus.total_tags(),
            },
            max_concurrent_transfers: NonZeroUsize::new(20),
            rounds: 3,
        };
        regression::save_baseline(&path, &saved).unwrap();
        let shown = path.display();
        let other_cap = format!(
            "{shown} was recorded with a transfer cap of 20, this run with a transfer cap of \
             30; record a new baseline by deleting {shown} and rerunning"
        );
        let other_rounds = format!(
            "{shown} was recorded over 3 rounds, this run over 4; record a new baseline by \
             deleting {shown} and rerunning"
        );
        for (argv, transfers, refused) in [
            (
                vec!["--regression", "--max-concurrent-transfers", "20", "sync"],
                cold(3),
                None,
            ),
            (
                vec!["--regression", "--max-concurrent-transfers", "30", "sync"],
                cold(3),
                Some(&other_cap),
            ),
            (
                vec!["--regression", "--max-concurrent-transfers", "20", "sync"],
                cold(4),
                Some(&other_rounds),
            ),
            (
                vec![
                    "--regression",
                    "--max-concurrent-transfers",
                    "30",
                    "--no-proxy",
                    "sync",
                ],
                cold(3),
                None,
            ),
            (
                vec!["--regression", "--tools", "dregsy", "sync"],
                cold(3),
                None,
            ),
            (
                vec!["--regression", "--max-concurrent-transfers", "30", "sync"],
                Vec::new(),
                None,
            ),
            (
                vec!["--max-concurrent-transfers", "30", "sync"],
                cold(3),
                None,
            ),
        ] {
            let args = parse_bench(&argv).unwrap();
            assert_eq!(
                ensure_baseline_comparable(&args, &transfers, &corpus, None, &path),
                refused.map_or(Ok(()), |text| Err(text.clone())),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_baseline_recorded_from_source_tags_is_refused_under_the_same_lock() {
        // Catches: a run that syncs locked digests being compared with a
        // baseline recorded when tools followed the tags, which the same
        // `locked_at` does not tell apart.
        let corpus = two_image_corpus();
        let at = lock::fixtures::locked_at("2026-10-30-214340");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        let args = parse_bench(&["--regression", "sync"]).unwrap();
        let cold: Vec<Transfer> = (1..=5).map(|round| Transfer::Sync { round }).collect();
        let saved = |sync_from| Baseline {
            wall_clock_secs: 1.0,
            requests: 1,
            total_bytes: 1,
            corpus_locked_at: Some(at.clone()),
            sync_from,
            corpus: report::CorpusInfo {
                images: corpus.images.len(),
                tags: corpus.total_tags(),
            },
            max_concurrent_transfers: Some(report::effective_transfer_cap(None)),
            rounds: 5,
        };
        let check = || ensure_baseline_comparable(&args, &cold, &corpus, Some(&at), &path);

        regression::save_baseline(&path, &saved(None)).unwrap();
        let shown = path.display();
        assert_eq!(
            check(),
            Err(format!(
                "{shown} was recorded syncing from source tags, this run syncing from locked \
                 digests; record a new baseline by deleting {shown} and rerunning"
            ))
        );
        regression::save_baseline(&path, &saved(Some(lock::SyncFrom::Digest))).unwrap();
        assert_eq!(check(), Ok(()));
    }

    #[test]
    fn rounds_repeat_sync_and_partial_and_leave_scale_and_prepare_at_one_pass() {
        // Catches: scale repeating, which multiplies its four corpus sizes, and
        // a sync with fewer rounds than asked for.
        let corpus = two_image_corpus();
        let dir = tempfile::tempdir().unwrap();
        let overrides = dir.path().join("overrides.yaml");
        std::fs::write(
            &overrides,
            "overrides:\n  - source: \"public.ecr.aws/lambda/python\"\n    tags: [\"3.10\"]\n",
        )
        .unwrap();
        let overrides = overrides.to_str().unwrap();
        let rounds = "4".parse().unwrap();
        for (requested, passes) in [
            (Scenario::Sync, 4),
            (Scenario::Partial, 4),
            (Scenario::Scale, 1),
            (Scenario::Prepare, 0),
            (Scenario::All, 9),
        ] {
            let transfers = plan(requested, &corpus, overrides, rounds).unwrap();
            assert_eq!(transfers.len(), passes, "{requested:?}");
        }
    }

    #[test]
    fn rounds_and_the_baseline_keep_their_proxy_logs_apart() {
        // Catches: round 2 truncating round 1's log, and the baseline, which
        // runs ocync's binary, truncating ocync's.
        let out = Path::new("results");
        let cold = Pass::Measured(ScenarioKind::Cold);
        let logs: BTreeSet<PathBuf> = [1, 2]
            .into_iter()
            .flat_map(|round| {
                [Tool::Ocync, Tool::OcyncBaseline]
                    .into_iter()
                    .map(move |tool| proxy_log(&round_dir(out, round), tool, cold))
            })
            .collect();
        assert_eq!(
            logs.iter().map(|p| p.to_str().unwrap()).collect::<Vec<_>>(),
            [
                "results/round-1/ocync-baseline-cold-proxy.jsonl",
                "results/round-1/ocync-cold-proxy.jsonl",
                "results/round-2/ocync-baseline-cold-proxy.jsonl",
                "results/round-2/ocync-cold-proxy.jsonl",
            ]
        );
    }

    #[test]
    fn the_regression_check_compares_the_median_round_of_ocync_alone() {
        // Catches: taking the first round, which is arbitrary, the mean, and
        // the baseline run, listed first in each round with other numbers.
        let round = |round, secs, requests| {
            let run = |tool, secs: f64, requests: u64| {
                let mut run = tool_run(tool, secs);
                run.proxy_metrics = Some(proxy::ProxyMetrics {
                    requests,
                    response_bytes: requests * 1000,
                    ..Default::default()
                });
                run
            };
            ScenarioResult {
                round: Some(round),
                ..ScenarioResult::unshuffled(
                    ScenarioKind::Cold,
                    vec![
                        run(Tool::OcyncBaseline, secs * 7.0, requests * 7),
                        run(Tool::Ocync, secs, requests),
                    ],
                )
            }
        };
        let scenarios = [
            round(1, 10.0, 100),
            round(2, 50.0, 900),
            round(3, 20.0, 300),
        ];
        let corpus = || report::CorpusInfo { images: 2, tags: 2 };

        let median = cold_ocync_median(&scenarios, corpus(), NonZeroUsize::new(20), None).unwrap();

        assert_eq!(median.wall_clock_secs, 20.0);
        assert_eq!(median.requests, 300);
        assert_eq!(median.total_bytes, 300_000);
        assert_eq!(median.rounds, 3);
        assert_eq!(median.max_concurrent_transfers, NonZeroUsize::new(20));
        assert_eq!(median.sync_from, None, "no lock, no digests to sync from");
        // Catches: a baseline saved from a locked run that a baseline of source
        // tags cannot be told from.
        let at = lock::fixtures::locked_at("2026-10-30-120000");
        let locked = cold_ocync_median(&scenarios, corpus(), None, Some(&at)).unwrap();
        assert_eq!(locked.sync_from, Some(lock::SyncFrom::Digest));
        assert!(cold_ocync_median(&[], corpus(), None, None).is_none());
    }

    #[test]
    fn a_round_without_proxy_metrics_leaves_the_regression_check_out_instead_of_counting_zero() {
        // Catches: `--no-proxy` saving a baseline of 0 requests and 0 bytes
        // that every later run then regresses against.
        let scenario = ScenarioResult {
            round: Some(1),
            ..ScenarioResult::unshuffled(ScenarioKind::Cold, vec![tool_run(Tool::Ocync, 10.0)])
        };
        let corpus = report::CorpusInfo { images: 2, tags: 2 };

        assert!(cold_ocync_median(&[scenario], corpus, None, None).is_none());
    }

    /// What a fake pass returns for one run of `tool`.
    fn finished(kind: ScenarioKind, tools: &[Tool]) -> Finished {
        vec![(kind, tools.iter().map(|&t| tool_run(t, 1.0)).collect())]
    }

    const TOOLS: [Tool; 4] = [
        Tool::Ocync,
        Tool::OcyncBaseline,
        Tool::Dregsy,
        Tool::Regsync,
    ];

    fn syncs(rounds: usize) -> Vec<Transfer> {
        (1..=rounds).map(|round| Transfer::Sync { round }).collect()
    }

    #[test]
    fn every_round_shuffles_the_tool_order_with_a_seed_of_its_own() {
        // Catches: the shuffle moving out of the round loop, which gives every
        // round one order and one seed.
        let mut seeds = [3, 100].into_iter();
        let (done, stop) = block_on(run_passes(
            &syncs(2),
            &TOOLS,
            || seeds.next().unwrap(),
            |tool| tool.to_string(),
            async |transfer, order| {
                let Transfer::Sync { .. } = transfer else {
                    unreachable!()
                };
                Ok(finished(ScenarioKind::Cold, order))
            },
        ));

        assert_eq!(stop, None);
        let seen: Vec<(Option<usize>, u64, Vec<Tool>)> = done
            .iter()
            .map(|r| (r.round, r.shuffle_seed, r.tool_order.clone()))
            .collect();
        assert_eq!(
            seen,
            [
                (
                    Some(1),
                    3,
                    vec![
                        Tool::Dregsy,
                        Tool::OcyncBaseline,
                        Tool::Ocync,
                        Tool::Regsync
                    ]
                ),
                (
                    Some(2),
                    100,
                    vec![
                        Tool::OcyncBaseline,
                        Tool::Dregsy,
                        Tool::Regsync,
                        Tool::Ocync
                    ]
                ),
            ]
        );
        // The runs were made in the order recorded.
        let ran: Vec<Tool> = done[1].runs.iter().map(|r| r.tool).collect();
        assert_eq!(ran, done[1].tool_order);
    }

    #[test]
    fn a_failure_in_round_three_keeps_rounds_one_and_two() {
        // Catches: a failed pass dropping the finished ones, or the run going
        // on past it.
        let mut asked = Vec::new();
        let (done, stop) = block_on(run_passes(
            &syncs(5),
            &TOOLS,
            || 1,
            |tool| tool.to_string(),
            async |transfer, order| {
                asked.push(transfer.round());
                if transfer.round() == Some(3) {
                    return Err("round 3 failed".to_owned());
                }
                Ok(finished(ScenarioKind::Cold, order))
            },
        ));

        assert_eq!(stop.as_deref(), Some("round 3 failed"));
        assert_eq!(
            done.iter().map(|r| r.round).collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        assert_eq!(
            asked,
            [Some(1), Some(2), Some(3)],
            "the run went on past the failure"
        );
    }

    #[test]
    fn scale_is_one_unshuffled_pass_after_the_rounds() {
        let transfers = [Transfer::Sync { round: 1 }, Transfer::Scale];
        let (done, stop) = block_on(run_passes(
            &transfers,
            &TOOLS,
            || 5,
            |tool| tool.to_string(),
            async |transfer, order| {
                let kind = match transfer {
                    Transfer::Scale => ScenarioKind::Scale { images: 10 },
                    _ => ScenarioKind::Cold,
                };
                Ok(finished(kind, order))
            },
        ));

        assert_eq!(stop, None);
        let scale = &done[1];
        assert_eq!((scale.round, scale.shuffle_seed), (None, 0));
        assert!(scale.tool_order.is_empty());
        assert_eq!(scale.runs.len(), 4, "scale gets the order as it was given");
    }

    #[test]
    fn a_seed_reproduces_its_shuffle() {
        let shuffled = |seed| {
            let mut tools = TOOLS;
            shuffle_tools(&mut tools, seed);
            tools
        };
        assert_eq!(shuffled(3), shuffled(3));
        assert_ne!(shuffled(3), shuffled(100));
        let mut one = [Tool::Ocync];
        shuffle_tools(&mut one, 3);
        assert_eq!(one, [Tool::Ocync]);
    }

    fn two_image_corpus() -> Corpus {
        serde_yaml::from_str(
            r#"
corpus: { target_registry: "r.example", target_prefix: "bench" }
images:
  - source: "public.ecr.aws/lambda/python"
    tags: ["3.12"]
  - source: "public.ecr.aws/amazoncorretto/amazoncorretto"
    tags: ["21"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn scenarios_name_the_refs_they_sync() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = dir.path().join("overrides.yaml");
        std::fs::write(
            &overrides,
            "overrides:\n  - source: \"public.ecr.aws/lambda/python\"\n    tags: [\"3.10\"]\n  \
             - source: \"public.ecr.aws/amazoncorretto/amazoncorretto\"\n    tags: [\"11\"]\n",
        )
        .unwrap();
        let corpus = two_image_corpus();
        let rounds = "3".parse().unwrap();
        let transfers = plan(Scenario::All, &corpus, overrides.to_str().unwrap(), rounds).unwrap();
        let names: Vec<String> = transfers.iter().map(ToString::to_string).collect();
        assert_eq!(
            names,
            [
                "Sync round 1",
                "Sync round 2",
                "Sync round 3",
                "Partial round 1",
                "Partial round 2",
                "Partial round 3",
                "Scale"
            ]
        );

        // (scenario, ref, in the scenario's refs)
        let python = "public.ecr.aws/lambda/python";
        let corretto = "public.ecr.aws/amazoncorretto/amazoncorretto";
        let cases = [
            (0, format!("{python}:3.12"), true),
            (0, format!("{python}:3.10"), false),
            (0, format!("{corretto}:11"), false),
            (6, format!("{python}:3.10"), false),
            (3, format!("{python}:3.12"), true),
            (3, format!("{python}:3.10"), true),
            (5, format!("{corretto}:21"), true),
            (5, format!("{corretto}:11"), true),
        ];
        for (at, image, expected) in cases {
            let refs = scenario_refs(&transfers[at], &corpus);
            let present = refs.iter().any(|r| r.to_string() == image);
            assert_eq!(present, expected, "{} {image}", transfers[at]);
        }
    }

    #[test]
    fn the_pre_warm_fetches_the_locked_blobs_of_every_tag_once_per_repository() {
        use lock::fixtures::{digest, entry, lock_of};
        let blobs = |fills: &str| fills.chars().map(digest).collect();
        let with = |fills: &str| lock::Entry {
            blobs: blobs(fills),
            ..entry('a', &[])
        };
        let lock = lock_of(&[
            ("docker.io/library/alpine:3", with("12")),
            // A second tag naming other blobs: also fetched, so a first-tag-only
            // pre-warm fails here.
            ("docker.io/library/alpine:3.20", with("23")),
            // Shared with the other tags of the image and with another image.
            ("quay.io/org/app:1", with("12")),
            ("quay.io/org/app:2", with("2")),
            ("quay.io/org/other:1", with("4")),
            // Not a ref the run syncs.
            ("quay.io/org/unsynced:1", with("9")),
        ]);
        let refs = lock
            .images
            .keys()
            .filter(|image| !image.source.ends_with("unsynced"))
            .cloned()
            .collect();
        let got: Vec<(String, String)> = blobs_to_warm(&lock, &refs)
            .into_iter()
            .map(|(repo, digest)| (repo.to_string(), digest.hex()[..1].to_owned()))
            .collect();
        // Already in order: the same blobs go first each run.
        let want = [
            ("quay.io/org/app", "1"),
            ("quay.io/org/app", "2"),
            ("quay.io/org/other", "4"),
            ("registry-1.docker.io/library/alpine", "1"),
            ("registry-1.docker.io/library/alpine", "2"),
            ("registry-1.docker.io/library/alpine", "3"),
        ]
        .map(|(repo, digest)| (repo.to_owned(), digest.to_owned()));
        assert_eq!(got, want);
    }

    #[test]
    fn the_pre_warm_names_the_registries_it_did_not_warm() {
        // Catches: a failed or dropped blob passing as a warm source.
        let warm = PrewarmStats {
            hits: 3,
            misses: 2,
            ..PrewarmStats::default()
        };
        let mut stats = BTreeMap::new();
        stats.insert("warm.example", warm);
        stats.insert("failed.example", {
            let mut s = PrewarmStats::default();
            s.record(BlobOutcome::Hit);
            s.record(BlobOutcome::Failed);
            s
        });
        // Catches: a registry with no blob cache named as one that failed to
        // warm.
        stats.insert(
            "nocache.example",
            PrewarmStats {
                unreported: 1,
                no_cache: true,
                not_requested: 5,
                ..PrewarmStats::default()
            },
        );
        stats.insert("late.example", {
            let mut s = PrewarmStats::default();
            s.record(BlobOutcome::PastDeadline);
            s
        });
        assert_eq!(
            unwarmed(&stats),
            BTreeMap::from([
                ("failed.example".to_owned(), 1),
                ("late.example".to_owned(), 1)
            ])
        );
    }

    #[test]
    fn only_a_partial_scenario_reads_the_overrides() {
        // Catches: a run without a partial scenario refusing to start for want
        // of an overrides file, and a partial run starting without one.
        let corpus = two_image_corpus();
        let missing = "/nonexistent/overrides.yaml";
        for requested in [Scenario::Sync, Scenario::Scale, Scenario::Prepare] {
            assert!(plan(requested, &corpus, missing, Rounds::DEFAULT).is_ok());
        }
        for requested in [Scenario::Partial, Scenario::All] {
            assert!(plan(requested, &corpus, missing, Rounds::DEFAULT).is_err());
        }
        let transfers = plan(Scenario::Prepare, &corpus, missing, Rounds::DEFAULT).unwrap();
        assert!(transfers.is_empty(), "prepare reads no lock");
    }

    #[test]
    fn a_failure_after_the_stop_keeps_the_stop_reason() {
        // Catches: a record that cannot be appended replacing the scenario
        // failure that stopped the run.
        let text = with_stop_reason("disk full".into(), Some("sync failed"));
        assert!(text.contains("disk full") && text.contains("sync failed"));
        assert_eq!(with_stop_reason("disk full".into(), None), "disk full");
    }

    #[test]
    fn a_failed_pass_keeps_both_its_own_error_and_its_cleanup_error() {
        // Catches: `run?; cleanup?;`, which drops the cleanup error when the
        // pass failed and so hides the repos left behind.
        let failed = || -> Result<(), Box<dyn std::error::Error>> { Err("sync failed".into()) };
        let cleanup_failed = || -> Result<(), String> { Err("failed to delete 1 repo(s)".into()) };
        let cases = [
            (Ok(()), Ok(()), None),
            (failed(), Ok(()), Some("sync failed")),
            (Ok(()), cleanup_failed(), Some("failed to delete 1 repo(s)")),
            (
                failed(),
                cleanup_failed(),
                Some("sync failed; also failed to delete 1 repo(s)"),
            ),
        ];
        for (pass, cleanup, want) in cases {
            let got = finish_pass(pass, cleanup).err().map(|e| e.to_string());
            assert_eq!(got.as_deref(), want);
        }
    }

    #[test]
    fn docker_hub_auth_from_env_both_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: env-var mutation is serialized by ENV_MUTEX above.
        unsafe {
            std::env::set_var("DOCKERHUB_USERNAME", "testuser");
            std::env::set_var("DOCKERHUB_ACCESS_TOKEN", "testtoken");
        }
        let result = docker_hub_auth_from_env();
        unsafe {
            std::env::remove_var("DOCKERHUB_USERNAME");
            std::env::remove_var("DOCKERHUB_ACCESS_TOKEN");
        }
        let auth = result.unwrap();
        assert_eq!(auth.username, "testuser");
        assert_eq!(auth.token, "testtoken");
    }

    #[test]
    fn docker_hub_auth_from_env_empty_returns_none() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: env-var mutation is serialized by ENV_MUTEX above.
        unsafe {
            std::env::set_var("DOCKERHUB_USERNAME", "");
            std::env::set_var("DOCKERHUB_ACCESS_TOKEN", "sometoken");
        }
        let result = docker_hub_auth_from_env();
        unsafe {
            std::env::remove_var("DOCKERHUB_USERNAME");
            std::env::remove_var("DOCKERHUB_ACCESS_TOKEN");
        }
        assert!(result.is_none());
    }

    #[test]
    fn docker_hub_auth_from_env_missing_returns_none() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: env-var mutation is serialized by ENV_MUTEX above.
        unsafe {
            std::env::remove_var("DOCKERHUB_USERNAME");
            std::env::remove_var("DOCKERHUB_ACCESS_TOKEN");
        }
        assert!(docker_hub_auth_from_env().is_none());
    }

    fn tool_run(tool: Tool, wall_clock_secs: f64) -> ToolRun {
        ToolRun {
            tool,
            wall_clock_secs,
            peak_rss_kb: None,
            proxy_metrics: None,
            phase_timing_ms: BTreeMap::new(),
        }
    }

    #[test]
    fn cold_ocync_runs_skip_scale_and_prepare_runs() {
        let scale = ScenarioResult::unshuffled(
            ScenarioKind::Scale { images: 10 },
            vec![tool_run(Tool::Ocync, 1.0)],
        );
        let prepare =
            ScenarioResult::unshuffled(ScenarioKind::Prepare, vec![tool_run(Tool::Ocync, 2.0)]);
        let cold = ScenarioResult::unshuffled(
            ScenarioKind::Cold,
            vec![tool_run(Tool::Dregsy, 9.0), tool_run(Tool::Ocync, 3.0)],
        );
        let scenarios = vec![scale, prepare, cold];

        let runs = cold_ocync_runs(&scenarios);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].wall_clock_secs, 3.0);
        assert!(cold_ocync_runs(&scenarios[..2]).is_empty());
    }

    #[test]
    fn every_pass_names_its_own_proxy_log() {
        let passes = [
            Pass::Prime,
            Pass::Measured(ScenarioKind::Cold),
            Pass::Measured(ScenarioKind::Warm),
            Pass::Measured(ScenarioKind::Partial),
            Pass::Measured(ScenarioKind::Scale { images: 10 }),
            Pass::Measured(ScenarioKind::Scale { images: 25 }),
            Pass::Measured(ScenarioKind::Prepare),
        ];
        let names: BTreeSet<String> = passes.iter().map(ToString::to_string).collect();
        assert_eq!(names.len(), passes.len(), "{names:?}");
    }

    #[test]
    fn parse_timing_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timing.jsonl");
        std::fs::write(
            &path,
            r#"{"phase":"discovery_start","elapsed_ms":0}
{"phase":"discovery_complete","elapsed_ms":1234}
{"phase":"execution_complete","elapsed_ms":5678}
"#,
        )
        .unwrap();
        let map = parse_timing_file(&path);
        assert_eq!(map.len(), 3);
        assert_eq!(map["discovery_start"], 0);
        assert_eq!(map["discovery_complete"], 1234);
        assert_eq!(map["execution_complete"], 5678);
    }

    #[test]
    fn parse_timing_file_missing() {
        let map = parse_timing_file(Path::new("/nonexistent/timing.jsonl"));
        assert!(map.is_empty());
    }

    #[test]
    fn parse_timing_file_malformed_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timing.jsonl");
        std::fs::write(
            &path,
            "not json\n{\"phase\":\"ok\",\"elapsed_ms\":42}\n{\"bad\":true}\n",
        )
        .unwrap();
        let map = parse_timing_file(&path);
        assert_eq!(map.len(), 1);
        assert_eq!(map["ok"], 42);
    }

    #[test]
    fn a_blob_response_is_classified_by_status_and_cache_headers() {
        // Catches: an error page or an unfollowed redirect counted as a warmed
        // blob, and a hit read off the wrong header.
        type Headers<'a> = &'a [(&'a str, &'a str)];
        let cases: [(u16, Headers<'_>, BlobOutcome); 18] = [
            (200, &[("x-cache", "Hit from cloudfront")], BlobOutcome::Hit),
            (
                200,
                &[("x-cache", "RefreshHit from cloudfront")],
                BlobOutcome::Hit,
            ),
            (200, &[("cf-cache-status", "HIT")], BlobOutcome::Hit),
            (200, &[("cf-cache-status", "REVALIDATED")], BlobOutcome::Hit),
            (200, &[("cf-cache-status", "UPDATING")], BlobOutcome::Hit),
            (200, &[("cf-cache-status", "STALE")], BlobOutcome::Hit),
            (200, &[("cf-cache-status", "EXPIRED")], BlobOutcome::Miss),
            (200, &[("cf-cache-status", "DYNAMIC")], BlobOutcome::Miss),
            (
                200,
                &[("x-cache", "Miss from cloudfront")],
                BlobOutcome::Miss,
            ),
            (200, &[("cf-cache-status", "MISS")], BlobOutcome::Miss),
            (200, &[], BlobOutcome::Unreported),
            (200, &[("server", "AmazonS3")], BlobOutcome::Unreported),
            (
                200,
                &[("x-cache", "Error from cloudfront")],
                BlobOutcome::Unreported,
            ),
            (206, &[("x-cache", "Hit from cloudfront")], BlobOutcome::Hit),
            (
                302,
                &[("x-cache", "Hit from cloudfront")],
                BlobOutcome::Failed,
            ),
            (404, &[], BlobOutcome::Failed),
            (
                403,
                &[("x-cache", "Hit from cloudfront")],
                BlobOutcome::Failed,
            ),
            (503, &[], BlobOutcome::Failed),
        ];
        for (status, headers, expected) in cases {
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            let mut map = reqwest::header::HeaderMap::new();
            for (name, value) in headers {
                map.insert(*name, value.parse().unwrap());
            }
            assert_eq!(outcome_of(status, &map), expected, "{status} {headers:?}");
        }
    }

    fn served(x_cache: &str) -> Result<reqwest::Response, String> {
        let response = http::Response::builder()
            .header("x-cache", x_cache)
            .body("blob bytes");
        Ok(reqwest::Response::from(response.unwrap()))
    }

    fn reset_mid_body() -> Result<reqwest::Response, String> {
        // Some bytes, then a reset.
        let body = reqwest::Body::wrap_stream(futures_util::stream::iter([
            Ok(b"part".to_vec()),
            Err(std::io::Error::other("connection reset")),
        ]));
        let response = http::Response::builder().body(body);
        Ok(reqwest::Response::from(response.unwrap()))
    }

    /// Warm a blob whose sends answer `script` in turn, and report the
    /// outcome and how many sends were made.
    fn warmed(
        script: Vec<Result<reqwest::Response, String>>,
        deadline: Instant,
    ) -> (BlobOutcome, usize) {
        let sends = script.len();
        let script = std::cell::RefCell::new(std::collections::VecDeque::from(script));
        let outcome = block_on(warm_blob(deadline, "blob", async || {
            script.borrow_mut().pop_front().expect("no send left")
        }));
        (outcome, sends - script.borrow().len())
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Run `future` to completion, failing rather than hanging if it takes
    /// over five seconds.
    fn block_on_within_five_seconds<T>(future: impl Future<Output = T>) -> T {
        block_on(async { tokio::time::timeout(Duration::from_secs(5), future).await })
            .expect("the future hung instead of ending at the deadline")
    }

    #[test]
    fn a_registry_that_never_answers_does_not_hold_back_another() {
        // Catches: one concurrency limit shared by all registries, so a
        // registry whose requests hang keeps another's blobs waiting for a slot.
        use registry::{Host, Repository};
        let digest = lock::fixtures::digest('1');
        // Many blobs of each, so that under one shared limit some of the
        // good registry's are sure to queue behind hung ones.
        let hung_paths: Vec<String> = (0..4 * PREWARM_PER_REGISTRY)
            .map(|i| format!("a{i}"))
            .collect();
        let fine_paths: Vec<String> = (0..4).map(|i| format!("b{i}")).collect();
        let blobs: Vec<_> = hung_paths
            .iter()
            .map(|path| (Host::Other("hung.example"), path))
            .chain(
                fine_paths
                    .iter()
                    .map(|path| (Host::Other("fine.example"), path)),
            )
            .map(|(host, path)| (Repository { host, path }, &digest))
            .collect();
        let deadline = Instant::now() + Duration::from_millis(100);
        let stats =
            block_on_within_five_seconds(warm_blobs(&blobs, deadline, &[], async |repo, _, _| {
                match repo.host {
                    Host::Other("hung.example") => std::future::pending().await,
                    _ => served("Hit from cloudfront"),
                }
            }))
            .stats;
        assert_eq!(stats["fine.example"].hits, fine_paths.len() as u64);
        assert_eq!(stats["hung.example"].past_deadline, hung_paths.len() as u64);
    }

    /// How a host answers every blob request: with this status and, if any,
    /// this `X-Cache`.
    type Answer = (u16, Option<&'static str>);

    /// Warm `blobs_each` blobs of each of `hosts`, which answer as given and
    /// of which `listed` are on the no-cache list. Returns what was found
    /// and how many requests each host received.
    fn warm_hosts(
        listed: &[&str],
        hosts: &[(&'static str, Answer)],
        blobs_each: usize,
    ) -> (Warmed<'static>, BTreeMap<String, usize>) {
        use registry::{Host, Repository};
        let digest: &'static Digest = Box::leak(Box::new(lock::fixtures::digest('1')));
        let paths: Vec<&'static str> = (0..blobs_each)
            .map(|i| &*Box::leak(format!("p{i}").into_boxed_str()))
            .collect();
        let blobs: Vec<_> = hosts
            .iter()
            .flat_map(|(host, _)| {
                paths.iter().map(move |path| Repository {
                    host: Host::Other(host),
                    path,
                })
            })
            .map(|repo| (repo, digest))
            .collect();
        let requests = std::cell::RefCell::new(BTreeMap::new());
        let warmed = block_on_within_five_seconds(warm_blobs(
            &blobs,
            Instant::now() + Duration::from_secs(10),
            listed,
            async |repo, _, _| {
                let host = repo.host.api_host();
                *requests.borrow_mut().entry(host.to_owned()).or_default() += 1;
                let (_, (status, x_cache)) = hosts.iter().find(|(h, _)| *h == host).unwrap();
                let mut response = http::Response::builder().status(*status);
                if let Some(x_cache) = x_cache {
                    response = response.header("x-cache", *x_cache);
                }
                Ok(reqwest::Response::from(
                    response.body("blob bytes").unwrap(),
                ))
            },
        ));
        (warmed, requests.into_inner())
    }

    const HIT: Answer = (200, Some("Hit from cloudfront"));
    const MISS: Answer = (200, Some("Miss from cloudfront"));
    const NO_HEADER: Answer = (200, None);

    #[test]
    fn a_listed_host_with_no_cache_header_is_probed_once_and_the_rest_left_alone() {
        // Catches: the whole corpus of a registry with no cache downloaded to
        // warm nothing, a host off the list probed, and the probe's result
        // spilling over to another host.
        let (warmed, requests) = warm_hosts(
            &["bare.example"],
            &[("bare.example", NO_HEADER), ("cdn.example", HIT)],
            20,
        );
        assert_eq!(requests["bare.example"], 1);
        let bare = &warmed.stats["bare.example"];
        assert!(bare.no_cache);
        assert_eq!((bare.unreported, bare.not_requested), (1, 19));
        assert_eq!(
            bare.summary(),
            "no blob cache, not warmed (1 checked, 19 not requested)"
        );
        assert!(warmed.cache_gained.is_empty());
        assert_eq!(requests["cdn.example"], 20);
        assert_eq!(warmed.stats["cdn.example"].hits, 20);
        assert!(!warmed.stats["cdn.example"].no_cache);
    }

    #[test]
    fn a_listed_host_that_gained_a_cache_is_warmed_in_full_and_named() {
        // Catches: a host left unwarmed because it is on the list, and a host
        // that gained a cache going unreported.
        let (warmed, requests) = warm_hosts(&["now-cdn.example"], &[("now-cdn.example", MISS)], 20);
        // The probe, then every blob.
        assert_eq!(requests["now-cdn.example"], 21);
        let stats = &warmed.stats["now-cdn.example"];
        assert_eq!((stats.misses, stats.no_cache), (20, false));
        assert_eq!(warmed.cache_gained, BTreeSet::from(["now-cdn.example"]));
    }

    #[test]
    fn a_listed_host_whose_probe_fails_is_warmed_like_any_other() {
        // Catches: a refused probe taken for a host with no cache, which
        // would leave it unwarmed without saying so.
        let (warmed, requests) = warm_hosts(&["down.example"], &[("down.example", (503, None))], 5);
        assert_eq!(requests["down.example"], 6);
        let stats = &warmed.stats["down.example"];
        assert_eq!((stats.failed, stats.no_cache), (5, false));
        assert!(warmed.cache_gained.is_empty());
    }

    #[test]
    fn only_a_reported_miss_counts_toward_the_wait() {
        // Catches: a registry's direct answers, with no cache header, counted
        // as misses and holding the run for the CDN wait, and a reported miss
        // not counted.
        let (warmed, requests) = warm_hosts(
            &[],
            &[("direct.example", NO_HEADER), ("edge.example", MISS)],
            10,
        );
        // Every blob is downloaded either way.
        assert_eq!(requests["direct.example"], 10);
        let direct = &warmed.stats["direct.example"];
        assert_eq!((direct.unreported, direct.misses), (10, 0));
        assert_eq!(warmed.stats["edge.example"].misses, 10);
        assert_eq!(
            direct.summary(),
            "10 blobs warmed (0 hits, 0 misses, 10 without a cache header), 0 failed, 0 past the deadline"
        );
    }

    #[test]
    fn a_blob_is_warmed_only_by_a_response_read_in_full() {
        let open = Instant::now() + Duration::from_secs(10);
        // Catches: a 2xx that says hit not being counted as one.
        assert_eq!(
            warmed(vec![served("Hit from cloudfront")], open),
            (BlobOutcome::Hit, 1)
        );
        // Catches: a body that fails mid-read counted as warmed, or given up on
        // without the second attempt.
        assert_eq!(
            warmed(vec![reset_mid_body(), served("Miss from cloudfront")], open),
            (BlobOutcome::Miss, 2)
        );
        // Catches: a body that keeps failing passing as a hit or miss, or being
        // attempted more than the limit.
        assert_eq!(
            warmed(
                vec![
                    reset_mid_body(),
                    reset_mid_body(),
                    served("Hit from cloudfront")
                ],
                open
            ),
            (BlobOutcome::Failed, 2)
        );
        // Catches: a send error, which already waited out its retries, being
        // sent again.
        assert_eq!(
            warmed(
                vec![Err("returned 503 (5 attempts)".into()), served("Hit")],
                open
            ),
            (BlobOutcome::Failed, 1)
        );
        // Catches: a request sent after the deadline.
        assert_eq!(
            warmed(
                vec![served("Hit from cloudfront")],
                Instant::now() - Duration::from_secs(1)
            ),
            (BlobOutcome::PastDeadline, 0)
        );
    }
}
