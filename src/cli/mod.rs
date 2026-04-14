//! CLI infrastructure: logging, output formatting, and signal handling.

pub(crate) mod commands;
pub(crate) mod config;
pub(crate) mod output;
pub(crate) mod progress;
pub(crate) mod shutdown;

use ocync_distribution::RegistryClient;
use ocync_distribution::auth::anonymous::AnonymousAuth;
use ocync_distribution::auth::detect::{ProviderKind, detect_provider_kind};
use ocync_distribution::auth::ecr::EcrAuth;
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;

use crate::cli::config::AuthType;
use crate::{Cli, LogFormat};

// ---------------------------------------------------------------------------
// Exit codes (grep/POSIX convention)
// ---------------------------------------------------------------------------

/// Process exit codes following the grep/POSIX convention.
///
/// - `Success` (0): all images synced or skipped
/// - `PartialFailure` (1): some images failed, some succeeded
/// - `Failure` (2): all images failed, or unclassified error
/// - `ConfigError` (3): invalid configuration file
/// - `AuthError` (4): authentication or authorization failure
///
/// Codes 3-4 are more specific than 2 (not more severe). Callers
/// can branch: `>= 3` means the fix is in config/credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCode {
    /// All images synced or skipped successfully.
    Success,
    /// Some images failed, some succeeded or were skipped.
    PartialFailure,
    /// All images failed, or unclassified program error.
    Failure,
    /// Invalid configuration file (parse, validation, env vars).
    ConfigError,
    /// Authentication or authorization failure.
    AuthError,
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        match code {
            ExitCode::Success => Self::from(0),
            ExitCode::PartialFailure => Self::from(1),
            ExitCode::Failure => Self::from(2),
            ExitCode::ConfigError => Self::from(3),
            ExitCode::AuthError => Self::from(4),
        }
    }
}

// ---------------------------------------------------------------------------
// CLI error type
// ---------------------------------------------------------------------------

/// Errors that can occur during CLI command execution.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// Configuration file errors (parse, validation, env vars).
    #[error("{0}")]
    Config(#[from] config::ConfigError),

    /// Registry/network operation errors.
    #[error("{0}")]
    Registry(#[from] ocync_distribution::Error),

    /// Tag filter pipeline errors.
    #[error("{0}")]
    Filter(#[from] ocync_sync::Error),

    /// Invalid user input.
    #[error("{0}")]
    Input(String),
}

impl CliError {
    /// Map this error to the appropriate process exit code.
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::ConfigError,
            Self::Registry(e) if e.is_auth_error() => ExitCode::AuthError,
            _ => ExitCode::Failure,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared auth provider builder
// ---------------------------------------------------------------------------

/// Strip scheme and trailing slashes from a registry URL to get a bare hostname.
///
/// Used to normalize URLs for comparison across config values and parsed references.
pub(crate) fn bare_hostname(s: &str) -> &str {
    s.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// Build a [`RegistryClient`] for the given hostname, using the appropriate
/// auth provider based on explicit `auth_type` or hostname auto-detection.
///
/// When `max_concurrent` is `Some`, the per-registry aggregate concurrency
/// cap is set to that value; otherwise the client default applies.
pub(crate) async fn build_registry_client(
    hostname: &str,
    auth_type: Option<&AuthType>,
    max_concurrent: Option<usize>,
) -> Result<RegistryClient, CliError> {
    let base_url = if hostname.starts_with("http://") || hostname.starts_with("https://") {
        hostname.to_string()
    } else {
        format!("https://{hostname}")
    };

    let url = Url::parse(&base_url)
        .map_err(|e| CliError::Input(format!("invalid registry URL '{base_url}': {e}")))?;

    let bare_host = bare_hostname(hostname);

    let provider_kind = match auth_type {
        Some(AuthType::Ecr) => Some(ProviderKind::Ecr),
        Some(AuthType::Anonymous) => None,
        Some(unsupported) => {
            tracing::warn!(
                auth_type = ?unsupported,
                registry = bare_host,
                "auth type not yet implemented, falling back to anonymous auth"
            );
            None
        }
        None => detect_provider_kind(bare_host),
    };

    let mut builder = match provider_kind {
        Some(ProviderKind::Ecr | ProviderKind::EcrPublic) => {
            let auth = EcrAuth::new(bare_host)
                .await
                .map_err(|e| CliError::Input(format!("ECR auth setup for '{bare_host}': {e}")))?;
            RegistryClient::builder(url).auth(auth)
        }
        _ => {
            let http = reqwest::Client::new();
            let auth = AnonymousAuth::new(bare_host, http);
            RegistryClient::builder(url).auth(auth)
        }
    };

    if let Some(n) = max_concurrent {
        builder = builder.max_concurrent(n);
    }

    builder
        .build()
        .map_err(|e| CliError::Input(format!("failed to build client for '{bare_host}': {e}")))
}

// ---------------------------------------------------------------------------
// Logging setup
// ---------------------------------------------------------------------------

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
        let original = std::env::var("KUBERNETES_SERVICE_HOST").ok();
        // SAFETY: test is single-threaded and restores the original value.
        unsafe { std::env::remove_var("KUBERNETES_SERVICE_HOST") };
        assert!(detect_log_format().is_none());
        if let Some(val) = original {
            // SAFETY: restoring original value.
            unsafe { std::env::set_var("KUBERNETES_SERVICE_HOST", val) };
        }
    }

    #[test]
    fn exit_code_all_variants_distinct() {
        assert_ne!(ExitCode::Success, ExitCode::PartialFailure);
        assert_ne!(ExitCode::PartialFailure, ExitCode::Failure);
        assert_ne!(ExitCode::Failure, ExitCode::ConfigError);
        assert_ne!(ExitCode::ConfigError, ExitCode::AuthError);
    }

    #[test]
    fn cli_error_exit_code_config() {
        let err = CliError::Config(config::ConfigError::Parse("bad".into()));
        assert_eq!(err.exit_code(), ExitCode::ConfigError);
    }

    #[test]
    fn cli_error_exit_code_auth_failed() {
        let err = CliError::Registry(ocync_distribution::Error::AuthFailed {
            registry: "example.com".into(),
            reason: "denied".into(),
        });
        assert_eq!(err.exit_code(), ExitCode::AuthError);
    }

    #[test]
    fn cli_error_exit_code_registry_401() {
        let err = CliError::Registry(ocync_distribution::Error::RegistryError {
            status: http::StatusCode::UNAUTHORIZED,
            message: "no".into(),
        });
        assert_eq!(err.exit_code(), ExitCode::AuthError);
    }

    #[test]
    fn cli_error_exit_code_registry_403() {
        let err = CliError::Registry(ocync_distribution::Error::RegistryError {
            status: http::StatusCode::FORBIDDEN,
            message: "denied".into(),
        });
        assert_eq!(err.exit_code(), ExitCode::AuthError);
    }

    #[test]
    fn cli_error_exit_code_registry_500() {
        let err = CliError::Registry(ocync_distribution::Error::RegistryError {
            status: http::StatusCode::INTERNAL_SERVER_ERROR,
            message: "broke".into(),
        });
        assert_eq!(err.exit_code(), ExitCode::Failure);
    }

    #[test]
    fn cli_error_exit_code_input() {
        let err = CliError::Input("bad url".into());
        assert_eq!(err.exit_code(), ExitCode::Failure);
    }

    #[test]
    fn cli_error_exit_code_filter() {
        let err = CliError::Filter(ocync_sync::Error::LatestWithoutSort);
        assert_eq!(err.exit_code(), ExitCode::Failure);
    }

    #[test]
    fn bare_hostname_strips_https() {
        assert_eq!(
            bare_hostname("https://registry.example.com"),
            "registry.example.com"
        );
    }

    #[test]
    fn bare_hostname_strips_http() {
        assert_eq!(bare_hostname("http://localhost:5000"), "localhost:5000");
    }

    #[test]
    fn bare_hostname_strips_trailing_slash() {
        assert_eq!(
            bare_hostname("https://registry.example.com/"),
            "registry.example.com"
        );
    }

    #[test]
    fn bare_hostname_noop_for_bare_host() {
        assert_eq!(
            bare_hostname("registry.example.com"),
            "registry.example.com"
        );
    }

    #[test]
    fn bare_hostname_preserves_port() {
        assert_eq!(bare_hostname("localhost:5000"), "localhost:5000");
    }
}
