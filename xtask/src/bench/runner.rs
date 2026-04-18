//! Tool execution: spawn benchmark tool processes with timing and output capture.

use std::path::Path;
use std::time::{Duration, Instant};

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
    /// Captured standard output.
    pub(crate) stdout: String,
    /// Peak resident set size in KB (from `/usr/bin/time -v`), if available.
    pub(crate) peak_rss_kb: Option<u64>,
}

/// Runs a tool with its version flag and returns the version string.
///
/// Version args: ocync="version", dregsy="--version", regsync="version".
pub(crate) async fn check_tool(tool: Tool) -> Result<String, String> {
    let version_arg = match tool {
        Tool::Dregsy => "--version",
        Tool::Ocync | Tool::Regsync => "version",
    };

    let output = tokio::process::Command::new(tool.binary())
        .arg(version_arg)
        .output()
        .await
        .map_err(|e| format!("failed to run {} {}: {}", tool.binary(), version_arg, e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let version = if !stdout.trim().is_empty() {
        stdout
    } else {
        stderr
    };

    Ok(version.trim().to_string())
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
    let output = if use_gnu_time {
        let mut time_args = vec!["-v", binary.as_ref()];
        time_args.extend(args.iter().copied());
        tokio::process::Command::new("/usr/bin/time")
            .args(&time_args)
            .env("HTTPS_PROXY", proxy_url)
            .output()
            .await
            .map_err(|e| format!("failed to spawn /usr/bin/time {}: {e}", tool.binary()))?
    } else {
        tokio::process::Command::new(binary.as_ref())
            .args(&args)
            .env("HTTPS_PROXY", proxy_url)
            .output()
            .await
            .map_err(|e| format!("failed to spawn {}: {e}", tool.binary()))?
    };

    let wall_clock = start.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Parse peak RSS from GNU time output.
    let peak_rss_kb = parse_gnu_time_rss(&stderr);

    // Log stderr when the tool exits non-zero so failures are diagnosable.
    // GNU time exits with the wrapped command's exit code.
    if !output.status.success() && !stderr.trim().is_empty() {
        eprintln!("  {} stderr:\n{}", tool, textwrap_indent(&stderr, "    "));
    }

    Ok(RunResult {
        wall_clock,
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        peak_rss_kb,
    })
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
}
