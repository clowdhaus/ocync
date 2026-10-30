//! Benchmark suite for comparing ocync against dregsy and regsync.

pub(crate) mod config_gen;
pub(crate) mod corpus;
pub(crate) mod ecr;
pub(crate) mod lock;
pub(crate) mod proxy;
pub(crate) mod registry;
pub(crate) mod regression;
pub(crate) mod remote;
pub(crate) mod report;
pub(crate) mod runner;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use corpus::Corpus;
use regression::Baseline;
use report::{BenchReport, ScenarioKind, ScenarioResult, ToolRun, find_scenario};
use runner::Tool;

/// Which tools a benchmark runs, shared by `bench` and `bench-remote`.
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

    /// Output directory for results (default: bench-results/{timestamp}).
    #[arg(long)]
    pub(crate) output: Option<String>,

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

/// The trimmed stdout of `command`, or `None` when it cannot be run, exits
/// non-zero, prints non-UTF-8, or prints nothing.
pub(crate) fn command_stdout(command: &mut std::process::Command) -> Option<String> {
    let output = command.output().ok().filter(|o| o.status.success())?;
    let text = String::from_utf8(output.stdout).ok()?;
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

    /// The cloud that hosts this registry, if it is one of the three.
    fn provider(self) -> Option<Provider> {
        match self {
            Self::Ecr => Some(Provider::Aws),
            Self::Gcr => Some(Provider::Gcp),
            Self::Acr => Some(Provider::Azure),
            Self::Other => None,
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
    let (transfers, prepare) = plan(args.scenario, &corpus, args.paths.partial_overrides())?;
    let lock = (!transfers.is_empty())
        .then(|| lock::Lock::load(&args.paths.lock_path()))
        .transpose()?;
    let corpus_locked_at = lock.as_ref().map(|lock| lock.locked_at.clone());
    let http = lock::http_client()?;
    let auth = corpus.dockerhub_auth.as_ref();
    if let Some(lock) = &lock {
        // Before the build and the CDN pre-warm, so a moved source costs
        // seconds rather than a wasted run.
        let refs = transfers
            .iter()
            .flat_map(|transfer| scenario_refs(transfer, &corpus))
            .collect();
        lock::ensure_unmoved(&http, lock, &refs, auth).await?;
    }

    let tools = &args.tool_selection.tools;

    eprintln!(
        "bench: scenario={} tools={} images={} tags={}",
        value_name(&args.scenario),
        runner::join_tools(tools),
        corpus.images.len(),
        corpus.total_tags(),
    );

    let ecr_client = ecr::client().await;
    ecr::validate_credentials(&ecr_client).await?;

    // Build ocync first, so its version probe reads the binary the run will
    // actually execute rather than whatever is on PATH from instance setup.
    let workspace_root = Path::new(".");
    if tools.contains(&Tool::Ocync) {
        eprintln!("bench: building ocync in release mode...");
        runner::build_ocync(workspace_root).await?;
    }

    // Check each tool binary and collect versions, along with any binary a tool
    // relays its transfers through, whose version moves that tool's numbers.
    let mut tool_versions: BTreeMap<Tool, String> = BTreeMap::new();
    let mut relay_versions: BTreeMap<String, String> = BTreeMap::new();
    for &tool in tools {
        let version = runner::check_tool(tool, workspace_root).await?;
        eprintln!("  {}: {version}", tool);
        tool_versions.insert(tool, version);
    }

    // Relays are shared between tools, so they are resolved once over the whole
    // set rather than per tool.
    for (binary, version) in runner::check_relays(tools).await? {
        eprintln!("  {binary} (relay): {version}");
        relay_versions.insert(binary, version);
    }

    let timestamp = utc_timestamp();
    let output_dir = match &args.output {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(format!("bench/results/{timestamp}")),
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

    let mut report = BenchReport {
        timestamp,
        instance,
        corpus_size: corpus.images.len(),
        total_tags: corpus.total_tags(),
        tool_versions,
        relay_versions,
        scenarios: Vec::new(),
    };

    // Pre-warm CDN so all tools see identically cached source manifests.
    //
    // The prepare scenario needs none of it. It runs one tool, pulls no blobs,
    // and reads a different corpus than the one warmed here, so warming for it
    // is hundreds of pointless requests against registries that rate-limit.
    // A mixed run still warms, because the scenarios beside it do transfer.
    if args.skip_prewarm {
        eprintln!("bench: skipping CDN pre-warm (--skip-prewarm)");
    } else if transfers.is_empty() {
        eprintln!("bench: skipping CDN pre-warm (prepare transfers nothing)");
    } else {
        cdn_prewarm(&corpus).await;
    }

    // A scenario after the first re-checks the sources, so a move during a long
    // run cannot put different content under later numbers. The first was
    // checked before the build. Any failure stops the scenarios still to run;
    // the finished ones are still reported and recorded, and the run then
    // returns the reason.
    let mut stopped: Option<String> = None;
    if let Some(lock) = &lock {
        for (position, transfer) in transfers.iter().enumerate() {
            let refs = scenario_refs(transfer, &corpus);
            if position > 0 {
                let check = lock::ensure_unmoved(&http, lock, &refs, auth).await;
                match lock::between_scenarios(check) {
                    lock::Next::Proceed { warning } => {
                        if let Some(warning) = warning {
                            eprintln!("WARNING: {warning}");
                        }
                    }
                    lock::Next::Stop(reason) => {
                        stopped = Some(reason);
                        break;
                    }
                }
            }

            // Randomize tool execution order to prevent systematic bias from
            // CDN warming, network burst credit depletion, etc.
            let mut shuffled = tools.to_vec();
            let seed = shuffle_tools(&mut shuffled);
            let tool_order: Vec<String> = shuffled.iter().map(ToString::to_string).collect();
            eprintln!(
                "bench: tool order for {transfer}: {} (seed: {seed})",
                tool_order.join(", ")
            );

            let target = ecr::Target {
                client: &ecr_client,
                lock,
            };
            let outcome: Result<(), Box<dyn std::error::Error>> = async {
                match transfer {
                    Transfer::Sync => {
                        let (cold, warm) =
                            run_sync(&args, &corpus, &target, &shuffled, &output_dir, seed).await?;
                        report.scenarios.push(cold);
                        report.scenarios.push(warm);
                    }
                    Transfer::Partial(partial_corpus) => {
                        let result = run_partial(
                            &args,
                            &corpus,
                            partial_corpus,
                            &target,
                            &shuffled,
                            &output_dir,
                            seed,
                        )
                        .await?;
                        report.scenarios.push(result);
                    }
                    Transfer::Scale => {
                        // Scale scenario uses ocync only per spec.
                        if tools.contains(&Tool::Ocync) {
                            let ocync_only = vec![Tool::Ocync];
                            let scale_results =
                                run_scale(&args, &corpus, &target, &ocync_only, &output_dir)
                                    .await?;
                            report.scenarios.extend(scale_results);
                        } else {
                            eprintln!("bench: scale scenario requires ocync (skipping)");
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = outcome {
                // A source that moved mid-scenario is not the tool's fault.
                let recheck = lock::ensure_unmoved(&http, lock, &refs, auth).await;
                stopped = Some(lock::explain(error.to_string(), recheck));
                break;
            }
        }
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

    report::write_report(&output_dir, &report)?;
    eprintln!("bench: summary written to {}", output_dir.display());

    let target_registry = &corpus.settings.target_registry;
    let registry = RegistryKind::of(target_registry);
    let reg_key = registry.archive_stem();
    let git_ref = git_ref();
    let record = report::RunRecord {
        timestamp: report.timestamp.clone(),
        git_ref,
        corpus_locked_at: corpus_locked_at.clone(),
        machine: report::MachineInfo {
            provider: registry.provider(),
            instance_type: report.instance.instance_type.clone(),
            arch: report.instance.arch.clone(),
            vcpus: report.instance.vcpus,
            memory_gib: report.instance.memory_mib as f64 / 1024.0,
            network: report.instance.network_performance.clone(),
            region: report.instance.region.clone(),
            image_id: report.instance.image_id.clone(),
        },
        relays: report.relay_versions.clone(),
        corpus: report::CorpusInfo {
            images: report.corpus_size,
            tags: report.total_tags,
        },
        scenarios: report
            .scenarios
            .iter()
            .map(|s| report::ScenarioRecord {
                name: s.kind.label(),
                shuffle_seed: s.shuffle_seed,
                tool_order: s.tool_order.iter().map(ToString::to_string).collect(),
                tools: s
                    .runs
                    .iter()
                    .map(|r| report::ToolRecord {
                        tool: r.tool.to_string(),
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
    };
    // The archive is tracked in git and read as a history of comparable runs.
    // A run whose machine could not be identified is not comparable to one on
    // a known instance, and the harness already says so with a WARNING per
    // missing field. Appending it anyway puts a laptop's numbers next to the
    // rig's under the same key, where nothing downstream can tell them apart.
    // The summary is still written; only the shared history is protected.
    let results_dir = Path::new("bench/results");
    if record.machine.instance_type == "unknown" {
        eprintln!(
            "bench: run record NOT appended (instance metadata unknown, so this \
             run is not comparable to the archived ones); summary only"
        );
    } else {
        report::append_record(results_dir, reg_key, record)?;
        eprintln!("bench: run record appended to bench/results/{reg_key}.json");
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

/// A scenario that syncs the main corpus, and so is checked against its lock.
#[derive(Debug)]
enum Transfer {
    /// Full sync, then re-sync.
    Sync,
    /// Incremental sync after some tags changed, of this corpus: the base
    /// corpus with the override tags applied.
    Partial(Corpus),
    /// Sync at increasing corpus sizes.
    Scale,
}

impl std::fmt::Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sync => "Sync",
            Self::Partial(_) => "Partial",
            Self::Scale => "Scale",
        })
    }
}

/// The scenarios a run executes, in order: those that sync from the lock, and
/// whether prepare follows. The overrides are read once, and only for a run
/// that has a partial scenario.
fn plan(
    requested: Scenario,
    corpus: &Corpus,
    overrides_path: &str,
) -> Result<(Vec<Transfer>, bool), String> {
    let partial = || {
        let overrides = corpus::load_overrides(overrides_path)?;
        Ok::<_, String>(Transfer::Partial(
            corpus.with_overrides(&overrides.overrides),
        ))
    };
    Ok(match requested {
        Scenario::Sync => (vec![Transfer::Sync], false),
        Scenario::Partial => (vec![partial()?], false),
        Scenario::Scale => (vec![Transfer::Scale], false),
        Scenario::Prepare => (Vec::new(), true),
        Scenario::All => (vec![Transfer::Sync, partial()?, Transfer::Scale], true),
    })
}

/// Every (source, tag) a scenario syncs.
fn scenario_refs(transfer: &Transfer, corpus: &Corpus) -> BTreeSet<lock::ImageRef> {
    let mut refs = lock::refs_of(&corpus.images);
    if let Transfer::Partial(partial) = transfer {
        refs.extend(lock::refs_of(&partial.images));
    }
    refs
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

/// The ocync run of the cold sync scenario, the only run a baseline is
/// comparable against.
fn cold_ocync_run(scenarios: &[ScenarioResult]) -> Option<&ToolRun> {
    find_scenario(scenarios, ScenarioKind::Cold)?
        .runs
        .iter()
        .find(|r| r.tool == Tool::Ocync)
}

/// Compare the current ocync run against a stored baseline.
///
/// Uses the ocync run of the cold sync scenario, never a scale or prepare run.
/// Returns an error if regression is detected.
fn check_regression(
    report: &BenchReport,
    threshold: u32,
    corpus_locked_at: Option<&lock::LockedAt>,
) -> Result<(), Box<dyn std::error::Error>> {
    let baseline_path = PathBuf::from("bench/results/baseline.json");

    if let Some(run) = cold_ocync_run(&report.scenarios) {
        let current = Baseline {
            wall_clock_secs: run.wall_clock_secs,
            requests: run.proxy_metrics.as_ref().map_or(0, |m| m.requests),
            total_bytes: run.proxy_metrics.as_ref().map_or(0, |m| m.response_bytes),
            corpus_locked_at: corpus_locked_at.cloned(),
        };

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
        eprintln!("bench: no cold sync ocync run; regression check skipped");
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

/// Run a single tool against the corpus and return a `ToolRun`.
async fn run_single_tool(
    args: &BenchArgs,
    tool: Tool,
    corpus: &Corpus,
    config_dir: &Path,
    output_dir: &Path,
    pass: Pass,
    dry_run: bool,
) -> Result<ToolRun, Box<dyn std::error::Error>> {
    let config_content = match tool {
        Tool::Ocync => config_gen::ocync_config(corpus),
        Tool::Dregsy => config_gen::dregsy_config(corpus),
        Tool::Regsync => config_gen::regsync_config(corpus),
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
        let log_path = output_dir.join(format!("{tool}-{pass}-proxy.jsonl"));
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
    let workspace_root = Path::new(".");
    let result = runner::run_tool(
        tool,
        &config_path,
        proxy_url.as_deref(),
        workspace_root,
        dry_run,
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
            "  {}: wall_clock={:.1}s status={} rss={} requests={} response_bytes={} mounts={}/{} source_pulls={}/{}",
            tool,
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
            "  {}: wall_clock={:.1}s status={} rss={}",
            tool,
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
    ensure_exited_zero(tool, pass, result.exit_status)?;

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
fn ensure_exited_zero(tool: Tool, pass: Pass, status: ExitStatus) -> Result<(), String> {
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

/// Sync scenario: full sync (measured), then immediate re-sync
/// (measured) with the same config directory so that ocync's
/// `TransferStateCache` persists between runs.
async fn run_sync(
    args: &BenchArgs,
    corpus: &Corpus,
    target: &ecr::Target<'_>,
    tools: &[Tool],
    output_dir: &Path,
    shuffle_seed: u64,
) -> Result<(ScenarioResult, ScenarioResult), Box<dyn std::error::Error>> {
    progress("bench: running cold+warm scenario");
    let mut cold_runs = Vec::new();
    let mut warm_runs = Vec::new();

    for &tool in tools {
        prepare_clean_slate();

        // Delete repos first to guarantee a truly empty target, then recreate.
        // create_repos alone skips existing repos which may contain partial
        // data from aborted runs.
        ecr::delete_repos(target.client, corpus).await?;
        ecr::create_repos(target.client, corpus).await?;

        // Single config_dir for cold + warm. The cache file lives
        // under this directory, so reusing it means the warm run
        // sees a fully populated cache.
        let config_dir = tempfile::tempdir()?;

        progress(&format!("  cold: {tool}"));
        let run_result: Result<(), Box<dyn std::error::Error>> = async {
            let cold = run_single_tool(
                args,
                tool,
                corpus,
                config_dir.path(),
                output_dir,
                Pass::Measured(ScenarioKind::Cold),
                false,
            )
            .await?;
            progress(&format!(
                "  cold: {tool} complete ({:.1}s)",
                cold.wall_clock_secs
            ));
            cold_runs.push(cold);

            ecr::verify_sync(target, corpus, tool, ScenarioKind::Cold).await?;

            progress(&format!("  warm: {tool}"));
            let warm = run_single_tool(
                args,
                tool,
                corpus,
                config_dir.path(),
                output_dir,
                Pass::Measured(ScenarioKind::Warm),
                false,
            )
            .await?;
            progress(&format!(
                "  warm: {tool} complete ({:.1}s)",
                warm.wall_clock_secs
            ));
            warm_runs.push(warm);

            ecr::verify_sync(target, corpus, tool, ScenarioKind::Warm).await?;

            Ok(())
        }
        .await;

        // Always clean up ECR repos, even if the tool run failed. A repo that
        // cannot be deleted fails the pass, so its content cannot satisfy the
        // next tool's digest check.
        let cleanup = ecr::delete_repos(target.client, corpus).await;
        finish_pass(run_result, cleanup)?;

        cooldown().await;
    }

    Ok((
        ScenarioResult {
            kind: ScenarioKind::Cold,
            runs: cold_runs,
            shuffle_seed,
            tool_order: tools.to_vec(),
        },
        ScenarioResult {
            kind: ScenarioKind::Warm,
            runs: warm_runs,
            shuffle_seed,
            tool_order: tools.to_vec(),
        },
    ))
}

/// Partial scenario: prime with base corpus, then sync with partial corpus.
async fn run_partial(
    args: &BenchArgs,
    base_corpus: &Corpus,
    partial_corpus: &Corpus,
    target: &ecr::Target<'_>,
    tools: &[Tool],
    output_dir: &Path,
    shuffle_seed: u64,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    progress("bench: running partial scenario");
    let mut runs = Vec::new();

    for &tool in tools {
        prepare_clean_slate();
        ecr::delete_repos(target.client, base_corpus).await?;
        ecr::create_repos(target.client, base_corpus).await?;
        progress(&format!("  partial: {} (priming)", tool));

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
                false,
            )
            .await?;
            progress(&format!("  partial: {} measuring", tool));

            let run = run_single_tool(
                args,
                tool,
                partial_corpus,
                config_dir.path(),
                output_dir,
                Pass::Measured(ScenarioKind::Partial),
                false,
            )
            .await?;
            progress(&format!(
                "  partial: {} complete ({:.1}s)",
                tool, run.wall_clock_secs
            ));
            runs.push(run);

            ecr::verify_sync(target, partial_corpus, tool, ScenarioKind::Partial).await?;
            Ok(())
        }
        .await;

        // Always clean up ECR repos, even if the tool run failed.
        let cleanup = ecr::delete_repos(target.client, base_corpus).await;
        finish_pass(run_result, cleanup)?;

        cooldown().await;
    }

    Ok(ScenarioResult {
        kind: ScenarioKind::Partial,
        runs,
        shuffle_seed,
        tool_order: tools.to_vec(),
    })
}

/// Scale scenario: run at increasing corpus sizes [10, 25, 50, all], one
/// scenario result per size so each carries the full metric table.
async fn run_scale(
    args: &BenchArgs,
    corpus: &Corpus,
    target: &ecr::Target<'_>,
    tools: &[Tool],
    output_dir: &Path,
) -> Result<Vec<ScenarioResult>, Box<dyn std::error::Error>> {
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
            prepare_clean_slate();
            ecr::delete_repos(target.client, &subset).await?;
            ecr::create_repos(target.client, &subset).await?;
            progress(&format!("  scale: {} @ {size} images", tool));

            let run_result: Result<(), Box<dyn std::error::Error>> = async {
                let config_dir = tempfile::tempdir()?;
                let run = run_single_tool(
                    args,
                    tool,
                    &subset,
                    config_dir.path(),
                    output_dir,
                    Pass::Measured(ScenarioKind::Scale { images: size }),
                    false,
                )
                .await?;
                runs.push(run);

                let kind = ScenarioKind::Scale { images: size };
                ecr::verify_sync(target, &subset, tool, kind).await?;
                Ok(())
            }
            .await;

            // Always clean up ECR repos, even if the tool run failed.
            let cleanup = ecr::delete_repos(target.client, &subset).await;
            finish_pass(run_result, cleanup)?;

            cooldown().await;
        }

        scale_results.push(ScenarioResult::unshuffled(
            ScenarioKind::Scale { images: size },
            runs,
        ));
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
        true,
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

/// Per-registry rate controller for CDN blob pre-warm.
///
/// Uses multiplicative increase / multiplicative decrease (MIMD) with
/// congestion epoch suppression. Each registry gets its own controller
/// so a slow registry doesn't throttle fast ones.
struct PrewarmRateController {
    /// Minimum delay between requests to this registry.
    interval: Duration,
    /// Last time a request was issued to this registry.
    last_request: Instant,
    /// Last time a throttle (429) was processed.
    last_throttle: Instant,
    /// Congestion epoch -- prevents cascade halvings from burst 429s.
    epoch: Duration,
}

impl PrewarmRateController {
    /// Floor: 20 req/sec max.
    const MIN_INTERVAL: Duration = Duration::from_millis(50);
    /// Ceiling: never wait longer than 30s between requests.
    const MAX_INTERVAL: Duration = Duration::from_secs(30);
    /// Congestion epoch: 100ms (matches AIMD design).
    const DEFAULT_EPOCH: Duration = Duration::from_millis(100);

    /// Create a new controller with the given initial interval.
    fn new(initial_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            interval: initial_interval,
            last_request: now - initial_interval,
            last_throttle: now - Self::DEFAULT_EPOCH - Duration::from_millis(1),
            epoch: Self::DEFAULT_EPOCH,
        }
    }

    /// Returns how long to sleep before the next request, or `None` if ready.
    fn delay(&self) -> Option<Duration> {
        let elapsed = self.last_request.elapsed();
        if elapsed < self.interval {
            Some(self.interval - elapsed)
        } else {
            None
        }
    }

    /// Mark that a request was just issued.
    fn mark_sent(&mut self) {
        self.last_request = Instant::now();
    }

    /// Record a successful response -- multiplicative increase.
    ///
    /// Decreases interval by 10% (floor [`MIN_INTERVAL`](Self::MIN_INTERVAL)).
    fn on_success(&mut self) {
        self.interval = self.interval.mul_f64(0.9).max(Self::MIN_INTERVAL);
    }

    /// Record a 429 throttle -- multiplicative decrease.
    ///
    /// Doubles interval (cap [`MAX_INTERVAL`](Self::MAX_INTERVAL)). If a
    /// throttle already occurred within the congestion epoch, this call is
    /// suppressed.
    fn on_throttle(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_throttle) > self.epoch {
            self.interval = (self.interval * 2).min(Self::MAX_INTERVAL);
            self.last_throttle = now;
        }
    }
}

/// Per-registry CDN pre-warm statistics.
#[derive(Default)]
struct PrewarmStats {
    hits: u64,
    misses: u64,
    errors: u64,
}

/// Pull every source manifest once to warm CDN edge caches.
///
/// Runs before any tool so all tools see identically warm CDN. Uses
/// OCI token exchange for authentication so requests succeed (2xx) and
/// `CloudFront` actually caches the responses. Unauthenticated 401s are
/// NOT cached by CDN and would warm nothing.
async fn cdn_prewarm(corpus: &Corpus) {
    let total = corpus.total_tags();
    let auth = corpus.dockerhub_auth.as_ref();
    eprintln!("bench: pre-warming CDN ({total} manifests)...");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut tokens = registry::Tokens::default();

    let mut hits = 0u64;
    let mut misses = 0u64;
    let mut errors = 0u64;

    for image in &corpus.images {
        let Some(source) = registry::split_source(&image.source) else {
            continue;
        };
        let (registry, repo) = (source.host.api_host(), source.path);
        if let Err(e) = tokens.get(&client, &source, auth).await {
            errors += image.tags.len() as u64;
            eprintln!("  CDN pre-warm: {registry}/{repo}: auth failed ({e}), skipping");
            continue;
        }

        for tag in &image.tags {
            let url = format!("https://{registry}/v2/{repo}/manifests/{tag}");
            let resp = tokens
                .send(&client, &source, auth, |token| {
                    client.head(&url).bearer_auth(token)
                })
                .await;
            match resp {
                Ok(r) if r.status().is_success() || r.status().is_redirection() => {
                    let x_cache = r
                        .headers()
                        .get("x-cache")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if x_cache.to_ascii_lowercase().contains("hit") {
                        hits += 1;
                    } else {
                        misses += 1;
                    }
                }
                Ok(r) => {
                    errors += 1;
                    eprintln!("  CDN pre-warm: {registry}/{repo}:{tag} -> {}", r.status());
                }
                Err(e) => {
                    errors += 1;
                    eprintln!("  CDN pre-warm: {registry}/{repo}:{tag} -> {e}");
                }
            }
        }
    }

    // The lock check already requested every one of these manifests, so the hit
    // count says nothing about how cold the edge was before this run.
    eprintln!(
        "bench: manifest CDN pre-warm done ({hits} hits, {misses} misses, {errors} errors; \
         after the lock check requested them, so hits do not show a cold edge)"
    );

    // Phase 2: Warm blob CDN by fetching unique blobs.
    //
    // Blob URLs are served via 302 redirects to CloudFront -- completely
    // different cache keys from manifests. Without this pass the first
    // tool to run pays a CDN miss penalty on every blob.
    eprintln!("bench: pre-warming blob CDN...");

    // Redirect-following client with a generous timeout for large blobs.
    let blob_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();

    // (repository, digest) pairs -- deduped so shared layers are fetched
    // exactly once.
    let mut unique_blobs: HashSet<(registry::RepoKey, String)> = HashSet::new();

    for image in &corpus.images {
        let Some(source) = registry::split_source(&image.source) else {
            continue;
        };
        let key = registry::RepoKey::from(&source);
        let (registry, repo) = (source.host.api_host(), source.path);

        // The manifest pass above already reported a repository that fails.
        if tokens.get(&client, &source, auth).await.is_err() {
            continue;
        }

        // GET one manifest per image to discover blob digests.
        // All tags for the same image reference the same layers, so the
        // first tag is sufficient.
        if let Some(tag) = image.tags.first() {
            let url = format!("https://{registry}/v2/{repo}/manifests/{tag}");
            let resp = tokens
                .send(&client, &source, auth, |token| {
                    client
                        .get(&url)
                        .bearer_auth(token)
                        .header("Accept", registry::MANIFEST_ACCEPT)
                })
                .await;

            if let Ok(r) = resp {
                if let Ok(body) = r.text().await {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "a public image manifest; a parse error is dropped unread"
                    )]
                    if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&body) {
                        let token = tokens.cached(&key).unwrap_or_default();
                        extract_blob_digests(&client, &manifest, &key, token, &mut unique_blobs)
                            .await;
                    }
                }
            }
        }
    }

    let blob_count = unique_blobs.len();
    eprintln!(
        "bench: warming {blob_count} unique blobs (20 concurrent, per-registry rate limited)..."
    );

    // Warm blobs concurrently (20 at a time) to avoid waiting 15-30 minutes
    // on a sequential 55 GB download. Uses a semaphore to bound concurrency
    // within a single-threaded tokio runtime.
    let blobs_vec: Vec<_> = unique_blobs.into_iter().collect();

    // Per-registry rate controllers and stats, initialized in a single pass.
    let rate_controllers: std::cell::RefCell<HashMap<String, PrewarmRateController>> =
        std::cell::RefCell::new(HashMap::new());
    let per_registry_stats: std::cell::RefCell<HashMap<String, PrewarmStats>> =
        std::cell::RefCell::new(HashMap::new());
    for (key, _) in &blobs_vec {
        rate_controllers
            .borrow_mut()
            .entry(key.host.clone())
            .or_insert_with(|| PrewarmRateController::new(Duration::from_millis(500)));
        per_registry_stats
            .borrow_mut()
            .entry(key.host.clone())
            .or_default();
    }

    // Total pre-warm timeout: 8 minutes (leaves 2 min headroom in 10-min budget).
    let prewarm_deadline = Instant::now() + Duration::from_secs(480);

    // Each repository's current token. A blob answered 401 exchanges a new one
    // for the repository's later blobs too.
    let live_tokens: std::cell::RefCell<HashMap<registry::RepoKey, String>> =
        std::cell::RefCell::new(
            blobs_vec
                .iter()
                .filter_map(|(key, _)| Some((key.clone(), tokens.cached(key)?.to_owned())))
                .collect(),
        );

    let sem = tokio::sync::Semaphore::new(20);
    const MAX_RETRIES: u32 = 3;

    let futs: Vec<_> = blobs_vec
        .iter()
        .map(|(key, digest)| {
            let sem = &sem;
            let blob_client = &blob_client;
            let per_registry_stats = &per_registry_stats;
            let rate_controllers = &rate_controllers;
            let live_tokens = &live_tokens;
            let registry = key.host.clone();
            let digest = digest.clone();
            async move {
                if !live_tokens.borrow().contains_key(key) {
                    per_registry_stats
                        .borrow_mut()
                        .entry(registry.clone())
                        .or_default()
                        .errors += 1;
                    return;
                }

                // Check pre-warm timeout before starting.
                if Instant::now() > prewarm_deadline {
                    return;
                }

                // Rate limit: compute delay, drop borrow, sleep, re-borrow.
                let delay = rate_controllers.borrow().get(&registry).unwrap().delay();
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                rate_controllers
                    .borrow_mut()
                    .get_mut(&registry)
                    .unwrap()
                    .mark_sent();

                // Acquire semaphore AFTER rate-controller wait.
                let _permit = sem.acquire().await.unwrap();

                let mut retries = 0u32;
                loop {
                    let resp = get_blob(blob_client, key, &digest, live_tokens, auth).await;
                    match resp {
                        Ok(r) if r.status().as_u16() == 429 => {
                            rate_controllers
                                .borrow_mut()
                                .get_mut(&registry)
                                .unwrap()
                                .on_throttle();
                            retries += 1;
                            if retries >= MAX_RETRIES {
                                eprintln!(
                                    "  blob pre-warm: {registry} {digest}: max retries on 429"
                                );
                                per_registry_stats
                                    .borrow_mut()
                                    .entry(registry.clone())
                                    .or_default()
                                    .errors += 1;
                                break;
                            }
                            let delay = rate_controllers.borrow().get(&registry).unwrap().delay();
                            if let Some(d) = delay {
                                tokio::time::sleep(d).await;
                            }
                            rate_controllers
                                .borrow_mut()
                                .get_mut(&registry)
                                .unwrap()
                                .mark_sent();
                            continue;
                        }
                        Ok(r) if r.status().is_success() || r.status().is_redirection() => {
                            let x_cache = r
                                .headers()
                                .get("x-cache")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            {
                                let mut stats = per_registry_stats.borrow_mut();
                                let entry = stats.entry(registry.clone()).or_default();
                                if x_cache.to_ascii_lowercase().contains("hit") {
                                    entry.hits += 1;
                                } else {
                                    entry.misses += 1;
                                }
                            }
                            // Consume body to complete the transfer.
                            let _ = r.bytes().await;
                            rate_controllers
                                .borrow_mut()
                                .get_mut(&registry)
                                .unwrap()
                                .on_success();
                            break;
                        }
                        Ok(r) => {
                            let status = r.status();
                            eprintln!("  blob pre-warm: {registry} {digest}: {status}");
                            per_registry_stats
                                .borrow_mut()
                                .entry(registry.clone())
                                .or_default()
                                .errors += 1;
                            break;
                        }
                        Err(e) => {
                            eprintln!("  blob pre-warm: {registry} {digest}: {e}");
                            per_registry_stats
                                .borrow_mut()
                                .entry(registry.clone())
                                .or_default()
                                .errors += 1;
                            break;
                        }
                    }
                }
            }
        })
        .collect();
    futures_util::future::join_all(futs).await;

    let (total_hits, total_misses, total_errors) = {
        let stats = per_registry_stats.borrow();
        let mut hits_sum = 0u64;
        let mut misses_sum = 0u64;
        let mut errors_sum = 0u64;
        for (registry, s) in stats.iter() {
            hits_sum += s.hits;
            misses_sum += s.misses;
            errors_sum += s.errors;
            let rate_ms = rate_controllers
                .borrow()
                .get(registry)
                .unwrap()
                .interval
                .as_millis();
            eprintln!(
                "  blob pre-warm: {registry}: {} hits, {} misses, {} errors, {rate_ms}ms/req",
                s.hits, s.misses, s.errors
            );
        }
        (hits_sum, misses_sum, errors_sum)
    };
    eprintln!(
        "bench: blob CDN pre-warm done ({total_hits} hits, {total_misses} misses, {total_errors} errors)"
    );

    // Wait for CDN propagation. CloudFront edges need time to fully
    // populate after the first request hits the origin.
    let wait_secs = 360; // 6 minutes
    eprintln!("bench: waiting {wait_secs}s for CDN propagation...");
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
}

/// GET a blob with the repository's current token. A 401 means the token
/// expired, so one is exchanged unless another blob already replaced it, and
/// the request is sent once more.
async fn get_blob(
    client: &reqwest::Client,
    key: &registry::RepoKey,
    digest: &str,
    live_tokens: &std::cell::RefCell<HashMap<registry::RepoKey, String>>,
    auth: Option<&corpus::DockerHubAuth>,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("https://{}/v2/{}/blobs/{digest}", key.host, key.path);
    let used = live_tokens.borrow().get(key).cloned().unwrap_or_default();
    let response = client.get(&url).bearer_auth(&used).send().await?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(response);
    }
    let current = live_tokens.borrow().get(key).cloned().unwrap_or_default();
    let fresh = if current == used {
        match registry::oci_token(client, &key.repository(), auth).await {
            Ok(fresh) => fresh,
            Err(_) => return Ok(response),
        }
    } else {
        current
    };
    live_tokens.borrow_mut().insert(key.clone(), fresh.clone());
    client.get(&url).bearer_auth(fresh).send().await
}

/// Collect layer and config blob digests from a manifest JSON value.
///
/// Works for both image manifests (`layers` + `config`) and index manifests
/// (`manifests` array -- fetches each child to discover its layers).
async fn extract_blob_digests(
    client: &reqwest::Client,
    manifest: &serde_json::Value,
    repo: &registry::RepoKey,
    token: &str,
    blobs: &mut HashSet<(registry::RepoKey, String)>,
) {
    // Image manifest: has `layers` array.
    if collect_image_blobs(manifest, repo, blobs) {
        return;
    }

    // Index manifest: has `manifests` array. Fetch each child to discover
    // its layers.
    if let Some(children) = manifest.get("manifests").and_then(|m| m.as_array()) {
        for child in children {
            let digest = match child.get("digest").and_then(|d| d.as_str()) {
                Some(d) => d,
                None => continue,
            };
            let url = format!("https://{}/v2/{}/manifests/{digest}", repo.host, repo.path);
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header(
                    "Accept",
                    "application/vnd.oci.image.manifest.v1+json,\
                     application/vnd.docker.distribution.manifest.v2+json",
                )
                .send()
                .await;
            if let Ok(r) = resp {
                if let Ok(body) = r.text().await {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "a public image manifest; a parse error is dropped unread"
                    )]
                    if let Ok(child_manifest) = serde_json::from_str::<serde_json::Value>(&body) {
                        collect_image_blobs(&child_manifest, repo, blobs);
                    }
                }
            }
        }
    }
}

/// Extract layer and config digests from an image manifest JSON.
///
/// Returns `true` if this is an image manifest (has `layers`), `false`
/// otherwise (caller should try index parsing).
fn collect_image_blobs(
    manifest: &serde_json::Value,
    repo: &registry::RepoKey,
    blobs: &mut HashSet<(registry::RepoKey, String)>,
) -> bool {
    let layers = match manifest.get("layers").and_then(|l| l.as_array()) {
        Some(l) => l,
        None => return false,
    };
    for layer in layers {
        if let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) {
            blobs.insert((repo.clone(), digest.to_owned()));
        }
    }
    if let Some(config) = manifest.get("config") {
        if let Some(digest) = config.get("digest").and_then(|d| d.as_str()) {
            blobs.insert((repo.clone(), digest.to_owned()));
        }
    }
    true
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

/// Shuffle a tool list in-place using Fisher-Yates with a timestamp seed.
/// Not cryptographic -- just enough to randomize execution order across runs.
/// Returns the seed used for reproducibility.
fn shuffle_tools(tools: &mut [Tool]) -> u64 {
    if tools.len() <= 1 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    let seed = hasher.finish();
    let mut state = seed;
    for i in (1..tools.len()).rev() {
        // xorshift64 step for cheap pseudo-randomness.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state as usize) % (i + 1);
        tools.swap(i, j);
    }
    seed
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

/// Returns a UTC timestamp string in `YYYY-MM-DD-HHMMSS` format.
fn utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Manual UTC formatting -- avoids a chrono/time dependency for one call site.
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
            ensure_exited_zero(Tool::Ocync, pass, ExitStatus::from_raw(0)),
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
                ensure_exited_zero(Tool::Ocync, pass, ExitStatus::from_raw(raw_status)),
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
    fn registry_kind_names_archive_and_provider() {
        for (host, stem, provider) in [
            (
                "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                "ecr",
                Some(Provider::Aws),
            ),
            (
                "111111111111.dkr.ecr.eu-west-1.amazonaws.com",
                "ecr",
                Some(Provider::Aws),
            ),
            ("gcr.io", "gcr", Some(Provider::Gcp)),
            ("us-docker.pkg.dev", "gcr", Some(Provider::Gcp)),
            ("europe-west1-docker.pkg.dev", "gcr", Some(Provider::Gcp)),
            ("myregistry.azurecr.io", "acr", Some(Provider::Azure)),
            (
                "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com",
                "ecr",
                Some(Provider::Aws),
            ),
            (
                "123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn",
                "ecr",
                Some(Provider::Aws),
            ),
            (
                "123456789012.DKR.ECR.US-EAST-1.AMAZONAWS.COM",
                "ecr",
                Some(Provider::Aws),
            ),
            ("myreg.azurecr.us", "acr", Some(Provider::Azure)),
            ("myreg.azurecr.cn", "acr", Some(Provider::Azure)),
            ("MyReg.AzureCR.io", "acr", Some(Provider::Azure)),
            ("ecr.example.com", "other", None),
            ("public.ecr.aws", "other", None),
            ("ghcr.io", "other", None),
            ("docker.io", "other", None),
            ("quay.io", "other", None),
        ] {
            let kind = RegistryKind::of(host);
            assert_eq!(kind.archive_stem(), stem, "{host}");
            assert_eq!(kind.provider(), provider, "{host}");
        }
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
        let (transfers, prepare) =
            plan(Scenario::All, &corpus, overrides.to_str().unwrap()).unwrap();
        assert!(prepare);
        let names: Vec<String> = transfers.iter().map(ToString::to_string).collect();
        assert_eq!(names, ["Sync", "Partial", "Scale"]);

        // (scenario, ref, in the scenario's refs)
        let python = "public.ecr.aws/lambda/python";
        let corretto = "public.ecr.aws/amazoncorretto/amazoncorretto";
        let cases = [
            (0, format!("{python}:3.12"), true),
            (0, format!("{python}:3.10"), false),
            (0, format!("{corretto}:11"), false),
            (2, format!("{python}:3.10"), false),
            (1, format!("{python}:3.12"), true),
            (1, format!("{python}:3.10"), true),
            (1, format!("{corretto}:21"), true),
            (1, format!("{corretto}:11"), true),
        ];
        for (at, image, expected) in cases {
            let refs = scenario_refs(&transfers[at], &corpus);
            let present = refs.iter().any(|r| r.to_string() == image);
            assert_eq!(present, expected, "{} {image}", transfers[at]);
        }
    }

    #[test]
    fn only_a_partial_scenario_reads_the_overrides() {
        // Catches: a run without a partial scenario refusing to start for want
        // of an overrides file, and a partial run starting without one.
        let corpus = two_image_corpus();
        let missing = "/nonexistent/overrides.yaml";
        for requested in [Scenario::Sync, Scenario::Scale, Scenario::Prepare] {
            assert!(plan(requested, &corpus, missing).is_ok());
        }
        for requested in [Scenario::Partial, Scenario::All] {
            assert!(plan(requested, &corpus, missing).is_err());
        }
        let (transfers, prepare) = plan(Scenario::Prepare, &corpus, missing).unwrap();
        assert!(transfers.is_empty() && prepare, "prepare reads no lock");
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
    fn cold_ocync_run_skips_scale_and_prepare_runs() {
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

        assert_eq!(cold_ocync_run(&scenarios).unwrap().wall_clock_secs, 3.0);
        assert!(cold_ocync_run(&scenarios[..2]).is_none());
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
    fn shuffle_tools_returns_seed() {
        let mut tools = vec![Tool::Ocync, Tool::Dregsy, Tool::Regsync];
        let seed = shuffle_tools(&mut tools);
        // Seed should be non-zero (timestamp-based).
        assert_ne!(seed, 0);
    }

    #[test]
    fn shuffle_tools_single_tool_returns_zero_seed() {
        let mut tools = vec![Tool::Ocync];
        let seed = shuffle_tools(&mut tools);
        assert_eq!(seed, 0, "single-tool list needs no shuffle");
        assert_eq!(tools[0], Tool::Ocync);
    }

    #[test]
    fn prewarm_rate_controller_ramps_up() {
        let mut ctrl = PrewarmRateController::new(Duration::from_millis(500));
        let initial = ctrl.interval;
        for _ in 0..10 {
            ctrl.on_success();
        }
        // 10 successes: 500 * 0.9^10 ~= 174ms
        assert!(ctrl.interval < initial);
        assert!(ctrl.interval > PrewarmRateController::MIN_INTERVAL);
    }

    #[test]
    fn prewarm_rate_controller_backs_off() {
        let mut ctrl = PrewarmRateController::new(Duration::from_millis(500));
        let before = ctrl.interval;
        ctrl.on_throttle();
        assert_eq!(ctrl.interval, before * 2);
    }

    #[test]
    fn prewarm_rate_controller_caps_backoff() {
        let mut ctrl = PrewarmRateController::new(Duration::from_millis(500));
        // Throttle repeatedly to hit the cap.
        for _ in 0..20 {
            ctrl.on_throttle();
            // Reset epoch so each throttle fires.
            ctrl.last_throttle = Instant::now() - ctrl.epoch - Duration::from_millis(1);
        }
        assert_eq!(ctrl.interval, PrewarmRateController::MAX_INTERVAL);
    }

    #[test]
    fn prewarm_rate_controller_epoch_suppresses_burst() {
        let mut ctrl = PrewarmRateController::new(Duration::from_millis(500));
        ctrl.on_throttle();
        let after_first = ctrl.interval;
        // Second throttle within epoch -- should be suppressed.
        ctrl.on_throttle();
        assert_eq!(
            ctrl.interval, after_first,
            "second throttle within epoch should be suppressed"
        );
    }

    #[test]
    fn prewarm_rate_controller_floor() {
        let mut ctrl = PrewarmRateController::new(Duration::from_millis(500));
        for _ in 0..200 {
            ctrl.on_success();
        }
        assert_eq!(ctrl.interval, PrewarmRateController::MIN_INTERVAL);
    }
}
