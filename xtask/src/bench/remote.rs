//! Remote benchmark orchestration via SSM Session Manager.
//!
//! Prerequisites: `session-manager-plugin` installed locally, AWS credentials,
//! a running bench instance.
//!
//! `cargo xtask bench-remote` pushes the current branch, then opens an
//! interactive SSM session that builds and runs benchmarks with live output.
//! `cargo xtask bench-remote --fetch` pulls results back from the instance.

use std::path::Path;

use clap::Args;

/// Arguments for the `bench-remote` subcommand.
#[derive(Args)]
pub(crate) struct BenchRemoteArgs {
    /// EC2 instance ID of the bench instance.
    #[arg(long, env = "BENCH_INSTANCE_ID")]
    pub(crate) instance_id: String,

    /// AWS region.
    #[arg(long, default_value = "us-east-1")]
    pub(crate) region: String,

    /// Fetch results from the last run instead of starting a new one.
    #[arg(long)]
    pub(crate) fetch: bool,

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

    /// Local directory to save fetched results.
    #[arg(long, default_value = "bench-results")]
    pub(crate) output: String,
}

/// Run the remote benchmark workflow.
pub(crate) async fn run(args: BenchRemoteArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Verify session-manager-plugin is installed.
    if std::process::Command::new("session-manager-plugin")
        .arg("--version")
        .output()
        .is_err()
    {
        return Err(
            "session-manager-plugin not found. Install it: \
             https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html"
                .into(),
        );
    }

    if args.fetch {
        return fetch_results(&args).await;
    }

    let instance_id = &args.instance_id;
    let region = &args.region;

    let git_ref = match &args.git_ref {
        Some(r) => r.clone(),
        None => current_branch()?,
    };

    // Step 1: Push current branch so the instance can pull it.
    eprintln!("bench-remote: pushing {git_ref}...");
    let push = std::process::Command::new("git")
        .args(["push", "origin", &git_ref])
        .status()?;
    if !push.success() {
        return Err("git push failed".into());
    }

    // Step 2: Build the bench command.
    let mut bench_args = format!("--tools {} --iterations {}", args.tools, args.iterations);
    if let Some(limit) = args.limit {
        bench_args.push_str(&format!(" --limit {limit}"));
    }
    bench_args.push_str(&format!(" {}", args.scenario));

    // Step 3: Upload the bench script via send-command (fast, no quoting
    // issues), then run it interactively via start-session for live output.
    let script_content = format!(
        r#"#!/bin/bash
set -e

echo '=== pulling {git_ref} ==='
cd ~/ocync
TOKEN=$(aws ssm get-parameter --name /ocync/bench/github-token --with-decryption --query Parameter.Value --output text --region {region})
git remote set-url origin "https://${{TOKEN}}@github.com/clowdhaus/ocync.git"
git fetch origin {git_ref} && git checkout {git_ref} && git reset --hard origin/{git_ref}
git remote set-url origin https://github.com/clowdhaus/ocync.git

echo '=== building ocync + bench-proxy ==='
cargo build --release --package ocync --package bench-proxy

echo '=== starting benchmarks ==='
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
export BENCH_TARGET_REGISTRY=${{ACCOUNT}}.dkr.ecr.{region}.amazonaws.com
cargo xtask bench {bench_args}

echo '=== benchmarks complete ==='
echo 'fetch results: cargo xtask bench-remote --fetch --instance-id {instance_id}'
"#
    );

    // Upload script to instance, then execute via send-command with a
    // high timeout (7200s = 2 hours). Poll /tmp/bench-progress.txt every
    // 30 seconds for live progress updates.
    use crate::bench::config_gen::base64_encode;
    let encoded = base64_encode(&script_content);
    let run_cmd = format!(
        "echo {encoded} | base64 -d > /tmp/bench-run.sh && chmod +x /tmp/bench-run.sh && runuser -l ec2-user -c 'bash /tmp/bench-run.sh'"
    );

    eprintln!("bench-remote: starting benchmarks on {instance_id}...");

    let params = serde_json::json!({ "commands": [run_cmd] });
    let output = tokio::process::Command::new("aws")
        .args([
            "ssm",
            "send-command",
            "--instance-ids",
            instance_id,
            "--region",
            region,
            "--document-name",
            "AWS-RunShellScript",
            "--timeout-seconds",
            "7200",
            "--parameters",
            &params.to_string(),
            "--query",
            "Command.CommandId",
            "--output",
            "text",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssm send-command failed: {stderr}").into());
    }

    let command_id = String::from_utf8(output.stdout)?.trim().to_string();
    eprintln!("bench-remote: command={command_id}");
    let mut last_progress = String::new();

    // Poll until completion, printing progress updates.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        // Check progress file.
        if let Ok(prog) = ssm_exec(
            instance_id,
            region,
            "cat /tmp/bench-progress.txt 2>/dev/null || true",
        )
        .await
        {
            let prog = prog.trim().to_string();
            if prog != last_progress && !prog.is_empty() {
                eprintln!("bench-remote: {prog}");
                last_progress = prog;
            }
        }

        // Check command status.
        let poll = tokio::process::Command::new("aws")
            .args([
                "ssm",
                "get-command-invocation",
                "--command-id",
                &command_id,
                "--instance-id",
                instance_id,
                "--region",
                region,
                "--query",
                "[Status]",
                "--output",
                "json",
            ])
            .output()
            .await?;

        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&poll.stdout) {
            match json[0].as_str() {
                Some("Success") => {
                    eprintln!("bench-remote: benchmarks complete. Fetching results...");
                    return fetch_results(&args).await;
                }
                Some("Failed") => {
                    eprintln!("bench-remote: benchmarks failed. Try --fetch for partial results.");
                    return fetch_results(&args).await;
                }
                Some("TimedOut") => {
                    return Err("benchmark timed out (>2 hours)".into());
                }
                _ => continue,
            }
        }
    }
}

/// Fetch results from the instance via SSM send-command (short, reliable).
async fn fetch_results(args: &BenchRemoteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let instance_id = &args.instance_id;
    let region = &args.region;

    eprintln!("bench-remote: fetching results from {instance_id}...");

    let ls_output = ssm_exec(
        instance_id,
        region,
        "ls -td /home/ec2-user/ocync/bench-results/2* 2>/dev/null | head -1",
    )
    .await?;
    let remote_dir = ls_output.trim();

    if remote_dir.is_empty() {
        return Err("no results directory found on instance".into());
    }

    let summary = ssm_exec(instance_id, region, &format!("cat {remote_dir}/summary.md")).await?;

    let archive_json = ssm_exec(
        instance_id,
        region,
        "ls -t /home/ec2-user/ocync/bench-results/runs/*.json 2>/dev/null | head -1 | xargs cat",
    )
    .await?;

    // Write results locally.
    let remote_basename = Path::new(remote_dir)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let local_dir = Path::new(&args.output).join(remote_basename.as_ref());
    std::fs::create_dir_all(&local_dir)?;

    let summary_path = local_dir.join("summary.md");
    std::fs::write(&summary_path, &summary)?;
    eprintln!("bench-remote: wrote {}", summary_path.display());

    if !archive_json.trim().is_empty() {
        let runs_dir = Path::new(&args.output).join("runs");
        std::fs::create_dir_all(&runs_dir)?;
        let archive_filename =
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&archive_json) {
                format!("{}.json", val["timestamp"].as_str().unwrap_or("unknown"))
            } else {
                format!("{remote_basename}.json")
            };
        let archive_path = runs_dir.join(&archive_filename);
        std::fs::write(&archive_path, &archive_json)?;
        eprintln!("bench-remote: wrote {}", archive_path.display());
    }

    eprintln!("\n{summary}");
    Ok(())
}

/// Get the current local git branch name.
fn current_branch() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err("failed to get current git branch".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Run a short command on the instance via SSM send-command and return stdout.
///
/// Only for quick, non-interactive commands (file reads, ls). The benchmark
/// itself runs through an interactive SSM session with live output.
async fn ssm_exec(
    instance_id: &str,
    region: &str,
    command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let params = serde_json::json!({ "commands": [command] });

    let output = tokio::process::Command::new("aws")
        .args([
            "ssm",
            "send-command",
            "--instance-ids",
            instance_id,
            "--region",
            region,
            "--document-name",
            "AWS-RunShellScript",
            "--timeout-seconds",
            "30",
            "--parameters",
            &params.to_string(),
            "--query",
            "Command.CommandId",
            "--output",
            "text",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssm send-command failed: {stderr}").into());
    }

    let command_id = String::from_utf8(output.stdout)?.trim().to_string();

    // Poll for completion (short commands only).
    for _ in 0..6 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let poll = tokio::process::Command::new("aws")
            .args([
                "ssm",
                "get-command-invocation",
                "--command-id",
                &command_id,
                "--instance-id",
                instance_id,
                "--region",
                region,
                "--query",
                "[Status,StandardOutputContent]",
                "--output",
                "json",
            ])
            .output()
            .await?;

        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&poll.stdout) {
            match json[0].as_str() {
                Some("Success") => return Ok(json[1].as_str().unwrap_or("").to_string()),
                Some("Failed") => return Err("ssm command failed".into()),
                _ => continue,
            }
        }
    }

    Err("ssm command timed out".into())
}
