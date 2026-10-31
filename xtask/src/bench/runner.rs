//! Tool execution: spawn benchmark tool processes with timing and output capture.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};

use super::remote::{CommitSha, git, resolve_commit};

/// Benchmark tool variant. Declaration order is the order of columns in the
/// reports, so ocync leads.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, clap::ValueEnum)]
pub(crate) enum Tool {
    /// The ocync OCI sync tool.
    Ocync,
    /// The ocync built from the `--baseline-ref` commit, run beside the
    /// current build to tell a regression from run-to-run noise. Never named
    /// on `--tools`: only `--baseline-ref` adds it.
    #[value(skip)]
    OcyncBaseline,
    /// The dregsy OCI sync tool.
    Dregsy,
    /// The regsync OCI sync tool.
    Regsync,
}

impl Tool {
    /// Returns the binary name for this tool.
    pub(crate) fn binary(&self) -> &'static str {
        match self {
            Tool::Ocync | Tool::OcyncBaseline => "ocync",
            Tool::Dregsy => "dregsy",
            Tool::Regsync => "regsync",
        }
    }

    /// Whether the tool is an ocync build, run with the same config and
    /// arguments whichever commit it was built from.
    pub(crate) fn is_ocync(self) -> bool {
        matches!(self, Tool::Ocync | Tool::OcyncBaseline)
    }
}

/// The name a tool carries in file names and run records. Distinct from
/// [`Tool::binary`] because the baseline shares ocync's binary name, and its
/// config and proxy log must not overwrite ocync's.
impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Tool::OcyncBaseline => "ocync-baseline",
            other => other.binary(),
        })
    }
}

/// The tools as a comma-separated list in the spelling `--tools` parses.
pub(crate) fn join_tools(tools: &[Tool]) -> String {
    tools
        .iter()
        .map(super::value_name)
        .collect::<Vec<_>>()
        .join(",")
}

/// The result of running a benchmark tool once.
#[derive(Debug)]
pub(crate) struct RunResult {
    /// Wall-clock time from process spawn to exit.
    pub(crate) wall_clock: Duration,
    /// How the process ended, including a kill by signal.
    pub(crate) exit_status: ExitStatus,
    /// Peak resident set size in KB (from `/usr/bin/time -v`), if available.
    pub(crate) peak_rss_kb: Option<u64>,
    /// Path to ocync's timing JSONL file (only set for ocync runs).
    pub(crate) timing_path: Option<PathBuf>,
}

/// Prefix of a recorded version where none could be determined.
///
/// Never recorded bare: [`unknown_version`] always appends why, so a check for
/// a lost-provenance run tests this as a prefix rather than for equality.
const UNKNOWN_VERSION: &str = "unknown";

/// Longest subprocess diagnostic carried into a recorded version.
const MAX_REASON_LEN: usize = 160;

/// Trims a subprocess diagnostic to something safe to commit.
///
/// These reasons come from local tooling and land in a git-tracked file, so an
/// absolute path would commit one machine's directory layout to the repository
/// and an unbounded line would fight the archive's stated aim of staying small
/// enough to accumulate. Paths are reduced to their final component.
fn sanitize_reason(reason: &str) -> String {
    let cleaned: Vec<String> = reason
        .split_whitespace()
        .map(|token| match token.rsplit_once('/') {
            Some((_, name)) if token.starts_with('/') && !name.is_empty() => name.to_string(),
            _ => token.to_string(),
        })
        .collect();

    let mut out = cleaned.join(" ");
    if out.chars().count() > MAX_REASON_LEN {
        out = out.chars().take(MAX_REASON_LEN).collect::<String>() + "...";
    }
    out
}

/// Records that no version could be determined, and why.
///
/// The reason belongs in the record rather than only on stderr. The archive
/// outlives the instance, the run log is streamed to the operator's terminal
/// and never persisted, and a bare `unknown` reads the same for a binary with
/// no build info, an absent Go toolchain and a working-tree build. A run whose
/// provenance was lost should say so where the numbers are.
fn unknown_version(reason: &str) -> String {
    format!("{UNKNOWN_VERSION} ({})", sanitize_reason(reason))
}

/// Records a version the binary reported while the authoritative lookup failed.
///
/// Several of these print a placeholder when built without their release
/// ldflags, and a placeholder is indistinguishable from a healthy release build
/// once it reaches the archive. Carrying the reason keeps that distinguishable.
fn reported_version(reported: &str, reason: &str) -> String {
    format!(
        "{reported} (module version unavailable: {})",
        sanitize_reason(reason)
    )
}

/// Chooses what to record for a relay whose module lookup failed.
///
/// Both outcomes carry the lookup failure. Neither may read as a plain
/// version, since that is what makes a lost record look like a healthy one.
fn relay_version_after_failed_lookup(reported: Option<String>, reason: &str) -> String {
    match reported {
        Some(reported) => reported_version(&reported, reason),
        None => unknown_version(&format!("{reason}, and --version reported nothing")),
    }
}

/// Returns the version argument(s) for a given tool.
///
/// `regsync` and `ocync` use a `version` subcommand. `dregsy` has no version
/// flag, so it is probed with `--version` purely to confirm the binary runs,
/// and the probe is expected to exit non-zero.
fn version_args(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::Dregsy => &["--version"],
        Tool::Regsync | Tool::Ocync | Tool::OcyncBaseline => &["version"],
    }
}

/// Whether a tool reports its own version.
///
/// `dregsy` has none to report: it takes no version flag, and its release
/// version is injected by ldflags that `go install` does not set. Its version
/// comes from the module stamped into the binary instead. Every other tool
/// exits zero from its version subcommand, so a failure there means the binary
/// is broken rather than merely quiet.
fn reports_version(tool: Tool) -> bool {
    match tool {
        Tool::Dregsy => false,
        Tool::Regsync | Tool::Ocync | Tool::OcyncBaseline => true,
    }
}

/// Binaries a tool delegates to, whose versions move its numbers.
///
/// `dregsy` transfers nothing itself. Its generated config sets `relay: skopeo`,
/// so skopeo moves every byte dregsy is credited with, and a skopeo change
/// shifts dregsy's measurements with nothing in dregsy's own version to explain
/// it.
///
/// Both Go tools authenticate to ECR through the credential helper, so a
/// release of it that changes token caching moves their request counts. ocync
/// calls the AWS SDK directly and does not use it.
fn relay_binaries(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::Dregsy => &["skopeo", "docker-credential-ecr-login"],
        Tool::Regsync => &["docker-credential-ecr-login"],
        Tool::Ocync | Tool::OcyncBaseline => &[],
    }
}

/// First non-blank line of a process's output, trimmed.
///
/// Tools pad their output and split it across streams, and only the first
/// meaningful line is wanted whether it is a version or a diagnostic.
fn first_line<'a>(primary: &'a str, fallback: &'a str) -> Option<&'a str> {
    primary
        .lines()
        .chain(fallback.lines())
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
}

/// Extracts a single-line version string from a completed version probe.
///
/// `Ok(None)` means the binary ran but reported no version, which callers
/// resolve from the module version stamped into it. That is distinct from the
/// literal string `unknown`, which a tool could legitimately print.
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
) -> Result<Option<String>, String> {
    if !success {
        if reports_version {
            let detail = first_line(stderr, stdout).unwrap_or("no output");
            return Err(format!("{name} version probe failed: {detail}"));
        }
        return Ok(None);
    }

    // Some tools write the version to stderr, so fall back to it.
    Ok(first_line(stdout, stderr).map(str::to_string))
}

/// Runs a version probe and returns a single-line version string.
///
/// Errors when the binary cannot be executed, which means it is missing, and
/// when a binary that reports a version fails to. `Ok(None)` means the binary
/// ran but reported no version of its own.
async fn probe_version(
    binary: &str,
    args: &[&str],
    reports_version: bool,
) -> Result<Option<String>, String> {
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

/// Version Go stamps into a binary built from a local working tree.
///
/// It names no released artifact, so it is not a usable record of what ran.
const DEVEL_VERSION: &str = "(devel)";

/// Extracts the module version from `go version -m` output.
///
/// The build info lists one `mod` line holding the main module's path and
/// version, followed by a `dep` line per dependency. Returns `None` for a
/// binary built from a working tree, whose stamped version identifies nothing.
fn module_version_from_build_info(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != "mod" {
            return None;
        }
        fields.next()?; // module path
        fields.next().filter(|v| *v != DEVEL_VERSION)
    })
}

/// Explains why a `go version -m` probe yielded no module version.
///
/// The probe runs under `sh`, so a missing Go toolchain and a binary missing
/// from `PATH` both surface as a non-zero exit rather than a spawn failure.
/// The tool's own text separates them, so it is passed through verbatim.
fn no_module_version_reason(success: bool, stdout: &str, stderr: &str) -> String {
    if success {
        return "build info carries no released module version".to_string();
    }

    let detail = first_line(stderr, stdout).unwrap_or("no output");
    format!("go version -m failed: {detail}")
}

/// Reads the module version stamped into a Go binary on `PATH`.
///
/// Returns the reason on failure rather than swallowing it, so a caller can
/// say what it recorded instead. Callers differ: one falls back to asking the
/// binary, the other records that no version is known.
async fn go_module_version(binary: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("go version -m \"$(command -v {binary})\""))
        .output()
        .await
        .map_err(|e| format!("could not run sh: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match module_version_from_build_info(&stdout) {
        Some(version) => Ok(version.to_string()),
        None => Err(no_module_version_reason(
            output.status.success(),
            &stdout,
            &stderr,
        )),
    }
}

/// Resolves the binary a tool is executed as.
///
/// ocync runs from the release build under `workspace_root` rather than
/// `PATH`, since that is what `build_ocync` just produced. For the baseline the
/// caller passes the root of its own checkout, see [`baseline_dir`]. A `PATH` lookup would find whatever
/// was installed when the instance was created, which can be months older, so
/// version probes must resolve the same way runs do or the record names a
/// binary that produced none of the numbers.
fn tool_path(tool: Tool, workspace_root: &Path) -> std::borrow::Cow<'static, str> {
    match tool {
        Tool::Ocync | Tool::OcyncBaseline => workspace_root
            .join("target/release/ocync")
            .to_string_lossy()
            .into_owned()
            .into(),
        Tool::Dregsy | Tool::Regsync => tool.binary().into(),
    }
}

/// Runs a benchmarked tool's version probe.
///
/// Falls back to the binary's stamped module version where the tool reports
/// none, so a run record names the version that produced its numbers. The
/// fallback keys off the probe reporting nothing rather than off a sentinel
/// string, so a tool that prints the word `unknown` is still recorded as
/// having reported it.
pub(crate) async fn check_tool(tool: Tool, workspace_root: &Path) -> Result<String, String> {
    let binary = tool_path(tool, workspace_root);

    if let Some(version) = probe_version(&binary, version_args(tool), reports_version(tool)).await?
    {
        return Ok(version);
    }

    match go_module_version(&binary).await {
        Ok(version) => Ok(version),
        Err(reason) => {
            eprintln!("WARNING: no version for {binary}: {reason}");
            Ok(unknown_version(&reason))
        }
    }
}

/// Returns the version of every binary the given tools relay through.
///
/// Keyed by binary name so the run record names what actually moved the bytes.
/// Takes the whole tool set because relays are shared: probing per tool would
/// read the credential helper's build info twice and warn twice about it.
pub(crate) async fn check_relays(tools: &[Tool]) -> Result<Vec<(String, String)>, String> {
    let binaries: BTreeSet<&'static str> = tools
        .iter()
        .flat_map(|&tool| relay_binaries(tool).iter().copied())
        .collect();

    let mut versions = Vec::new();
    for binary in binaries {
        // These are all Go binaries, and the stamped module version names the
        // artifact exactly. Asking the binary is the weaker source: the
        // credential helper prints "development" whatever it was built from,
        // because `go install` sets none of its Makefile's ldflags.
        let version = match go_module_version(binary).await {
            Ok(version) => version,
            Err(reason) => {
                eprintln!("WARNING: no module version for {binary}: {reason}");

                // Still probe, so a missing binary fails here rather than
                // midway through a run. A relay need not accept `--version`,
                // so a non-zero exit is not itself an error. Either way the
                // reason is carried, since what the binary reports may be a
                // placeholder that reads like a healthy release build.
                let reported = probe_version(binary, &["--version"], false).await?;
                relay_version_after_failed_lookup(reported, &reason)
            }
        };
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
        Err(format!("cargo build --release failed ({status})"))
    }
}

/// Where baseline checkouts live, one directory per commit.
const BASELINE_ROOT: &str = "bench/.baseline";

/// The checkout the baseline build lives in. One per commit, so a second run
/// at the same commit reuses the checkout and its build.
pub(crate) fn baseline_dir(commit: &CommitSha) -> PathBuf {
    Path::new(BASELINE_ROOT).join(commit.to_string())
}

/// Makes `commit` available as a clean git worktree of the repository at
/// `repo`, and leaves no other baseline checkout there.
///
/// A checkout from an earlier run is reused only if it is at `commit` with
/// nothing modified, since a benchmark of a tree that is not the commit says
/// nothing about it.
fn ensure_checkout(repo: &Path, commit: &CommitSha) -> Result<(), String> {
    let err = |e: Box<dyn std::error::Error>| e.to_string();
    let dir = repo.join(baseline_dir(commit));
    let io = |path: &Path, e: std::io::Error| format!("cannot remove {}: {e}", path.display());
    // A checkout deleted behind git's back stays registered and would make
    // the add refuse, and would be listed below as one to remove.
    git(repo, &["worktree", "prune"]).map_err(err)?;
    let registered: Vec<PathBuf> = git(repo, &["worktree", "list", "--porcelain"])
        .map_err(err)?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| Path::new(path).canonicalize().ok())
        .collect();
    let entries = std::fs::read_dir(repo.join(BASELINE_ROOT))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| *path != dir);
    for other in entries {
        // Git's own removal for a worktree, which also drops its registration;
        // anything else in the directory is not git's to remove.
        let is_worktree = other
            .canonicalize()
            .is_ok_and(|path| registered.contains(&path));
        if is_worktree {
            git(
                repo,
                &["worktree", "remove", "--force", &other.to_string_lossy()],
            )
            .map_err(err)?;
        } else if other.is_dir() && !other.is_symlink() {
            std::fs::remove_dir_all(&other).map_err(|e| io(&other, e))?;
        } else {
            std::fs::remove_file(&other).map_err(|e| io(&other, e))?;
        }
    }

    if !dir.join(".git").exists() {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                &dir.to_string_lossy(),
                &commit.to_string(),
            ],
        )
        .map_err(err)?;
    }
    let head = resolve_commit(&dir, "HEAD").map_err(err)?;
    if head != *commit {
        return Err(format!(
            "{} is at {head}, not the baseline {commit}; remove it",
            dir.display()
        ));
    }
    if !git(&dir, &["status", "--porcelain"])
        .map_err(err)?
        .is_empty()
    {
        return Err(format!(
            "{} has local changes, so it is not the baseline {commit}; remove it",
            dir.display()
        ));
    }
    Ok(())
}

/// Checks `commit` out into its own git worktree and builds ocync there.
///
/// The build gets a target directory inside the checkout, so it cannot replace
/// the current build whatever `CARGO_TARGET_DIR` says.
pub(crate) async fn build_baseline(commit: &CommitSha) -> Result<(), String> {
    ensure_checkout(Path::new("."), commit)?;
    let status = tokio::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            "ocync",
            "--target-dir",
            "target",
        ])
        .current_dir(baseline_dir(commit))
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo build for the baseline: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo build of the baseline {} failed ({status})",
            commit.short()
        ))
    }
}

/// The flags of `ocync sync` that every round passes, which a baseline must
/// accept. `-v` is the short form of `--verbose`.
const SYNC_FLAGS: [(&str, &str); 2] = [("--json", "--json"), ("--verbose", "-v")];

/// The flags of [`SYNC_FLAGS`] that `help`, the output of `ocync sync --help`,
/// does not list, by the long name.
fn missing_sync_flags(help: &str) -> Vec<&'static str> {
    let listed = |flag: &str| {
        help.split(|c: char| c.is_whitespace() || c == ',')
            .any(|word| word == flag)
    };
    SYNC_FLAGS
        .iter()
        .filter(|(long, short)| !listed(long) && !listed(short))
        .map(|(long, _)| *long)
        .collect()
}

/// Fails unless the baseline build for `commit` accepts what a round gives it:
/// the config the run will generate, which `ocync validate` checks without
/// touching a registry, and the flags `sync` is run with, which `sync --help`
/// lists. A commit too old for either is refused before any round runs.
pub(crate) fn check_baseline(commit: &CommitSha, config: &Path) -> Result<(), String> {
    check_baseline_in(&baseline_dir(commit), config)
}

/// [`check_baseline`] for the checkout at `root`.
fn check_baseline_in(root: &Path, config: &Path) -> Result<(), String> {
    let binary = tool_path(Tool::OcyncBaseline, root);
    let run = |args: &[&str]| {
        std::process::Command::new(binary.as_ref())
            .args(args)
            .output()
            .map_err(|e| format!("failed to run {binary}: {e}"))
    };

    let help = run(&["sync", "--help"])?;
    if let Some(flag) = missing_sync_flags(&String::from_utf8_lossy(&help.stdout)).first() {
        return Err(format!(
            "the baseline build's `sync --help` does not list {flag}, which every round passes"
        ));
    }

    let validate = run(&["validate", &config.to_string_lossy()])?;
    if validate.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&validate.stderr);
    let stdout = String::from_utf8_lossy(&validate.stdout);
    Err(format!(
        "the baseline build rejected the generated config: {}",
        first_line(&stderr, &stdout).unwrap_or("no output")
    ))
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
///
/// `label` is how the tool is named to the operator in the streamed output.
pub(crate) async fn run_tool(
    tool: Tool,
    label: &str,
    config_path: &Path,
    proxy_url: Option<&str>,
    workspace_root: &Path,
    dry_run: bool,
) -> Result<RunResult, String> {
    let config_str = config_path.to_string_lossy();
    let mut args: Vec<&str> = match tool {
        Tool::Ocync | Tool::OcyncBaseline => {
            vec!["sync", "--config", &config_str, "--json", "-v"]
        }
        Tool::Dregsy => vec!["-config", &config_str],
        Tool::Regsync => vec!["once", "-c", &config_str],
    };
    // Only ocync has a mode that resolves every mapping and then stops, which
    // is what isolates the prepare phase. The scenario that asks for it runs
    // ocync alone for exactly that reason.
    if dry_run {
        assert_eq!(
            tool,
            Tool::Ocync,
            "dry-run is an ocync-only mode; no competitor has an equivalent"
        );
        args.push("--dry-run");
    }

    let binary = tool_path(tool, workspace_root);

    // For ocync, set OCYNC_TIMING_FILE so the engine writes phase timing JSONL.
    let timing_path = if tool.is_ocync() {
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

    let tool_name = label.to_owned();
    let tool_name_err = tool_name.clone();
    // ocync --json writes structured JSON to stdout for the reporting
    // pipeline; don't echo it to the operator. Other tools write
    // human-readable progress to stdout, so stream those.
    let echo_stdout = !tool.is_ocync();
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
        eprintln!("  {label} stderr:\n{}", textwrap_indent(&stderr, "    "));
    }

    Ok(RunResult {
        wall_clock,
        exit_status: status,
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
    fn join_tools_is_the_form_tools_flag_parses() {
        let tools = [Tool::Regsync, Tool::Ocync, Tool::Dregsy];
        let joined = join_tools(&tools);
        assert_eq!(joined, "regsync,ocync,dregsy");
        let parsed = crate::bench::parse_bench(&["--tools", &joined, "prepare"]).unwrap();
        assert_eq!(parsed.tool_selection.tools, tools);
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

        assert_eq!(
            version, None,
            "a failed probe must report no version, not diagnostic text"
        );
    }

    #[test]
    fn zero_exit_probe_output_is_taken_verbatim() {
        // dregsy builds from an unpinned commit, so nothing fixes its exit
        // code. Were it to start exiting zero, its log line must not be
        // rescued by a sentinel comparison and recorded as a version.
        let logged = "time=\"2026-04-26T13:17:46Z\" level=info msg=\"dregsy \"";
        let version =
            version_from_probe("dregsy", reports_version(Tool::Dregsy), true, logged, "").unwrap();

        assert_eq!(version, Some(logged.to_string()));
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
    fn an_unknown_version_records_why() {
        // The archive outlives the instance and the run log is never
        // persisted, so a bare "unknown" would leave a lost-provenance run
        // indistinguishable from an absent Go toolchain months later. The
        // exact shape is asserted because bench/CLAUDE.md documents it.
        assert_eq!(
            unknown_version("sh: go: command not found"),
            "unknown (sh: go: command not found)"
        );
    }

    #[test]
    fn neither_relay_outcome_reads_as_a_plain_version() {
        // Whichever way the fallback goes, the record must show that the
        // authoritative lookup failed. A bare "development" is what makes a
        // lost record indistinguishable from a healthy release build.
        let reason = "sh: go: command not found";

        let answered = relay_version_after_failed_lookup(
            Some("docker-credential-ecr-login (...) development".to_string()),
            reason,
        );
        assert!(
            answered.contains("module version unavailable"),
            "{answered}"
        );
        assert!(answered.contains("command not found"), "{answered}");

        let silent = relay_version_after_failed_lookup(None, reason);
        assert!(silent.starts_with(UNKNOWN_VERSION), "{silent}");
        assert!(silent.contains("command not found"), "{silent}");
        assert!(
            silent.contains("--version reported nothing"),
            "the second failure is also part of why: {silent}"
        );
    }

    #[test]
    fn a_reported_version_carries_a_failed_module_lookup() {
        // The credential helper prints this placeholder whatever it was built
        // from, so without the reason the record reads like a healthy release.
        let recorded = reported_version(
            "docker-credential-ecr-login (github.com/awslabs/...) development",
            "sh: go: command not found",
        );

        assert!(
            recorded.starts_with("docker-credential-ecr-login"),
            "{recorded}"
        );
        assert!(
            recorded.contains("module version unavailable"),
            "{recorded}"
        );
        assert!(recorded.contains("command not found"), "{recorded}");
    }

    #[test]
    fn a_recorded_reason_carries_no_local_paths_and_is_bounded() {
        // These land in a git-tracked file, so an absolute path would commit
        // one machine's directory layout to the repository.
        let recorded =
            unknown_version("could not read Go build info from /Users/someone/go/bin/skopeo");
        assert!(!recorded.contains("/Users/"), "{recorded}");
        assert!(recorded.contains("skopeo"), "{recorded}");

        let long = unknown_version(&"x".repeat(500));
        assert!(
            long.chars().count() < 200,
            "unbounded reason: {}",
            long.len()
        );
    }

    #[test]
    fn first_line_prefers_the_primary_stream() {
        // The two-argument shape exists only to encode this precedence, so a
        // swap must fail here rather than silently archive a warning as a
        // version.
        assert_eq!(
            first_line("from stdout", "from stderr"),
            Some("from stdout")
        );
        assert_eq!(first_line("\n  \nreal line\n", ""), Some("real line"));
        // Some tools report only on stderr, which is why a fallback exists.
        assert_eq!(first_line("   ", "from stderr"), Some("from stderr"));
        assert_eq!(first_line("", ""), None);
    }

    #[test]
    fn failure_reason_passes_through_the_tools_own_text() {
        // The probe runs under sh, so a missing Go toolchain and a binary
        // missing from PATH both arrive as a non-zero exit. Only the tool's
        // text tells them apart, so it must survive verbatim.
        let missing_go = no_module_version_reason(false, "", "sh: go: command not found");
        assert!(missing_go.contains("command not found"), "{missing_go}");

        let missing_binary =
            no_module_version_reason(false, "", "stat : no such file or directory");
        assert!(
            missing_binary.contains("no such file or directory"),
            "{missing_binary}"
        );

        // Go writing its diagnostic to stdout must not read as "no output".
        let on_stdout = no_module_version_reason(false, "some diagnostic", "");
        assert!(on_stdout.contains("some diagnostic"), "{on_stdout}");
    }

    #[test]
    fn failure_reason_distinguishes_a_binary_without_build_info() {
        let reason = no_module_version_reason(true, "", "");
        assert!(
            reason.contains("no released module version"),
            "a zero-exit probe with no mod line is not a probe failure: {reason}"
        );
    }

    #[test]
    fn module_version_comes_only_from_the_main_module_line() {
        // `go version -m` writes nothing to stdout for a non-Go binary, and
        // go_module_version discards stderr, so empty input is the only shape
        // the parser sees then.
        for (name, output, expected) in [
            (
                // Real output: every dep line after the mod line is ignored.
                "main module line",
                "/usr/local/bin/dregsy: go1.26.2\n\
                 \tpath\tgithub.com/xelalexv/dregsy/cmd/dregsy\n\
                 \tmod\tgithub.com/xelalexv/dregsy\tv0.0.0-20250317074629-e92e79a50145\th1:abc=\n\
                 \tdep\tgithub.com/sirupsen/logrus\tv1.9.3\th1:def=\n",
                Some("v0.0.0-20250317074629-e92e79a50145"),
            ),
            (
                // Go stamps (devel) for a working-tree build. It names no
                // released artifact, so recording it would give a run's
                // provenance the same authority as a real version.
                "locally built binary",
                "/usr/local/bin/dregsy: go1.26.2\n\
                 \tmod\tgithub.com/xelalexv/dregsy\t(devel)\t\n",
                None,
            ),
            ("empty output", "", None),
            (
                // Dependencies must not be mistaken for the module version.
                "dependencies only",
                "/usr/local/bin/x: go1.26.2\n\
                 \tdep\tgithub.com/sirupsen/logrus\tv1.9.3\th1:def=\n",
                None,
            ),
        ] {
            assert_eq!(module_version_from_build_info(output), expected, "{name}");
        }
    }

    #[test]
    fn ocync_is_probed_at_the_binary_the_run_executes() {
        // A PATH lookup would find the ocync installed when the instance was
        // built, which can be months older than the one just compiled, and
        // the record would credit it with HEAD's numbers.
        let root = Path::new("/w");
        assert_eq!(
            tool_path(Tool::Ocync, root),
            "/w/target/release/ocync",
            "ocync must resolve to the workspace build, matching run_tool"
        );
        assert_eq!(tool_path(Tool::Dregsy, root), "dregsy");
        assert_eq!(tool_path(Tool::Regsync, root), "regsync");
    }

    #[test]
    fn the_baseline_is_ocyncs_binary_in_its_own_checkout_under_its_own_name() {
        // Catches: the baseline resolving to the current build, which would
        // compare ocync with itself, and sharing ocync's name, which would let
        // its config and proxy log overwrite ocync's.
        let names: BTreeSet<String> = [
            Tool::Ocync,
            Tool::OcyncBaseline,
            Tool::Dregsy,
            Tool::Regsync,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(Tool::OcyncBaseline.to_string(), "ocync-baseline");
        assert_eq!(Tool::OcyncBaseline.binary(), "ocync");
        assert_eq!(
            tool_path(Tool::OcyncBaseline, Path::new("/b")),
            "/b/target/release/ocync"
        );
        assert!(Tool::Ocync.is_ocync() && Tool::OcyncBaseline.is_ocync());
        assert!(!Tool::Dregsy.is_ocync() && !Tool::Regsync.is_ocync());
        assert!(relay_binaries(Tool::OcyncBaseline).is_empty());
    }

    #[test]
    fn a_baseline_that_rejects_the_config_or_a_flag_fails_with_its_own_message() {
        use std::os::unix::fs::PermissionsExt;

        // A stand-in for `ocync`, answering `sync --help` and `validate`.
        let baseline = |script: &str| {
            let root = tempfile::tempdir().unwrap();
            let binary = tool_path(Tool::OcyncBaseline, root.path()).into_owned();
            std::fs::create_dir_all(Path::new(&binary).parent().unwrap()).unwrap();
            std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            check_baseline_in(root.path(), Path::new("config.yaml"))
        };
        let help =
            r#"[ "$1" = sync ] && echo "  -v, --verbose  --json  --config <path>" && exit 0"#;

        assert_eq!(
            baseline(&format!("{help}\n[ \"$1\" = validate ] && exit 0")),
            Ok(())
        );
        assert_eq!(
            baseline(&format!(
                "{help}\necho \"error: unknown field auth_type\" >&2; exit 2"
            )),
            Err("the baseline build rejected the generated config: \
                 error: unknown field auth_type"
                .to_string())
        );
        // Catches: a baseline too old for the flags every round passes being
        // run, and failing in round 1 with the pre-warm already spent.
        assert_eq!(
            baseline(r#"[ "$1" = sync ] && echo "  --config <path>" && exit 0"#),
            Err("the baseline build's `sync --help` does not list --json, \
                 which every round passes"
                .to_string())
        );
    }

    #[test]
    fn the_sync_flags_are_listed_in_either_form() {
        for (help, missing) in [
            ("  -v, --verbose  Increase\n      --json  Output", vec![]),
            ("  -v  Increase\n  --json", vec![]),
            ("  --verbose\n  --json", vec![]),
            ("  --verbose-ish\n  --json", vec!["--verbose"]),
            ("  -vv\n  --json-lines", vec!["--json", "--verbose"]),
            ("", vec!["--json", "--verbose"]),
        ] {
            assert_eq!(missing_sync_flags(help), missing, "{help:?}");
        }
    }

    /// A repository with two commits, and their ids, oldest first.
    fn repo_with_two_commits() -> (tempfile::TempDir, [CommitSha; 2]) {
        use super::super::remote::git_in;

        let repo = tempfile::tempdir().unwrap();
        git_in(repo.path(), &["init", "-q", "-b", "main"]);
        let mut commits = Vec::new();
        for message in ["one", "two"] {
            git_in(
                repo.path(),
                &["commit", "-q", "--allow-empty", "-m", message],
            );
            commits.push(resolve_commit(repo.path(), "HEAD").unwrap());
        }
        let [first, second] = <[CommitSha; 2]>::try_from(commits).unwrap();
        (repo, [first, second])
    }

    #[test]
    fn a_checkout_is_reused_only_at_its_commit_and_clean_and_alone() {
        use super::super::remote::git_in;

        let (repo, [first, second]) = repo_with_two_commits();
        let repo = repo.path();
        let dir = |commit: &CommitSha| repo.join(baseline_dir(commit));
        // The checkouts are git's own, so git's identity must not be needed.
        let ensure = |commit: &CommitSha| ensure_checkout(repo, commit);

        assert_eq!(ensure(&first), Ok(()));
        assert!(dir(&first).join(".git").exists());
        assert_eq!(ensure(&first), Ok(()), "a second run reuses it");

        // Catches: reusing a checkout someone moved off its commit.
        git_in(
            &dir(&first),
            &["checkout", "-q", "--detach", &second.to_string()],
        );
        let err = ensure(&first).unwrap_err();
        assert!(
            err.contains(&format!("is at {second}, not the baseline {first}")),
            "{err}"
        );
        git_in(
            &dir(&first),
            &["checkout", "-q", "--detach", &first.to_string()],
        );

        // Catches: benchmarking a tree with local changes as if it were the
        // commit.
        std::fs::write(dir(&first).join("edited"), "x").unwrap();
        let err = ensure(&first).unwrap_err();
        assert!(err.contains("has local changes"), "{err}");
        std::fs::remove_file(dir(&first).join("edited")).unwrap();
        assert_eq!(ensure(&first), Ok(()));

        // Catches: checkouts piling up, one build's worth of disk each.
        assert_eq!(ensure(&second), Ok(()));
        assert!(dir(&second).join(".git").exists());
        assert!(!dir(&first).exists(), "the other checkout was kept");

        // Catches: a stray file or an unregistered directory in the checkout
        // directory failing the build instead of being cleared.
        let root = repo.join(BASELINE_ROOT);
        std::fs::write(root.join("stray"), "x").unwrap();
        std::fs::create_dir_all(root.join("leftover").join("nested")).unwrap();
        assert_eq!(ensure(&second), Ok(()));
        assert!(!root.join("stray").exists() && !root.join("leftover").exists());
        assert!(
            dir(&second).join(".git").exists(),
            "the wanted one was cleared"
        );
    }

    #[test]
    fn a_checkout_deleted_behind_gits_back_is_made_again() {
        let (repo, [first, _]) = repo_with_two_commits();
        let repo = repo.path();
        ensure_checkout(repo, &first).unwrap();
        std::fs::remove_dir_all(repo.join(baseline_dir(&first))).unwrap();

        assert_eq!(ensure_checkout(repo, &first), Ok(()));
        assert!(repo.join(baseline_dir(&first)).join(".git").exists());
    }

    #[test]
    fn relays_cover_every_binary_that_moves_a_tools_numbers() {
        // dregsy delegates every transfer to skopeo, and both Go tools reach
        // ECR through the credential helper. ocync calls the AWS SDK directly.
        assert_eq!(
            relay_binaries(Tool::Dregsy),
            &["skopeo", "docker-credential-ecr-login"]
        );
        assert_eq!(
            relay_binaries(Tool::Regsync),
            &["docker-credential-ecr-login"]
        );
        assert!(relay_binaries(Tool::Ocync).is_empty());
    }

    #[test]
    fn a_placeholder_reporting_relay_still_needs_its_module_version() {
        // The credential helper exits zero and prints "development" whatever
        // it was built from, so the probe alone would record that as a version
        // and the module lookup must take precedence.
        let probed = version_from_probe(
            "docker-credential-ecr-login",
            false,
            true,
            "docker-credential-ecr-login (github.com/awslabs/amazon-ecr-credential-helper/ecr-login) development\n",
            "",
        )
        .unwrap();

        assert!(
            probed.is_some_and(|v| v.contains("development")),
            "probe is expected to yield a placeholder, which is why relays prefer the module version"
        );
    }

    #[test]
    fn successful_probe_reads_the_version() {
        assert_eq!(
            version_from_probe("ocync", true, true, "ocync 0.6.0\n", "").unwrap(),
            Some("ocync 0.6.0".to_string())
        );
        // regsync pads its output and precedes the tag with a blank line.
        assert_eq!(
            version_from_probe("regsync", true, true, "\nVCSTag:     v0.11.5\n", "").unwrap(),
            Some("VCSTag:     v0.11.5".to_string())
        );
        // skopeo reports cleanly on stdout with a zero exit.
        assert_eq!(
            version_from_probe("skopeo", true, true, "skopeo version 1.24.0\n", "").unwrap(),
            Some("skopeo version 1.24.0".to_string())
        );
    }

    #[test]
    fn successful_probe_falls_back_to_stderr() {
        assert_eq!(
            version_from_probe("ocync", true, true, "   ", "ocync 0.6.0").unwrap(),
            Some("ocync 0.6.0".to_string())
        );
    }

    #[test]
    fn successful_probe_with_no_output_reports_nothing() {
        assert_eq!(
            version_from_probe("ocync", true, true, "", "").unwrap(),
            None
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
    fn parse_gnu_time_rss_reads_the_maximum_resident_set_line() {
        for (name, stderr, expected) in [
            (
                "gnu time -v output",
                "\tCommand being timed: \"./target/release/ocync\"\n\
                 \tMaximum resident set size (kbytes): 59468\n\
                 \tExit status: 0\n",
                Some(59468),
            ),
            (
                "no time info",
                "error: some tool output\nno time info here\n",
                None,
            ),
            ("empty", "", None),
        ] {
            assert_eq!(parse_gnu_time_rss(stderr), expected, "{name}");
        }
    }
}
