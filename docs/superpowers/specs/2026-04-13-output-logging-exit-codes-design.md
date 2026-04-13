# Output, Logging, and Exit Codes

Goal: implement "silence means success" — the key UX differentiator vs dregsy/skopeo. At default verbosity, a fully up-to-date sync produces one summary line. Failures are always visible.

## Current state

- `crates/ocync-sync/src/progress.rs` — `ProgressReporter` trait with `NullProgress` only
- `src/cli/mod.rs` — `ExitCode::Success` (0), `Failure` (1), `Error` (2); all `Err` variants map to code 2
- `src/cli/output.rs` — URL redaction only
- `src/cli/commands/synchronize.rs` — uses `NullProgress`; has `format_bytes` and `print_summary`
- `src/cli/commands/copy.rs` — uses `NullProgress`; manual status printing
- `crates/ocync-sync/src/lib.rs` — `ImageStatus::Failed { error: String, retries: u32 }` (no typed error kind)

## Design: verbosity levels

| Level | stderr (tracing) | stdout | What shows |
|-------|-----------------|--------|------------|
| `-q` | error | nothing | Zero output on success. Errors on stderr via tracing. NullProgress used. |
| default | info | summary | Failed images on stderr + summary on stdout. Skipped/synced images SILENT. |
| `-v` | debug | summary | Above + synced images (bytes, duration) + skipped (reason) on stderr. |
| `-vv` | trace | summary | Above + per-blob operations via tracing (HEAD, mount, push). |
| `-vvv` | trace | summary | Above + HTTP headers/bodies via tracing (auth redacted by HTTP crate caps). |

Key principle: per-image lines go to stderr (interleaved with tracing). Summary goes to stdout (pipeable). `--json` adds JSON report to stdout; TextProgress still writes per-image lines to stderr.

## A. ErrorKind on ImageStatus::Failed

### What changes

`crates/ocync-sync/src/lib.rs`:

```rust
/// Classification of the operation that failed during image transfer.
///
/// Classifies *what operation* failed, not *why* it failed. The `error`
/// string on `ImageStatus::Failed` carries the cause. This separation lets
/// output formatters group/style by operation type while preserving the
/// full error context.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Source manifest could not be pulled.
    ManifestPull,
    /// Target manifest could not be pushed.
    ManifestPush,
    /// Blob transfer (pull, push, or mount) failed.
    BlobTransfer,
}
```

Add `Display` impl: `"manifest pull"`, `"manifest push"`, `"blob transfer"`.

`ImageStatus::Failed` becomes:

```rust
Failed {
    /// What operation failed.
    kind: ErrorKind,
    /// Error message describing the failure.
    error: String,
    /// Number of retry attempts made.
    retries: u32,
}
```

### Why 3 variants, not 5

The original plan proposed `Auth` and `Other`. Neither has a construction site. Auth failures during transfer surface as HTTP 401/403 inside the operation-level errors — an auth failure during manifest pull is `ErrorKind::ManifestPull` with an error string mentioning "401 Unauthorized". Adding unused variants violates scope discipline.

### Construction sites (engine.rs)

| Site | Line | Operation | ErrorKind |
|------|------|-----------|-----------|
| Source pull failed | ~563 | Discovery pulls source manifest | `ManifestPull` |
| Blob transfer failed | ~701 | Execution transfers blobs | `BlobTransfer` |
| Manifest push failed | ~746 | Execution pushes manifests | `ManifestPush` |

### Blast radius

8 mechanical updates:
- 3 construction sites in engine.rs (add `kind` field)
- 3 test constructions in lib.rs (`make_report` test helper calls)
- 1 test helper in engine.rs (`make_image_result`)
- 1 pattern match in copy.rs (`Failed { error, retries }` → `Failed { error, retries, .. }` or explicit `kind`)

9 other pattern matches across engine.rs and engine_integration.rs already use `{ .. }` and auto-adapt.

### JSON output change

The JSON serialization of `SyncReport` gains a `kind` field on failed images:

```json
{
  "status": "failed",
  "kind": "manifest_pull",
  "error": "registry returned 404: not found",
  "retries": 3
}
```

Pre-v1, no backward compatibility concern.

## B. Exit codes

### What changes

`src/cli/mod.rs` — rename existing variants for clarity, add 2 new:

```rust
pub(crate) enum ExitCode {
    /// All images synced or skipped successfully.
    Success,          // 0
    /// Some images failed, some succeeded or were skipped.
    PartialFailure,   // 1
    /// All images failed, or unclassified program error.
    Failure,          // 2
    /// Invalid configuration file (parse, validation, env vars).
    ConfigError,      // 3
    /// Authentication or authorization failure.
    AuthError,        // 4
}
```

Codes 3 and 4 are more specific than 2, not more severe. CI scripts can branch: `>= 3` means the fix is in config/credentials, not in the images.

### Error classification

Add `exit_code()` method on `CliError`:

```rust
impl CliError {
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::ConfigError,
            Self::Registry(e) if e.is_auth_error() => ExitCode::AuthError,
            _ => ExitCode::Failure,
        }
    }
}
```

Add `is_auth_error()` on `ocync_distribution::Error`:

```rust
pub fn is_auth_error(&self) -> bool {
    match self {
        Self::AuthFailed { .. } => true,
        Self::RegistryError { status, .. } => {
            *status == http::StatusCode::UNAUTHORIZED
                || *status == http::StatusCode::FORBIDDEN
        }
        _ => false,
    }
}
```

`CliError::Input` maps to code 2 (Failure) — it covers diverse errors (bad URL, missing tag, ECR setup) that don't cleanly fit config or auth categories.

### main.rs wiring

```rust
Err(err) => {
    eprintln!("error: {err}");
    err.exit_code().into()
}
```

### SyncReport mapping

`SyncReport::exit_code()` stays as `i32` (sync crate doesn't know CLI types). The CLI maps:

```rust
match report.exit_code() {
    0 => Ok(ExitCode::Success),
    1 => Ok(ExitCode::PartialFailure),
    _ => Ok(ExitCode::Failure),
}
```

### Existing command updates

All commands that currently use `ExitCode::Failure` or `ExitCode::Error` are updated to use the renamed variants. The auth check command continues returning `PartialFailure` (1) when any registry ping fails — refining it to distinguish auth vs network failures is out of scope.

## C. TextProgress reporter

### Architecture decision: CLI crate, not sync crate

`TextProgress` is presentation logic. The sync crate defines the `ProgressReporter` trait contract; the CLI crate provides the implementation. This matches the existing pattern: `NullProgress` (library default) vs `TextProgress` (CLI presentation).

### What changes

`src/cli/progress.rs` (new file):

```rust
use std::cell::RefCell;
use std::io::{self, Write};
use std::time::Duration;

use ocync_sync::progress::ProgressReporter;
use ocync_sync::{ImageResult, ImageStatus, SyncReport};

use crate::cli::output::{format_bytes, format_duration};

/// Verbosity-aware text progress reporter.
///
/// Per-image status lines go to stderr (alongside tracing logs).
/// The run summary goes to stdout (pipeable, parseable).
///
/// Uses `RefCell` for interior mutability because the `ProgressReporter`
/// trait takes `&self` and the engine runs on a single-threaded runtime.
pub(crate) struct TextProgress {
    verbosity: u8,
    stderr: RefCell<Box<dyn Write>>,
    stdout: RefCell<Box<dyn Write>>,
}
```

Constructor: `TextProgress::new(verbosity)` uses real stderr/stdout. `TextProgress::with_writers(verbosity, stderr, stdout)` for testing with `Vec<u8>` buffers.

### image_started

No-op at all verbosity levels. Tracing handles debug-level start events.

### image_completed

| Verbosity | Failed | Synced | Skipped |
|-----------|--------|--------|---------|
| 0 (default) | stderr | silent | silent |
| 1+ (-v) | stderr | stderr | stderr |

Format (stderr):
```
FAILED  source -> target  (manifest push: connection refused)
synced  source -> target  (187 MB, 14s)
skipped source -> target  (digest match)
```

UPPERCASE for failures (visual pop). Lowercase for success states.

### run_completed

Summary to stdout (always, unless zero images):
```
sync complete: 3 synced, 47 skipped, 1 failed | blobs: 12 transferred, 34 mounted | 432 MB in 47s
```

## D. Formatting helpers

### What changes

`src/cli/output.rs`:

Move `format_bytes` from `synchronize.rs` → `output.rs` as `pub(crate)`. Existing tests move with it.

Add `format_duration`:

```rust
/// Format a duration as a human-readable string.
///
/// - Sub-second: `"0.3s"`
/// - Seconds: `"47s"`
/// - Minutes: `"2m 13s"`
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}
```

## E. Command wiring

### synchronize.rs

- `run()` gains `progress: &dyn ProgressReporter` parameter — `main.rs` constructs the right reporter based on quiet/verbose and passes it in
- Remove `print_summary()` function entirely — `TextProgress::run_completed` handles it
- `write_output()` becomes JSON-only: only writes when `args.json` is true, otherwise no-op
- Update `ExitCode` variant names in the `SyncReport::exit_code()` mapping

### copy.rs

- `run()` gains `progress: &dyn ProgressReporter` parameter
- Remove manual status printing (lines 72-85) — `TextProgress::image_completed` handles it

### watch.rs

- `synchronize::run()` receives the progress reporter; watch passes it through

### main.rs

- Construct `TextProgress::new(cli.verbose)` or `NullProgress` based on `cli.quiet`
- Pass `&progress` to command handlers
- `Err` arm uses `err.exit_code()` instead of hardcoded `ExitCode::Error`

## F. Tests

### Unit tests

**ErrorKind:**
- Display for all 3 variants
- At least one integration test per variant asserts correct discriminant

**format_duration:**
- `Duration::from_millis(300)` → `"0.3s"`
- `Duration::from_secs(5)` → `"5s"`
- `Duration::from_secs(47)` → `"47s"`
- `Duration::from_secs(60)` → `"1m 0s"`
- `Duration::from_secs(133)` → `"2m 13s"`

**format_bytes:**
- Existing tests move from synchronize.rs to output.rs

**Exit codes:**
- `ExitCode::ConfigError` → 3
- `ExitCode::AuthError` → 4
- `CliError::exit_code()`: Config→3, Registry(AuthFailed)→4, Registry(RegistryError 401)→4, Registry(RegistryError 500)→2, Input→2, Filter→2
- `is_auth_error()`: AuthFailed→true, 401→true, 403→true, 500→false, Other→false

**TextProgress output capture (using Vec<u8> writers):**
- Verbosity 0 + Failed → stderr contains `"FAILED"`
- Verbosity 0 + Synced → stderr empty
- Verbosity 0 + Skipped → stderr empty
- Verbosity 1 + Synced → stderr contains `"synced"`
- Verbosity 1 + Skipped → stderr contains `"skipped"`
- `run_completed` → stdout contains `"sync complete:"`
- Empty report (0 images) → stdout empty (no summary)

### Integration test updates

All `ImageStatus::Failed { .. }` patterns in engine_integration.rs: mechanical `kind` field additions. Existing `{ .. }` patterns auto-adapt. At least one test per ErrorKind variant:

- Manifest pull failure test (e.g., source 404) → assert `ErrorKind::ManifestPull`
- Blob transfer failure test (e.g., blob push 500) → assert `ErrorKind::BlobTransfer`
- Manifest push failure test (e.g., target manifest PUT 500) → assert `ErrorKind::ManifestPush`

## Out of scope

- Watch command always returning Success on cycle errors — watch behavior bug, not output/logging
- Drain deadline abandoned transfers — shutdown concern, separate PR
- Auth check command distinguishing auth vs network failures — separate concern
- Progress bars (indicatif) — PR #19 per the plan
