//! Tool execution: spawn benchmark tool processes with timing and output capture.

use std::path::{Path, PathBuf};
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
    /// Path to ocync's timing JSONL file (only set for ocync runs).
    pub(crate) timing_path: Option<PathBuf>,
}

/// Version string recorded when a tool cannot report its own version.
const UNKNOWN_VERSION: &str = "unknown";

/// Returns the version argument(s) for a given tool.
///
/// `regsync` and `ocync` use a `version` subcommand. `dregsy` has no version
/// flag, so it is probed with `--version` purely to confirm the binary runs,
/// and the probe is expected to exit non-zero.
fn version_args(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::Dregsy => &["--version"],
        Tool::Regsync | Tool::Ocync => &["version"],
    }
}

/// Whether a tool reports its own version.
///
/// `dregsy` has none to report: it takes no version flag, and its release
/// version is injected by ldflags that `go install` does not set. Every other
/// tool exits zero from its version subcommand, so a failure there means the
/// binary is broken rather than merely quiet.
fn reports_version(tool: Tool) -> bool {
    match tool {
        Tool::Dregsy => false,
        Tool::Regsync | Tool::Ocync => true,
    }
}

/// Binaries a tool delegates its transfers to, whose versions move its numbers.
///
/// `dregsy` transfers nothing itself. Its generated config sets `relay: skopeo`,
/// so skopeo moves every byte dregsy is credited with, and a skopeo change
/// shifts dregsy's measurements with nothing in dregsy's own version to explain
/// it. Recorded alongside the benchmarked tools for that reason.
fn relay_binaries(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::Dregsy => &["skopeo"],
        Tool::Regsync | Tool::Ocync => &[],
    }
}

/// Extracts a single-line version string from a completed version probe.
///
/// A non-zero exit yields diagnostic text rather than a version, so it is
/// never recorded as one. Where the binary reports a version this is an error,
/// since these probes are the only gate run before the benchmark starts. Where
/// it reports none it is expected, and the probe served only to confirm the
/// binary runs.
fn version_from_probe(
    name: &str,
    reports_version: bool,
    success: bool,
    stdout: &str,
    stderr: &str,
) -> Result<String, String> {
    if !success {
        if reports_version {
            let detail = stderr
                .lines()
                .chain(stdout.lines())
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no output")
                .trim();
            return Err(format!("{name} version probe failed: {detail}"));
        }
        return Ok(UNKNOWN_VERSION.to_string());
    }

    // Some tools write the version to stderr, so fall back to it.
    let raw = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };

    // Take only the first non-empty line to keep the summary clean.
    Ok(raw
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(UNKNOWN_VERSION)
        .trim()
        .to_string())
}

/// Runs a version probe and returns a single-line version string.
///
/// Errors when the binary cannot be executed, which means it is missing, and
/// when a binary that reports a version fails to. [`UNKNOWN_VERSION`] is
/// returned only where there is no version to report, in which case the pinned
/// version is recorded in the benchmark instance bootstrap instead.
async fn probe_version(
    binary: &str,
    args: &[&str],
    reports_version: bool,
) -> Result<String, String> {
    let output = tokio::process::Command::new(binary)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to run {binary} {args:?}: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    version_from_probe(
        binary,
        reports_version,
        output.status.success(),
        &stdout,
        &stderr,
    )
}

/// Runs a benchmarked tool's version probe.
pub(crate) async fn check_tool(tool: Tool) -> Result<String, String> {
    probe_version(tool.binary(), version_args(tool), reports_version(tool)).await
}

/// Returns the version of every binary `tool` relays its transfers through.
///
/// Keyed by binary name so the run record names what actually moved the bytes.
pub(crate) async fn check_relays(tool: Tool) -> Result<Vec<(String, String)>, String> {
    let mut versions = Vec::new();
    for &binary in relay_binaries(tool) {
        let version = probe_version(binary, &["--version"], true).await?;
        versions.push((binary.to_string(), version));
    }
    Ok(versions)
}

/// Builds ocync and bench-proxy in release mode from the given workspace root.
///
/// Touches all workspace `.rs` files before building to force cargo's
/// mtime-based fingerprinting to detect changes. After `git checkout` or
/// `git reset --hard`, git may preserve mtimes on unchanged files, causing
/// cargo to consider stale object files "fresh" and skip recompilation.
pub(crate) async fn build_ocync(workspace_root: &Path) -> Result<(), String> {
    // Force source mtimes newer than any cached dep-info so cargo
    // recompiles workspace crates even if git preserved mtimes.
    for dir in ["crates", "src", "bench/proxy/src"] {
        let target = workspace_root.join(dir);
        if target.is_dir() {
            touch_rs_files(&target);
        }
    }

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

/// Touch all `.rs` files under `dir` to set their mtime to now.
///
/// This is a local-path defense for `build_ocync()`. In the remote bench
/// path, `rm -rf target/` already forces a full rebuild, making this a
/// no-op. For local runs (or if `rm -rf target/` is ever removed), this
/// ensures cargo sees workspace sources as newer than cached dep-info.
fn touch_rs_files(dir: &Path) {
    let Ok(walker) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.is_dir() {
            touch_rs_files(&path);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // Explicitly set mtime -- opening with O_APPEND without writing
            // does NOT update mtime on Linux ext4/xfs (POSIX requires a
            // write, not just an open).
            if let Ok(f) = std::fs::File::open(&path) {
                let _ = f.set_modified(now);
            }
        }
    }
}

/// Spawns a benchmark tool with timing and output capture.
///
/// Sets `HTTPS_PROXY` to `proxy_url` so all HTTP traffic routes through the
/// proxy. The bench-proxy CA must be installed in the system trust store
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
    proxy_url: Option<&str>,
    workspace_root: &Path,
) -> Result<RunResult, String> {
    let config_str = config_path.to_string_lossy();
    let args: Vec<&str> = match tool {
        Tool::Ocync => vec!["sync", "--config", &config_str, "--json", "-v"],
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

    // For ocync, set OCYNC_TIMING_FILE so the engine writes phase timing JSONL.
    let timing_path = if tool == Tool::Ocync {
        Some(
            config_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("timing.jsonl"),
        )
    } else {
        None
    };

    let start = Instant::now();

    // Wrap with /usr/bin/time -v to capture peak RSS. GNU time writes its
    // report to stderr; the tool's own stderr is interleaved but the RSS
    // line has a distinctive prefix we can parse.
    let use_gnu_time = Path::new("/usr/bin/time").exists();
    let mut cmd = if use_gnu_time {
        let mut time_args = vec!["-v", binary.as_ref()];
        time_args.extend(args.iter().copied());
        let mut c = tokio::process::Command::new("/usr/bin/time");
        c.args(&time_args);
        c
    } else {
        let mut c = tokio::process::Command::new(binary.as_ref());
        c.args(&args);
        c
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(url) = proxy_url {
        cmd.env("HTTPS_PROXY", url);
    }
    if let Some(ref tp) = timing_path {
        cmd.env("OCYNC_TIMING_FILE", tp);
    }
    let spawn_desc = if use_gnu_time {
        format!("/usr/bin/time {}", tool.binary())
    } else {
        tool.binary().to_string()
    };
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {spawn_desc}: {e}"))?;

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
        timing_path,
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
    fn version_args_per_tool() {
        assert_eq!(version_args(Tool::Dregsy), &["--version"]);
        assert_eq!(version_args(Tool::Regsync), &["version"]);
        assert_eq!(version_args(Tool::Ocync), &["version"]);
    }

    /// The exact stderr a `go install` build of dregsy writes for `--version`,
    /// which earlier runs stored as its version string.
    const DREGSY_PROBE_STDERR: &str = "time=\"2026-04-26T13:17:46Z\" level=error \
         msg=\"flag provided but not defined: -version\"\nUsage of dregsy:";

    #[test]
    fn failed_probe_is_not_recorded_as_a_version() {
        let version = version_from_probe(
            "dregsy",
            reports_version(Tool::Dregsy),
            false,
            "",
            DREGSY_PROBE_STDERR,
        )
        .unwrap();

        assert_eq!(version, "unknown");
        assert!(
            !version.contains("flag provided but not defined"),
            "diagnostic output leaked into the recorded version: {version}"
        );
    }

    #[test]
    fn failed_probe_errors_for_a_tool_that_reports_a_version() {
        // These probes are the only gate before the benchmark runs, so a broken
        // ocync or regsync must not pass as an absent version.
        for tool in [Tool::Ocync, Tool::Regsync] {
            let err = version_from_probe(
                tool.binary(),
                reports_version(tool),
                false,
                "",
                "error: linker not found",
            )
            .expect_err("a tool that reports a version must fail loudly");
            assert!(
                err.contains("version probe failed") && err.contains("linker not found"),
                "unhelpful error for {tool}: {err}"
            );
        }
    }

    #[test]
    fn relay_probe_failure_is_an_error() {
        // skopeo moves every byte dregsy is credited with, so a broken skopeo
        // must not be recorded as an absent version either.
        let err = version_from_probe("skopeo", true, false, "", "skopeo: command failed")
            .expect_err("a relay binary must fail loudly");
        assert!(err.contains("skopeo version probe failed"), "{err}");
    }

    #[test]
    fn dregsy_relays_through_skopeo() {
        assert_eq!(relay_binaries(Tool::Dregsy), &["skopeo"]);
        assert!(relay_binaries(Tool::Ocync).is_empty());
        assert!(relay_binaries(Tool::Regsync).is_empty());
    }

    #[test]
    fn successful_probe_reads_the_version() {
        assert_eq!(
            version_from_probe("ocync", true, true, "ocync 0.6.0\n", "").unwrap(),
            "ocync 0.6.0"
        );
        // regsync pads its output and precedes the tag with a blank line.
        assert_eq!(
            version_from_probe("regsync", true, true, "\nVCSTag:     v0.11.5\n", "").unwrap(),
            "VCSTag:     v0.11.5"
        );
        // skopeo reports cleanly on stdout with a zero exit.
        assert_eq!(
            version_from_probe("skopeo", true, true, "skopeo version 1.24.0\n", "").unwrap(),
            "skopeo version 1.24.0"
        );
    }

    #[test]
    fn successful_probe_falls_back_to_stderr() {
        assert_eq!(
            version_from_probe("ocync", true, true, "   ", "ocync 0.6.0").unwrap(),
            "ocync 0.6.0"
        );
    }

    #[test]
    fn successful_probe_with_no_output_is_unknown() {
        assert_eq!(
            version_from_probe("ocync", true, true, "", "").unwrap(),
            "unknown"
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
