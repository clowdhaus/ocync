//! CLI infrastructure: logging, output formatting, and signal handling.

pub(crate) mod commands;
pub(crate) mod config;
#[allow(dead_code)]
pub(crate) mod output;
pub(crate) mod shutdown;

use tracing_subscriber::{EnvFilter, fmt};

use crate::{Cli, LogFormat};

/// Initialize the tracing subscriber based on CLI flags and environment.
///
/// Priority: `RUST_LOG` env var > CLI flags > auto-detection (Kubernetes → JSON).
/// Sensitive HTTP transport crates are capped at `warn` to prevent credential
/// leakage in request/response headers at trace level.
pub(crate) fn setup_logging(cli: &Cli) {
    let detected = detect_log_format();
    let format = cli.log_format.or(detected).unwrap_or(LogFormat::Text);

    let filter = verbosity_filter(cli.quiet, cli.verbose);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    // Cap HTTP transport crates to prevent credential leakage at trace level.
    // These are appended unconditionally so they apply even when RUST_LOG is set.
    let env_filter = env_filter
        .add_directive("hyper=warn".parse().unwrap())
        .add_directive("h2=warn".parse().unwrap())
        .add_directive("reqwest=warn".parse().unwrap())
        .add_directive("rustls=warn".parse().unwrap())
        .add_directive("tower=warn".parse().unwrap());

    match format {
        LogFormat::Json => {
            fmt()
                .json()
                .with_env_filter(env_filter)
                .with_target(false)
                .init();
        }
        LogFormat::Text => {
            fmt().with_env_filter(env_filter).with_target(false).init();
        }
    }
}

/// Map CLI verbosity flags to a tracing filter directive.
fn verbosity_filter(quiet: bool, verbose: u8) -> &'static str {
    if quiet {
        "error"
    } else {
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    }
}

/// Auto-detect log format from the environment.
///
/// Returns `Some(LogFormat::Json)` when running inside Kubernetes
/// (detected via `KUBERNETES_SERVICE_HOST`).
fn detect_log_format() -> Option<LogFormat> {
    if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
        Some(LogFormat::Json)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_quiet_is_error() {
        assert_eq!(verbosity_filter(true, 0), "error");
        assert_eq!(verbosity_filter(true, 3), "error");
    }

    #[test]
    fn verbosity_default_is_info() {
        assert_eq!(verbosity_filter(false, 0), "info");
    }

    #[test]
    fn verbosity_one_is_debug() {
        assert_eq!(verbosity_filter(false, 1), "debug");
    }

    #[test]
    fn verbosity_two_plus_is_trace() {
        assert_eq!(verbosity_filter(false, 2), "trace");
        assert_eq!(verbosity_filter(false, 3), "trace");
    }

    #[test]
    fn detect_log_format_no_kubernetes() {
        // Remove the env var if set, check detection.
        let original = std::env::var("KUBERNETES_SERVICE_HOST").ok();
        // SAFETY: test is single-threaded and restores the original value.
        unsafe { std::env::remove_var("KUBERNETES_SERVICE_HOST") };
        assert!(detect_log_format().is_none());
        if let Some(val) = original {
            // SAFETY: restoring original value.
            unsafe { std::env::set_var("KUBERNETES_SERVICE_HOST", val) };
        }
    }
}
