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

/// Arguments for the `bench-remote` subcommand.
#[derive(Args)]
pub(crate) struct BenchRemoteArgs {
    /// Cloud provider (aws, gcp, azure). Reads connection details from
    /// `bench/terraform/<provider>/bench.json`.
    #[arg(long)]
    pub(crate) provider: String,

    /// Fetch results from the last run instead of starting a new one. While a
    /// run is live these are its partial results.
    #[arg(long)]
    pub(crate) fetch: bool,

    /// Kill a benchmark still running on the instance and start fresh.
    /// Without it, a live run is left alone and this command fails.
    #[arg(long, conflicts_with = "fetch")]
    pub(crate) force: bool,

    /// Git ref to checkout on the instance (default: current branch).
    #[arg(long, conflicts_with = "fetch")]
    pub(crate) git_ref: Option<String>,

    /// Tools to benchmark (passed through to `cargo xtask bench`).
    #[arg(long, default_value = "ocync")]
    pub(crate) tools: String,

    /// Scenario to run (passed through to `cargo xtask bench`).
    #[arg(long, default_value = "all")]
    pub(crate) scenario: String,

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
    #[arg(long, default_value = "bench/results")]
    pub(crate) output: String,
}

/// Connection details written by Terraform.
#[derive(Debug, Deserialize)]
struct BenchConfig {
    provider: String,
    host: String,
    user: String,
    /// Absolute path to the Terraform-generated SSH private key for this
    /// instance. Written by `local_sensitive_file.ssh_key`.
    ssh_key_path: String,
}

/// Returns the path to bench.json for a given provider.
fn config_path(provider: &str) -> PathBuf {
    PathBuf::from(format!("bench/terraform/{provider}/bench.json"))
}

/// Read and parse bench.json for the given provider.
#[expect(
    clippy::disallowed_methods,
    reason = "reads the bench's own configuration"
)]
fn read_config(provider: &str) -> Result<BenchConfig, Box<dyn std::error::Error>> {
    // Validate the provider directory exists before reading config.
    let provider_dir = PathBuf::from(format!("bench/terraform/{provider}"));
    if !provider_dir.is_dir() {
        let available = std::fs::read_dir("bench/terraform")
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return Err(format!("unknown provider '{provider}'. Available: {available}").into());
    }

    let path = config_path(provider);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "failed to read {}: {e}. Did you run `terraform apply` in bench/terraform/{provider}/?",
            path.display()
        )
    })?;
    let config: BenchConfig = serde_json::from_str(&content)?;
    Ok(config)
}

/// Run the remote benchmark workflow.
pub(crate) fn run(args: BenchRemoteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = read_config(&args.provider)?;

    if args.fetch {
        return fetch_results(&config, &args.output);
    }

    let git_ref = match &args.git_ref {
        Some(r) => r.clone(),
        None => current_branch()?,
    };

    // Push current branch so the instance can pull it.
    eprintln!("bench-remote: pushing {git_ref}...");
    let push = Command::new("git")
        .args(["push", "origin", &git_ref])
        .status()?;
    if !push.success() {
        return Err("git push failed".into());
    }

    // Resolve to exact SHA after pushing. The remote script checks out this
    // SHA, not the branch name. This makes the build immune to force-pushes
    // that land between our push and the instance's fetch -- the SHA is
    // immutable even if the branch ref moves.
    let git_sha = resolve_commit(Path::new("."), &git_ref)?;
    eprintln!("bench-remote: pinned to {git_sha} ({git_ref})");

    // The benchmark's arguments, unquoted: `remote_script` quotes each one.
    let mut bench_args = vec!["--tools".to_string(), args.tools.clone()];
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
    bench_args.push(args.scenario.clone());

    // Determine the target registry from the provider.
    let registry_env = match config.provider.as_str() {
        "aws" => {
            "TOKEN=$(curl -s -X PUT 'http://169.254.169.254/latest/api/token' -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600')\nREGION=$(curl -s -H \"X-aws-ec2-metadata-token: $TOKEN\" http://169.254.169.254/latest/meta-data/placement/region)\nACCOUNT=$(aws sts get-caller-identity --query Account --output text)\nexport BENCH_TARGET_REGISTRY=${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com".to_string()
        }
        _ => {
            return Err(format!(
                "provider '{}' not yet supported. Add registry env setup for this provider.",
                config.provider
            )
            .into());
        }
    };

    let script = remote_script(
        &git_ref,
        &git_sha,
        &bench_args,
        &registry_env,
        ExistingRun::from_force(args.force),
    );

    eprintln!(
        "bench-remote: connecting to {}@{}...",
        config.user, config.host
    );

    let exit_code = match ssh_stream(&config, &script)? {
        RemoteOutcome::Refused => {
            return Err(
                "a benchmark is still running on the instance; pass --force to kill it, \
                 or --fetch to copy the live run's partial results"
                    .into(),
            );
        }
        RemoteOutcome::Exited(code) => code,
    };

    if exit_code != 0 {
        eprintln!("bench-remote: remote script exited with code {exit_code}");
        eprintln!("bench-remote: fetching partial results...");
    } else {
        eprintln!("bench-remote: fetching results...");
    }

    fetch_results(&config, &args.output)?;
    Ok(())
}

/// How the remote script ended.
enum RemoteOutcome {
    /// The guard found a live run and left it alone.
    Refused,
    /// The script ran to its end with this exit code.
    Exited(i32),
}

impl RemoteOutcome {
    /// A script that printed [`RUN_IN_PROGRESS_LINE`] was refused, whatever
    /// its exit code: `set -e` makes any failing command exit with any code,
    /// the refusal's own `exit 3` included.
    fn new(refusal_line_seen: bool, exit_code: Option<i32>) -> Self {
        if refusal_line_seen {
            Self::Refused
        } else {
            Self::Exited(exit_code.unwrap_or(1))
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

    // Write script to stdin, then close to signal EOF.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }

    // Stream stdout line by line for real-time progress.
    let mut refused = false;
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            refused |= line == RUN_IN_PROGRESS_LINE;
            eprintln!("{line}");
        }
    }

    let status = child.wait()?;
    Ok(RemoteOutcome::new(refused, status.code()))
}

/// Fetch results from the instance via SCP.
fn fetch_results(
    config: &BenchConfig,
    local_output: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Find the most recent results directory.
    let ls_output = ssh_run(
        config,
        "ls -td ~/ocync/bench/results/2* 2>/dev/null | head -1",
    )?;
    let remote_dir = ls_output.trim();

    if remote_dir.is_empty() {
        return Err("no results directory found on instance".into());
    }

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

    // SCP the results directory (no trailing slash on source).
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

    // Also fetch all per-registry JSON archives (excluding baseline.json).
    let json_files = ssh_run(
        config,
        "ls ~/ocync/bench/results/*.json 2>/dev/null | grep -v baseline",
    )?;

    for json_file in json_files.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let json_basename = Path::new(json_file)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let local_json = Path::new(local_output).join(json_basename.as_ref());
        eprintln!(
            "bench-remote: copying {json_file} to {}",
            local_json.display()
        );
        let status = Command::new("scp")
            .args(ssh_args(config))
            .arg(format!("{}@{}:{}", config.user, config.host, json_file))
            .arg(local_json.to_str().unwrap_or("."))
            .status()?;
        if !status.success() {
            eprintln!("bench-remote: warning: failed to copy {json_file}");
        }
    }

    // Pull the auto-updated performance page back to the local working tree.
    // The bench harness updates docs/src/content/performance.md between the
    // BENCH:START/END and BENCH-WARM:START/END markers on the remote instance.
    // Without this fetch, the update stays on the instance and is never committed.
    let perf_page = "docs/src/content/performance.md";
    let remote_perf = format!("~/ocync/{perf_page}");
    let local_perf = Path::new(perf_page);
    if local_perf.exists() {
        eprintln!("bench-remote: updating {perf_page}");
        let status = Command::new("scp")
            .args(ssh_args(config))
            .arg(format!("{}@{}:{}", config.user, config.host, remote_perf))
            .arg(local_perf)
            .status()?;
        if !status.success() {
            eprintln!("bench-remote: warning: failed to fetch updated {perf_page}");
        }
    }

    // Print the summary.
    let summary = ssh_run(
        config,
        &format!("cat {remote_dir}/summary.md 2>/dev/null || true"),
    )?;
    if !summary.trim().is_empty() {
        eprintln!("\n{summary}");
    }

    Ok(())
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
/// results and the run's own files. The run-file pattern is quoted so the
/// shell cannot expand it against the files present. Runs with fd 9 closed
/// like every git command in the script, see [`remote_script`].
const GIT_CLEAN: &str =
    "git clean -fdx -e target/ -e bench/results/ -e 'bench/.bench-run.*' -e bench/.tmp/ 9>&-";

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
LOCK_FILE=bench/.bench-run.lock
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
echo "bench-remote: started (PID $BENCH_PID), streaming log..."

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

/// Runs `git rev-parse` with `args` in `repo` and returns its trimmed output,
/// or an error carrying git's own message.
fn git_rev_parse(repo: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(repo)
        .arg("rev-parse")
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// A full commit object name: 40 lowercase hex digits, safe to place in a
/// shell command unescaped.
#[derive(Debug)]
struct CommitSha(String);

impl CommitSha {
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
fn resolve_commit(repo: &Path, git_ref: &str) -> Result<CommitSha, Box<dyn std::error::Error>> {
    CommitSha::parse(&git_rev_parse(
        repo,
        &["--verify", &format!("{git_ref}^{{commit}}")],
    )?)
}

/// Get the current local git branch name.
fn current_branch() -> Result<String, Box<dyn std::error::Error>> {
    git_rev_parse(Path::new("."), &["--abbrev-ref", "HEAD"])
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;

    use super::*;

    #[test]
    fn parse_bench_config() {
        let json = r#"{"provider":"aws","host":"54.123.45.67","user":"ec2-user","ssh_key_path":"/tmp/.ssh-key"}"#;
        let config: BenchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.provider, "aws");
        assert_eq!(config.host, "54.123.45.67");
        assert_eq!(config.user, "ec2-user");
        assert_eq!(config.ssh_key_path, "/tmp/.ssh-key");
    }

    #[test]
    fn read_config_missing_provider_dir() {
        // A provider with no terraform directory should produce a
        // helpful error listing available providers.
        let err = read_config("nonexistent").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown provider 'nonexistent'"),
            "expected provider error, got: {msg}"
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
        assert_eq!(
            config_path("aws"),
            PathBuf::from("bench/terraform/aws/bench.json")
        );
        assert_eq!(
            config_path("gcp"),
            PathBuf::from("bench/terraform/gcp/bench.json")
        );
    }

    fn git(repo: &Path, args: &[&str]) -> String {
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

    #[test]
    fn resolve_commit_follows_the_ref_not_head() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(repo.path(), &["branch", "other"]);
        git(repo.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);
        let other = git(repo.path(), &["rev-parse", "other"]);

        let resolved = resolve_commit(repo.path(), "other").unwrap();

        assert_eq!(resolved.to_string(), other);
        assert_ne!(
            resolved.to_string(),
            git(repo.path(), &["rev-parse", "HEAD"])
        );
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
    fn fetch_conflicts_with_force_and_git_ref() {
        use clap::Parser;
        for extra in [&["--force"][..], &["--git-ref", "main"][..]] {
            let argv = ["xtask", "--provider", "aws", "--fetch"]
                .into_iter()
                .chain(extra.iter().copied());
            let err = Cli::try_parse_from(argv).err().unwrap();
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
        assert!(Cli::try_parse_from(["xtask", "--provider", "aws", "--fetch"]).is_ok());
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
            "export BENCH_TARGET_REGISTRY=registry.example.com",
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
        assert_eq!(git_lines.len(), 3, "fetch, checkout, clean: {git_lines:?}");

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
            "closed\nclosed\nclosed\n"
        );
    }

    #[test]
    fn a_refusal_is_the_sentinel_line_not_exit_3() {
        assert!(matches!(
            RemoteOutcome::new(true, Some(0)),
            RemoteOutcome::Refused
        ));
        assert!(matches!(
            RemoteOutcome::new(false, Some(3)),
            RemoteOutcome::Exited(3)
        ));
        assert!(matches!(
            RemoteOutcome::new(false, None),
            RemoteOutcome::Exited(1)
        ));
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
        git(repo.path(), &["init", "-q", "-b", "main"]);
        let bench = repo.path().join("bench");
        std::fs::create_dir(&bench).unwrap();
        let run_files = ["lock", "log", "pgid", "sh", "exit"].map(|ext| {
            let file = bench.join(format!(".bench-run.{ext}"));
            std::fs::write(&file, "").unwrap();
            file
        });
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
        assert!(!stray.exists(), "the stray file survived");
    }

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
    fn session_leader(script: &str, lock: &Path) -> std::process::Child {
        Command::new("setsid")
            .args(["bash", "-c", script])
            .env("LOCK_FILE", lock)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Whether any process is left in process group `pgid`.
    fn group_alive(pgid: u32) -> bool {
        Command::new("bash")
            .args(["-c", r#"kill -0 -- "-$0" 2>/dev/null"#, &pgid.to_string()])
            .status()
            .unwrap()
            .success()
    }

    /// Runs the guard for `existing` followed by `echo proceeded`, with
    /// `path` as the guard's `PATH` when given.
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

    #[test]
    fn existing_run_guard_refuses_kills_or_proceeds() {
        use std::os::unix::process::ExitStatusExt;

        const REFUSAL: &str = "bench-remote: a benchmark is already running on this instance";
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
                stdout.contains(REFUSAL),
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
