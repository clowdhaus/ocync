//! Tests for the auth-provider dispatch logic in `build_registry_client`.
//!
//! Each `auth_type` config value and each auto-detected `ProviderKind` must
//! route to the auth provider with the expected `name()`; explicit
//! `ghcr | docker_config` with no docker config file is a `CliError::Input`.
//!
//! Tests that read `DockerConfig::load_default()` set the process-global
//! `DOCKER_CONFIG` to a tempdir under the shared environment lock, a hand-rolled
//! mutex (`serial_test` is not a workspace dependency).

use std::ffi::OsString;
use std::sync::MutexGuard;

use ocync_distribution::RegistryClient;

use crate::cli::config::{AuthType, BasicCredentials, RegistryConfig};
use crate::cli::test_support::lock_env;
use crate::cli::{CliError, Registry, RunCredentials, build_registry_client};

/// Build the client for `cfg`, with credentials of its own.
async fn build(cfg: &RegistryConfig) -> Result<RegistryClient, CliError> {
    build_registry_client(Registry::Configured(cfg), &RunCredentials::default()).await
}

/// Sets `DOCKER_CONFIG` and restores its prior value on drop, holding the
/// shared environment lock meanwhile.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    prior: Option<OsString>,
}

impl EnvGuard {
    fn set_docker_config(value: &std::path::Path) -> Self {
        let lock = lock_env();
        let prior = std::env::var_os("DOCKER_CONFIG");
        // SAFETY: env mutations are serialized by `lock_env`.
        unsafe {
            std::env::set_var("DOCKER_CONFIG", value);
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: env mutations are serialized by `lock_env`.
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var("DOCKER_CONFIG", v),
                None => std::env::remove_var("DOCKER_CONFIG"),
            }
        }
    }
}

/// A tempdir holding a valid (empty) docker `config.json`.
fn tempdir_with_config() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::fs::write(dir.path().join("config.json"), "{}").expect("write config.json");
    dir
}

/// A tempdir with no `config.json`, so `load_default` errors.
fn tempdir_without_config() -> tempfile::TempDir {
    tempfile::tempdir().expect("create tempdir")
}

/// A `RegistryConfig` for `url`; the URL is only auto-detected when `auth_type` is unset.
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
        credentials: Some(BasicCredentials {
            username: "user".to_owned(),
            password: "pass".to_owned(),
        }),
        ..config(url, Some(AuthType::Basic))
    }
}

fn config_with_token(url: &str) -> RegistryConfig {
    RegistryConfig {
        token: Some("ci-token".to_owned()),
        ..config(url, Some(AuthType::StaticToken))
    }
}

/// Builds each config and asserts the provider name it dispatches to.
async fn assert_dispatch(cases: Vec<(&str, RegistryConfig, &str)>) {
    for (name, cfg, want) in cases {
        let client = build(&cfg).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(client.auth_name(), Some(want), "{name}");
    }
}

/// These dispatch on `auth_type` or hostname alone, so a docker config
/// directory with no `config.json` must not matter.
#[tokio::test]
async fn dispatches_without_a_docker_config() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    // `token` is a serde alias for `static_token`; the docs promise both spellings.
    let token_alias: AuthType = serde_yaml::from_str("token").expect("alias parses");
    assert_eq!(token_alias, AuthType::StaticToken);

    assert_dispatch(vec![
        (
            "explicit ecr",
            config(
                "123456789012.dkr.ecr.us-east-1.amazonaws.com",
                Some(AuthType::Ecr),
            ),
            "ecr",
        ),
        (
            "explicit basic",
            config_with_basic("registry.example.com"),
            "basic",
        ),
        (
            "explicit static_token",
            config_with_token("registry.example.com"),
            "static-token",
        ),
        (
            "token alias",
            RegistryConfig {
                token: Some("ci-token".to_owned()),
                ..config("registry.example.com", Some(token_alias))
            },
            "static-token",
        ),
        (
            "explicit gar",
            config("us-docker.pkg.dev", Some(AuthType::Gar)),
            "gcp",
        ),
        ("explicit gcr", config("gcr.io", Some(AuthType::Gcr)), "gcp"),
        (
            "explicit acr",
            config("example.azurecr.io", Some(AuthType::Acr)),
            "acr",
        ),
        (
            "explicit anonymous",
            config("registry.example.com", Some(AuthType::Anonymous)),
            "anonymous",
        ),
        (
            "autodetect ecr",
            config("123456789012.dkr.ecr.us-east-1.amazonaws.com", None),
            "ecr",
        ),
        (
            "autodetect ecr public",
            config("public.ecr.aws", None),
            "ecr-public",
        ),
        ("autodetect gar", config("us-docker.pkg.dev", None), "gcp"),
        ("autodetect gcr", config("gcr.io", None), "gcp"),
        ("autodetect acr", config("example.azurecr.io", None), "acr"),
        // ghcr.io autodetects to docker-config; an explicit static_token must win
        // (static_token rather than ecr keeps this offline).
        (
            "static_token overrides autodetect for ghcr.io",
            config_with_token("ghcr.io"),
            "static-token",
        ),
    ])
    .await;
}

/// With a docker config present, the docker-config providers are selected.
#[tokio::test]
async fn dispatches_to_docker_config_when_one_exists() {
    let dir = tempdir_with_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    // The provider name does not depend on whether the file has an entry for the host.
    assert_dispatch(vec![
        (
            "explicit ghcr alias",
            config("ghcr.io", Some(AuthType::Ghcr)),
            "docker-config",
        ),
        (
            "explicit docker_config",
            config("registry.example.com", Some(AuthType::DockerConfig)),
            "docker-config",
        ),
        (
            "autodetect cgr.dev",
            config("cgr.dev", None),
            "docker-config",
        ),
        (
            "autodetect ghcr.io",
            config("ghcr.io", None),
            "docker-config",
        ),
        // Docker Hub shares the docker-config-then-anonymous path and is the most common.
        (
            "autodetect docker.io",
            config("docker.io", None),
            "docker-config",
        ),
        (
            "explicit anonymous overrides autodetect for cgr.dev",
            config("cgr.dev", Some(AuthType::Anonymous)),
            "anonymous",
        ),
    ])
    .await;
}

/// Autodetected hosts fall back to anonymous when no docker config exists.
#[tokio::test]
async fn autodetect_falls_back_to_anonymous_without_a_docker_config() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    assert_dispatch(vec![
        ("cgr.dev", config("cgr.dev", None), "anonymous"),
        (
            "unknown host",
            config("registry.unknown.example", None),
            "anonymous",
        ),
        ("docker.io", config("docker.io", None), "anonymous"),
    ])
    .await;
}

/// An ECR hostname that names no region is a setup error, not refused
/// credentials. Breaks if ECR setup errors exit as denied.
#[tokio::test]
async fn an_ecr_host_with_no_region_is_not_denied() {
    let cfg = config("registry.example.com", Some(AuthType::Ecr));
    let Err(err) = build(&cfg).await else {
        panic!("built a client for a host with no region");
    };
    assert_eq!(err.exit_code(), crate::cli::ExitCode::Failure, "{err}");
}

#[tokio::test]
async fn ghcr_deprecation_fires_at_most_once() {
    // The warn is gated by a process-global `Once` so long-running modes
    // (`watch`) do not spam the log. The counter is process-global too, so
    // measure the delta from a baseline.
    let baseline = crate::cli::GHCR_DEPRECATION_FIRES.load(std::sync::atomic::Ordering::Relaxed);

    for _ in 0..3 {
        let dir = tempdir_with_config();
        let _g = EnvGuard::set_docker_config(dir.path());
        let cfg = config("ghcr.io", Some(AuthType::Ghcr));
        let _ = build(&cfg).await.expect("build client");
    }

    let after = crate::cli::GHCR_DEPRECATION_FIRES.load(std::sync::atomic::Ordering::Relaxed);
    let delta = after - baseline;
    assert!(
        delta <= 1,
        "Ghcr deprecation warn should fire at most once across 3 dispatch calls; \
         got {delta} fires (baseline={baseline}, after={after})"
    );
}

/// A credential helper that cannot run, found by auto-detection, is a
/// failure, not denied credentials, as with an explicit `docker_config`.
/// Breaks if the auto-detected path reports every helper failure as denied.
#[tokio::test]
async fn a_helper_that_cannot_run_is_not_denied() {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"credHelpers": {"registry.example.com": "ocync-test-missing"}}"#,
    )
    .expect("write config.json");
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("registry.example.com", None);
    let Err(err) = build(&cfg).await else {
        panic!("built a client with a helper that cannot run");
    };
    assert_eq!(err.exit_code(), crate::cli::ExitCode::Failure, "{err}");
}

/// `docker.io` is rewritten to `registry-1.docker.io` for the HTTP endpoint
/// while auth scope keeps the canonical name. The `endpoint_host` unit test
/// would not catch the rewrite being dropped from `build_registry_client`.
#[tokio::test]
async fn docker_io_is_rewritten_to_registry_1_for_endpoint() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cfg = config("docker.io", None);
    let client = build(&cfg).await.expect("build client");
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

/// Explicit `ghcr` / `docker_config` with no config file is a setup error.
#[tokio::test]
async fn explicit_docker_config_providers_error_without_a_config_file() {
    let dir = tempdir_without_config();
    let _g = EnvGuard::set_docker_config(dir.path());
    let cases = [
        ("ghcr", config("ghcr.io", Some(AuthType::Ghcr))),
        (
            "docker_config",
            config("registry.example.com", Some(AuthType::DockerConfig)),
        ),
    ];
    for (name, cfg) in cases {
        let err = build(&cfg).await.expect_err("expected error");
        assert!(
            matches!(err, CliError::Input(_)),
            "{name}: expected CliError::Input, got {err:?}"
        );
    }
}

/// There is no `AuthType::EcrPublic`: `ecr_public` is a parse error at config load.
#[test]
fn auth_type_ecr_public_is_a_parse_error() {
    let parsed: Result<AuthType, _> = serde_yaml::from_str("ecr_public");
    assert!(
        parsed.is_err(),
        "auth_type 'ecr_public' must be rejected at parse time (no AuthType::EcrPublic variant)"
    );
}

/// A registry configured as `http://` is reached over plain HTTP, for both
/// its requests and the token exchange. Covers every auth type whose
/// provider exchanges tokens with the registry itself.
#[tokio::test]
async fn configured_http_registry_is_reached_over_http() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!("Bearer realm=\"{}/token\",service=\"test\"", server.uri()),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"t"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/repo/tags/list"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"name":"repo","tags":["v1"]}"#),
        )
        .mount(&server)
        .await;
    let docker_dir = tempdir_with_config();
    let _env = EnvGuard::set_docker_config(docker_dir.path());

    for cfg in [
        config(&server.uri(), Some(AuthType::Anonymous)),
        config_with_basic(&server.uri()),
        config(&server.uri(), Some(AuthType::DockerConfig)),
    ] {
        let client = build(&cfg).await.expect("build client");
        let tags = client
            .list_tags(&"repo".parse().unwrap())
            .await
            .unwrap_or_else(|e| panic!("{:?} over http: {e}", cfg.auth_type));
        assert_eq!(tags, ["v1"]);
    }
}

#[test]
fn reference_host_resolves_to_its_config_entry() {
    let registries = [
        config("http://localhost:5000", None),
        config("registry-1.docker.io", None),
    ];
    for (host, url) in [
        ("localhost:5000", "http://localhost:5000"),
        ("docker.io", "registry-1.docker.io"),
    ] {
        let Registry::Configured(found) = Registry::resolve(&registries, host) else {
            panic!("{host} should match its config entry");
        };
        assert_eq!(found.url, url, "{host}");
    }
    assert!(matches!(
        Registry::resolve(&registries, "https://ghcr.io/"),
        Registry::Host("ghcr.io")
    ));
}
