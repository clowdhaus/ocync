//! Tool execution: spawn benchmark tool processes with timing and output capture.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};

/// Benchmark tool variant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Tool {
    /// The ocync OCI sync tool.
    Ocync,
    /// The dregsy OCI sync tool.
    Dregsy,
    /// The regsync OCI sync tool.
    Regsync,
}

impl Tool {
    /// Returns the binary name for this tool.
    pub(crate) fn binary(&self) -> &'static str {
        match self {
            Tool::Ocync => "ocync",
            Tool::Dregsy => "dregsy",
            Tool::Regsync => "regsync",
        }
    }

    /// Parses a tool name case-insensitively.
    pub(crate) fn parse(s: &str) -> Result<Tool, String> {
        match s.to_ascii_lowercase().as_str() {
            "ocync" => Ok(Tool::Ocync),
            "dregsy" => Ok(Tool::Dregsy),
            "regsync" => Ok(Tool::Regsync),
            other => Err(format!(
                "unknown tool: {other:?}; expected one of: ocync, dregsy, regsync"
            )),
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.binary())
    }
}

/// The result of running a benchmark tool once.
#[derive(Debug)]
pub(crate) struct RunResult {
    /// Wall-clock time from process spawn to exit.
    pub(crate) wall_clock: Duration,
    /// Exit code, or `None` if the process was killed by a signal.
    pub(crate) exit_code: Option<i32>,
    /// Peak resident set size in KB (from `/usr/bin/time -v`), if available.
    pub(crate) peak_rss_kb: Option<u64>,
}

/// Runs a tool with `version` and returns a single-line version string.
pub(crate) async fn check_tool(tool: Tool) -> Result<String, String> {
    let output = tokio::process::Command::new(tool.binary())
        .arg("version")
        .output()
        .await
        .map_err(|e| format!("failed to run {} version: {}", tool.binary(), e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let raw = if !stdout.trim().is_empty() {
        stdout
    } else {
        stderr
    };

    // Take only the first non-empty line to keep the summary clean.
    let version = raw
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("unknown")
        .trim();

    Ok(version.to_string())
}

/// Builds ocync and bench-proxy in release mode from the given workspace root.
pub(crate) async fn build_ocync(workspace_root: &Path) -> Result<(), String> {
    let status = tokio::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "--package",
            "ocync",
            "--package",
            "bench-proxy",
        ])
        .current_dir(workspace_root)
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo build: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo build --release exited with status {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
        ))
    }
}

/// Spawns a benchmark tool with timing and output capture.
///
/// Sets `HTTPS_PROXY` to `proxy_url` so all HTTP traffic routes through the
/// proxy. The mitmproxy CA must be installed in the system trust store
/// (via `update-ca-trust`) so that both rustls-native-certs (ocync) and
/// OpenSSL (dregsy/regsync via Go) trust the MITM'd connections.
///
/// Tool-specific arguments:
/// - ocync: `sync --config <path> --json`
/// - dregsy: `-config <path>`
/// - regsync: `once -c <path>`
pub(crate) async fn run_tool(
    tool: Tool,
    config_path: &Path,
    proxy_url: &str,
    workspace_root: &Path,
) -> Result<RunResult, String> {
    let config_str = config_path.to_string_lossy();
    let args: Vec<&str> = match tool {
        Tool::Ocync => vec!["sync", "--config", &config_str, "--json"],
        Tool::Dregsy => vec!["-config", &config_str],
        Tool::Regsync => vec!["once", "-c", &config_str],
    };

    // Use the workspace release binary for ocync (just built by build_ocync),
    // PATH lookup for external tools (dregsy, regsync).
    let binary: std::borrow::Cow<'_, str> = match tool {
        Tool::Ocync => workspace_root
            .join("target/release/ocync")
            .to_string_lossy()
            .into_owned()
            .into(),
        _ => tool.binary().into(),
    };

    let start = Instant::now();

    // Wrap with /usr/bin/time -v to capture peak RSS. GNU time writes its
    // report to stderr; the tool's own stderr is interleaved but the RSS
    // line has a distinctive prefix we can parse.
    let use_gnu_time = Path::new("/usr/bin/time").exists();
    let mut child = if use_gnu_time {
        let mut time_args = vec!["-v", binary.as_ref()];
        time_args.extend(args.iter().copied());
        tokio::process::Command::new("/usr/bin/time")
            .args(&time_args)
            .env("HTTPS_PROXY", proxy_url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn /usr/bin/time {}: {e}", tool.binary()))?
    } else {
        tokio::process::Command::new(binary.as_ref())
            .args(&args)
            .env("HTTPS_PROXY", proxy_url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", tool.binary()))?
    };

    // Stream stdout and stderr line-by-line while capturing for RunResult.
    // Lines go to stderr (not stdout) so they flow through the nohup log
    // file and SSH pipe without interfering with structured output.
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let tool_name = tool.to_string();
    let tool_name_err = tool_name.clone();
    // ocync --json writes structured JSON to stdout for the reporting
    // pipeline; don't echo it to the operator. Other tools write
    // human-readable progress to stdout, so stream those.
    let echo_stdout = tool != Tool::Ocync;
    let stdout_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout_pipe);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            if echo_stdout {
                eprint!("  [{tool_name}] {line}");
            }
            line.clear();
        }
    });

    let stderr_handle = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr_pipe);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            if !is_gnu_time_line(&line) {
                eprint!("  [{tool_name_err}] {line}");
            }
            buf.push_str(&line);
            line.clear();
        }
        buf
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("failed to wait for {}: {e}", tool.binary()))?;
    let wall_clock = start.elapsed();

    stdout_handle
        .await
        .map_err(|e| format!("stdout task failed: {e}"))?;
    let stderr = stderr_handle
        .await
        .map_err(|e| format!("stderr task failed: {e}"))?;

    let peak_rss_kb = parse_gnu_time_rss(&stderr);

    if !status.success() && !stderr.trim().is_empty() {
        eprintln!("  {} stderr:\n{}", tool, textwrap_indent(&stderr, "    "));
    }

    Ok(RunResult {
        wall_clock,
        exit_code: status.code(),
        peak_rss_kb,
    })
}

/// Returns `true` if the line is part of GNU `/usr/bin/time -v` output
/// rather than the wrapped tool's own stderr.
fn is_gnu_time_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.starts_with("Command exited")
        || trimmed.starts_with("Command being timed")
        || trimmed.starts_with("User time")
        || trimmed.starts_with("System time")
        || trimmed.starts_with("Percent of CPU")
        || trimmed.starts_with("Elapsed")
        || trimmed.starts_with("Average")
        || trimmed.starts_with("Maximum")
        || trimmed.starts_with("Major")
        || trimmed.starts_with("Minor")
        || trimmed.starts_with("Voluntary")
        || trimmed.starts_with("Involuntary")
        || trimmed.starts_with("Swaps")
        || trimmed.starts_with("File system")
        || trimmed.starts_with("Socket")
        || trimmed.starts_with("Signals")
        || trimmed.starts_with("Page size")
        || trimmed.starts_with("Exit status")
}

/// Parse `Maximum resident set size (kbytes): NNN` from GNU time -v output.
fn parse_gnu_time_rss(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Maximum resident set size (kbytes):") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Indents every line of `text` with `prefix`.
fn textwrap_indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_parse_case_insensitive() {
        assert_eq!(Tool::parse("Ocync").unwrap(), Tool::Ocync);
        assert_eq!(Tool::parse("DREGSY").unwrap(), Tool::Dregsy);
        assert_eq!(Tool::parse("regsync").unwrap(), Tool::Regsync);
        assert_eq!(
            Tool::parse("unknown").unwrap_err(),
            "unknown tool: \"unknown\"; expected one of: ocync, dregsy, regsync"
        );
    }

    #[test]
    fn tool_binary_names() {
        assert_eq!(Tool::Ocync.binary(), "ocync");
        assert_eq!(Tool::Dregsy.binary(), "dregsy");
        assert_eq!(Tool::Regsync.binary(), "regsync");
    }

    #[test]
    fn textwrap_indent_adds_prefix() {
        let input = "line one\nline two";
        let result = textwrap_indent(input, "  ");
        assert_eq!(result, "  line one\n  line two");
    }

    #[test]
    fn gnu_time_lines_detected() {
        assert!(is_gnu_time_line(
            "\tCommand being timed: \"./target/release/ocync\""
        ));
        assert!(is_gnu_time_line(
            "\tMaximum resident set size (kbytes): 59468"
        ));
        assert!(is_gnu_time_line(
            "\tElapsed (wall clock) time (h:mm:ss or m:ss): 4:33.12"
        ));
        assert!(is_gnu_time_line("\tExit status: 0"));
        assert!(is_gnu_time_line("\tVoluntary context switches: 12345"));
        assert!(is_gnu_time_line(
            "\tMinor (reclaiming a frame) page faults: 100"
        ));
        assert!(is_gnu_time_line("\tPage size (bytes): 4096"));
        assert!(is_gnu_time_line("\tFile system inputs: 0"));
        assert!(is_gnu_time_line("\tSocket messages sent: 0"));
        assert!(is_gnu_time_line("\tSignals delivered: 0"));
        assert!(is_gnu_time_line("\tSwaps: 0"));
        assert!(is_gnu_time_line("\tAverage shared text size (kbytes): 0"));
        assert!(is_gnu_time_line("\tUser time (seconds): 0.07"));
        assert!(is_gnu_time_line("\tSystem time (seconds): 0.01"));
        assert!(is_gnu_time_line("\tPercent of CPU this job got: 100%"));
        assert!(is_gnu_time_line("Command exited with non-zero status 2"));
    }

    #[test]
    fn tool_output_not_detected_as_gnu_time() {
        assert!(!is_gnu_time_line("error: HTTP request failed"));
        assert!(!is_gnu_time_line(
            "time=\"2026-04-19\" level=info msg=\"starting\""
        ));
        assert!(!is_gnu_time_line("syncing 42 images to ECR"));
        assert!(!is_gnu_time_line(""));
    }

    #[test]
    fn parse_gnu_time_rss_extracts_value() {
        let stderr = "\tCommand being timed: \"./target/release/ocync\"\n\
                      \tMaximum resident set size (kbytes): 59468\n\
                      \tExit status: 0\n";
        assert_eq!(parse_gnu_time_rss(stderr), Some(59468));
    }

    #[test]
    fn parse_gnu_time_rss_missing_returns_none() {
        let stderr = "error: some tool output\nno time info here\n";
        assert_eq!(parse_gnu_time_rss(stderr), None);
    }

    #[test]
    fn parse_gnu_time_rss_empty_returns_none() {
        assert_eq!(parse_gnu_time_rss(""), None);
    }
}
