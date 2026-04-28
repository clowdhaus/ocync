//! Tests for the auth-provider dispatch logic in [`build_registry_client`].
//!
//! Asserts that each `auth_type` config value, and each auto-detected
//! `ProviderKind`, routes to the auth provider with the expected `name()`.
//! Covers the negative `CliError::Input` path for explicit
//! `auth_type: ghcr | docker_config` when no docker config file is present.
//!
//! ## Env-var isolation
//!
//! Tests that exercise `DockerConfig::load_default()` via the unset-`auth_type`
//! fallback or explicit `ghcr` / `docker_config` set `DOCKER_CONFIG` to a
//! tempdir. `DOCKER_CONFIG` is process-global, so a single hand-rolled
//! `Mutex<()>` (`DISPATCH_ENV_GUARD`) serializes them. `serial_test` is not a
//! workspace dependency; the project's "prefer hand-written under ~100 lines
//! over a crate" rule directs us to inline this guard.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

use crate::cli::config::{AuthType, BasicCredentials, RegistryConfig};
use crate::cli::{CliError, build_registry_client};

/// Process-wide guard for tests that mutate `DOCKER_CONFIG`. Held for the
/// duration of a single test; dropped automatically.
static DISPATCH_ENV_GUARD: Mutex<()> = Mutex::new(());

/// RAII helper that sets `DOCKER_CONFIG` and restores its prior value on drop.
///
/// Holds `DISPATCH_ENV_GUARD` so tests using this helper run serially.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    prior: Option<OsString>,
}

impl EnvGuard {
    /// Set `DOCKER_CONFIG` to `value` for the lifetime of this guard.
    fn set_docker_config(value: &std::path::Path) -> Self {
        // Recover from poison: the prior test's panic doesn't make the env
        // state we're about to overwrite any more or less safe to mutate.
        let lock = DISPATCH_ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("DOCKER_CONFIG");
        // SAFETY: env mutations are serialized by DISPATCH_ENV_GUARD.
        unsafe {
            std::env::set_var("DOCKER_CONFIG", value);
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: env mutations are serialized by DISPATCH_ENV_GUARD.
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var("DOCKER_CONFIG", v),
                None => std::env::remove_var("DOCKER_CONFIG"),
            }
        }
    }
}

/// Build a tempdir containing a valid (empty) docker config.json and return
/// the directory path (suitable for `DOCKER_CONFIG`). The tempdir is
/// scrubbed when the returned `tempfile::TempDir` is dropped.
fn tempdir_with_config() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::fs::write(dir.path().join("config.json"), "{}").expect("write config.json");
    dir
}

/// Tempdir with no config.json (`load_default` returns Err).
fn tempdir_without_config() -> tempfile::TempDir {
    tempfile::tempdir().expect("create tempdir")
}

/// Minimal `RegistryConfig` builder that satisfies validation for the given
/// `auth_type`. URL is set to a host that won't be auto-detected unless
/// `auth_type` is unset and the caller wants auto-detect to fire.
fn config(url: &str, auth_type: Option<AuthType>) -> RegistryConfig {
    RegistryConfig {
        url: url.to_owned(),
        auth_type,
        credentials: None,
        token: None,
        max_concurrent: None,
        head_first: false,
        aws_profile: None,
    }
}

fn config_with_basic(url: &str) -> RegistryConfig {
    RegistryConfig {
        url: url.to_owned(),
        auth_type: Some(AuthType::Basic),
        credentials: Some(BasicCredentials {
            username: "user".to_owned(),
            password: "pass".to_owned(),
        }),
        token: None,
        max_concurrent: None,
        head_first: false,
        aws_profile: None,
    }
}

fn config_with_token(url: &str) -> RegistryConfig {
    RegistryConfig {
        url: url.to_owned(),
        auth_type: Some(AuthType::StaticToken),
        credentials: None,
        token: Some("ci-token".to_owned()),
        max_concurrent: None,
        head_first: false,
        aws_profile: None,
    }
}

// ---------------------------------------------------------------------------
// Explicit auth_type cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_ecr_dispatches_to_ecr() {
    let cfg = config(
        "123456789012.dkr.ecr.us-east-1.amazonaws.com",
        Some(AuthType::Ecr),
    );
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("ecr"));
}

#[tokio::test]
async fn explicit_basic_with_credentials_dispatches_to_basic() {
    let cfg = config_with_basic("registry.example.com");
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("basic"));
}

#[tokio::test]
async fn explicit_static_token_dispatches_to_static_token() {
    let cfg = config_with_token("registry.example.com");
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("static-token"));
}

#[tokio::test]
async fn auth_type_token_alias_dispatches_to_static_token() {
    // `token` is a serde alias for `static_token` (see config.rs:195). If a
    // future refactor renames StaticToken or drops the alias, this test
    // surfaces the regression at parse time -- the docs at
    // configuration.md:126 promise both spellings work.
    let parsed: AuthType =
        serde_yaml::from_str("token").expect("auth_type 'token' must parse to StaticToken");
    assert_eq!(parsed, AuthType::StaticToken);
    let cfg = RegistryConfig {
        url: "registry.example.com".to_owned(),
        auth_type: Some(parsed),
        credentials: None,
        token: Some("ci-token".to_owned()),
        max_concurrent: None,
        head_first: false,
        aws_profile: None,
    };
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("static-token"));
}

#[tokio::test]
async fn explicit_gar_dispatches_to_gcp() {
    let cfg = config("us-docker.pkg.dev", Some(AuthType::Gar));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("gcp"));
}

#[tokio::test]
async fn explicit_gcr_dispatches_to_gcp() {
    let cfg = config("gcr.io", Some(AuthType::Gcr));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("gcp"));
}

#[tokio::test]
async fn explicit_acr_dispatches_to_acr() {
    let cfg = config("example.azurecr.io", Some(AuthType::Acr));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("acr"));
}

#[tokio::test]
async fn explicit_ghcr_alias_dispatches_to_docker_config() {
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("ghcr.io", Some(AuthType::Ghcr));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    // The Ghcr branch routes to DockerConfigAuth and emits a warn-level
    // deprecation hint at most once per process via `warn_ghcr_deprecation_once`
    // (see `ghcr_deprecation_fires_at_most_once` for the Once-gating
    // assertion). Routing is verified here.
    assert_eq!(client.auth_name(), Some("docker-config"));
}

#[tokio::test]
async fn ghcr_deprecation_fires_at_most_once() {
    // The deprecation warn is gated by a process-global `Once` so that
    // long-running modes (e.g. `watch`) don't spam the log. We assert via
    // GHCR_DEPRECATION_FIRES, a #[cfg(test)] counter incremented inside
    // the Once body. The counter is process-global; another test may have
    // already incremented it, so we measure the delta from a baseline.
    let baseline = crate::cli::GHCR_DEPRECATION_FIRES.load(std::sync::atomic::Ordering::Relaxed);

    for _ in 0..3 {
        let dir = tempdir_with_config();
        let _g = EnvGuard::set_docker_config(dir.path());
        let cfg = config("ghcr.io", Some(AuthType::Ghcr));
        let _ = build_registry_client(&cfg.url, Some(&cfg))
            .await
            .expect("build client");
    }

    let after = crate::cli::GHCR_DEPRECATION_FIRES.load(std::sync::atomic::Ordering::Relaxed);
    let delta = after - baseline;
    assert!(
        delta <= 1,
        "Ghcr deprecation warn should fire at most once across 3 dispatch calls; \
         got {delta} fires (baseline={baseline}, after={after})"
    );
}

#[tokio::test]
async fn explicit_docker_config_dispatches_to_docker_config() {
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("registry.example.com", Some(AuthType::DockerConfig));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("docker-config"));
}

#[tokio::test]
async fn explicit_anonymous_dispatches_to_anonymous() {
    let cfg = config("registry.example.com", Some(AuthType::Anonymous));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("anonymous"));
}

// ---------------------------------------------------------------------------
// Auto-detect cases (no auth_type set)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn autodetect_ecr_hostname_dispatches_to_ecr() {
    let cfg = config("123456789012.dkr.ecr.us-east-1.amazonaws.com", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("ecr"));
}

#[tokio::test]
async fn autodetect_ecr_public_dispatches_to_ecr_public() {
    let cfg = config("public.ecr.aws", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("ecr-public"));
}

#[tokio::test]
async fn autodetect_gar_dispatches_to_gcp() {
    let cfg = config("us-docker.pkg.dev", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("gcp"));
}

#[tokio::test]
async fn autodetect_gcr_dispatches_to_gcp() {
    let cfg = config("gcr.io", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("gcp"));
}

#[tokio::test]
async fn autodetect_acr_dispatches_to_acr() {
    let cfg = config("example.azurecr.io", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("acr"));
}

#[tokio::test]
async fn autodetect_cgr_dev_with_no_docker_config_falls_back_to_anonymous() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("cgr.dev", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("anonymous"));
}

#[tokio::test]
async fn autodetect_cgr_dev_with_docker_config_uses_docker_config() {
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("cgr.dev", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    // Provider name is "docker-config" regardless of whether the config
    // contains a matching entry for cgr.dev. Per-host credential lookup
    // happens inside DockerConfigAuth and does not affect the name.
    assert_eq!(client.auth_name(), Some("docker-config"));
}

#[tokio::test]
async fn autodetect_ghcr_io_with_docker_config_uses_docker_config() {
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("ghcr.io", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("docker-config"));
}

#[tokio::test]
async fn autodetect_unknown_hostname_with_no_docker_config_falls_back_to_anonymous() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("registry.unknown.example", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("anonymous"));
}

#[tokio::test]
async fn autodetect_docker_io_with_docker_config_uses_docker_config() {
    // ProviderKind::DockerHub shares the docker-config-then-anonymous
    // fallback path with Ghcr / Chainguard / unknown (mod.rs:277-301).
    // This row was missing from the matrix; the dispatch path is the most
    // common one (every Docker Hub user hits it) so it's worth pinning.
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("docker.io", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("docker-config"));
}

#[tokio::test]
async fn autodetect_docker_io_with_no_docker_config_falls_back_to_anonymous() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("docker.io", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("anonymous"));
}

#[tokio::test]
async fn docker_io_is_rewritten_to_registry_1_for_endpoint() {
    // mod.rs:143 rewrites `docker.io` to `registry-1.docker.io` for the
    // HTTP endpoint, while keeping the canonical `docker.io` for auth scope.
    // The endpoint_host helper test (mod.rs:515) covers the function in
    // isolation, but the dispatch path applies the rewrite via base_url
    // construction (mod.rs:168-179); a regression dropping the rewrite from
    // build_registry_client would not be caught by the helper test alone.
    // RegistryClient::registry_authority() exposes the resolved base URL host.
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("docker.io", None);
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    let authority = client
        .registry_authority()
        .expect("client has a host")
        .as_str()
        .to_owned();
    assert_eq!(
        authority, "registry-1.docker.io:443",
        "expected docker.io to rewrite to registry-1.docker.io, got '{authority}'"
    );
}

// ---------------------------------------------------------------------------
// Override cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_anonymous_overrides_autodetect_for_cgr_dev() {
    // Even though autodetect would route cgr.dev through docker-config (when
    // a config file is present), explicit auth_type: anonymous wins.
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("cgr.dev", Some(AuthType::Anonymous));
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("anonymous"));
}

#[tokio::test]
async fn explicit_static_token_overrides_autodetect_for_ghcr_io() {
    // Mirror of `explicit_anonymous_overrides_autodetect_for_cgr_dev`.
    // `ghcr.io` autodetects to ProviderKind::Ghcr -> docker-config (with
    // anonymous fallback). Setting auth_type: static_token must win and
    // route to StaticTokenAuth regardless of hostname autodetection.
    // Uses static_token instead of ecr (which would attempt real AWS SDK
    // setup on a non-ECR hostname) to keep the test offline.
    let cfg = config_with_token("ghcr.io");
    let client = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect("build client");
    assert_eq!(client.auth_name(), Some("static-token"));
}

// ---------------------------------------------------------------------------
// Negative cases: explicit ghcr / docker_config with no config file errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_ghcr_with_no_docker_config_errors() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("ghcr.io", Some(AuthType::Ghcr));
    let err = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect_err("expected error");
    assert!(
        matches!(err, CliError::Input(_)),
        "expected CliError::Input, got {err:?}"
    );
}

#[tokio::test]
async fn explicit_docker_config_with_no_docker_config_errors() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("registry.example.com", Some(AuthType::DockerConfig));
    let err = build_registry_client(&cfg.url, Some(&cfg))
        .await
        .expect_err("expected error");
    assert!(
        matches!(err, CliError::Input(_)),
        "expected CliError::Input, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// AuthType::EcrPublic does not exist: explicit `auth_type: ecr_public` is a
// serde parse error during config load, not a dispatch-time error.
// ---------------------------------------------------------------------------

#[test]
fn auth_type_ecr_public_is_a_parse_error() {
    let yaml = "ecr_public";
    let parsed: Result<AuthType, _> = serde_yaml::from_str(yaml);
    assert!(
        parsed.is_err(),
        "auth_type 'ecr_public' must be rejected at parse time (no AuthType::EcrPublic variant)"
    );
}

#[tokio::test]
async fn ecr_dispatch_with_aws_profile_succeeds() {
    let cfg = RegistryConfig {
        url: "123456789012.dkr.ecr.us-east-1.amazonaws.com".to_owned(),
        auth_type: Some(AuthType::Ecr),
        credentials: None,
        token: None,
        max_concurrent: None,
        head_first: false,
        aws_profile: Some("vendor".to_owned()),
    };
    let client = build_registry_client("123456789012.dkr.ecr.us-east-1.amazonaws.com", Some(&cfg))
        .await
        .expect("ECR dispatch with aws_profile should succeed");
    assert_eq!(client.auth_name(), Some("ecr"));
}
