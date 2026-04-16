//! Benchmark suite for comparing ocync against dregsy and regsync.

pub(crate) mod config_gen;
pub(crate) mod corpus;
pub(crate) mod ecr;
pub(crate) mod proxy;
pub(crate) mod regression;
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

    /// Number of iterations for the cold scenario (default: 3).
    #[arg(long, default_value = "3")]
    pub(crate) iterations: usize,

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
}

/// Benchmark scenario to run (CLI subcommand).
#[derive(Subcommand, Debug, Clone, Copy)]
pub(crate) enum Scenario {
    /// Clean target, full sync -- measures raw transfer performance.
    Cold,
    /// Re-sync with nothing changed -- measures skip optimization.
    Warm,
    /// Re-sync after ~5% of tags changed -- measures incremental sync.
    Partial,
    /// Run at increasing corpus sizes -- measures scaling behavior (ocync only).
    Scale,
    /// Run all scenarios in sequence.
    All,
}

/// A single runnable scenario (excludes the `All` meta-variant).
#[derive(Clone, Copy)]
enum RunnableScenario {
    Cold,
    Warm,
    Partial,
    Scale,
}

/// Run the benchmark suite.
pub(crate) async fn run(args: BenchArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Parse corpus (apply limit if set).
    let loaded = corpus::load(&args.corpus)?;
    let corpus = match args.limit {
        Some(n) => loaded.limit(n),
        None => loaded,
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
        None => PathBuf::from(format!("bench-results/{timestamp}")),
    };

    // 5. Create report.
    let mut report = BenchReport {
        timestamp,
        corpus_size: corpus.images.len(),
        total_tags: corpus.total_tags(),
        tool_versions,
        scenarios: Vec::new(),
        scale: Vec::new(),
    };

    // 6. Expand All into individual scenarios, then dispatch each.
    let scenarios: Vec<RunnableScenario> = match args.scenario {
        Scenario::All => vec![
            RunnableScenario::Cold,
            RunnableScenario::Warm,
            RunnableScenario::Partial,
            RunnableScenario::Scale,
        ],
        Scenario::Cold => vec![RunnableScenario::Cold],
        Scenario::Warm => vec![RunnableScenario::Warm],
        Scenario::Partial => vec![RunnableScenario::Partial],
        Scenario::Scale => vec![RunnableScenario::Scale],
    };

    for scenario in scenarios {
        match scenario {
            RunnableScenario::Cold => {
                let result = run_cold(&args, &corpus, &tools, &ecr_client, &output_dir).await?;
                report.scenarios.push(result);
            }
            RunnableScenario::Warm => {
                let result = run_warm(&args, &corpus, &tools, &ecr_client, &output_dir).await?;
                report.scenarios.push(result);
            }
            RunnableScenario::Partial => {
                let overrides = corpus::load_overrides(&args.partial_overrides)?;
                let partial_corpus = corpus.with_overrides(&overrides.overrides);
                let partial_corpus = match args.limit {
                    Some(n) => partial_corpus.limit(n),
                    None => partial_corpus,
                };
                let result =
                    run_partial(&args, &corpus, &partial_corpus, &tools, &ecr_client, &output_dir)
                        .await?;
                report.scenarios.push(result);
            }
            RunnableScenario::Scale => {
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
        }
    }

    // 7. Write report.
    report::write_report(&output_dir, &report)?;
    eprintln!("bench: report written to {}", output_dir.display());

    // 8. Handle regression mode.
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
    let baseline_path = PathBuf::from("bench-results/baseline.json");

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
    let metrics = proxy::aggregate(&entries);

    eprintln!(
        "  {}: wall_clock={:.1}s exit={} requests={} response_bytes={} mounts={}/{}",
        tool,
        result.wall_clock.as_secs_f64(),
        result
            .exit_code
            .map_or_else(|| "signal".to_string(), |c| c.to_string()),
        metrics.total_requests,
        report::format_bytes(metrics.total_response_bytes),
        metrics.mount_successes,
        metrics.mount_attempts,
    );

    // 7. Parse ocync JSON if applicable.
    let ocync_json = if tool == Tool::Ocync && !result.stdout.trim().is_empty() {
        serde_json::from_str(&result.stdout).ok()
    } else {
        None
    };

    Ok(ToolRun {
        tool: tool.to_string(),
        wall_clock_secs: result.wall_clock.as_secs_f64(),
        exit_code: result.exit_code,
        proxy_metrics: Some(metrics),
        ocync_json,
    })
}

/// Cold scenario: N iterations per tool, pick median by wall-clock.
async fn run_cold(
    args: &BenchArgs,
    corpus: &Corpus,
    tools: &[Tool],
    ecr_client: &aws_sdk_ecr::Client,
    output_dir: &Path,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    eprintln!(
        "bench: running cold scenario ({} iterations)",
        args.iterations
    );
    let mut runs = Vec::new();

    for &tool in tools {
        let mut iteration_runs = Vec::new();

        for i in 0..args.iterations {
            eprintln!("  cold: {} iteration {}/{}", tool, i + 1, args.iterations);

            ecr::create_repos(ecr_client, corpus).await?;

            let config_dir = tempfile::tempdir()?;
            let run = run_single_tool(args, tool, corpus, config_dir.path(), output_dir).await?;

            ecr::delete_repos(ecr_client, corpus).await?;

            iteration_runs.push(run);

            if i + 1 < args.iterations {
                cooldown().await;
            }
        }

        // Pick the median run by wall-clock time.
        iteration_runs.sort_by(|a, b| a.wall_clock_secs.total_cmp(&b.wall_clock_secs));
        let median_idx = iteration_runs.len() / 2;
        let median = iteration_runs.swap_remove(median_idx);
        runs.push(median);

        cooldown().await;
    }

    Ok(ScenarioResult {
        scenario: "Cold sync (median)".to_string(),
        runs,
    })
}

/// Warm scenario: per tool, prime run (unmeasured) then measured run.
async fn run_warm(
    args: &BenchArgs,
    corpus: &Corpus,
    tools: &[Tool],
    ecr_client: &aws_sdk_ecr::Client,
    output_dir: &Path,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    eprintln!("bench: running warm scenario");
    let mut runs = Vec::new();

    for &tool in tools {
        eprintln!("  warm: {}", tool);

        ecr::create_repos(ecr_client, corpus).await?;

        // Prime run (unmeasured).
        let config_dir = tempfile::tempdir()?;
        let _prime = run_single_tool(args, tool, corpus, config_dir.path(), output_dir).await?;
        eprintln!("  warm: {} prime complete, running measured pass", tool);

        // Measured run.
        let config_dir = tempfile::tempdir()?;
        let run = run_single_tool(args, tool, corpus, config_dir.path(), output_dir).await?;
        runs.push(run);

        ecr::delete_repos(ecr_client, corpus).await?;

        cooldown().await;
    }

    Ok(ScenarioResult {
        scenario: "Warm sync (no-op)".to_string(),
        runs,
    })
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
    eprintln!("bench: running partial scenario");
    let mut runs = Vec::new();

    for &tool in tools {
        eprintln!("  partial: {}", tool);

        ecr::create_repos(ecr_client, base_corpus).await?;

        // Prime with base corpus (unmeasured).
        let config_dir = tempfile::tempdir()?;
        let _prime = run_single_tool(args, tool, base_corpus, config_dir.path(), output_dir).await?;
        eprintln!(
            "  partial: {} prime complete, running with partial corpus",
            tool
        );

        // Measured run with partial corpus.
        let config_dir = tempfile::tempdir()?;
        let run = run_single_tool(args, tool, partial_corpus, config_dir.path(), output_dir).await?;
        runs.push(run);

        ecr::delete_repos(ecr_client, base_corpus).await?;

        cooldown().await;
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
    eprintln!("bench: running scale scenario");
    let total = corpus.images.len();
    let mut sizes: Vec<usize> = [10, 25, 50, total]
        .iter()
        .copied()
        .filter(|&s| s <= total)
        .collect();
    sizes.dedup();

    let mut points = Vec::new();

    for &size in &sizes {
        eprintln!("  scale: {size} images");
        let subset = corpus.limit(size);
        let mut results: BTreeMap<String, f64> = BTreeMap::new();

        for &tool in tools {
            eprintln!("  scale: {} @ {size} images", tool);

            ecr::create_repos(ecr_client, &subset).await?;

            let config_dir = tempfile::tempdir()?;
            let run = run_single_tool(args, tool, &subset, config_dir.path(), output_dir).await?;
            results.insert(tool.to_string(), run.wall_clock_secs);

            ecr::delete_repos(ecr_client, &subset).await?;

            cooldown().await;
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

    // Manual UTC formatting — avoids a chrono/time dependency for one call site.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since Unix epoch to (year, month, day) — civil calendar algorithm.
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
            Scenario::Cold => f.write_str("cold"),
            Scenario::Warm => f.write_str("warm"),
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
        assert_eq!(Scenario::Cold.to_string(), "cold");
        assert_eq!(Scenario::Warm.to_string(), "warm");
        assert_eq!(Scenario::Partial.to_string(), "partial");
        assert_eq!(Scenario::Scale.to_string(), "scale");
        assert_eq!(Scenario::All.to_string(), "all");
    }
}
