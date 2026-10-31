//! Remote benchmark orchestration via SSH.
//!
//! Prerequisites: git push access to the repo and Terraform applied for the
//! target provider (`bench/terraform/<provider>/`).
//!
//! `cargo xtask bench-remote --provider aws` pushes the current branch, SSHs
//! into the bench instance, builds and runs benchmarks with live streamed output,
//! then pulls results back via SCP.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Args;
use serde::Deserialize;

use super::gar::{ArtifactRegistryTarget, ENV_CREDENTIALS};
use super::gcloud::{Gcloud, Outputs, Provisioned, lock_local_run};
use super::report::{ARCHIVE_DIR, ArchiveMode, Deferred, PERFORMANCE_PAGE};
use super::runner::join_tools;
use super::target::{ENV_REGISTRY, Kind, ensure_artifact_registry_applies};
use super::{Provider, RunTimestamp, Scenario, ToolSelection, finish_pass, value_name};

/// Common SSH options for all remote connections.
///
/// `accept-new` trusts the host key on first connect (the instance is
/// ephemeral; re-created on every `terraform apply`). `BatchMode`
/// prevents interactive prompts from hanging the automation. `IdentitiesOnly`
/// stops `ssh-agent` from offering unrelated keys before the instance key.
const SSH_OPTS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=30",
    "-o",
    "IdentitiesOnly=yes",
];

/// Builds the SSH/SCP arg vector for a given config: the Terraform-generated
/// identity file followed by the common options.
fn ssh_args(config: &BenchConfig) -> Vec<&str> {
    let mut args = vec!["-i", &config.ssh_key_path];
    args.extend_from_slice(SSH_OPTS);
    args
}

/// Where on the instance the Google federation file is copied to, relative to
/// the bench user's home.
const REMOTE_CREDENTIALS: &str = ".bench-gar-credentials.json";

/// Arguments for the `bench-remote` subcommand.
#[derive(Args)]
pub(crate) struct BenchRemoteArgs {
    /// Cloud provider. Reads connection details from
    /// `bench/terraform/<provider>/bench.json`.
    #[arg(long)]
    pub(crate) provider: Provider,

    /// Registry the instance syncs into. Artifact Registry needs the Google
    /// stack applied (`bench/terraform/gcp`) and `gcloud` signed in here: this
    /// command makes a repository for each round and deletes them when it ends.
    #[arg(long, value_enum, default_value = "ecr")]
    pub(crate) target: Kind,

    /// Copy the newest results on the instance instead of starting a run.
    /// They are a live run's partial results only once its benchmark has
    /// started; before that they are the previous run's. Conflicts with every
    /// option that shapes a run.
    #[arg(
        long,
        conflicts_with_all = [
            "force",
            "git_ref",
            "target",
            "tools",
            "baseline_ref",
            "rounds",
            "max_concurrent_transfers",
            "scenario",
            "limit",
            "skip_registries",
            "skip_prewarm",
            "no_proxy",
        ]
    )]
    pub(crate) fetch: bool,

    /// Kill a benchmark still running on the instance and start fresh.
    /// Without it, a live run is left alone and this command fails.
    #[arg(long)]
    pub(crate) force: bool,

    /// Git ref to checkout on the instance (default: current branch). A ref
    /// older than the change that added `bench --archive` cannot run, because
    /// the instance's `bench` would not know the option, and one older than
    /// `--max-concurrent-transfers` cannot take that option for the same reason.
    #[arg(long)]
    pub(crate) git_ref: Option<String>,

    #[command(flatten)]
    pub(crate) tool_selection: ToolSelection,

    /// Scenario to run (passed through to `cargo xtask bench`).
    #[arg(long, value_enum, default_value = "all")]
    pub(crate) scenario: Scenario,

    /// Use first N images only (passed through to `cargo xtask bench`).
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Comma-separated list of source registries to skip (passed through
    /// to `cargo xtask bench`).
    #[arg(long)]
    pub(crate) skip_registries: Option<String>,

    /// Skip CDN pre-warming (passed through to `cargo xtask bench`).
    #[arg(long)]
    pub(crate) skip_prewarm: bool,

    /// Disable bench-proxy on the remote instance.
    #[arg(long)]
    pub(crate) no_proxy: bool,

    /// Local directory to save fetched results.
    #[arg(long, default_value = ARCHIVE_DIR)]
    pub(crate) output: String,
}

/// Connection details written by Terraform.
#[derive(Debug, Deserialize)]
struct BenchConfig {
    host: String,
    user: String,
    /// Absolute path to the Terraform-generated SSH private key for this
    /// instance. Written by `local_sensitive_file.ssh_key`.
    ssh_key_path: String,
}

/// Returns the path to bench.json for a given provider.
fn config_path(provider: Provider) -> PathBuf {
    PathBuf::from(format!("bench/terraform/{provider}/bench.json"))
}

/// Read and parse bench.json for the given provider.
#[expect(
    clippy::disallowed_methods,
    reason = "reads the bench's own configuration"
)]
fn read_config(provider: Provider) -> Result<BenchConfig, Box<dyn std::error::Error>> {
    let path = config_path(provider);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "failed to read {}: {e}. Did you run `terraform apply` in bench/terraform/{provider}/?",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&content)?)
}

/// Run the remote benchmark workflow.
pub(crate) fn run(args: BenchRemoteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = read_config(args.provider)?;

    if args.fetch {
        let Some(remote_dir) = newest_results_dir(&config)? else {
            return Err("no results directory found on instance".into());
        };
        if fetch_results(&config, &args.output, &remote_dir)? == Fetch::CopiedWithoutRecord {
            eprintln!(
                "bench-remote: {remote_dir} holds no deferred record; copied it, \
                 applied nothing to the archive or the performance page"
            );
        }
        return Ok(());
    }

    // Decided before the push, so a provider with no setup is refused before
    // anything leaves this machine.
    ensure_placement(args.provider)?;
    args.tool_selection.ensure_baseline_applies(args.scenario)?;
    args.tool_selection.ensure_cap_applies()?;
    if args.target == Kind::ArtifactRegistry {
        ensure_artifact_registry_applies(args.scenario, &args.tool_selection.run_tools())?;
    }
    if let Some(commit) = &args.tool_selection.baseline_ref {
        ensure_on_origin(Path::new("."), commit)?;
    }

    // Held, with the Terraform outputs read, until this function returns,
    // and so also after the push: a missing apply is refused before anything
    // leaves this machine, and a second Artifact Registry run on this machine
    // cannot sweep the repositories made below.
    let (_local_run, outputs) = match args.target {
        Kind::Ecr => (None, None),
        Kind::ArtifactRegistry => (Some(lock_local_run()?), Some(Outputs::read()?)),
    };

    let git_ref = match &args.git_ref {
        Some(r) => r.clone(),
        None => current_branch()?,
    };

    eprintln!("bench-remote: pushing {git_ref}...");
    let push = Command::new("git")
        .args(["push", "origin", &git_ref])
        .status()?;
    if !push.success() {
        return Err("git push failed".into());
    }

    // The remote script checks out this SHA, not the branch name, so a
    // force-push landing between our push and the instance's fetch cannot
    // change what is built.
    let git_sha = resolve_commit(Path::new("."), &git_ref)?;
    eprintln!("bench-remote: pinned to {git_sha} ({git_ref})");

    // This run's start time, chosen here: it names the results directory the
    // fetch takes and stamps the run's record.
    let timestamp = RunTimestamp::now();

    // Made after the push, so a refused or failed push creates nothing. The
    // local lock above covers another run on this machine; the probe covers
    // one whose local process died but whose benchmark is still going on the
    // instance, which the sweep would otherwise empty. Dropped on every way out
    // of this function, which deletes the repositories.
    let mut provisioned = match outputs {
        None => None,
        Some(outputs) => {
            may_provision(
                RunLock::parse(&ssh_run(&config, &run_lock_probe())?)?,
                ExistingRun::from_force(args.force),
            )?;
            Some(Provisioned::create(
                Gcloud::installed(),
                outputs,
                &timestamp,
                args.tool_selection.rounds,
            )?)
        }
    };

    let result = run_on_instance(
        &args,
        &config,
        &git_ref,
        &git_sha,
        &timestamp,
        provisioned.as_ref(),
    );
    let cleanup = provisioned.as_mut().map_or(Ok(()), Provisioned::delete);
    finish_pass(result, cleanup)
}

/// Runs the benchmark on the instance and fetches its results.
fn run_on_instance(
    args: &BenchRemoteArgs,
    config: &BenchConfig,
    git_ref: &str,
    git_sha: &CommitSha,
    timestamp: &RunTimestamp,
    provisioned: Option<&Provisioned>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The directory the fetch takes, named by this run's start time. A run
    // that wrote no results leaves nothing at that path, and no earlier run's
    // directory can stand in for it.
    let results_dir = timestamp.results_dir().to_string_lossy().into_owned();

    if let Some(provisioned) = provisioned {
        copy_credentials(config, &provisioned.credentials_file)?;
    }

    let bench_args = bench_args(args, timestamp);

    let script = remote_script(
        git_ref,
        git_sha,
        &bench_args,
        &registry_env(provisioned.map(|p| &p.target)),
        ExistingRun::from_force(args.force),
    );

    eprintln!(
        "bench-remote: connecting to {}@{}...",
        config.user, config.host
    );

    let exit_code = match ssh_stream(config, &script)? {
        RemoteOutcome::Refused => {
            return Err(LIVE_RUN_REFUSAL.into());
        }
        RemoteOutcome::FailedBeforeStart(code) => {
            return Err(format!(
                "remote script exited with code {code} before the benchmark started; \
                 not fetching results"
            )
            .into());
        }
        RemoteOutcome::Ran(code) => code,
    };

    if exit_code != 0 {
        eprintln!("bench-remote: fetching partial results...");
    } else {
        eprintln!("bench-remote: fetching results...");
    }

    let results = fetch_run_results(config, &args.output, &results_dir);
    run_result(exit_code, results)
}

/// What the fetch of a run's results directory found.
#[derive(Debug, PartialEq, Eq)]
enum Fetch {
    /// The directory was on the instance and is copied, and the record it
    /// carries is applied.
    Copied,
    /// The directory is copied but holds no deferred record, so nothing was
    /// applied: it is from a run on code that predates `bench --archive`.
    CopiedWithoutRecord,
    /// There was no such directory: the run wrote no results.
    NoResults,
}

/// The result of a run whose script exited with `exit_code` and whose fetch
/// ended as `fetched`. A nonzero exit is an error whatever the fetch did, so a
/// refused or failed run is never a success to the caller; a failed fetch is
/// named alongside it. A script that exited 0 without writing results did not
/// do its job either.
fn run_result(
    exit_code: i32,
    fetched: Result<Fetch, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    match (exit_code, fetched) {
        (0, Ok(Fetch::Copied)) => Ok(()),
        (0, Ok(Fetch::CopiedWithoutRecord)) => Err(
            "remote script exited with code 0 but its results hold no deferred record, so \
             nothing reached the archive or the performance page"
                .into(),
        ),
        (0, Ok(Fetch::NoResults)) => {
            Err("remote script exited with code 0 but wrote no results".into())
        }
        (0, Err(e)) => Err(e),
        (code, Ok(_)) => Err(format!("remote script exited with code {code}").into()),
        (code, Err(e)) => Err(format!(
            "remote script exited with code {code}; fetching results failed: {e}"
        )
        .into()),
    }
}

/// The arguments `cargo xtask bench` runs with on the instance, unquoted:
/// `remote_script` quotes each one. The run starts at `timestamp`, which names
/// its results directory, and, so that the instance writes no tracked file,
/// defers its record and page blocks there until the fetch applies them
/// locally.
fn bench_args(args: &BenchRemoteArgs, timestamp: &RunTimestamp) -> Vec<String> {
    let mut bench_args = vec![
        "--archive".to_string(),
        value_name(&ArchiveMode::Defer),
        "--run-timestamp".to_string(),
        timestamp.to_string(),
        "--target".to_string(),
        value_name(&args.target),
        "--tools".to_string(),
        join_tools(&args.tool_selection.tools),
        "--rounds".to_string(),
        args.tool_selection.rounds.to_string(),
    ];
    if let Some(commit) = &args.tool_selection.baseline_ref {
        bench_args.extend(["--baseline-ref".to_string(), commit.to_string()]);
    }
    if let Some(cap) = args.tool_selection.max_concurrent_transfers {
        bench_args.extend(["--max-concurrent-transfers".to_string(), cap.to_string()]);
    }
    if let Some(limit) = args.limit {
        bench_args.extend(["--limit".to_string(), limit.to_string()]);
    }
    if let Some(skip) = &args.skip_registries {
        bench_args.extend(["--skip-registries".to_string(), skip.clone()]);
    }
    if args.skip_prewarm {
        bench_args.push("--skip-prewarm".to_string());
    }
    if args.no_proxy {
        bench_args.push("--no-proxy".to_string());
    }
    bench_args.push(value_name(&args.scenario));
    bench_args
}

/// Fails unless `provider` has registry env setup.
fn ensure_placement(provider: Provider) -> Result<(), String> {
    if provider != Provider::Aws {
        return Err(format!(
            "provider '{provider}' not yet supported. Add registry env setup for this provider."
        ));
    }
    Ok(())
}

/// The shell that points the benchmark at its target registry on the instance:
/// ECR in the instance's own account, or the repositories this machine made.
fn registry_env(target: Option<&ArtifactRegistryTarget>) -> String {
    match target {
        None => format!(
            "TOKEN=$(curl -s -X PUT 'http://169.254.169.254/latest/api/token' -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600')\nREGION=$(curl -s -H \"X-aws-ec2-metadata-token: $TOKEN\" http://169.254.169.254/latest/meta-data/placement/region)\nACCOUNT=$(aws sts get-caller-identity --query Account --output text)\nexport {ENV_REGISTRY}=${{ACCOUNT}}.dkr.ecr.${{REGION}}.amazonaws.com"
        ),
        // Every value was parsed as an id or a host, so none holds shell syntax.
        Some(target) => target
            .env()
            .iter()
            .map(|(name, value)| format!("export {name}={value}"))
            .chain([format!(
                "export {ENV_CREDENTIALS}=\"$HOME/{REMOTE_CREDENTIALS}\""
            )])
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Copies the Google federation file to the instance, readable by its user
/// alone: it is created closed before any content is in it.
fn copy_credentials(config: &BenchConfig, local: &Path) -> Result<(), Box<dyn std::error::Error>> {
    ssh_run(
        config,
        &format!("umask 077 && touch {REMOTE_CREDENTIALS} && chmod 600 {REMOTE_CREDENTIALS}"),
    )?;
    let status = Command::new("scp")
        .args(ssh_args(config))
        .arg(local)
        .arg(format!(
            "{}@{}:{REMOTE_CREDENTIALS}",
            config.user, config.host
        ))
        .status()?;
    if !status.success() {
        return Err("scp of the Google credentials file failed".into());
    }
    Ok(())
}

/// A line the remote script prints that says how far it got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sentinel {
    /// [`RUN_IN_PROGRESS_LINE`]: the guard found a live run.
    Refusal,
    /// [`BENCH_STARTED_LINE`]: the benchmark runner was launched.
    Started,
}

impl Sentinel {
    /// The sentinel `line` is, if any.
    fn from_line(line: &str) -> Option<Self> {
        match line {
            RUN_IN_PROGRESS_LINE => Some(Self::Refusal),
            BENCH_STARTED_LINE => Some(Self::Started),
            _ => None,
        }
    }
}

/// How the remote script ended.
#[derive(Debug, PartialEq, Eq)]
enum RemoteOutcome {
    /// The guard found a live run and left it alone.
    Refused,
    /// The script failed before the benchmark started, so it wrote no results
    /// to fetch.
    FailedBeforeStart(i32),
    /// The benchmark started and the script ended with this exit code.
    Ran(i32),
}

impl RemoteOutcome {
    /// The outcome of a script whose first sentinel line was `sentinel`.
    /// A refusal is the line, whatever the exit code: `set -e` makes any
    /// failing command exit with any code, the refusal's own `exit 3`
    /// included. Without a sentinel the script never reached the benchmark.
    fn new(sentinel: Option<Sentinel>, exit_code: Option<i32>) -> Self {
        let code = exit_code.unwrap_or(1);
        match sentinel {
            Some(Sentinel::Refusal) => Self::Refused,
            Some(Sentinel::Started) => Self::Ran(code),
            None => Self::FailedBeforeStart(code),
        }
    }
}

/// Command that receives the script on stdin. `setsid -w` makes the script
/// lead its own process group, so `$$` is a group id `--force` can kill, and
/// waits so the script's exit status is the command's. The benchmark runner is
/// a member of this group and, being a child of the script, outlives the SSH
/// session without a session of its own.
const REMOTE_COMMAND: &str = "setsid -w bash -s";

/// Pipe a script to the remote instance via SSH and stream stdout.
fn ssh_stream(
    config: &BenchConfig,
    script: &str,
) -> Result<RemoteOutcome, Box<dyn std::error::Error>> {
    let mut child = Command::new("ssh")
        .args(ssh_args(config))
        .args(["-o", "ServerAliveInterval=30"])
        .args(["-o", "ServerAliveCountMax=120"])
        .arg(format!("{}@{}", config.user, config.host))
        .arg(REMOTE_COMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    // `stdin` drops at the end of this block, which closes it so the remote
    // `bash -s` sees end of input; hoisting it out would hang the run.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }

    let mut sentinel = None;
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            sentinel = sentinel.or(Sentinel::from_line(&line));
            eprintln!("{line}");
        }
    }

    let status = child.wait()?;
    Ok(RemoteOutcome::new(sentinel, status.code()))
}

/// The newest results directory on the instance, as a path there, if any.
fn newest_results_dir(config: &BenchConfig) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let ls_output = ssh_run(
        config,
        "ls -td ~/ocync/bench/results/2* 2>/dev/null | head -1",
    )?;
    Ok(Some(ls_output.trim().to_string()).filter(|dir| !dir.is_empty()))
}

/// Fetches the run's results directory, relative to the repository on the
/// instance, if the run wrote it.
fn fetch_run_results(
    config: &BenchConfig,
    local_output: &str,
    results_dir: &str,
) -> Result<Fetch, Box<dyn std::error::Error>> {
    let remote_dir = format!("~/ocync/{results_dir}");
    let found = ssh_run(
        config,
        &format!("if [ -d {remote_dir} ]; then echo present; fi"),
    )?;
    if found.trim() != "present" {
        eprintln!("bench-remote: the run wrote no results; copying nothing");
        return Ok(Fetch::NoResults);
    }
    fetch_results(config, local_output, &remote_dir)
}

/// Copies the results directory `remote_dir` from the instance into
/// `local_output`, then writes the run record and the performance-page blocks
/// it carries into the local archive and page, as a local run does.
fn fetch_results(
    config: &BenchConfig,
    local_output: &str,
    remote_dir: &str,
) -> Result<Fetch, Box<dyn std::error::Error>> {
    let remote_basename = Path::new(remote_dir)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let local_dir = Path::new(local_output).join(remote_basename.as_ref());

    // Ensure the parent exists but NOT local_dir itself. OpenSSH >= 9.0
    // uses SFTP by default, which copies the source directory INTO an
    // existing target -- creating a nested duplicate (dir/dir/files).
    // Let SCP create the final directory.
    std::fs::create_dir_all(local_output)?;
    if local_dir.exists() {
        std::fs::remove_dir_all(&local_dir)?;
    }

    eprintln!(
        "bench-remote: copying {remote_dir} to {}",
        local_dir.display()
    );
    let status = Command::new("scp")
        .args(["-r"])
        .args(ssh_args(config))
        .arg(format!("{}@{}:{}", config.user, config.host, remote_dir))
        .arg(local_output)
        .status()?;

    if !status.success() {
        return Err("scp failed".into());
    }

    // The instance writes no tracked file. What a local run would have put in
    // the archive and the page is in the directory just copied, and goes in
    // here; applying it again is harmless.
    let fetched = match Deferred::read(&local_dir)? {
        Some(deferred) => {
            deferred.apply(Path::new(ARCHIVE_DIR), Path::new(PERFORMANCE_PAGE))?;
            Fetch::Copied
        }
        None => Fetch::CopiedWithoutRecord,
    };

    let summary = ssh_run(
        config,
        &format!("cat {remote_dir}/summary.md 2>/dev/null || true"),
    )?;
    if !summary.trim().is_empty() {
        eprintln!("\n{summary}");
    }

    Ok(fetched)
}

/// Run a single command on the remote instance and capture its stdout.
fn ssh_run(config: &BenchConfig, command: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("ssh")
        .args(ssh_args(config))
        .arg(format!("{}@{}", config.user, config.host))
        .arg(command)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh command failed: {stderr}").into());
    }

    Ok(String::from_utf8(output.stdout)?)
}

/// Escapes a string for use inside single quotes in a shell command.
///
/// Replaces each `'` with `'\''` (end single-quote, backslash-escaped
/// literal single-quote, restart single-quote).
fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Why a run is refused while another benchmark holds the instance.
const LIVE_RUN_REFUSAL: &str = "a benchmark is still running on the instance; pass --force to kill it, \
     or --fetch to copy the newest results there (a live run's partial results \
     once its benchmark has started)";

/// The run lock, relative to the repository on the instance.
const RUN_LOCK: &str = "bench/.bench-run.lock";

/// Whether a benchmark holds the instance's run lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunLock {
    /// No run holds it, or the instance has no repository yet.
    Free,
    /// A run holds it.
    Held,
}

impl RunLock {
    /// The answer [`run_lock_probe`] printed.
    fn parse(output: &str) -> Result<Self, String> {
        match output.trim() {
            "free" => Ok(Self::Free),
            "held" => Ok(Self::Held),
            other => Err(format!("run lock probe printed {other:?}")),
        }
    }
}

/// Asks the instance whether its run lock is held, without taking it for
/// longer than the question. Only [`FLOCK_HELD_EXIT`] means held, as in
/// [`existing_run_guard`]; any other `flock` failure is an error, never taken
/// for a free lock. No repository yet means no run.
fn run_lock_probe() -> String {
    format!(
        r#"cd ~/ocync 2>/dev/null || {{ echo free; exit 0; }}
rc=0; flock -n -E {FLOCK_HELD_EXIT} {RUN_LOCK} true || rc=$?
case "$rc" in
  0) echo free ;;
  {FLOCK_HELD_EXIT}) echo held ;;
  *) echo "flock failed (exit $rc); is util-linux installed?" >&2; exit 1 ;;
esac"#
    )
}

/// Refuses to touch Artifact Registry while a live run would be refused: the
/// leftover sweep would delete that run's repositories. `--force` kills the
/// run, so its repositories go with it.
fn may_provision(lock: RunLock, existing: ExistingRun) -> Result<(), String> {
    match (lock, existing) {
        (RunLock::Held, ExistingRun::Refuse) => Err(LIVE_RUN_REFUSAL.into()),
        _ => Ok(()),
    }
}

/// What to do with a benchmark still running on the instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingRun {
    /// Print [`RUN_IN_PROGRESS_LINE`] and stop, leaving it running.
    Refuse,
    /// Kill it and start fresh.
    Kill,
}

impl ExistingRun {
    /// The policy `--force` selects.
    fn from_force(force: bool) -> Self {
        if force { Self::Kill } else { Self::Refuse }
    }
}

/// Line the remote script prints, before exiting 3, when it refuses to
/// replace a live run.
const RUN_IN_PROGRESS_LINE: &str = "bench-remote: a benchmark is already running on this instance";

/// Line the remote script prints right after it launches the benchmark runner.
/// Results fetched after it are this run's, whole or partial.
const BENCH_STARTED_LINE: &str = "bench-remote: benchmark started, streaming log...";

/// Seconds `--force` waits for a killed run to release the run lock.
const KILL_WAIT_SECS: u32 = 30;

/// Exit status `flock -E` reports when the lock is held by someone else. No
/// other `flock` failure, such as 127 for a missing binary, uses it.
const FLOCK_HELD_EXIT: u8 = 75;

/// Shell that classifies the status `$rc` of a `flock -E` attempt: 0 means the
/// lock is taken, [`FLOCK_HELD_EXIT`] means it is held by someone else (the
/// caller's `held` branch), and anything else is a failure that must not be
/// mistaken for a held lock.
fn flock_status_check(held: &str) -> String {
    format!(
        r#"if [ "$rc" -eq {FLOCK_HELD_EXIT} ]; then
  {held}
elif [ "$rc" -ne 0 ]; then
  echo "bench-remote: error: flock failed (exit $rc); is util-linux installed?"
  exit 1
fi"#
    )
}

/// Shell that takes the run lock on fd 9, handling a live holder per
/// `existing`, and then records this script's process group in `$PGID_FILE`.
///
/// Needs `$LOCK_FILE` and `$PGID_FILE`. The lock is held until every process
/// holding fd 9 exits, so it spans checkout, build and the benchmark and
/// outlives the SSH session, and the kernel drops it when they die. Every
/// process that can hold fd 9 is in the process group `$PGID_FILE` names, which
/// is never removed: liveness is the lock alone, and the file only says whom
/// `--force` kills. It is read only while the lock is held by someone else.
///
/// Only `flock` exit status [`FLOCK_HELD_EXIT`] means "held". Any other failure
/// stops the script without the refusal line, so it is never taken for a live
/// run.
///
/// A new run never attaches to an old one, since the old one may be running a
/// binary built from different source.
fn existing_run_guard(existing: ExistingRun) -> String {
    let on_held = match existing {
        ExistingRun::Refuse => format!(
            r#"echo "{RUN_IN_PROGRESS_LINE}"
  exit 3"#
        ),
        ExistingRun::Kill => {
            let wait_status = flock_status_check(&format!(
                r#"echo "bench-remote: error: the run lock is still held after {KILL_WAIT_SECS}s"
  exit 1"#
            ));
            format!(
                r#"OLD_PGID=$(cat "$PGID_FILE" 2>/dev/null || true)
  if [[ "$OLD_PGID" =~ ^[0-9]+$ ]] && [ "$OLD_PGID" -gt 1 ]; then
    echo "bench-remote: --force killing process group $OLD_PGID"
    kill -9 -- "-$OLD_PGID" 2>/dev/null || true
  fi
  rc=0; flock -w {KILL_WAIT_SECS} -E {FLOCK_HELD_EXIT} 9 || rc=$?
  {wait_status}"#
            )
        }
    };
    let status = flock_status_check(&on_held);
    format!(
        r#"exec 9>"$LOCK_FILE"
rc=0; flock -n -E {FLOCK_HELD_EXIT} 9 || rc=$?
{status}
echo "$$" > "$PGID_FILE""#
    )
}

/// Shell that runs the benchmark with its own arguments and records how it
/// ended.
///
/// Fixed text: the benchmark's arguments arrive as `"$@"`, never as script
/// source. It runs in the remote script's process group, so `--force` kills the
/// harness and every tool it spawns with the rest of the run. Needs
/// `$EXIT_FILE`. The exit status is captured with `if` because `set -e` would
/// otherwise end the script before recording it.
const BENCH_RUNNER: &str = r#"set -euo pipefail
if cargo xtask bench "$@" 2>&1; then BENCH_EXIT=0; else BENCH_EXIT=$?; fi
echo "$BENCH_EXIT" > "$EXIT_FILE"
echo "[bench-remote] exit code: $BENCH_EXIT"
exit "$BENCH_EXIT""#;

/// Removes untracked files left by previous builds, keeping the build cache,
/// results, the baseline checkouts and the run's own files. The run-file pattern is quoted so the
/// shell cannot expand it against the files present. Runs with fd 9 closed
/// like every git command in the script, see [`remote_script`].
const GIT_CLEAN: &str = "git clean -fdx -e target/ -e bench/results/ -e 'bench/.bench-run.*' -e bench/.tmp/ -e bench/.baseline/ 9>&-";

/// Shell that starts the benchmark runner in the background in the script's
/// own process group, passing `bench_args` as one quoted word each so no
/// value is ever shell source.
fn runner_launch(bench_args: &[String]) -> String {
    let quoted: Vec<String> = bench_args
        .iter()
        .map(|arg| format!("'{}'", shell_escape(arg)))
        .collect();
    format!(
        r#"bash "$RUNNER_FILE" {} >> "$LOG_FILE" 2>&1 &"#,
        quoted.join(" ")
    )
}

/// The script piped to [`REMOTE_COMMAND`] on the instance.
///
/// Every value is already shell-escaped or trusted: `bench_args` are quoted
/// here, `registry_env` is shell text, and a [`CommitSha`] is hex.
///
/// Git commands run with fd 9 (the run lock) closed: git can daemonize a
/// background gc that calls `setsid`, which would hold the lock from outside
/// the process group `--force` kills.
fn remote_script(
    git_ref: &str,
    git_sha: &CommitSha,
    bench_args: &[String],
    registry_env: &str,
    existing: ExistingRun,
) -> String {
    let existing_run = existing_run_guard(existing);
    let escaped_git_ref = shell_escape(git_ref);
    let runner_launch = runner_launch(bench_args);
    format!(
        r#"#!/bin/bash
set -euo pipefail
exec 2>&1

# Wait for cloud-init to finish (repo clone, tool install, etc.).
if command -v cloud-init &>/dev/null; then
  STATUS=$(cloud-init status 2>/dev/null | awk '{{print $2}}' || echo "unknown")
  if [ "$STATUS" = "running" ]; then
    echo "bench-remote: waiting for cloud-init to finish..."
    cloud-init status --wait >/dev/null 2>&1 || true
    echo "bench-remote: cloud-init complete"
  fi
fi

if [ ! -d ~/ocync ]; then
  echo "bench-remote: error: ~/ocync not found after cloud-init. Check the bootstrap log:"
  echo "  ssh $USER@$(hostname -I | awk '{{print $1}}') 'sudo tail -50 /var/log/user-data.log'"
  exit 1
fi

cd ~/ocync
LOCK_FILE={RUN_LOCK}
PGID_FILE=bench/.bench-run.pgid
LOG_FILE=bench/.bench-run.log
EXIT_FILE=bench/.bench-run.exit
RUNNER_FILE=bench/.bench-run.sh
# The run lock is held from here to the end of the benchmark, so a second
# invocation cannot delete target/ under a run that is still checking out or
# building. The script leads its own process group (see REMOTE_COMMAND) and
# the benchmark runner stays in it, so --force can end the run at any stage.
{existing_run}

# Pull code and build. Use exact SHA (not branch name) so force-pushes
# between our local push and the instance's fetch cannot change what gets
# built. The SHA is resolved locally before pushing and is immutable.
echo '[1/4] Checking out {git_sha} ({escaped_git_ref})...'
git fetch origin 9>&-
# Instances carry edits to these tracked files from runs on older code, which
# wrote them; a run on this code never does, so this only discards output that
# bench-remote already copied after that run. Without it the checkout below
# refuses once main changes either file.
git checkout -- bench/results/ docs/src/content/performance.md 9>&-
git checkout '{git_sha}' --detach 9>&-
# Remove untracked files left by previous builds or deleted from the repo.
{GIT_CLEAN}

echo '[2/4] Building ocync + bench-proxy...'
source ~/.bench-env 2>/dev/null || true
source ~/.cargo/env
# Nuke the entire target directory. cargo's mtime-based fingerprinting
# is unreliable after git operations (git does not preserve mtimes), and
# partial cleans (cargo clean -p, cargo clean --release) miss transitive
# deps, LTO artifacts, and the xtask dev build itself. A full nuke adds
# ~5 minutes (rebuilds deps from source) but guarantees every binary
# matches the checked-out source. Worth it vs hours of debugging stale
# binaries.
rm -rf target/
CARGO_INCREMENTAL=0 cargo build --release --package ocync --package bench-proxy 2>&1 | tee /tmp/bench-build.log
# Verify cargo actually recompiled our code.
if ! grep -q 'Compiling ocync ' /tmp/bench-build.log; then
  echo "FATAL: cargo did not recompile ocync -- stale build artifacts"
  exit 1
fi
cp target/release/ocync ~/.cargo/bin/ocync
cp target/release/bench-proxy ~/.cargo/bin/bench-proxy

echo '[3/4] Starting benchmarks...'
# Use real disk for temp files, not tmpfs (/tmp is RAM-backed on AL2023
# and fills up with multi-GB blob staging data).
export TMPDIR=$HOME/ocync/bench/.tmp
rm -rf "$TMPDIR"
mkdir -p "$TMPDIR"
{registry_env}

# Launch in the background with output to the log file. The runner stays in
# this script's process group, so --force kills the harness and every tool it
# spawned. fd 9 (the run lock) is inherited, so the lock outlives this SSH
# session.
> "$LOG_FILE"
rm -f "$EXIT_FILE"
cat > "$RUNNER_FILE" <<'RUNNER_EOF'
{BENCH_RUNNER}
RUNNER_EOF
export EXIT_FILE
{runner_launch}
BENCH_PID=$!
echo "{BENCH_STARTED_LINE}"

# Stream log until bench exits. --pid= makes tail exit when process dies.
tail -n+1 -f "$LOG_FILE" --pid="$BENCH_PID"

# Read exit code from file (reliable, not subject to wait race).
if [ -f "$EXIT_FILE" ]; then
  EXIT=$(cat "$EXIT_FILE")
  rm -f "$EXIT_FILE"
else
  EXIT=1
fi
echo '[4/4] Benchmarks complete.'
exit $EXIT
"#
    )
}

/// Runs `git` with `args` in `repo` and returns its trimmed output, or an error
/// carrying git's own message.
pub(crate) fn git(repo: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git").current_dir(repo).args(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Runs `git rev-parse` with `args` in `repo`.
fn git_rev_parse(repo: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    git(repo, &[&["rev-parse"], args].concat())
}

/// Fails unless a remote branch holds `commit`, as of a fetch now. The
/// instance fetches from origin, so a commit only this machine has cannot be
/// built there.
fn ensure_on_origin(repo: &Path, commit: &CommitSha) -> Result<(), Box<dyn std::error::Error>> {
    // Pruning drops the tracking ref of a branch deleted on origin, and the
    // pattern leaves out the branches of any other remote.
    git(repo, &["fetch", "--prune", "origin"])?;
    let sha = commit.to_string();
    if git(
        repo,
        &["branch", "-r", "--contains", &sha, "--list", "origin/*"],
    )?
    .is_empty()
    {
        return Err(format!(
            "the baseline commit {sha} is not on origin, where the instance fetches from; \
             push it first"
        )
        .into());
    }
    Ok(())
}

/// A full commit object name: 40 lowercase hex digits, safe to place in a
/// shell command unescaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitSha(String);

impl CommitSha {
    /// A fixed id for tests that name a commit but never look one up.
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self("0123456789abcdef0123456789abcdef01234567".to_owned())
    }

    /// The leading 7 digits, for a label a reader can type.
    pub(crate) fn short(&self) -> &str {
        &self.0[..7]
    }

    /// Accepts only 40 lowercase hex digits.
    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error>> {
        if s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!("git resolved to {s:?}, not a 40-digit lowercase hex commit id").into())
        }
    }
}

impl std::fmt::Display for CommitSha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolves `git_ref` to the commit it names in the repository at `repo`.
pub(crate) fn resolve_commit(
    repo: &Path,
    git_ref: &str,
) -> Result<CommitSha, Box<dyn std::error::Error>> {
    CommitSha::parse(&git_rev_parse(
        repo,
        &["--verify", &format!("{git_ref}^{{commit}}")],
    )?)
}

/// Get the current local git branch name.
fn current_branch() -> Result<String, Box<dyn std::error::Error>> {
    git_rev_parse(Path::new("."), &["--abbrev-ref", "HEAD"])
}

/// Runs `git` in `repo` for a test, asserting it succeeds. Anything of the
/// developer's own config is kept out of the run.
#[cfg(test)]
pub(crate) fn git_in(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        // A developer's gpgsign, hooks or templates must not reach the test.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=bench",
            "-c",
            "user.email=bench@example.com",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::io::BufRead;

    use super::*;

    #[test]
    fn parse_bench_config() {
        let json = r#"{"provider":"aws","host":"54.123.45.67","user":"ec2-user","ssh_key_path":"/tmp/.ssh-key"}"#;
        let config: BenchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.host, "54.123.45.67");
        assert_eq!(config.user, "ec2-user");
        assert_eq!(config.ssh_key_path, "/tmp/.ssh-key");
    }

    #[test]
    fn read_config_without_bench_json_says_to_apply_terraform() {
        let msg = read_config(Provider::Azure).unwrap_err().to_string();
        assert!(
            msg.contains("bench/terraform/azure/bench.json") && msg.contains("terraform apply"),
            "got: {msg}"
        );
    }

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape("main"), "main");
    }

    #[test]
    fn shell_escape_with_single_quote() {
        assert_eq!(shell_escape("it's"), "it'\\''s");
    }

    #[test]
    fn shell_escape_multiple_quotes() {
        assert_eq!(shell_escape("a'b'c"), "a'\\''b'\\''c");
    }

    #[test]
    fn config_path_uses_provider() {
        for (provider, expected) in [
            (Provider::Aws, "bench/terraform/aws/bench.json"),
            (Provider::Gcp, "bench/terraform/gcp/bench.json"),
            (Provider::Azure, "bench/terraform/azure/bench.json"),
        ] {
            assert_eq!(config_path(provider), PathBuf::from(expected), "{provider}");
        }
    }

    #[test]
    fn resolve_commit_follows_the_ref_not_head() {
        let repo = tempfile::tempdir().unwrap();
        git_in(repo.path(), &["init", "-q", "-b", "main"]);
        git_in(repo.path(), &["commit", "-q", "--allow-empty", "-m", "one"]);
        git_in(repo.path(), &["branch", "other"]);
        git_in(repo.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);
        let other = git_in(repo.path(), &["rev-parse", "other"]);

        let resolved = resolve_commit(repo.path(), "other").unwrap();

        assert_eq!(resolved.to_string(), other);
        assert_ne!(
            resolved.to_string(),
            git_in(repo.path(), &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn a_baseline_commit_must_be_on_origin_before_the_instance_is_asked_for_it() {
        let origin = tempfile::tempdir().unwrap();
        git_in(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
        let work = tempfile::tempdir().unwrap();
        git_in(work.path(), &["init", "-q", "-b", "main"]);
        git_in(
            work.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        git_in(
            work.path(),
            &["commit", "-q", "--allow-empty", "-m", "pushed"],
        );
        git_in(work.path(), &["push", "-q", "origin", "main"]);
        let pushed = resolve_commit(work.path(), "HEAD").unwrap();
        git_in(
            work.path(),
            &["commit", "-q", "--allow-empty", "-m", "local only"],
        );
        let local = resolve_commit(work.path(), "HEAD").unwrap();

        // Catches: a baseline only this machine has, which the instance could
        // not fetch, failing after the push and the build.
        assert!(ensure_on_origin(work.path(), &pushed).is_ok());
        let pushed_ref = &pushed.to_string();
        let refused = |commit: &CommitSha| {
            ensure_on_origin(work.path(), commit)
                .unwrap_err()
                .to_string()
        };
        let not_on_origin = |commit: &CommitSha| {
            format!(
                "the baseline commit {commit} is not on origin, where the instance fetches \
                 from; push it first"
            )
        };
        assert_eq!(refused(&local), not_on_origin(&local));

        // Catches: a commit only another remote has, and one on a branch since
        // deleted on origin, both of which the instance cannot fetch.
        let fork = tempfile::tempdir().unwrap();
        git_in(fork.path(), &["init", "-q", "--bare", "-b", "main"]);
        git_in(
            work.path(),
            &["remote", "add", "fork", fork.path().to_str().unwrap()],
        );
        git_in(work.path(), &["push", "-q", "fork", "main"]);
        assert_eq!(refused(&local), not_on_origin(&local));

        git_in(work.path(), &["push", "-q", "origin", "main:gone"]);
        assert!(ensure_on_origin(work.path(), &local).is_ok());
        // Deleted at origin itself, so only a pruning fetch drops the tracking ref.
        git_in(origin.path(), &["branch", "-q", "-D", "gone"]);
        git_in(
            origin.path(),
            &["update-ref", "refs/heads/main", pushed_ref],
        );
        assert_eq!(refused(&local), not_on_origin(&local));
    }

    #[test]
    fn resolve_commit_error_carries_gits_message() {
        let not_a_repo = tempfile::tempdir().unwrap();

        let err = resolve_commit(not_a_repo.path(), "main").unwrap_err();

        let msg = err.to_string();
        let detail = msg
            .strip_prefix("git rev-parse --verify main^{commit} failed: ")
            .unwrap_or_else(|| panic!("unexpected error shape: {msg}"));
        assert!(!detail.is_empty(), "git's stderr missing: {msg}");
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: BenchRemoteArgs,
    }

    #[test]
    fn fetch_conflicts_with_every_run_option() {
        use clap::Parser;
        let head =
            crate::bench::command_stdout(Command::new("git").args(["rev-parse", "HEAD"])).unwrap();
        for extra in [
            &["--force"][..],
            &["--git-ref", "main"][..],
            &["--target", "ecr"][..],
            &["--tools", "ocync"][..],
            &["--rounds", "3"][..],
            &["--max-concurrent-transfers", "20"][..],
            &["--baseline-ref", &head][..],
            &["--scenario", "sync"][..],
            &["--limit", "3"][..],
            &["--skip-registries", "docker.io"][..],
            &["--skip-prewarm"][..],
            &["--no-proxy"][..],
        ] {
            let argv = ["xtask", "--provider", "aws", "--fetch"]
                .into_iter()
                .chain(extra.iter().copied());
            let err = Cli::try_parse_from(argv).err().unwrap();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{extra:?}"
            );
        }
        assert!(Cli::try_parse_from(["xtask", "--provider", "aws", "--fetch"]).is_ok());
        assert!(
            Cli::try_parse_from(["xtask", "--provider", "aws", "--fetch", "--output", "out"])
                .is_ok()
        );
    }

    #[test]
    fn closed_set_options_parse_to_types_and_refuse_anything_else() {
        use clap::Parser;
        let run = Cli::try_parse_from(["xtask", "--provider", "gcp", "--scenario", "prepare"])
            .unwrap()
            .args;
        assert_eq!(run.provider, Provider::Gcp);
        assert!(matches!(run.scenario, Scenario::Prepare));

        for (name, argv) in [
            ("provider", &["--provider", "digitalocean"][..]),
            ("scenario", &["--provider", "aws", "--scenario", "cold"][..]),
        ] {
            let err = Cli::try_parse_from(["xtask"].into_iter().chain(argv.iter().copied()))
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted"));
            assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue, "{name}");
        }
    }

    #[test]
    fn a_provider_without_registry_setup_is_refused_by_name() {
        assert!(registry_env(None).contains(ENV_REGISTRY));
        assert_eq!(ensure_placement(Provider::Aws), Ok(()));
        for (provider, expected) in [
            (
                Provider::Gcp,
                "provider 'gcp' not yet supported. Add registry env setup for this provider.",
            ),
            (
                Provider::Azure,
                "provider 'azure' not yet supported. Add registry env setup for this provider.",
            ),
        ] {
            assert_eq!(ensure_placement(provider), Err(expected.to_string()));
        }
    }

    #[test]
    fn the_instance_is_told_its_repositories_and_where_the_credentials_are() {
        let target = ArtifactRegistryTarget {
            location: "europe-west9".parse().unwrap(),
            project: "example-project".parse().unwrap(),
            repositories: ["run-r1", "run-r2", "run-r3"]
                .map(|id| id.parse().unwrap())
                .to_vec(),
        };
        assert_eq!(
            registry_env(Some(&target)),
            "export BENCH_TARGET_REGISTRY=europe-west9-docker.pkg.dev\n\
             export BENCH_GAR_PROJECT=example-project\n\
             export BENCH_GAR_REPOSITORIES=run-r1,run-r2,run-r3\n\
             export GOOGLE_APPLICATION_CREDENTIALS=\"$HOME/.bench-gar-credentials.json\""
        );
    }

    #[test]
    fn the_target_reaches_the_benchmark_on_the_instance() {
        // Catches: `bench-remote` choosing Artifact Registry and the instance
        // defaulting to ECR, or the reverse.
        use clap::Parser;
        let timestamp = RunTimestamp::parse("2026-10-30-120000").unwrap();
        let passed = |extra: &[&str]| {
            let argv = ["xtask", "--provider", "aws"]
                .into_iter()
                .chain(extra.iter().copied());
            let args = Cli::try_parse_from(argv).unwrap().args;
            let bench_args = bench_args(&args, &timestamp);
            let argv: Vec<&str> = bench_args.iter().map(String::as_str).collect();
            crate::bench::parse_bench(&argv).unwrap().target
        };
        assert_eq!(passed(&[]), Kind::Ecr);
        assert_eq!(
            passed(&["--target", "artifact-registry"]),
            Kind::ArtifactRegistry
        );
    }

    #[test]
    fn force_selects_the_kill_policy() {
        assert_eq!(ExistingRun::from_force(true), ExistingRun::Kill);
        assert_eq!(ExistingRun::from_force(false), ExistingRun::Refuse);
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn script_for(existing: ExistingRun) -> String {
        remote_script(
            "main",
            &CommitSha::parse(SHA).unwrap(),
            &[
                "--tools".to_string(),
                "ocync".to_string(),
                "all".to_string(),
            ],
            &format!("export {ENV_REGISTRY}=registry.example.com"),
            existing,
        )
    }

    #[test]
    fn remote_script_guards_before_anything_destructive() {
        for existing in [ExistingRun::Refuse, ExistingRun::Kill] {
            let script = script_for(existing);
            let at = |needle: &str| {
                script
                    .find(needle)
                    .unwrap_or_else(|| panic!("{needle:?} missing from the script"))
            };
            let guard = at("flock -n");
            assert!(
                guard < at("git checkout"),
                "{existing:?}: guard after checkout"
            );
            assert!(guard < at("rm -rf target/"), "{existing:?}: guard after rm");
            assert!(at(r#"bash "$RUNNER_FILE""#) > at("rm -rf target/"));
            assert!(
                !script.contains("setsid bash"),
                "{existing:?}: the runner must stay in the script's group"
            );
            assert!(
                !script.contains("rm -f \"$PGID_FILE\""),
                "{existing:?}: the group file must never be removed"
            );
            assert_eq!(
                script.matches(r#"> "$PGID_FILE""#).count(),
                1,
                "{existing:?}: only the guard records the group"
            );
        }
    }

    #[test]
    fn git_runs_with_the_run_lock_closed() {
        let script = script_for(ExistingRun::Refuse);
        let git_lines: Vec<&str> = script
            .lines()
            .filter(|line| line.starts_with("git "))
            .collect();
        assert_eq!(
            git_lines.len(),
            4,
            "fetch, restore, checkout, clean: {git_lines:?}"
        );

        // Each line runs against a stand-in `git` that reports whether fd 9
        // was open when it was called.
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let probe = format!(
            "git() {{ if [ -e /dev/fd/9 ]; then echo open; else echo closed; fi >> \"$SEEN\"; }}\n\
             exec 9>/dev/null\n{}",
            git_lines.join("\n")
        );
        let status = Command::new("bash")
            .args(["-c", &probe])
            .env("SEEN", &seen)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            "closed\nclosed\nclosed\nclosed\n"
        );
    }

    #[test]
    fn the_outcome_follows_the_sentinel_line_not_the_exit_code() {
        let rows = [
            (Some(Sentinel::Refusal), Some(0), RemoteOutcome::Refused),
            (Some(Sentinel::Refusal), Some(3), RemoteOutcome::Refused),
            (Some(Sentinel::Started), Some(0), RemoteOutcome::Ran(0)),
            (Some(Sentinel::Started), Some(3), RemoteOutcome::Ran(3)),
            (Some(Sentinel::Started), None, RemoteOutcome::Ran(1)),
            // A failure with no started line never reached the benchmark,
            // whatever its exit code.
            (None, Some(3), RemoteOutcome::FailedBeforeStart(3)),
            (None, Some(1), RemoteOutcome::FailedBeforeStart(1)),
            (None, None, RemoteOutcome::FailedBeforeStart(1)),
        ];
        for (sentinel, code, want) in rows {
            assert_eq!(
                RemoteOutcome::new(sentinel, code),
                want,
                "{sentinel:?} {code:?}"
            );
        }
    }

    #[test]
    fn sentinel_lines_are_matched_whole() {
        assert_eq!(
            Sentinel::from_line(RUN_IN_PROGRESS_LINE),
            Some(Sentinel::Refusal)
        );
        assert_eq!(
            Sentinel::from_line(BENCH_STARTED_LINE),
            Some(Sentinel::Started)
        );
        assert_eq!(
            Sentinel::from_line(&format!("{BENCH_STARTED_LINE} (PID 1)")),
            None
        );
        assert_eq!(Sentinel::from_line("[1/4] Updating repo..."), None);
    }

    #[test]
    fn the_script_prints_the_started_line_only_after_launching_the_runner() {
        let script = script_for(ExistingRun::Refuse);

        assert_eq!(script.matches(BENCH_STARTED_LINE).count(), 1);
        assert!(
            script.find(BENCH_STARTED_LINE).unwrap()
                > script.find(r#"bash "$RUNNER_FILE""#).unwrap()
        );
    }

    #[test]
    fn the_exit_code_and_the_fetch_decide_the_result() {
        let err = |exit, fetched| run_result(exit, fetched).unwrap_err().to_string();

        assert!(run_result(0, Ok(Fetch::Copied)).is_ok());
        assert_eq!(
            err(0, Ok(Fetch::NoResults)),
            "remote script exited with code 0 but wrote no results"
        );
        assert!(
            err(0, Ok(Fetch::CopiedWithoutRecord)).contains("no deferred record"),
            "a copied run that applied nothing is not a success"
        );
        assert_eq!(err(0, Err("scp failed".into())), "scp failed");
        // A failed script is an error whatever the fetch found.
        for fetched in [
            Ok(Fetch::Copied),
            Ok(Fetch::CopiedWithoutRecord),
            Ok(Fetch::NoResults),
        ] {
            assert_eq!(err(3, fetched), "remote script exited with code 3");
        }
        assert_eq!(
            err(1, Err("scp failed".into())),
            "remote script exited with code 1; fetching results failed: scp failed"
        );
    }

    #[test]
    fn the_benchmark_defers_its_record_into_the_directory_bench_remote_names() {
        use clap::Parser;
        let args = Cli::try_parse_from(["xtask", "--provider", "aws", "--limit", "3"])
            .unwrap()
            .args;
        let timestamp = RunTimestamp::parse("2026-10-30-120000").unwrap();
        let bench_args = bench_args(&args, &timestamp);
        let argv: Vec<&str> = bench_args.iter().map(String::as_str).collect();

        // The args are what `bench` itself parses, so a rename of either
        // option breaks this rather than a run on the instance.
        let parsed = crate::bench::parse_bench(&argv).unwrap();
        assert_eq!(parsed.archive, ArchiveMode::Defer);
        // The one value that names the directory fetched and stamps the record.
        assert_eq!(parsed.run_timestamp, Some(timestamp));
        assert_eq!(parsed.output, None);
        assert_eq!(parsed.limit, Some(3));
        assert_eq!(
            crate::bench::parse_bench(&["sync"]).unwrap().archive,
            ArchiveMode::Write,
            "a local run keeps writing the tracked files"
        );
    }

    #[test]
    fn the_transfer_cap_reaches_the_benchmark_on_the_instance_only_when_set() {
        // Catches: `bench-remote` accepting the option and dropping it, so a
        // run labelled with a cap measures ocync's default.
        use clap::Parser;
        let timestamp = RunTimestamp::parse("2026-10-30-120000").unwrap();
        let passed = |extra: &[&str]| {
            let argv = ["xtask", "--provider", "aws"]
                .into_iter()
                .chain(extra.iter().copied());
            let args = Cli::try_parse_from(argv).unwrap().args;
            bench_args(&args, &timestamp)
        };

        let capped = passed(&["--max-concurrent-transfers", "20"]);
        let argv: Vec<&str> = capped.iter().map(String::as_str).collect();
        assert_eq!(
            crate::bench::parse_bench(&argv)
                .unwrap()
                .tool_selection
                .max_concurrent_transfers,
            std::num::NonZeroUsize::new(20)
        );

        assert!(
            !passed(&[])
                .iter()
                .any(|a| a == "--max-concurrent-transfers")
        );
    }

    #[test]
    fn older_runs_edits_are_discarded_under_the_lock_before_the_checkout() {
        for existing in [ExistingRun::Refuse, ExistingRun::Kill] {
            let script = script_for(existing);
            let at = |needle: &str| {
                script
                    .find(needle)
                    .unwrap_or_else(|| panic!("{needle:?} missing from the script"))
            };
            let restore = at("git checkout -- bench/results/ docs/src/content/performance.md");
            assert!(
                at("flock -n") < restore,
                "{existing:?}: restore before lock"
            );
            assert!(
                restore < at("git checkout '"),
                "{existing:?}: restore after checkout"
            );
            assert!(
                script[restore..].lines().next().unwrap().ends_with(" 9>&-"),
                "{existing:?}: restore keeps the run lock open"
            );
            assert!(!script.contains("ls -td"), "no listing to agree with");
        }
    }

    /// One call of `fetch_run_results` in a child of this test binary, run in
    /// `work` with `ssh` and `scp` stubs leading its `PATH`, so this process's
    /// environment is untouched. The `ssh` stub answers the directory probe
    /// with present or absent; the `scp` stub copies `fixture` in place of the
    /// run's directory and logs the call. Returns what the child printed of the
    /// result and the `scp` log.
    fn fetch_via_stubs(
        work: &Path,
        fixture: &Path,
        dir_present: bool,
        results_dir: &str,
    ) -> (String, String) {
        let bin = work.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [
            (
                "ssh",
                r#"case "$*" in
  *'[ -d'*) [ -n "$STUB_PRESENT" ] && echo present ;;
esac
exit 0"#,
            ),
            (
                "scp",
                r#"echo "$@" >> "$STUB_LOG"
src="${@: -2:1}"
cp -r "$STUB_FIXTURE" "${@: -1}/$(basename "${src#*:}")""#,
            ),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }

        let log = work.join("scp.log");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                // Test names omit the crate, which `module_path!` leads with.
                &format!(
                    "{}::fetch_with_stubs",
                    module_path!().split_once("::").unwrap().1
                ),
                "--nocapture",
            ])
            .current_dir(work)
            .env("PATH", path)
            .env("STUB_CHILD", results_dir)
            .env("STUB_PRESENT", if dir_present { "1" } else { "" })
            .env("STUB_FIXTURE", fixture)
            .env("STUB_LOG", &log)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the child did not pass: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let result = stdout
            .lines()
            .find_map(|line| line.strip_prefix("FETCH RESULT: "))
            .unwrap_or_else(|| panic!("the child reported no result: {stdout}"))
            .to_string();
        (result, std::fs::read_to_string(&log).unwrap_or_default())
    }

    /// The child of [`fetch_via_stubs`]; a no-op in a normal test run.
    #[test]
    fn fetch_with_stubs() {
        let Some(results_dir) = std::env::var_os("STUB_CHILD") else {
            return;
        };
        let config = BenchConfig {
            host: "h".into(),
            user: "u".into(),
            ssh_key_path: "/k".into(),
        };
        let fetched = fetch_run_results(&config, "out", results_dir.to_str().unwrap());
        println!("FETCH RESULT: {fetched:?}");
    }

    #[test]
    fn the_fetch_copies_a_present_run_once_and_an_absent_one_not_at_all() {
        use crate::bench::report::{Deferred, PageBlocks, RunRecord, tests::sample_report};
        let work = tempfile::tempdir().unwrap();
        let archive = work.path().join("bench/results/ecr.json");
        let records = || -> usize {
            std::fs::read_to_string(&archive)
                .map(|json| serde_json::from_str::<Vec<RunRecord>>(&json).unwrap().len())
                .unwrap_or(0)
        };
        let report = sample_report();
        let record = RunRecord::of(&report, "abc1234".into(), None);

        let with_record = tempfile::tempdir().unwrap();
        Deferred::new("ecr", Some(record), PageBlocks::of(&report))
            .write(with_record.path())
            .unwrap();
        let without_record = tempfile::tempdir().unwrap();
        let dir = "bench/results/2026-04-15-100000";

        // Absent: the run wrote no results. Nothing is copied or recorded, even
        // with a fixture at hand that a stray copy would pick up.
        let (result, scp) = fetch_via_stubs(work.path(), with_record.path(), false, dir);
        assert_eq!(result, "Ok(NoResults)");
        assert_eq!(scp, "", "nothing may be copied");
        assert_eq!(records(), 0);
        assert!(!work.path().join("out").exists());

        // Present: copied and recorded, and a second fetch records nothing more.
        for _ in 0..2 {
            let (result, scp) = fetch_via_stubs(work.path(), with_record.path(), true, dir);
            assert_eq!(result, "Ok(Copied)");
            assert!(scp.contains("2026-04-15-100000"), "{scp}");
            assert_eq!(records(), 1);
        }

        // Present without a deferred record: copied, nothing applied.
        let (result, _) = fetch_via_stubs(work.path(), without_record.path(), true, dir);
        assert_eq!(result, "Ok(CopiedWithoutRecord)");
        assert_eq!(records(), 1);
    }

    #[test]
    fn remote_script_pins_the_resolved_sha() {
        let script = script_for(ExistingRun::Refuse);
        assert!(script.contains(&format!("git checkout '{SHA}' --detach")));
    }

    #[test]
    fn commit_sha_accepts_only_forty_lowercase_hex_digits() {
        assert_eq!(CommitSha::parse(SHA).unwrap().to_string(), SHA);
        for bad in [
            "",
            "0123456789abcdef",
            "0123456789ABCDEF0123456789ABCDEF01234567",
            "0123456789abcdef0123456789abcdef0123456'; rm -rf ~; '",
            "0123456789abcdef0123456789abcdef012345678",
        ] {
            assert!(CommitSha::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn git_clean_keeps_the_run_files_and_removes_strays() {
        let repo = tempfile::tempdir().unwrap();
        git_in(repo.path(), &["init", "-q", "-b", "main"]);
        let bench = repo.path().join("bench");
        std::fs::create_dir(&bench).unwrap();
        let run_files = ["lock", "log", "pgid", "sh", "exit"].map(|ext| {
            let file = bench.join(format!(".bench-run.{ext}"));
            std::fs::write(&file, "").unwrap();
            file
        });
        let baseline_build = bench.join(".baseline/0123456/target/release/ocync");
        std::fs::create_dir_all(baseline_build.parent().unwrap()).unwrap();
        std::fs::write(&baseline_build, "").unwrap();
        let stray = bench.join("stray");
        std::fs::write(&stray, "").unwrap();

        let status = Command::new("bash")
            .args(["-c", GIT_CLEAN])
            .current_dir(repo.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();

        assert!(status.success());
        for file in &run_files {
            assert!(file.exists(), "{} was cleaned", file.display());
        }
        assert!(baseline_build.exists(), "the baseline build was cleaned");
        assert!(!stray.exists(), "the stray file survived");
    }

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Previous {
        /// A run holds the lock, in its own process group.
        Live,
        /// The run's group leader is gone, but another member of the group
        /// still holds the lock.
        LeaderGone,
        /// No run holds the lock, and a stale group file names a bystander.
        Dead,
    }

    /// Starts `script` as its own session leader, so its pid is its group id.
    #[cfg(target_os = "linux")]
    fn session_leader(script: &str, lock: &Path) -> std::process::Child {
        Command::new("setsid")
            .args(["bash", "-c", script])
            .env("LOCK_FILE", lock)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Whether any process is left in process group `pgid`.
    #[cfg(target_os = "linux")]
    fn group_alive(pgid: u32) -> bool {
        Command::new("bash")
            .args(["-c", r#"kill -0 -- "-$0" 2>/dev/null"#, &pgid.to_string()])
            .status()
            .unwrap()
            .success()
    }

    /// Runs the guard for `existing` followed by `echo proceeded`, with
    /// `path` as the guard's `PATH` when given.
    #[cfg(target_os = "linux")]
    fn run_guard(
        existing: ExistingRun,
        lock: &Path,
        pgid_file: &Path,
        path: Option<&Path>,
    ) -> std::process::Output {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(format!(
                "set -euo pipefail\necho \"guard pid $$\"\n{}\necho proceeded",
                existing_run_guard(existing)
            ))
            .env("LOCK_FILE", lock)
            .env("PGID_FILE", pgid_file);
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    }

    /// A held lock without `--force` refuses before any repository is swept
    /// or made; an unreadable probe is an error, never taken as free.
    #[test]
    fn a_live_run_keeps_its_repositories() {
        assert_eq!(
            may_provision(RunLock::Held, ExistingRun::Refuse),
            Err(LIVE_RUN_REFUSAL.to_string())
        );
        assert_eq!(may_provision(RunLock::Held, ExistingRun::Kill), Ok(()));
        assert_eq!(may_provision(RunLock::Free, ExistingRun::Refuse), Ok(()));
        assert_eq!(RunLock::parse("held\n"), Ok(RunLock::Held));
        assert_eq!(RunLock::parse("free\n"), Ok(RunLock::Free));
        assert_eq!(
            RunLock::parse(""),
            Err("run lock probe printed \"\"".to_string())
        );
    }

    // The probe is shell for the Linux bench instance: it needs util-linux
    // `flock`.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_lock_probe_says_held_only_for_a_held_lock() {
        // Catches: any `flock` failure, such as a missing binary, read as a
        // live run (or a free lock), and a held lock read as free.
        let home = tempfile::tempdir().unwrap();
        let probe = |path: Option<&str>| {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(run_lock_probe())
                .env("HOME", home.path());
            if let Some(path) = path {
                command.env("PATH", path);
            }
            let out = command.output().unwrap();
            (out.status.success(), String::from_utf8(out.stdout).unwrap())
        };

        assert_eq!(probe(None), (true, "free\n".into()), "no repository yet");

        let bench = home.path().join("ocync/bench");
        std::fs::create_dir_all(&bench).unwrap();
        assert_eq!(probe(None), (true, "free\n".into()), "lock not taken");

        let lock = std::fs::File::create(bench.join(".bench-run.lock")).unwrap();
        lock.lock().unwrap();
        assert_eq!(probe(None), (true, "held\n".into()), "lock held");
        assert_eq!(
            probe(Some("/nonexistent")),
            (false, String::new()),
            "flock missing"
        );
    }

    // The generated script targets the Linux bench instance: it needs
    // util-linux `setsid` and `flock`.
    #[cfg(target_os = "linux")]
    #[test]
    fn existing_run_guard_refuses_kills_or_proceeds() {
        use std::os::unix::process::ExitStatusExt;

        // (policy, previous run, guard exit, refusal printed, proceeded,
        //  previous run survives)
        let rows = [
            (ExistingRun::Refuse, Previous::Live, 3, true, false, true),
            (ExistingRun::Kill, Previous::Live, 0, false, true, false),
            (
                ExistingRun::Refuse,
                Previous::LeaderGone,
                3,
                true,
                false,
                true,
            ),
            (
                ExistingRun::Kill,
                Previous::LeaderGone,
                0,
                false,
                true,
                false,
            ),
            (ExistingRun::Refuse, Previous::Dead, 0, false, true, true),
            (ExistingRun::Kill, Previous::Dead, 0, false, true, true),
        ];
        for (n, (existing, previous, code, refusal, proceeded, survives)) in
            rows.into_iter().enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let lock = dir.path().join("run.lock");
            let pgid_file = dir.path().join("run.pgid");

            // The live run takes the lock before saying so. In the
            // leader-gone row a `sleep` in the group inherits fd 9 and the
            // leader exits. The dead row's bystander only exists so a stale
            // group file has someone to hit.
            let script = match previous {
                Previous::Live => r#"exec 9>"$LOCK_FILE"; flock -n 9; echo held; exec sleep 30"#,
                Previous::LeaderGone => r#"exec 9>"$LOCK_FILE"; flock -n 9; sleep 30 & echo held"#,
                Previous::Dead => "echo idle; exec sleep 30",
            };
            let mut leader = session_leader(script, &lock);
            let pgid = leader.id();
            let mut ready = String::new();
            BufReader::new(leader.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            std::fs::write(&pgid_file, pgid.to_string()).unwrap();
            let leader_exit = (previous == Previous::LeaderGone).then(|| leader.wait().unwrap());
            if let Some(exit) = leader_exit {
                assert!(exit.success(), "row {n}: setup leader: {exit}");
                assert!(group_alive(pgid), "row {n}: the lock holder is gone");
            }

            let out = run_guard(existing, &lock, &pgid_file, None);
            let stdout = String::from_utf8(out.stdout).unwrap();

            let survived = if survives {
                let alive = group_alive(pgid);
                Command::new("bash")
                    .args(["-c", r#"kill -9 -- "-$0" 2>/dev/null"#, &pgid.to_string()])
                    .status()
                    .unwrap();
                leader.wait().unwrap();
                alive
            } else {
                // The guard, not this test, must have ended it: by SIGKILL.
                let by_guard = leader_exit.is_some() || leader.wait().unwrap().signal() == Some(9);
                // A killed member may linger as a zombie until it is reaped.
                let gone = (0..100).any(|_| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    !group_alive(pgid)
                });
                by_guard && gone
            };
            assert_eq!(
                out.status.code(),
                Some(code),
                "row {n}: guard exit: {stdout}"
            );
            assert_eq!(
                stdout.contains(RUN_IN_PROGRESS_LINE),
                refusal,
                "row {n}: refusal line: {stdout}"
            );
            assert_eq!(
                stdout.contains("proceeded"),
                proceeded,
                "row {n}: proceeded: {stdout}"
            );
            // A guard that proceeds records its own group, and one that stops
            // leaves the previous run's group named.
            let guard_pid = stdout
                .lines()
                .find_map(|line| line.strip_prefix("guard pid "))
                .unwrap();
            let previous_pgid = pgid.to_string();
            let recorded = if proceeded { guard_pid } else { &previous_pgid };
            assert_eq!(
                std::fs::read_to_string(&pgid_file).unwrap().trim(),
                recorded,
                "row {n}: group file"
            );
            assert!(survived, "row {n}: previous run survival, want {survives}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_flock_that_fails_is_an_error_not_a_held_lock() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        // A `flock` that exits 127 ahead of any real one on the guard's PATH.
        let stub = bin.join("flock");
        std::fs::write(&stub, "#!/bin/sh\nexit 127\n").unwrap();
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());

        for existing in [ExistingRun::Refuse, ExistingRun::Kill] {
            let out = run_guard(
                existing,
                &dir.path().join("run.lock"),
                &dir.path().join("run.pgid"),
                Some(Path::new(&path)),
            );
            let stdout = String::from_utf8(out.stdout).unwrap();

            assert_eq!(out.status.code(), Some(1), "{existing:?}: {stdout}");
            assert!(
                stdout.contains("bench-remote: error: flock failed (exit 127)"),
                "{existing:?}: {stdout}"
            );
            assert!(
                !stdout.contains(RUN_IN_PROGRESS_LINE),
                "{existing:?}: {stdout}"
            );
            assert!(!stdout.contains("proceeded"), "{existing:?}: {stdout}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_remote_command_runs_the_script_as_a_group_leader() {
        use std::os::unix::process::CommandExt;

        // The script exits 9 unless `setsid -w` made it lead its own group,
        // and 5 otherwise, which the command must pass through. The command
        // itself starts as a group leader, as `ssh` does under a shell, so
        // `setsid` forks and only `-w` carries the script's status back.
        let mut child = Command::new("bash")
            .args(["-c", REMOTE_COMMAND])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"[ \"$(ps -o pgid= -p $$ | tr -d ' ')\" = \"$$\" ] || exit 9\nexit 5\n")
            .unwrap();

        assert_eq!(child.wait().unwrap().code(), Some(5));
    }

    #[test]
    fn bench_runner_records_a_failing_run_and_passes_its_arguments_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| dir.path().join(name);
        // `cargo` is a function so the test needs no executable on disk. It
        // fails with 7 and records one argument per line.
        std::fs::write(
            file("runner.sh"),
            format!(
                "cargo() {{ printf '%s\\n' \"$@\" > \"$SEEN_ARGS\"; return 7; }}\n{BENCH_RUNNER}"
            ),
        )
        .unwrap();
        let bench_args = ["--tools", "oc ync", "it's", "cold"].map(String::from);

        // The launch line is the one the remote script runs.
        let status = Command::new("bash")
            .args(["-c", &format!("{}\nwait $!", runner_launch(&bench_args))])
            .env("RUNNER_FILE", file("runner.sh"))
            .env("LOG_FILE", file("log"))
            .env("EXIT_FILE", file("exit"))
            .env("SEEN_ARGS", file("args"))
            .status()
            .unwrap();

        let read = |name: &str| std::fs::read_to_string(file(name)).unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(read("exit").trim(), "7");
        assert_eq!(read("args"), "xtask\nbench\n--tools\noc ync\nit's\ncold\n");
    }
}
