//! Remote benchmark orchestration via SSH.
//!
//! Prerequisites: `~/.ssh/id_ed25519` exists and its public key is on GitHub,
//! Terraform applied for the target provider (`bench/terraform/<provider>/`).
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
/// prevents interactive prompts from hanging the automation.
const SSH_OPTS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=30",
];

/// Arguments for the `bench-remote` subcommand.
#[derive(Args)]
pub(crate) struct BenchRemoteArgs {
    /// Cloud provider (aws, gcp, azure). Reads connection details from
    /// `bench/terraform/<provider>/bench.json`.
    #[arg(long)]
    pub(crate) provider: String,

    /// Fetch results from the last run instead of starting a new one.
    #[arg(long)]
    pub(crate) fetch: bool,

    /// Kill any running benchmark and start fresh.
    #[arg(long)]
    pub(crate) force: bool,

    /// Git ref to checkout on the instance (default: current branch).
    #[arg(long)]
    pub(crate) git_ref: Option<String>,

    /// Tools to benchmark (passed through to `cargo xtask bench`).
    #[arg(long, default_value = "ocync,dregsy,regsync")]
    pub(crate) tools: String,

    /// Scenario to run (passed through to `cargo xtask bench`).
    #[arg(long, default_value = "all")]
    pub(crate) scenario: String,

    /// Number of cold-sync iterations (passed through to `cargo xtask bench`).
    #[arg(long, default_value = "1")]
    pub(crate) iterations: String,

    /// Use first N images only (passed through to `cargo xtask bench`).
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Comma-separated list of source registries to skip (passed through
    /// to `cargo xtask bench`).
    #[arg(long)]
    pub(crate) skip_registries: Option<String>,

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
}

/// Returns the path to bench.json for a given provider.
fn config_path(provider: &str) -> PathBuf {
    PathBuf::from(format!("bench/terraform/{provider}/bench.json"))
}

/// Read and parse bench.json for the given provider.
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

    // Build the bench command args.
    let mut bench_args = format!("--tools {} --iterations {}", args.tools, args.iterations);
    if let Some(limit) = args.limit {
        bench_args.push_str(&format!(" --limit {limit}"));
    }
    if let Some(ref skip) = args.skip_registries {
        bench_args.push_str(&format!(" --skip-registries {skip}"));
    }
    bench_args.push_str(&format!(" {}", args.scenario));

    // Determine the target registry from the provider.
    let registry_env = match config.provider.as_str() {
        "aws" => {
            "ACCOUNT=$(aws sts get-caller-identity --query Account --output text)\nexport BENCH_TARGET_REGISTRY=${ACCOUNT}.dkr.ecr.us-east-1.amazonaws.com".to_string()
        }
        _ => {
            return Err(format!(
                "provider '{}' not yet supported. Add registry env setup for this provider.",
                config.provider
            )
            .into());
        }
    };

    let force_clause = if args.force {
        r#"
# --force: kill any running bench and clean up.
if [ -f "$PID_FILE" ]; then
  echo "bench-remote: --force killing PID $(cat "$PID_FILE")"
  kill -9 $(cat "$PID_FILE") 2>/dev/null || true
  rm -f "$PID_FILE"
fi
pkill -9 -f "bench-proxy serve" 2>/dev/null || true
sleep 1
"#
    } else {
        ""
    };

    let script = format!(
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
  echo "bench-remote: error: ~/ocync not found after cloud-init. Check user-data logs:"
  echo "  ssh $USER@$(hostname -I | awk '{{print $1}}') 'sudo cat /var/log/cloud-init-output.log | tail -50'"
  exit 1
fi

cd ~/ocync
PID_FILE=bench/.bench-run.pid
LOG_FILE=bench/.bench-run.log
{force_clause}
# Check for existing run.
if [ -f "$PID_FILE" ]; then
  OLD_PID=$(cat "$PID_FILE")
  if kill -0 "$OLD_PID" 2>/dev/null; then
    if [ -f "$LOG_FILE" ]; then
      echo "bench-remote: attaching to run in progress (PID $OLD_PID)"
      tail -n+1 -f "$LOG_FILE" --pid="$OLD_PID"
      exit 0
    else
      echo "bench-remote: PID $OLD_PID alive but log missing, cleaning up"
      kill -9 "$OLD_PID" 2>/dev/null || true
      pkill -9 -f "bench-proxy serve" 2>/dev/null || true
      rm -f "$PID_FILE"
      sleep 1
    fi
  else
    echo "bench-remote: cleaning up stale PID file (PID $OLD_PID)"
    rm -f "$PID_FILE"
  fi
fi

# Pull code and build.
echo '[1/4] Pulling {git_ref}...'
git fetch origin '{git_ref}' && git checkout '{git_ref}' && git reset --hard 'origin/{git_ref}'

echo '[2/4] Building ocync + bench-proxy...'
source ~/.bench-env 2>/dev/null || true
source ~/.cargo/env
cargo build --release --package ocync --package bench-proxy
cp target/release/ocync ~/.cargo/bin/ocync
cp target/release/bench-proxy ~/.cargo/bin/bench-proxy

echo '[3/4] Starting benchmarks...'
# Use real disk for temp files, not tmpfs (/tmp is RAM-backed on AL2023
# and fills up with multi-GB blob staging data).
export TMPDIR=$HOME/ocync/bench/.tmp
rm -rf "$TMPDIR"
mkdir -p "$TMPDIR"
{registry_env}

# Launch detached with output to log file.
EXIT_FILE=bench/.bench-run.exit
> "$LOG_FILE"
rm -f "$EXIT_FILE"
(
  cargo xtask bench {bench_args} 2>&1
  BENCH_EXIT=$?
  echo "$BENCH_EXIT" > "$EXIT_FILE"
  echo "[bench-remote] exit code: $BENCH_EXIT"
  rm -f "$PID_FILE"
  exit $BENCH_EXIT
) >> "$LOG_FILE" 2>&1 &
BENCH_PID=$!
echo "$BENCH_PID" > "$PID_FILE"
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
    );

    eprintln!(
        "bench-remote: connecting to {}@{}...",
        config.user, config.host
    );

    let exit_code = ssh_stream(&config, &script)?;

    if exit_code != 0 {
        eprintln!("bench-remote: remote script exited with code {exit_code}");
        eprintln!("bench-remote: fetching partial results...");
    } else {
        eprintln!("bench-remote: fetching results...");
    }

    fetch_results(&config, &args.output)?;
    Ok(())
}

/// Pipe a script to the remote instance via SSH and stream stdout.
///
/// Returns the exit code of the remote script.
fn ssh_stream(config: &BenchConfig, script: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let mut child = Command::new("ssh")
        .args(SSH_OPTS)
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
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            eprintln!("{line}");
        }
    }

    let status = child.wait()?;
    Ok(status.code().unwrap_or(1))
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
        .args(SSH_OPTS)
        .arg(format!("{}@{}:{}", config.user, config.host, remote_dir))
        .arg(local_output)
        .status()?;

    if !status.success() {
        return Err("scp failed".into());
    }

    // Also fetch the per-registry JSON archive.
    let json_file = ssh_run(
        config,
        "ls -t ~/ocync/bench/results/*.json 2>/dev/null | grep -v baseline | head -1",
    )?;
    let json_file = json_file.trim();

    if !json_file.is_empty() {
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
            .args(SSH_OPTS)
            .arg(format!("{}@{}:{}", config.user, config.host, json_file))
            .arg(local_json.to_str().unwrap_or("."))
            .status()?;
        if !status.success() {
            eprintln!("bench-remote: warning: failed to copy {json_file}");
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
        .args(SSH_OPTS)
        .arg(format!("{}@{}", config.user, config.host))
        .arg(command)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh command failed: {stderr}").into());
    }

    Ok(String::from_utf8(output.stdout)?)
}

/// Get the current local git branch name.
fn current_branch() -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err("failed to get current git branch".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bench_config() {
        let json = r#"{"provider":"aws","host":"54.123.45.67","user":"ec2-user"}"#;
        let config: BenchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.provider, "aws");
        assert_eq!(config.host, "54.123.45.67");
        assert_eq!(config.user, "ec2-user");
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
}
