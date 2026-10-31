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
use ocync_distribution::Digest;
use serde::{Deserialize, Serialize};

use corpus::Corpus;
use regression::Baseline;
use report::{BenchReport, Prewarm, ScenarioKind, ScenarioResult, ToolRun, find_scenario};
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
    let transfers = plan(args.scenario, &corpus, args.paths.partial_overrides())?;
    let prepare = matches!(args.scenario, Scenario::Prepare | Scenario::All);
    let lock = (!transfers.is_empty())
        .then(|| lock::Lock::load(&args.paths.lock_path()))
        .transpose()?;
    let corpus_locked_at = lock.as_ref().map(|lock| lock.locked_at.clone());
    let http = registry::http_client()?;
    let auth = corpus.dockerhub_auth.as_ref();
    let refs: BTreeSet<_> = transfers
        .iter()
        .flat_map(|transfer| scenario_refs(transfer, &corpus))
        .collect();
    if let Some(lock) = &lock {
        // Before the build and the CDN pre-warm, so a moved source costs
        // seconds rather than a wasted run.
        lock::ensure_unmoved(&http, lock, &refs, auth)
            .await
            .map_err(|e| match e {
                lock::RecheckError::Lookup(text) => format!(
                    "{text}\nUse --skip-registries <host> to leave a registry out, or retry later."
                ),
                lock::RecheckError::Moved(text) => text,
            })?;
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

    let mut report = BenchReport {
        timestamp,
        instance,
        corpus_size: corpus.images.len(),
        total_tags: corpus.total_tags(),
        tool_versions,
        relay_versions,
        prewarm,
        scenarios: Vec::new(),
    };

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

    report::write_report(&output_dir, &report)
        .map_err(|e| with_stop_reason(e, stopped.as_deref()))?;
    eprintln!("bench: summary written to {}", output_dir.display());

    let target_registry = &corpus.settings.target_registry;
    let registry = RegistryKind::of(target_registry);
    let reg_key = registry.archive_stem();
    let git_ref = git_ref();
    let record = report::RunRecord::of(
        &report,
        git_ref,
        registry.provider(),
        corpus_locked_at.clone(),
    );
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
        report::append_record(results_dir, reg_key, record)
            .map_err(|e| with_stop_reason(e, stopped.as_deref()))?;
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

/// The scenarios a run executes, in order, that sync from the lock. The
/// overrides are read once, and only for a run that has a partial scenario.
fn plan(
    requested: Scenario,
    corpus: &Corpus,
    overrides_path: &str,
) -> Result<Vec<Transfer>, String> {
    let partial = || {
        let overrides = corpus::load_overrides(overrides_path)?;
        Ok::<_, String>(Transfer::Partial(
            corpus.with_overrides(&overrides.overrides),
        ))
    };
    Ok(match requested {
        Scenario::Sync => vec![Transfer::Sync],
        Scenario::Partial => vec![partial()?],
        Scenario::Scale => vec![Transfer::Scale],
        Scenario::Prepare => Vec::new(),
        Scenario::All => vec![Transfer::Sync, partial()?, Transfer::Scale],
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
            corpus: report::CorpusInfo {
                images: report.corpus_size,
                tags: report.total_tags,
            },
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

/// How one blob's pre-warm ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlobOutcome {
    /// Served from the CDN cache.
    Hit,
    /// Served from the origin, which warms the cache.
    Miss,
    /// Refused, unreachable or still failing after the attempts.
    Failed,
    /// Not warmed by the deadline: not sent, or not finished.
    PastDeadline,
}

/// Classify the final response to a blob request. A hit is what `CloudFront`
/// (`X-Cache`) or Cloudflare (`CF-Cache-Status`) says it is; any other
/// success is a miss. A status that is not 2xx, an unfollowed redirect
/// included, warmed nothing.
fn outcome_of(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> BlobOutcome {
    if !status.is_success() {
        return BlobOutcome::Failed;
    }
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let hit = header("x-cache").is_some_and(|v| v.to_ascii_lowercase().contains("hit"))
        || header("cf-cache-status").is_some_and(|v| v.eq_ignore_ascii_case("hit"));
    if hit {
        BlobOutcome::Hit
    } else {
        BlobOutcome::Miss
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
    /// Blobs that could not be warmed.
    failed: u64,
    /// Blobs not finished because the deadline came first.
    past_deadline: u64,
}

impl PrewarmStats {
    /// Count one blob.
    fn record(&mut self, outcome: BlobOutcome) {
        let count = match outcome {
            BlobOutcome::Hit => &mut self.hits,
            BlobOutcome::Miss => &mut self.misses,
            BlobOutcome::Failed => &mut self.failed,
            BlobOutcome::PastDeadline => &mut self.past_deadline,
        };
        *count += 1;
    }
}

/// The (repository, digest) of every blob the pre-warm fetches: all those the
/// lock records for the refs the run syncs, deduplicated, so every tag is
/// covered.
fn blobs_to_warm<'a>(
    lock: &'a lock::Lock,
    refs: &'a BTreeSet<lock::ImageRef>,
) -> HashSet<(registry::Repository<'a>, &'a Digest)> {
    let mut blobs = HashSet::new();
    for image in refs {
        let repo = registry::split_source(&image.source)
            .expect("ensure_unmoved refuses a source with no registry");
        let entry = lock
            .images
            .get(image)
            .expect("ensure_unmoved refuses a ref the lock lacks");
        blobs.extend(entry.blobs.iter().map(|blob| (repo, blob)));
    }
    blobs
}

/// Hosts with a blob that was not warmed.
fn unwarmed(stats: &BTreeMap<&str, PrewarmStats>) -> Vec<String> {
    stats
        .iter()
        .filter(|(_, s)| s.failed + s.past_deadline > 0)
        .map(|(host, _)| (*host).to_owned())
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

/// Warm every blob, at most [`PREWARM_PER_REGISTRY`] at a time per registry,
/// each by `deadline`, and count the outcomes by registry. `send` requests one
/// blob, `what` naming it in notices.
async fn warm_blobs<'a>(
    blobs: &HashSet<(registry::Repository<'a>, &'a Digest)>,
    deadline: Instant,
    send: impl AsyncFn(&registry::Repository<'_>, &Digest, &str) -> Result<reqwest::Response, String>,
) -> BTreeMap<&'a str, PrewarmStats> {
    let hosts: BTreeSet<&str> = blobs.iter().map(|(repo, _)| repo.host.api_host()).collect();
    let permits: HashMap<&str, tokio::sync::Semaphore> = hosts
        .into_iter()
        .map(|host| (host, tokio::sync::Semaphore::new(PREWARM_PER_REGISTRY)))
        .collect();
    let outcomes = futures_util::future::join_all(blobs.iter().map(|(repo, digest)| {
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
    let mut stats: BTreeMap<&str, PrewarmStats> = BTreeMap::new();
    for (host, outcome) in outcomes {
        stats.entry(host).or_default().record(outcome);
    }
    stats
}

/// Pull every unique source blob once to warm CDN edge caches.
///
/// Runs before any tool so all tools see identically warm CDN. Uses
/// OCI token exchange for authentication so requests succeed (2xx) and
/// `CloudFront` actually caches the responses. Unauthenticated 401s are
/// NOT cached by CDN and would warm nothing. The blobs come from the lock,
/// so every tag the run syncs is warmed and no manifest is read.
///
/// The outcome says which registries were not warmed, for the run record.
async fn cdn_prewarm(
    client: &reqwest::Client,
    auth: Option<&corpus::DockerHubAuth>,
    lock: &lock::Lock,
    refs: &BTreeSet<lock::ImageRef>,
) -> Prewarm {
    // Every blob is held to this time, so the downloads end by it. The CDN
    // wait below follows.
    let deadline = Instant::now() + PREWARM_DEADLINE;
    let access = registry::Access::new(client, auth);
    let blobs = blobs_to_warm(lock, refs);

    // Blob URLs are served via 302 redirects to CloudFront -- completely
    // different cache keys from manifests. Without this pass the first
    // tool to run pays a CDN miss penalty on every blob.
    let hosts: BTreeSet<&str> = blobs.iter().map(|(repo, _)| repo.host.api_host()).collect();
    eprintln!(
        "bench: warming {} unique blobs ({PREWARM_PER_REGISTRY} concurrent per registry, \
         {} at most across {} registries)...",
        blobs.len(),
        PREWARM_PER_REGISTRY * hosts.len(),
        hosts.len()
    );
    let stats = warm_blobs(&blobs, deadline, async |repo, digest, what| {
        let url = format!(
            "https://{}/v2/{}/blobs/{digest}",
            repo.host.api_host(),
            repo.path
        );
        access.send(repo, what, |client| client.get(&url)).await
    })
    .await;
    for (registry, s) in &stats {
        eprintln!(
            "  pre-warm: {registry}: {} blobs warmed ({} hits, {} misses), {} failed, {} past the deadline",
            s.hits + s.misses,
            s.hits,
            s.misses,
            s.failed,
            s.past_deadline
        );
    }
    eprintln!("bench: blob CDN pre-warm done");
    let unwarmed = unwarmed(&stats);
    if !unwarmed.is_empty() {
        eprintln!(
            "WARNING: pre-warm: not warmed: {}; the tool that runs first may pay the CDN misses and tool order matters",
            unwarmed.join(", ")
        );
    }

    // Wait for CDN propagation. CloudFront edges need time to fully
    // populate after the first request hits the origin.
    let wait_secs = 360; // 6 minutes
    eprintln!("bench: waiting {wait_secs}s for CDN propagation...");
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
    Prewarm::Done { unwarmed }
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
        let transfers = plan(Scenario::All, &corpus, overrides.to_str().unwrap()).unwrap();
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
        let mut got: Vec<(String, String)> = blobs_to_warm(&lock, &refs)
            .into_iter()
            .map(|(repo, digest)| (repo.to_string(), digest.hex()[..1].to_owned()))
            .collect();
        got.sort();
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
        stats.insert("late.example", {
            let mut s = PrewarmStats::default();
            s.record(BlobOutcome::PastDeadline);
            s
        });
        assert_eq!(unwarmed(&stats), ["failed.example", "late.example"]);
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
        let transfers = plan(Scenario::Prepare, &corpus, missing).unwrap();
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
    fn a_blob_response_is_classified_by_status_and_cache_headers() {
        // Catches: an error page or an unfollowed redirect counted as a warmed
        // blob, and a hit read off the wrong header.
        type Headers<'a> = &'a [(&'a str, &'a str)];
        let cases: [(u16, Headers<'_>, BlobOutcome); 11] = [
            (200, &[("x-cache", "Hit from cloudfront")], BlobOutcome::Hit),
            (
                200,
                &[("x-cache", "RefreshHit from cloudfront")],
                BlobOutcome::Hit,
            ),
            (200, &[("cf-cache-status", "HIT")], BlobOutcome::Hit),
            (
                200,
                &[("x-cache", "Miss from cloudfront")],
                BlobOutcome::Miss,
            ),
            (200, &[("cf-cache-status", "MISS")], BlobOutcome::Miss),
            (200, &[], BlobOutcome::Miss),
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
    fn a_blob_not_finished_by_the_deadline_is_dropped_whatever_it_is_waiting_on() {
        // Catches: a send that retries or waits past the deadline holding the
        // pre-warm with it.
        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = block_on_within_five_seconds(warm_blob(deadline, "blob", async || {
            std::future::pending::<Result<reqwest::Response, String>>().await
        }));
        assert_eq!(outcome, BlobOutcome::PastDeadline);
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
        let blobs: HashSet<_> = hung_paths
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
        let stats = block_on_within_five_seconds(warm_blobs(
            &blobs,
            deadline,
            async |repo, _, _| match repo.host {
                Host::Other("hung.example") => std::future::pending().await,
                _ => served("Hit from cloudfront"),
            },
        ));
        assert_eq!(stats["fine.example"].hits, fine_paths.len() as u64);
        assert_eq!(stats["hung.example"].past_deadline, hung_paths.len() as u64);
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
