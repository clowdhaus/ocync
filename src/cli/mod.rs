//! CLI infrastructure: logging, output formatting, and signal handling.

pub(crate) mod commands;
pub(crate) mod config;
pub(crate) mod health;
pub(crate) mod output;
pub(crate) mod progress;
pub(crate) mod shutdown;

#[cfg(test)]
mod auth_dispatch_tests;

use ocync_distribution::auth::Credentials;
use ocync_distribution::auth::acr::AcrAuth;
use ocync_distribution::auth::configured;
use ocync_distribution::auth::detect::{ProviderKind, detect_provider_kind};
use ocync_distribution::auth::docker::DockerConfig;
use ocync_distribution::auth::ecr::EcrAuth;
use ocync_distribution::auth::ecr_public::EcrPublicAuth;
use ocync_distribution::auth::gcp::{GcpAuth, SharedGoogleToken};
use ocync_distribution::auth::static_token::StaticTokenAuth;
use ocync_distribution::{AwsCredentialProviders, RegistryClient};
use ocync_sync::RunOutcome;

use std::sync::Once;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing_subscriber::{EnvFilter, fmt};
use url::Url;

use crate::cli::config::{AuthType, RegistryConfig};
use crate::{Cli, LogFormat};

/// Gates the `auth_type: ghcr` deprecation warning to once per process, not
/// once per `build_registry_client` call, which would spam `watch` logs.
static GHCR_DEPRECATION_ONCE: Once = Once::new();

/// Counts `Once` body runs so tests can assert the warn fires at most once
/// without a tracing subscriber harness.
#[cfg(test)]
pub(super) static GHCR_DEPRECATION_FIRES: AtomicUsize = AtomicUsize::new(0);

/// Emit the `auth_type: ghcr` deprecation warning at most once per process.
fn warn_ghcr_deprecation_once(host: &str) {
    GHCR_DEPRECATION_ONCE.call_once(|| {
        tracing::warn!(
            registry = host,
            "auth_type: ghcr is deprecated; use auth_type: docker_config or auth_type: static_token instead"
        );
        #[cfg(test)]
        GHCR_DEPRECATION_FIRES.fetch_add(1, Ordering::Relaxed);
    });
}

/// Process exit codes following the grep/POSIX convention: 0 success, 1 partial
/// failure, 2 failure, 3 config error, 4 auth error.
///
/// Codes 3-4 are more specific than 2, not more severe: `>= 3` means the fix
/// is in config or credentials.
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

impl ExitCode {
    /// Map a [`SyncReport`](ocync_sync::SyncReport) outcome to a CLI exit code.
    pub(crate) fn from_report(outcome: RunOutcome) -> Self {
        match outcome {
            RunOutcome::Clean => Self::Success,
            RunOutcome::Partial => Self::PartialFailure,
            RunOutcome::AllFailed => Self::Failure,
        }
    }
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

    /// Credential resolution failed for a registry.
    ///
    /// Distinct from [`Self::Input`] so that a run whose only failures were
    /// denials exits with the dedicated auth code. The provider crates return
    /// their own error types here, so the cause arrives already formatted.
    #[error("{0}")]
    Auth(String),
}

impl CliError {
    /// Map this error to the appropriate process exit code.
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::ConfigError,
            Self::Filter(_) => ExitCode::ConfigError,
            Self::Registry(e) if e.is_auth_error() => ExitCode::AuthError,
            Self::Auth(_) => ExitCode::AuthError,
            _ => ExitCode::Failure,
        }
    }
}

/// Strip scheme and trailing slashes from a registry URL to get a bare hostname,
/// for comparing config values with parsed references.
pub(crate) fn bare_hostname(s: &str) -> &str {
    s.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// Map a registry hostname to the host serving its v2 API.
///
/// Docker Hub's canonical `docker.io` appears in references and token scopes,
/// but the v2 API is at `registry-1.docker.io`; bare `docker.io` redirects to
/// the marketing site. Only the HTTP endpoint is rewritten: auth scope, token
/// service and [`bare_hostname`] stay `docker.io`, which its token server
/// expects.
pub(crate) fn endpoint_host(hostname: &str) -> &str {
    match hostname {
        "docker.io" => "registry-1.docker.io",
        other => other,
    }
}

/// The registry a client is built for.
#[derive(Clone, Copy)]
pub(crate) enum Registry<'a> {
    /// A registry entry from the config file: its `url`, scheme included,
    /// and its auth settings.
    Configured(&'a RegistryConfig),
    /// A bare host from an image reference that no config entry matches.
    /// Reached over HTTPS with auth auto-detected from the host.
    Host(&'a str),
}

impl<'a> Registry<'a> {
    /// The entry in `registries` for an image reference's registry `host`,
    /// matched after the Docker Hub endpoint rewrite, else the bare host.
    pub(crate) fn resolve(registries: &'a [RegistryConfig], host: &'a str) -> Self {
        let host = bare_hostname(host);
        let endpoint = endpoint_host(host);
        registries
            .iter()
            .find(|r| endpoint_host(bare_hostname(&r.url)) == endpoint)
            .map_or(Self::Host(host), Self::Configured)
    }

    /// The config entry, when the registry has one.
    pub(crate) fn config(self) -> Option<&'a RegistryConfig> {
        match self {
            Self::Configured(config) => Some(config),
            Self::Host(_) => None,
        }
    }
}

/// The credentials one run's clients share: AWS credentials by profile and
/// region, and Google's Application Default Credentials. Create one per run:
/// the next run, such as the next watch cycle, loads them afresh.
#[derive(Debug, Default)]
pub(crate) struct RunCredentials {
    pub(crate) aws: AwsCredentialProviders,
    pub(crate) gcp: SharedGoogleToken,
}

/// Build a [`RegistryClient`] for `registry`, using the appropriate auth
/// provider based on explicit `auth_type` or hostname auto-detection. Cloud
/// providers take their credentials from `credentials`, which one run shares.
pub(crate) async fn build_registry_client(
    registry: Registry<'_>,
    credentials: &RunCredentials,
) -> Result<RegistryClient, CliError> {
    let aws_credentials = &credentials.aws;
    // Idempotent. reqwest is built with rustls-no-provider, so a provider must
    // exist before `reqwest::Client::new()`; this covers callers bypassing `main`.
    ocync_distribution::install_crypto_provider();
    let registry_config = registry.config();
    let (bare_host, address) = match registry {
        Registry::Configured(config)
            if config.url.starts_with("http://") || config.url.starts_with("https://") =>
        {
            (bare_hostname(&config.url), config.url.clone())
        }
        Registry::Configured(RegistryConfig { url: host, .. }) => {
            (host.as_str(), format!("https://{host}"))
        }
        Registry::Host(host) => (host, format!("https://{host}")),
    };
    let mut url = Url::parse(&address)
        .map_err(|e| CliError::Input(format!("invalid registry URL '{address}': {e}")))?;
    // Requests go to the endpoint host while `bare_host` stays the auth scope
    // and service (the Docker Hub split).
    let endpoint = endpoint_host(bare_host);
    if endpoint != bare_host {
        url.set_host(Some(endpoint))
            .map_err(|e| CliError::Input(format!("invalid registry host '{endpoint}': {e}")))?;
    }

    let http = reqwest::Client::new();

    let auth_type = registry_config.and_then(|r| r.auth_type.as_ref());

    let mut builder = match auth_type {
        Some(AuthType::Ecr) => {
            let profile = registry_config.and_then(|r| r.aws_profile.as_deref());
            let auth = EcrAuth::new(bare_host, profile, aws_credentials)?;
            RegistryClient::builder(url).auth(auth)
        }
        Some(AuthType::Basic) => {
            // Config validation ensures credentials are present for Basic auth.
            let creds = registry_config
                .and_then(|r| r.credentials.as_ref())
                .expect("basic auth requires credentials (validated)");
            let auth = configured::Auth::basic(
                &url,
                http,
                Credentials::Basic {
                    username: creds.username.clone(),
                    password: creds.password.clone(),
                },
            );
            RegistryClient::builder(url).auth(auth)
        }
        Some(AuthType::StaticToken) => {
            // Config validation ensures token is present for Token auth.
            let tok = registry_config
                .and_then(|r| r.token.as_deref())
                .expect("token auth requires token field (validated)");
            let auth = StaticTokenAuth::new(endpoint, tok);
            RegistryClient::builder(url).auth(auth)
        }
        Some(AuthType::Gar | AuthType::Gcr) => {
            let auth = GcpAuth::new(&url, http, aws_credentials, &credentials.gcp).await?;
            RegistryClient::builder(url).auth(auth)
        }
        Some(AuthType::Acr) => {
            let auth = AcrAuth::new(&url).await?;
            RegistryClient::builder(url).auth(auth)
        }
        Some(AuthType::Ghcr | AuthType::DockerConfig) => {
            if matches!(auth_type, Some(AuthType::Ghcr)) {
                warn_ghcr_deprecation_once(bare_host);
            }
            // A missing config file is a setup problem, not a rejected
            // credential: `Input`, not `Auth`.
            let docker_config = DockerConfig::load_default().map_err(|e| {
                CliError::Input(format!(
                    "failed to load docker config for '{bare_host}': {e}"
                ))
            })?;
            let auth =
                configured::Auth::docker_config(&url, http, endpoint, &docker_config).await?;
            RegistryClient::builder(url).auth(auth)
        }
        Some(AuthType::Anonymous) => {
            let auth = configured::Auth::anonymous(&url, http);
            RegistryClient::builder(url).auth(auth)
        }
        None => {
            // Detected once; the result is matched below.
            match detect_provider_kind(bare_host) {
                Some(ProviderKind::Ecr) => {
                    let auth = EcrAuth::new(bare_host, None, aws_credentials)?;
                    RegistryClient::builder(url).auth(auth)
                }
                Some(ProviderKind::EcrPublic) => {
                    let auth = EcrPublicAuth::new(http.clone(), aws_credentials)?;
                    RegistryClient::builder(url).auth(auth)
                }
                Some(ProviderKind::Gar | ProviderKind::Gcr) => {
                    let auth = GcpAuth::new(&url, http, aws_credentials, &credentials.gcp).await?;
                    RegistryClient::builder(url).auth(auth)
                }
                Some(ProviderKind::Acr) => {
                    let auth = AcrAuth::new(&url).await?;
                    RegistryClient::builder(url).auth(auth)
                }
                Some(ProviderKind::Ghcr | ProviderKind::DockerHub | ProviderKind::Chainguard)
                | None => {
                    // Docker config falls back to an anonymous exchange without creds.
                    match DockerConfig::load_default() {
                        Ok(config) => {
                            let auth =
                                configured::Auth::docker_config(&url, http, endpoint, &config)
                                    .await?;
                            RegistryClient::builder(url).auth(auth)
                        }
                        Err(_) => {
                            tracing::debug!(
                                registry = bare_host,
                                "no docker config found, using anonymous auth"
                            );
                            let auth = configured::Auth::anonymous(&url, http);
                            RegistryClient::builder(url).auth(auth)
                        }
                    }
                }
            }
        }
    };

    if let Some(n) = registry_config.and_then(|r| r.max_concurrent) {
        builder = builder.max_concurrent(n);
    }

    builder
        .build()
        .map_err(|e| CliError::Input(format!("failed to build client for '{bare_host}': {e}")))
}

/// Initialize the tracing subscriber based on CLI flags.
///
/// Priority: `RUST_LOG` env var > `--log-format` > text default. Sensitive
/// HTTP transport crates are capped at `warn` to prevent credential leakage
/// in request/response headers at trace level.
pub(crate) fn setup_logging(cli: &Cli) {
    let format = cli.log_format.unwrap_or(LogFormat::Text);

    let filter = verbosity_filter(cli.quiet, cli.verbose);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    // Cap HTTP transport crates so trace level cannot leak credentials from
    // headers, even when RUST_LOG is set.
    let env_filter = env_filter
        .add_directive("hyper=warn".parse().unwrap())
        .add_directive("h2=warn".parse().unwrap())
        .add_directive("reqwest=warn".parse().unwrap())
        .add_directive("rustls=warn".parse().unwrap())
        .add_directive("tower=warn".parse().unwrap());

    // stderr, not stdout: `--json` writes its document to stdout, and log lines
    // would interleave into it.
    match format {
        LogFormat::Json => {
            fmt()
                .json()
                .with_env_filter(env_filter)
                .with_target(false)
                .with_writer(std::io::stderr)
                .init();
        }
        LogFormat::Text => {
            fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .with_writer(std::io::stderr)
                .init();
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_filter_maps_flags_to_directives() {
        let cases = [
            ("quiet", true, 0, "error"),
            ("quiet beats verbose", true, 3, "error"),
            ("default", false, 0, "info"),
            ("-v", false, 1, "debug"),
            ("-vv", false, 2, "trace"),
            ("-vvv", false, 3, "trace"),
        ];
        for (name, quiet, verbose, want) in cases {
            assert_eq!(verbosity_filter(quiet, verbose), want, "{name}");
        }
    }

    #[test]
    fn cli_error_exit_codes() {
        use ocync_distribution::Error as DistError;
        let registry_error = |status, message: &str| {
            CliError::Registry(DistError::RegistryError {
                status,
                message: message.into(),
            })
        };
        let cases = [
            (
                "config",
                CliError::Config(config::ConfigError::Parse("bad".into())),
                ExitCode::ConfigError,
            ),
            (
                "auth failed",
                CliError::Registry(DistError::AuthFailed {
                    registry: "example.com".into(),
                    reason: "denied".into(),
                }),
                ExitCode::AuthError,
            ),
            (
                "registry 401",
                registry_error(http::StatusCode::UNAUTHORIZED, "no"),
                ExitCode::AuthError,
            ),
            (
                "registry 403",
                registry_error(http::StatusCode::FORBIDDEN, "denied"),
                ExitCode::AuthError,
            ),
            (
                "registry 500",
                registry_error(http::StatusCode::INTERNAL_SERVER_ERROR, "broke"),
                ExitCode::Failure,
            ),
            // The negative half of `auth`: a plain input error must not claim the auth code.
            (
                "input",
                CliError::Input("bad url".into()),
                ExitCode::Failure,
            ),
            // A mapping denied credentials carries its own code, so a run denied
            // everywhere is distinguishable from a generic failure.
            (
                "auth",
                CliError::Auth("authentication failed for registry 'x': 403 Forbidden".into()),
                ExitCode::AuthError,
            ),
            (
                "filter",
                CliError::Filter(ocync_sync::Error::LatestWithoutSort),
                ExitCode::ConfigError,
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(err.exit_code(), want, "{name}");
        }
    }

    #[test]
    fn bare_hostname_strips_scheme_and_trailing_slash() {
        let cases = [
            (
                "https",
                "https://registry.example.com",
                "registry.example.com",
            ),
            ("http with port", "http://localhost:5000", "localhost:5000"),
            (
                "trailing slash",
                "https://registry.example.com/",
                "registry.example.com",
            ),
            (
                "already bare",
                "registry.example.com",
                "registry.example.com",
            ),
            ("bare with port", "localhost:5000", "localhost:5000"),
        ];
        for (name, input, want) in cases {
            assert_eq!(bare_hostname(input), want, "{name}");
        }
    }

    #[test]
    fn endpoint_host_rewrites_only_canonical_docker_io() {
        let cases = [
            ("docker hub", "docker.io", "registry-1.docker.io"),
            ("cgr.dev", "cgr.dev", "cgr.dev"),
            ("ghcr.io", "ghcr.io", "ghcr.io"),
            ("public ecr", "public.ecr.aws", "public.ecr.aws"),
            (
                "already rewritten",
                "registry-1.docker.io",
                "registry-1.docker.io",
            ),
            // Different servers entirely; only the exact canonical name is rewritten.
            ("docker.io with port", "docker.io:5000", "docker.io:5000"),
            ("docker.io subdomain", "sub.docker.io", "sub.docker.io"),
        ];
        for (name, input, want) in cases {
            assert_eq!(endpoint_host(input), want, "{name}");
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
    fn all_auth_types_are_handled() {
        // Exhaustive match ensures no AuthType variant is unhandled.
        // If a new variant is added, this fails to compile.
        let variants = [
            AuthType::Ecr,
            AuthType::Gar,
            AuthType::Gcr,
            AuthType::Acr,
            AuthType::Ghcr,
            AuthType::Anonymous,
            AuthType::Basic,
            AuthType::StaticToken,
            AuthType::DockerConfig,
        ];
        for variant in &variants {
            match variant {
                AuthType::Ecr => {}
                AuthType::Gar => {}
                AuthType::Gcr => {}
                AuthType::Acr => {}
                AuthType::Ghcr => {}
                AuthType::Anonymous => {}
                AuthType::Basic => {}
                AuthType::StaticToken => {}
                AuthType::DockerConfig => {}
            }
        }
    }
}
