//! Benchmark suite for comparing ocync against dregsy and regsync.

pub(crate) mod config_gen;
pub(crate) mod corpus;
pub(crate) mod ecr;
pub(crate) mod proxy;
pub(crate) mod regression;
pub(crate) mod remote;
pub(crate) mod report;
pub(crate) mod runner;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use corpus::Corpus;
use regression::Baseline;
use report::{BenchReport, ScalePoint, ScenarioResult, ToolRun};
use runner::Tool;

/// Arguments for the bench subcommand.
#[derive(Args)]
pub(crate) struct BenchArgs {
    #[command(subcommand)]
    pub(crate) scenario: Scenario,

    /// Comma-separated list of tools to benchmark (default: all).
    #[arg(long, value_delimiter = ',', default_value = "ocync,dregsy,regsync")]
    pub(crate) tools: Vec<String>,

    /// Path to corpus YAML file.
    #[arg(long, default_value = "bench/corpus.yaml")]
    pub(crate) corpus: String,

    /// Path to partial sync overrides YAML (applied on top of corpus for the partial scenario).
    #[arg(long, default_value = "bench/corpus-partial-overrides.yaml")]
    pub(crate) partial_overrides: String,

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
}

/// Benchmark scenario to run (CLI subcommand).
#[derive(Subcommand, Debug, Clone, Copy)]
pub(crate) enum Scenario {
    /// Full sync (cold) then immediate re-sync (warm).
    Sync,
    /// Re-sync after ~5% of tags changed -- measures incremental sync.
    Partial,
    /// Run at increasing corpus sizes -- measures scaling behavior (ocync only).
    Scale,
    /// Run all scenarios in sequence.
    All,
}

/// Read an SSM parameter by name, returning `None` on any failure.
fn read_ssm_parameter(name: &str) -> Option<String> {
    std::process::Command::new("aws")
        .args([
            "ssm",
            "get-parameter",
            "--name",
            name,
            "--with-decryption",
            "--query",
            "Parameter.Value",
            "--output",
            "text",
            "--region",
            "us-east-1",
        ])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read Docker Hub credentials from environment variables.
///
/// Set by cloud-init on bench instances. Returns `None` if either
/// variable is missing or empty.
fn docker_hub_auth_from_env() -> Option<(String, String)> {
    let username = std::env::var("DOCKERHUB_USERNAME")
        .ok()
        .filter(|s| !s.is_empty())?;
    let token = std::env::var("DOCKERHUB_ACCESS_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some((username, token))
}

/// Write a progress line to stderr.
fn progress(msg: &str) {
    eprintln!("{msg}");
}

/// Derives the registry key from a target registry hostname.
///
/// Used to choose the JSON archive filename (`ecr.json`, `gcr.json`, etc.).
fn registry_key(target_registry: &str) -> &'static str {
    if target_registry.contains(".ecr.") && target_registry.contains(".amazonaws.com") {
        "ecr"
    } else if target_registry == "gcr.io"
        || target_registry.ends_with(".gcr.io")
        || target_registry.ends_with("-docker.pkg.dev")
    {
        "gcr"
    } else if target_registry.ends_with(".azurecr.io") {
        "acr"
    } else {
        "other"
    }
}

/// Returns the short git commit hash of HEAD.
fn git_ref() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Derives the cloud provider name from a registry key.
fn provider_for_registry(key: &str) -> &'static str {
    match key {
        "ecr" => "aws",
        "gcr" => "gcp",
        "acr" => "azure",
        _ => "other",
    }
}

/// Run the benchmark suite.
pub(crate) async fn run(args: BenchArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Parse corpus (apply skip-registries filter, then limit).
    let loaded = corpus::load(&args.corpus)?;
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

    // Inject Docker Hub auth from env vars (cloud-init) or SSM (local dev).
    // Note: SSM fallback requires IAM permissions not in the current Terraform
    // (removed when migrating to SSH). Kept for local dev with personal creds.
    if let Some((username, token)) = docker_hub_auth_from_env() {
        eprintln!("bench: Docker Hub auth configured (username={username})");
        corpus.dockerhub_auth = Some(corpus::DockerHubAuth { username, token });
    } else if let Some(token) = read_ssm_parameter("/ocync/bench/dockerhub-access-token") {
        let username = read_ssm_parameter("/ocync/bench/dockerhub-username")
            .unwrap_or_else(|| "ocync-bench".into());
        eprintln!("bench: Docker Hub auth configured via SSM (username={username})");
        corpus.dockerhub_auth = Some(corpus::DockerHubAuth { username, token });
    } else {
        eprintln!("bench: Docker Hub auth not available, using anonymous pulls");
    };

    // 2. Parse tools from args.
    let tools: Vec<Tool> = args
        .tools
        .iter()
        .map(|s| Tool::parse(s))
        .collect::<Result<Vec<_>, _>>()?;

    eprintln!(
        "bench: scenario={} tools={} images={} tags={}",
        args.scenario,
        args.tools.join(","),
        corpus.images.len(),
        corpus.total_tags(),
    );

    // 3. Validate prerequisites.
    let ecr_client = ecr::client().await;
    ecr::validate_credentials(&ecr_client).await?;

    // Check each tool binary and collect versions.
    let mut tool_versions: BTreeMap<String, String> = BTreeMap::new();
    for &tool in &tools {
        let version = runner::check_tool(tool).await?;
        eprintln!("  {}: {version}", tool);
        tool_versions.insert(tool.to_string(), version);
    }

    // Build ocync from workspace if it is one of the tools.
    if tools.contains(&Tool::Ocync) {
        eprintln!("bench: building ocync in release mode...");
        runner::build_ocync(Path::new(".")).await?;
    }

    // 4. Determine output directory.
    let timestamp = utc_timestamp();
    let output_dir = match &args.output {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(format!("bench/results/{timestamp}")),
    };

    // 5. Create report.
    let instance = report::InstanceInfo::collect();
    eprintln!(
        "bench: instance={} cpu={} vcpus={} mem={}MiB net={} region={}",
        instance.instance_type,
        instance.cpu_model,
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
        scenarios: Vec::new(),
        scale: Vec::new(),
    };

    // 6. Expand All into individual scenarios, then dispatch each.
    let scenarios: Vec<Scenario> = match args.scenario {
        Scenario::All => vec![Scenario::Sync, Scenario::Partial, Scenario::Scale],
        other => vec![other],
    };

    for scenario in scenarios {
        match scenario {
            Scenario::Sync => {
                let (cold, warm) =
                    run_sync(&args, &corpus, &tools, &ecr_client, &output_dir).await?;
                report.scenarios.push(cold);
                report.scenarios.push(warm);
            }
            Scenario::Partial => {
                let overrides = corpus::load_overrides(&args.partial_overrides)?;
                let partial_corpus = corpus.with_overrides(&overrides.overrides);
                let partial_corpus = match args.limit {
                    Some(n) => partial_corpus.limit(n),
                    None => partial_corpus,
                };
                let result = run_partial(
                    &args,
                    &corpus,
                    &partial_corpus,
                    &tools,
                    &ecr_client,
                    &output_dir,
                )
                .await?;
                report.scenarios.push(result);
            }
            Scenario::Scale => {
                // Scale scenario uses ocync only per spec.
                if tools.contains(&Tool::Ocync) {
                    let ocync_only = vec![Tool::Ocync];
                    let points =
                        run_scale(&args, &corpus, &ocync_only, &ecr_client, &output_dir).await?;
                    report.scale = points;
                } else {
                    eprintln!("bench: scale scenario requires ocync (skipping)");
                }
            }
            Scenario::All => unreachable!("expanded above"),
        }
    }

    // 7. Write report.
    report::write_report(&output_dir, &report)?;
    eprintln!("bench: summary written to {}", output_dir.display());

    // 8. Append compact run record to per-registry JSON archive.
    let target_registry = &corpus.settings.target_registry;
    let reg_key = registry_key(target_registry);
    let git_ref = git_ref();
    let record = report::RunRecord {
        timestamp: report.timestamp.clone(),
        git_ref,
        machine: report::MachineInfo {
            provider: provider_for_registry(reg_key).into(),
            instance_type: report.instance.instance_type.clone(),
            arch: report.instance.arch.clone(),
            vcpus: report.instance.vcpus,
            memory_gib: report.instance.memory_mib as f64 / 1024.0,
            network: report.instance.network_performance.clone(),
            region: report.instance.region.clone(),
        },
        corpus: report::CorpusInfo {
            images: report.corpus_size,
            tags: report.total_tags,
        },
        scenarios: report
            .scenarios
            .iter()
            .map(|s| report::ScenarioRecord {
                name: s.scenario.clone(),
                tools: s
                    .runs
                    .iter()
                    .map(|r| {
                        let metrics = r.proxy_metrics.as_ref();
                        report::ToolRecord {
                            tool: r.tool.clone(),
                            version: report
                                .tool_versions
                                .get(&r.tool)
                                .cloned()
                                .unwrap_or_default(),
                            wall_clock_secs: r.wall_clock_secs,
                            peak_rss_kb: r.peak_rss_kb,
                            requests: metrics.map_or(0, |m| m.total_requests),
                            response_bytes: metrics.map_or(0, |m| m.total_response_bytes),
                            source_blob_gets: metrics.map_or(0, |m| m.source_blob_gets),
                            source_blob_bytes: metrics.map_or(0, |m| m.source_blob_bytes),
                            mount_successes: metrics.map_or(0, |m| m.mount_successes),
                            mount_attempts: metrics.map_or(0, |m| m.mount_attempts),
                            duplicate_blob_gets: metrics.map_or(0, |m| m.duplicate_blob_gets),
                            rate_limit_429s: metrics.map_or(0, |m| m.status_429_count),
                        }
                    })
                    .collect(),
            })
            .collect(),
    };
    let results_dir = Path::new("bench/results");
    report::append_record(results_dir, reg_key, record)?;
    eprintln!("bench: run record appended to bench/results/{reg_key}.json");

    // 9. Handle regression mode.
    if args.regression {
        check_regression(&report, args.threshold_pct)?;
    }

    Ok(())
}

/// Compare the current ocync run against a stored baseline.
///
/// Uses the first ocync run found across all scenarios (typically the cold
/// sync median when running `all`). Returns an error if regression is detected.
fn check_regression(
    report: &BenchReport,
    threshold: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let baseline_path = PathBuf::from("bench/results/baseline.json");

    // Find the ocync cold-sync run for regression comparison.
    let ocync_run = report
        .scenarios
        .iter()
        .flat_map(|s| &s.runs)
        .find(|r| r.tool == "ocync");

    if let Some(run) = ocync_run {
        let current = Baseline {
            wall_clock_secs: run.wall_clock_secs,
            total_requests: run.proxy_metrics.as_ref().map_or(0, |m| m.total_requests),
            total_bytes: run
                .proxy_metrics
                .as_ref()
                .map_or(0, |m| m.total_response_bytes),
        };

        if baseline_path.exists() {
            let baseline = regression::load_baseline(&baseline_path)?;
            let result = regression::compare(&current, &baseline, threshold);
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
        eprintln!("bench: no ocync run found for regression comparison");
    }

    Ok(())
}

/// Run a single tool against the corpus and return a `ToolRun`.
async fn run_single_tool(
    args: &BenchArgs,
    tool: Tool,
    corpus: &Corpus,
    config_dir: &Path,
    output_dir: &Path,
) -> Result<ToolRun, Box<dyn std::error::Error>> {
    // 1. Generate config.
    let config_content = match tool {
        Tool::Ocync => config_gen::ocync_config(corpus),
        Tool::Dregsy => config_gen::dregsy_config(corpus),
        Tool::Regsync => config_gen::regsync_config(corpus),
    };

    // 2. Write config to disk.
    let config_path = config_dir.join(format!("{}.yaml", tool));
    std::fs::write(&config_path, &config_content)
        .map_err(|e| format!("write config {}: {e}", config_path.display()))?;

    // 3. Start bench-proxy (our pure-Rust MITM replacement for mitmdump).
    //
    // The proxy binary, CA cert, and CA private key are configured via the
    // `--proxy-binary`, `--proxy-ca`, and `--proxy-ca-key` CLI flags, which
    // default to paths populated by the instance bootstrap (user-data.sh).
    //
    // The log lives under `output_dir` (not the auto-cleaned tempdir) so
    // post-run analysis (`grep mount= …proxy.jsonl`) can inspect exactly
    // which API calls the tool issued.
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("create output dir {}: {e}", output_dir.display()))?;
    let log_path = output_dir.join(format!("{tool}-proxy.jsonl"));
    let handle = proxy::start(
        &args.proxy_binary,
        &args.proxy_ca,
        &args.proxy_ca_key,
        &log_path,
        args.proxy_port,
    )
    .await?;
    let proxy_url = handle.proxy_url();

    // 4. Run the tool.
    //
    // The bench-proxy CA must be installed in the system trust store (via
    // update-ca-trust on AL2023) so that rustls-native-certs and OpenSSL
    // both trust the MITM'd connections. HTTPS_PROXY alone is sufficient.
    let workspace_root = Path::new(".");
    let result = runner::run_tool(tool, &config_path, &proxy_url, workspace_root).await?;

    // 6. Stop proxy, parse log, aggregate metrics.
    let entries = proxy::stop(handle).await?;
    let metrics = proxy::aggregate(&entries, &corpus.settings.target_registry);

    // Abort on source registry 429s. These are unrecoverable within the
    // rate limit window (unlike ECR 429s which AIMD handles). Continuing
    // would produce tainted data for every subsequent tool/iteration.
    let source_429s: Vec<&str> = entries
        .iter()
        .filter(|e| e.status == 429 && e.host != corpus.settings.target_registry)
        .map(|e| e.host.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !source_429s.is_empty() {
        return Err(format!(
            "aborting: source registry rate limit hit (429) from: {}. \
             Use --skip-registries {} to exclude, or wait for the rate limit window to reset.",
            source_429s.join(", "),
            source_429s.join(","),
        )
        .into());
    }

    let rss_display = result
        .peak_rss_kb
        .map(|kb| report::format_bytes(kb * 1024))
        .unwrap_or_else(|| "n/a".into());
    eprintln!(
        "  {}: wall_clock={:.1}s exit={} rss={} requests={} response_bytes={} mounts={}/{} source_pulls={}/{}",
        tool,
        result.wall_clock.as_secs_f64(),
        result
            .exit_code
            .map_or_else(|| "signal".to_string(), |c| c.to_string()),
        rss_display,
        metrics.total_requests,
        report::format_bytes(metrics.total_response_bytes),
        metrics.mount_successes,
        metrics.mount_attempts,
        metrics.source_blob_gets,
        report::format_bytes(metrics.source_blob_bytes),
    );

    Ok(ToolRun {
        tool: tool.to_string(),
        wall_clock_secs: result.wall_clock.as_secs_f64(),
        exit_code: result.exit_code,
        peak_rss_kb: result.peak_rss_kb,
        proxy_metrics: Some(metrics),
    })
}

/// Sync scenario: full sync (measured), then immediate re-sync
/// (measured) with the same config directory so that ocync's
/// `TransferStateCache` persists between runs.
async fn run_sync(
    args: &BenchArgs,
    corpus: &Corpus,
    tools: &[Tool],
    ecr_client: &aws_sdk_ecr::Client,
    output_dir: &Path,
) -> Result<(ScenarioResult, ScenarioResult), Box<dyn std::error::Error>> {
    progress("bench: running cold+warm scenario");
    let mut cold_runs = Vec::new();
    let mut warm_runs = Vec::new();

    for (tool_idx, &tool) in tools.iter().enumerate() {
        // Single config_dir for cold + warm. The cache file lives
        // under this directory, so reusing it means the warm run
        // sees a fully populated cache.
        let config_dir = tempfile::tempdir()?;

        // Cold run.
        progress(&format!("  cold: {tool}"));
        ecr::create_repos(ecr_client, corpus).await?;
        let cold = run_single_tool(args, tool, corpus, config_dir.path(), output_dir).await?;
        if cold.exit_code != Some(0) {
            return Err(format!(
                "{tool} cold sync failed (exit {:?}); aborting -- \
                 tainted data cannot be compared",
                cold.exit_code
            )
            .into());
        }
        progress(&format!(
            "  cold: {tool} complete ({:.1}s)",
            cold.wall_clock_secs
        ));
        cold_runs.push(cold);

        // Warm run: target still populated, same config_dir.
        progress(&format!("  warm: {tool}"));
        let warm = run_single_tool(args, tool, corpus, config_dir.path(), output_dir).await?;
        if warm.exit_code != Some(0) {
            return Err(format!(
                "{tool} warm sync failed (exit {:?}); aborting -- \
                 tainted data cannot be compared",
                warm.exit_code
            )
            .into());
        }
        progress(&format!(
            "  warm: {tool} complete ({:.1}s)",
            warm.wall_clock_secs
        ));
        warm_runs.push(warm);

        ecr::delete_repos(ecr_client, corpus).await?;

        if tool_idx + 1 < tools.len() {
            cooldown().await;
        }
    }

    Ok((
        ScenarioResult {
            scenario: "Cold sync".to_string(),
            runs: cold_runs,
        },
        ScenarioResult {
            scenario: "Warm sync (no-op)".to_string(),
            runs: warm_runs,
        },
    ))
}

/// Partial scenario: prime with base corpus, then sync with partial corpus.
async fn run_partial(
    args: &BenchArgs,
    base_corpus: &Corpus,
    partial_corpus: &Corpus,
    tools: &[Tool],
    ecr_client: &aws_sdk_ecr::Client,
    output_dir: &Path,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    progress("bench: running partial scenario");
    let mut runs = Vec::new();

    for (tool_idx, &tool) in tools.iter().enumerate() {
        progress(&format!("  partial: {} (priming)", tool));

        ecr::create_repos(ecr_client, base_corpus).await?;

        // Prime with base corpus (unmeasured). Reuse the same
        // config_dir so ocync's TransferStateCache persists.
        let config_dir = tempfile::tempdir()?;
        let prime = run_single_tool(args, tool, base_corpus, config_dir.path(), output_dir).await?;
        if prime.exit_code != Some(0) {
            return Err(format!(
                "{tool} partial prime failed (exit {:?}); aborting",
                prime.exit_code
            )
            .into());
        }
        progress(&format!("  partial: {} measuring", tool));

        // Measured run with partial corpus (same config_dir).
        let run =
            run_single_tool(args, tool, partial_corpus, config_dir.path(), output_dir).await?;
        if run.exit_code != Some(0) {
            return Err(format!(
                "{tool} partial sync failed (exit {:?}); aborting",
                run.exit_code
            )
            .into());
        }
        progress(&format!(
            "  partial: {} complete ({:.1}s)",
            tool, run.wall_clock_secs
        ));
        runs.push(run);

        ecr::delete_repos(ecr_client, base_corpus).await?;

        if tool_idx + 1 < tools.len() {
            cooldown().await;
        }
    }

    Ok(ScenarioResult {
        scenario: "Partial sync (~5% changed)".to_string(),
        runs,
    })
}

/// Scale scenario: run at increasing corpus sizes [10, 25, 50, all].
async fn run_scale(
    args: &BenchArgs,
    corpus: &Corpus,
    tools: &[Tool],
    ecr_client: &aws_sdk_ecr::Client,
    output_dir: &Path,
) -> Result<Vec<ScalePoint>, Box<dyn std::error::Error>> {
    progress("bench: running scale scenario");
    let total = corpus.images.len();
    let mut sizes: Vec<usize> = [10, 25, 50, total]
        .iter()
        .copied()
        .filter(|&s| s <= total)
        .collect();
    sizes.dedup();

    let mut points = Vec::new();

    for &size in &sizes {
        progress(&format!("  scale: {size} images"));
        let subset = corpus.limit(size);
        let mut results: BTreeMap<String, f64> = BTreeMap::new();

        for (tool_idx, &tool) in tools.iter().enumerate() {
            progress(&format!("  scale: {} @ {size} images", tool));

            ecr::create_repos(ecr_client, &subset).await?;

            let config_dir = tempfile::tempdir()?;
            let run = run_single_tool(args, tool, &subset, config_dir.path(), output_dir).await?;
            if run.exit_code != Some(0) {
                return Err(format!(
                    "{tool} scale sync failed at {size} images (exit {:?}); aborting",
                    run.exit_code
                )
                .into());
            }
            results.insert(tool.to_string(), run.wall_clock_secs);

            ecr::delete_repos(ecr_client, &subset).await?;

            if tool_idx + 1 < tools.len() {
                cooldown().await;
            }
        }

        points.push(ScalePoint {
            images: size,
            results,
        });
    }

    Ok(points)
}

/// Sleep for 30 seconds between benchmark runs to let rate limits recover.
async fn cooldown() {
    eprintln!("bench: cooling down for 30s...");
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
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

impl std::fmt::Display for Scenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scenario::Sync => f.write_str("sync"),
            Scenario::Partial => f.write_str("partial"),
            Scenario::Scale => f.write_str("scale"),
            Scenario::All => f.write_str("all"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_to_civil_unix_epoch() {
        // Day 0 = 1970-01-01.
        assert_eq!(days_to_civil(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_civil_known_date() {
        // 2026-04-15 = day 20558 since epoch.
        // (date -d '2026-04-15' +%s) / 86400 = 20558
        assert_eq!(days_to_civil(20558), (2026, 4, 15));
    }

    #[test]
    fn days_to_civil_leap_year_boundary() {
        // 2000-02-29 = day 11016.
        assert_eq!(days_to_civil(11016), (2000, 2, 29));
        // 2000-03-01 = day 11017.
        assert_eq!(days_to_civil(11017), (2000, 3, 1));
    }

    #[test]
    fn days_to_civil_non_leap_century() {
        // 1900 is not a leap year (divisible by 100 but not 400).
        // 1900-02-28 is valid, 1900-03-01 follows.
        // 1900-03-01 = day -25567 ... negative days not supported (u64).
        // Use 2100-02-28 instead: day 47540, 2100-03-01 = day 47541.
        assert_eq!(days_to_civil(47540), (2100, 2, 28));
        assert_eq!(days_to_civil(47541), (2100, 3, 1));
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
    fn scenario_display() {
        assert_eq!(Scenario::Sync.to_string(), "sync");
        assert_eq!(Scenario::Partial.to_string(), "partial");
        assert_eq!(Scenario::Scale.to_string(), "scale");
        assert_eq!(Scenario::All.to_string(), "all");
    }

    #[test]
    fn provider_for_registry_mapping() {
        assert_eq!(provider_for_registry("ecr"), "aws");
        assert_eq!(provider_for_registry("gcr"), "gcp");
        assert_eq!(provider_for_registry("acr"), "azure");
        assert_eq!(provider_for_registry("other"), "other");
        assert_eq!(provider_for_registry("unknown"), "other");
    }

    #[test]
    fn registry_key_ecr() {
        assert_eq!(
            registry_key("123456789012.dkr.ecr.us-east-1.amazonaws.com"),
            "ecr"
        );
        assert_eq!(
            registry_key("111111111111.dkr.ecr.eu-west-1.amazonaws.com"),
            "ecr"
        );
    }

    #[test]
    fn registry_key_gcr() {
        assert_eq!(registry_key("gcr.io"), "gcr");
        assert_eq!(registry_key("us-docker.pkg.dev"), "gcr");
        assert_eq!(registry_key("europe-west1-docker.pkg.dev"), "gcr");
    }

    #[test]
    fn registry_key_acr() {
        assert_eq!(registry_key("myregistry.azurecr.io"), "acr");
    }

    #[test]
    fn registry_key_other() {
        assert_eq!(registry_key("ghcr.io"), "other");
        assert_eq!(registry_key("docker.io"), "other");
        assert_eq!(registry_key("quay.io"), "other");
    }

    #[test]
    fn docker_hub_auth_from_env_both_set() {
        // SAFETY: test-only; env var mutation is safe in single-threaded
        // test runs (cargo test runs each #[test] in its own thread, but
        // these env var names are unique to this test suite).
        unsafe {
            std::env::set_var("DOCKERHUB_USERNAME", "testuser");
            std::env::set_var("DOCKERHUB_ACCESS_TOKEN", "testtoken");
        }
        let result = docker_hub_auth_from_env();
        unsafe {
            std::env::remove_var("DOCKERHUB_USERNAME");
            std::env::remove_var("DOCKERHUB_ACCESS_TOKEN");
        }
        assert_eq!(result, Some(("testuser".into(), "testtoken".into())));
    }

    #[test]
    fn docker_hub_auth_from_env_empty_returns_none() {
        unsafe {
            std::env::set_var("DOCKERHUB_USERNAME", "");
            std::env::set_var("DOCKERHUB_ACCESS_TOKEN", "sometoken");
        }
        let result = docker_hub_auth_from_env();
        unsafe {
            std::env::remove_var("DOCKERHUB_USERNAME");
            std::env::remove_var("DOCKERHUB_ACCESS_TOKEN");
        }
        assert_eq!(result, None);
    }

    #[test]
    fn docker_hub_auth_from_env_missing_returns_none() {
        unsafe {
            std::env::remove_var("DOCKERHUB_USERNAME");
            std::env::remove_var("DOCKERHUB_ACCESS_TOKEN");
        }
        assert_eq!(docker_hub_auth_from_env(), None);
    }
}
