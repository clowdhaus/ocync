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

    // Build the bench command args. Shell-escape all user-supplied
    // values to prevent injection via crafted tool names, registry
    // names, or scenario strings.
    let mut bench_args = format!("--tools '{}'", shell_escape(&args.tools));
    if let Some(limit) = args.limit {
        bench_args.push_str(&format!(" --limit {limit}"));
    }
    if let Some(ref skip) = args.skip_registries {
        bench_args.push_str(&format!(" --skip-registries '{}'", shell_escape(skip)));
    }
    if args.skip_prewarm {
        bench_args.push_str(" --skip-prewarm");
    }
    if args.no_proxy {
        bench_args.push_str(" --no-proxy");
    }
    bench_args.push_str(&format!(" '{}'", shell_escape(&args.scenario)));

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
    /// [`RUN_IN_PROGRESS_EXIT`] included.
    fn new(refusal_line_seen: bool, exit_code: Option<i32>) -> Self {
        if refusal_line_seen {
            Self::Refused
        } else {
            Self::Exited(exit_code.unwrap_or(1))
        }
    }
}

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
        .arg("bash -s")
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

/// Line the remote script prints, before exiting [`RUN_IN_PROGRESS_EXIT`],
/// when it refuses to replace a live run.
const RUN_IN_PROGRESS_LINE: &str = "bench-remote: a benchmark is already running on this instance";

/// Exit status of the remote script when it refuses to replace a live run.
const RUN_IN_PROGRESS_EXIT: i32 = 3;

/// Seconds `--force` waits for a killed run to release the run lock.
const KILL_WAIT_SECS: u32 = 30;

/// Shell that takes the run lock on fd 9, handling a live holder per
/// `existing`, and then forgets the previous run's process group.
///
/// Needs `$LOCK_FILE` and `$PGID_FILE`. The lock is held until every process
/// holding fd 9 exits, so it spans checkout, build and the benchmark and
/// outlives the SSH session, and the kernel drops it when they die. Liveness
/// is the lock alone. `$PGID_FILE`, written by the benchmark's own process
/// group leader, only says whom to kill, and is read only while the lock is
/// held by someone else.
///
/// A new run never attaches to an old one, since the old one may be running a
/// binary built from different source.
fn existing_run_guard(existing: ExistingRun) -> String {
    let on_held = match existing {
        ExistingRun::Refuse => format!(
            r#"echo "{RUN_IN_PROGRESS_LINE}"
  exit {RUN_IN_PROGRESS_EXIT}"#
        ),
        ExistingRun::Kill => format!(
            r#"OLD_PGID=$(cat "$PGID_FILE" 2>/dev/null || true)
  if [[ "$OLD_PGID" =~ ^[0-9]+$ ]] && [ "$OLD_PGID" -gt 1 ]; then
    echo "bench-remote: --force killing process group $OLD_PGID"
    kill -9 -- "-$OLD_PGID" 2>/dev/null || true
  fi
  if ! flock -w {KILL_WAIT_SECS} 9; then
    echo "bench-remote: error: the running benchmark did not release the run lock in {KILL_WAIT_SECS}s"
    exit 1
  fi"#
        ),
    };
    format!(
        r#"exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  {on_held}
fi
rm -f "$PGID_FILE""#
    )
}

/// Shell that records the script's own process group in `$PGID_FILE`, so
/// `--force` can end a run that is still checking out or building, before the
/// benchmark's group exists. sshd starts the remote command as a session
/// leader; a script that does not lead its own group records nothing, since
/// killing its group would kill unrelated processes.
const RECORD_OWN_GROUP: &str = r#"if [ "$(ps -o pgid= -p $$ | tr -d ' ')" = "$$" ]; then
  echo "$$" > "$PGID_FILE"
fi"#;

/// Shell that stops the script when a tool the run lock needs is missing.
/// Without it a missing `flock` fails the lock test and reads as a live run.
const REQUIRE_UTIL_LINUX: &str = r#"for tool in flock setsid; do
  if ! command -v "$tool" >/dev/null; then
    echo "bench-remote: error: $tool (util-linux) is not installed on the instance"
    exit 1
  fi
done"#;

/// Shell that runs the benchmark and records how it ended.
///
/// Run as its own session leader (`setsid`), so `$$` is the process group of
/// the harness and every tool it spawns, and `--force` can kill them together.
/// Needs `$PGID_FILE` and `$EXIT_FILE`. The exit status is captured with `if`
/// because `set -e` would otherwise end the script before recording it.
fn bench_runner(bench_args: &str) -> String {
    format!(
        r#"set -euo pipefail
echo "$$" > "$PGID_FILE"
if cargo xtask bench {bench_args} 2>&1; then BENCH_EXIT=0; else BENCH_EXIT=$?; fi
echo "$BENCH_EXIT" > "$EXIT_FILE"
echo "[bench-remote] exit code: $BENCH_EXIT"
rm -f "$PGID_FILE"
exit "$BENCH_EXIT""#
    )
}

/// The script piped to `bash -s` on the instance.
///
/// Every value is already shell-escaped or trusted: `bench_args` and
/// `registry_env` are shell text, `git_sha` is the hex object name from
/// `rev-parse --verify`.
fn remote_script(
    git_ref: &str,
    git_sha: &str,
    bench_args: &str,
    registry_env: &str,
    existing: ExistingRun,
) -> String {
    let existing_run = existing_run_guard(existing);
    let escaped_git_ref = shell_escape(git_ref);
    let runner = bench_runner(bench_args);
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
# building.
{REQUIRE_UTIL_LINUX}
{existing_run}
{RECORD_OWN_GROUP}
# Descendants that left the killed run's process group. cargo xtask execs the
# harness, so its command line is the built binary's.
pkill -9 -f 'xtask bench|dregsy|regsync|skopeo|bench-proxy serve|target/release/ocync' 2>/dev/null || true
sleep 1

# Pull code and build. Use exact SHA (not branch name) so force-pushes
# between our local push and the instance's fetch cannot change what gets
# built. The SHA is resolved locally before pushing and is immutable.
echo '[1/4] Checking out {git_sha} ({escaped_git_ref})...'
git fetch origin
git checkout '{git_sha}' --detach
# Remove untracked files left by previous builds or deleted from the repo.
git clean -fdx -e target/ -e bench/results/ -e bench/.bench-run.* -e bench/.tmp/

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

# Launch detached with output to log file, as its own process group so
# --force can kill the harness and every tool it spawned. fd 9 (the run lock)
# is inherited, so the lock outlives this SSH session.
> "$LOG_FILE"
rm -f "$EXIT_FILE"
cat > "$RUNNER_FILE" <<'RUNNER_EOF'
{runner}
RUNNER_EOF
export PGID_FILE EXIT_FILE
setsid bash "$RUNNER_FILE" >> "$LOG_FILE" 2>&1 &
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

/// Resolves `git_ref` to the commit it names in the repository at `repo`.
fn resolve_commit(repo: &Path, git_ref: &str) -> Result<String, Box<dyn std::error::Error>> {
    git_rev_parse(repo, &["--verify", &format!("{git_ref}^{{commit}}")])
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

        assert_eq!(resolved, other);
        assert_ne!(resolved, git(repo.path(), &["rev-parse", "HEAD"]));
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

    fn script_for(existing: ExistingRun) -> String {
        remote_script(
            "main",
            "0123456789abcdef0123456789abcdef01234567",
            "--tools 'ocync' 'all'",
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
            let guard = at("flock -n 9");
            assert!(guard < at("pkill"), "{existing:?}: guard after pkill");
            assert!(
                guard < at("git checkout"),
                "{existing:?}: guard after checkout"
            );
            assert!(guard < at("rm -rf target/"), "{existing:?}: guard after rm");
            assert!(at("setsid bash") > at("rm -rf target/"));
        }
    }

    #[test]
    fn remote_script_records_its_group_after_the_guard_and_before_checkout() {
        let script = script_for(ExistingRun::Kill);
        let at = |needle: &str| script.find(needle).unwrap();
        assert!(at("flock -n 9") < at(RECORD_OWN_GROUP));
        assert!(at(RECORD_OWN_GROUP) < at("git checkout"));
        assert!(at(REQUIRE_UTIL_LINUX) < at("flock -n 9"));
    }

    #[test]
    fn record_own_group_records_only_a_group_leader() {
        let dir = tempfile::tempdir().unwrap();
        let pgid_file = dir.path().join("pgid");
        let run = |leader: bool| {
            let mut cmd = if leader {
                let mut c = Command::new("setsid");
                c.args(["-w", "bash", "-c"]);
                c
            } else {
                let mut c = Command::new("bash");
                c.arg("-c");
                c
            };
            // The pid is echoed so the file can be compared with it.
            cmd.arg(format!("echo $$; {RECORD_OWN_GROUP}"))
                .env("PGID_FILE", &pgid_file)
                .output()
                .unwrap()
        };

        let leader = run(true);
        let pid = String::from_utf8(leader.stdout).unwrap();
        assert_eq!(
            std::fs::read_to_string(&pgid_file).unwrap().trim(),
            pid.trim()
        );

        std::fs::remove_file(&pgid_file).unwrap();
        run(false);
        assert!(!pgid_file.exists(), "a non-leader recorded its group");
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
    fn remote_script_kills_only_under_force() {
        let kill = r#"kill -9 -- "-$OLD_PGID""#;
        assert!(script_for(ExistingRun::Kill).contains(kill));
        let refuse = script_for(ExistingRun::Refuse);
        assert!(!refuse.contains(kill));
        assert!(refuse.contains("exit 3"));
    }

    #[test]
    fn remote_script_pins_the_resolved_sha() {
        let script = script_for(ExistingRun::Refuse);
        assert!(
            script.contains("git checkout '0123456789abcdef0123456789abcdef01234567' --detach")
        );
    }

    #[derive(Clone, Copy)]
    enum Previous {
        /// A run holds the lock, in its own process group.
        Live,
        /// No run holds the lock, and a stale group file names a bystander.
        Dead,
    }

    /// Starts `sleep` as its own session leader, so its pid is its group id.
    fn session_leader(script: &str, lock: &Path) -> std::process::Child {
        Command::new("setsid")
            .args(["bash", "-c", script])
            .env("LOCK_FILE", lock)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn existing_run_guard_refuses_kills_or_proceeds() {
        use std::os::unix::process::ExitStatusExt;

        const REFUSAL: &str = "bench-remote: a benchmark is already running on this instance";
        // (policy, previous run, guard exit, refusal printed, proceeded,
        //  group file left behind, previous run survives)
        let rows = [
            (
                ExistingRun::Refuse,
                Previous::Live,
                3,
                true,
                false,
                true,
                true,
            ),
            (
                ExistingRun::Kill,
                Previous::Live,
                0,
                false,
                true,
                false,
                false,
            ),
            (
                ExistingRun::Refuse,
                Previous::Dead,
                0,
                false,
                true,
                false,
                true,
            ),
            (
                ExistingRun::Kill,
                Previous::Dead,
                0,
                false,
                true,
                false,
                true,
            ),
        ];
        for (n, (existing, previous, code, refusal, proceeded, file_left, survives)) in
            rows.into_iter().enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let lock = dir.path().join("run.lock");
            let pgid_file = dir.path().join("run.pgid");

            // The live run takes the lock before saying so; the dead row's
            // bystander only exists so a stale group file has someone to hit.
            let script = match previous {
                Previous::Live => r#"exec 9>"$LOCK_FILE"; flock -n 9; echo held; exec sleep 30"#,
                Previous::Dead => "echo idle; exec sleep 30",
            };
            let mut other = session_leader(script, &lock);
            let mut ready = String::new();
            BufReader::new(other.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            std::fs::write(&pgid_file, other.id().to_string()).unwrap();

            let out = Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "set -euo pipefail\n{}\necho proceeded",
                    existing_run_guard(existing)
                ))
                .env("LOCK_FILE", &lock)
                .env("PGID_FILE", &pgid_file)
                .output()
                .unwrap();
            let stdout = String::from_utf8(out.stdout).unwrap();

            let survived = if survives {
                let alive = other.try_wait().unwrap().is_none();
                other.kill().ok();
                other.wait().unwrap();
                alive
            } else {
                // The guard, not this test, must have ended it: by SIGKILL.
                other.wait().unwrap().signal() == Some(9)
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
            assert_eq!(pgid_file.exists(), file_left, "row {n}: group file");
            assert!(survived, "row {n}: previous run survival, want {survives}");
        }
    }

    #[test]
    fn bench_runner_records_a_failing_run() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| dir.path().join(name);
        // `cargo` is a function so the test needs no executable on disk. It
        // fails with 7 and records what the runner handed it.
        let script = format!(
            "cargo() {{ echo \"$*\" > \"$SEEN_ARGS\"; cat \"$PGID_FILE\" > \"$SEEN_PGID\"; \
             echo \"$$\" > \"$SEEN_PID\"; return 7; }}\n{}",
            bench_runner("--tools 'ocync' 'cold'")
        );

        let status = Command::new("bash")
            .args(["-c", &script])
            .env("PGID_FILE", file("pgid"))
            .env("EXIT_FILE", file("exit"))
            .env("SEEN_ARGS", file("args"))
            .env("SEEN_PGID", file("seen-pgid"))
            .env("SEEN_PID", file("seen-pid"))
            .status()
            .unwrap();

        let read = |name: &str| std::fs::read_to_string(file(name)).unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(read("exit").trim(), "7");
        assert_eq!(read("args").trim(), "xtask bench --tools ocync cold");
        assert_eq!(
            read("seen-pgid"),
            read("seen-pid"),
            "group file names the runner"
        );
        assert!(!file("pgid").exists(), "group file removed at the end");
    }
}
