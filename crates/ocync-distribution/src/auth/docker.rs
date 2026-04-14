//! Docker `config.json` credential resolution and helper execution.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use super::anonymous::AnonymousAuth;
use super::basic::BasicAuth;
use super::{AuthProvider, Credentials, Scope, Token};
use crate::error::Error;

/// Docker Hub hostnames that all resolve to the same credential entry.
const DOCKER_HUB_ALIASES: &[&str] = &[
    "docker.io",
    "index.docker.io",
    "registry-1.docker.io",
    "https://index.docker.io/v1/",
    "https://index.docker.io/v2/",
];

/// Deserialized `~/.docker/config.json`.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct DockerConfig {
    /// Static credentials keyed by registry hostname.
    pub auths: HashMap<String, AuthEntry>,
    /// Credential helpers keyed by registry hostname.
    #[serde(rename = "credHelpers")]
    pub cred_helpers: HashMap<String, String>,
    /// Default credential store (e.g. "desktop", "osxkeychain").
    #[serde(rename = "credsStore")]
    pub creds_store: Option<String>,
}

/// A single auth entry from the `auths` map.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct AuthEntry {
    /// Base64-encoded `username:password`.
    pub auth: Option<String>,
    /// Username (when stored separately).
    pub username: Option<String>,
    /// Password (when stored separately).
    pub password: Option<String>,
}

impl fmt::Debug for AuthEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthEntry")
            .field("auth", &"[REDACTED]")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

impl DockerConfig {
    /// Load from the default Docker config path (`~/.docker/config.json`).
    pub fn load_default() -> Result<Self, Error> {
        let path = default_config_path()
            .ok_or_else(|| Error::Other("unable to determine home directory".into()))?;
        Self::load_from(&path)
    }

    /// Load from a specific file path.
    pub fn load_from(path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::Other(format!(
                "failed to read docker config at {}: {e}",
                path.display()
            ))
        })?;
        serde_json::from_str(&contents).map_err(|e| {
            Error::Other(format!(
                "failed to parse docker config at {}: {e}",
                path.display()
            ))
        })
    }
}

/// Resolve credentials for a registry from a Docker config.
///
/// Checks `auths` first (static credentials), then `credHelpers`, then the
/// default `credsStore`. Handles Docker Hub hostname normalization.
pub fn resolve_from_docker_config(
    config: &DockerConfig,
    registry: &str,
) -> Result<Option<Credentials>, Error> {
    // Try static auths first.
    if let Some(creds) = lookup_auths(config, registry)? {
        return Ok(Some(creds));
    }

    // Try credential helpers.
    if let Some(helper) = lookup_cred_helper(config, registry) {
        let creds = run_credential_helper(&helper, registry)?;
        return Ok(Some(creds));
    }

    // Try default credential store.
    if let Some(ref store) = config.creds_store {
        match run_credential_helper(store, registry) {
            Ok(creds) => return Ok(Some(creds)),
            Err(_) => {
                // Default store may not have creds for this registry — not an error.
                tracing::debug!(
                    registry,
                    store,
                    "default credential store had no credentials"
                );
            }
        }
    }

    Ok(None)
}

/// Look up static credentials from the `auths` map.
fn lookup_auths(config: &DockerConfig, registry: &str) -> Result<Option<Credentials>, Error> {
    // Direct match.
    if let Some(entry) = config.auths.get(registry) {
        if let Some(creds) = entry_to_credentials(entry)? {
            return Ok(Some(creds));
        }
    }

    // Try with/without https:// prefix.
    let with_scheme = format!("https://{registry}");
    if let Some(entry) = config.auths.get(&with_scheme) {
        if let Some(creds) = entry_to_credentials(entry)? {
            return Ok(Some(creds));
        }
    }

    // Docker Hub aliases — if the requested registry is any Docker Hub alias,
    // try all other aliases.
    if is_docker_hub(registry) {
        for alias in DOCKER_HUB_ALIASES {
            if let Some(entry) = config.auths.get(*alias) {
                if let Some(creds) = entry_to_credentials(entry)? {
                    return Ok(Some(creds));
                }
            }
        }
    }

    Ok(None)
}

/// Look up a credential helper for the given registry.
fn lookup_cred_helper(config: &DockerConfig, registry: &str) -> Option<String> {
    if let Some(helper) = config.cred_helpers.get(registry) {
        return Some(helper.clone());
    }

    // Docker Hub aliases.
    if is_docker_hub(registry) {
        for alias in DOCKER_HUB_ALIASES {
            if let Some(helper) = config.cred_helpers.get(*alias) {
                return Some(helper.clone());
            }
        }
    }

    None
}

/// Convert an `AuthEntry` to `Credentials`.
fn entry_to_credentials(entry: &AuthEntry) -> Result<Option<Credentials>, Error> {
    // Prefer the `auth` field (base64 encoded).
    if let Some(ref encoded) = entry.auth {
        if !encoded.is_empty() {
            let (username, password) = decode_auth(encoded)?;
            return Ok(Some(Credentials::Basic { username, password }));
        }
    }

    // Fall back to separate username/password fields.
    if let (Some(username), Some(password)) = (&entry.username, &entry.password) {
        return Ok(Some(Credentials::Basic {
            username: username.clone(),
            password: password.clone(),
        }));
    }

    Ok(None)
}

/// Decode a base64-encoded `username:password` string.
fn decode_auth(encoded: &str) -> Result<(String, String), Error> {
    let decoded = BASE64.decode(encoded).map_err(|e| Error::AuthFailed {
        registry: String::new(),
        reason: format!("invalid base64 in auth field: {e}"),
    })?;

    let text = String::from_utf8(decoded).map_err(|e| Error::AuthFailed {
        registry: String::new(),
        reason: format!("auth field is not valid UTF-8: {e}"),
    })?;

    let (username, password) = text.split_once(':').ok_or_else(|| Error::AuthFailed {
        registry: String::new(),
        reason: "auth field does not contain ':'".into(),
    })?;

    Ok((username.to_owned(), password.to_owned()))
}

/// Execute a Docker credential helper and return the credentials.
///
/// Runs `docker-credential-{helper} get` with the registry on stdin.
fn run_credential_helper(helper: &str, registry: &str) -> Result<Credentials, Error> {
    let program = format!("docker-credential-{helper}");

    let output = Command::new(&program)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(registry.as_bytes())?;
            }
            child.wait_with_output()
        })
        .map_err(|e| Error::CredentialHelperFailed {
            helper: program.clone(),
            reason: format!("failed to execute: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::CredentialHelperFailed {
            helper: program,
            reason: format!("exited with {}: {}", output.status, stderr.trim()),
        });
    }

    #[derive(Deserialize)]
    struct HelperResponse {
        #[serde(rename = "Username")]
        username: String,
        #[serde(rename = "Secret")]
        secret: String,
    }

    let resp: HelperResponse =
        serde_json::from_slice(&output.stdout).map_err(|e| Error::CredentialHelperFailed {
            helper: program,
            reason: format!("invalid JSON response: {e}"),
        })?;

    Ok(Credentials::Basic {
        username: resp.username,
        password: resp.secret,
    })
}

/// Auth provider that resolves credentials from a Docker `config.json`.
///
/// On the first `get_token()` call, resolves credentials for the
/// configured registry hostname from the provided [`DockerConfig`].
/// If credentials are found, delegates to [`BasicAuth`]; otherwise
/// falls back to [`AnonymousAuth`].
pub struct DockerConfigAuth {
    /// The registry hostname to look up in the Docker config.
    registry: String,
    /// The registry base URL for the underlying auth provider.
    base_url: String,
    /// The loaded Docker config.
    config: DockerConfig,
    /// HTTP client shared with the inner auth provider.
    http: reqwest::Client,
    /// Lazily initialized inner provider.
    inner: tokio::sync::OnceCell<Box<dyn AuthProvider>>,
}

impl fmt::Debug for DockerConfigAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DockerConfigAuth")
            .field("registry", &self.registry)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl DockerConfigAuth {
    /// Create a new Docker config auth provider for the given hostname.
    ///
    /// Uses HTTPS by default.
    pub fn new(registry: impl Into<String>, config: DockerConfig, http: reqwest::Client) -> Self {
        let registry = registry.into();
        let base_url = format!("https://{registry}");
        Self {
            registry,
            base_url,
            config,
            http,
            inner: tokio::sync::OnceCell::new(),
        }
    }

    /// Create a Docker config auth provider with an explicit base URL (for testing).
    #[cfg(test)]
    fn with_config_and_base_url(
        config: DockerConfig,
        registry: impl Into<String>,
        base_url: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            registry: registry.into(),
            base_url: base_url.into(),
            config,
            http,
            inner: tokio::sync::OnceCell::new(),
        }
    }

    /// Resolve credentials and build the appropriate inner provider.
    fn resolve_inner(&self) -> Result<Box<dyn AuthProvider>, Error> {
        let http = self.http.clone();
        match resolve_from_docker_config(&self.config, &self.registry)? {
            Some(creds) => {
                tracing::debug!(
                    registry = %self.registry,
                    "docker config: resolved credentials, using basic auth"
                );
                Ok(Box::new(BasicAuth::with_base_url(
                    self.base_url.clone(),
                    http,
                    creds,
                )))
            }
            None => {
                tracing::debug!(
                    registry = %self.registry,
                    "docker config: no credentials found, using anonymous auth"
                );
                Ok(Box::new(AnonymousAuth::with_base_url(
                    self.base_url.clone(),
                    http,
                )))
            }
        }
    }
}

impl AuthProvider for DockerConfigAuth {
    fn name(&self) -> &'static str {
        "docker-config"
    }

    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        let scopes = scopes.to_vec();
        Box::pin(async move {
            let inner = self
                .inner
                .get_or_try_init(|| async { self.resolve_inner() })
                .await?;
            inner.get_token(&scopes).await
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if let Some(inner) = self.inner.get() {
                inner.invalidate().await;
            }
        })
    }
}

/// Check if a hostname is a Docker Hub alias.
fn is_docker_hub(registry: &str) -> bool {
    DOCKER_HUB_ALIASES
        .iter()
        .any(|alias| registry == *alias || registry == alias.trim_start_matches("https://"))
}

/// Get the default Docker config path.
fn default_config_path() -> Option<PathBuf> {
    // Check DOCKER_CONFIG env var first.
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_with_auths() {
        let json = r#"{
            "auths": {
                "ghcr.io": {
                    "auth": "dXNlcjpwYXNz"
                },
                "https://index.docker.io/v1/": {
                    "auth": "ZG9ja2VyOnNlY3JldA=="
                }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.auths.len(), 2);
        assert!(config.auths.contains_key("ghcr.io"));
    }

    #[test]
    fn decode_auth_valid() {
        // "user:pass" base64-encoded
        let (user, pass) = decode_auth("dXNlcjpwYXNz").unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "pass");
    }

    #[test]
    fn decode_auth_with_colon_in_password() {
        // "user:pa:ss" base64-encoded
        let encoded = BASE64.encode("user:pa:ss");
        let (user, pass) = decode_auth(&encoded).unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "pa:ss");
    }

    #[test]
    fn decode_auth_invalid_base64() {
        let result = decode_auth("not-valid-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn decode_auth_no_colon() {
        // "userpass" (no colon) base64-encoded
        let encoded = BASE64.encode("userpass");
        let result = decode_auth(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn lookup_exact_match() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "dXNlcjpwYXNz" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "ghcr.io")
            .unwrap()
            .unwrap();
        assert!(
            matches!(creds, Credentials::Basic { username, password } if username == "user" && password == "pass")
        );
    }

    #[test]
    fn lookup_with_scheme_prefix() {
        let json = r#"{
            "auths": {
                "https://ghcr.io": { "auth": "dXNlcjpwYXNz" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "ghcr.io")
            .unwrap()
            .unwrap();
        assert!(matches!(creds, Credentials::Basic { .. }));
    }

    #[test]
    fn lookup_not_found() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "dXNlcjpwYXNz" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let result = resolve_from_docker_config(&config, "quay.io").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn lookup_docker_hub_alias() {
        let json = r#"{
            "auths": {
                "https://index.docker.io/v1/": { "auth": "ZG9ja2VyOnNlY3JldA==" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();

        // Look up via "docker.io" — should find the creds stored under the full URL alias.
        let creds = resolve_from_docker_config(&config, "docker.io")
            .unwrap()
            .unwrap();
        assert!(matches!(&creds, Credentials::Basic { username, .. } if username == "docker"));

        // Also via "registry-1.docker.io".
        let creds = resolve_from_docker_config(&config, "registry-1.docker.io")
            .unwrap()
            .unwrap();
        assert!(matches!(creds, Credentials::Basic { .. }));
    }

    #[test]
    fn parse_config_with_cred_helpers() {
        let json = r#"{
            "credHelpers": {
                "123456789012.dkr.ecr.us-east-1.amazonaws.com": "ecr-login",
                "gcr.io": "gcloud"
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.cred_helpers.len(), 2);
        assert_eq!(
            config
                .cred_helpers
                .get("123456789012.dkr.ecr.us-east-1.amazonaws.com")
                .unwrap(),
            "ecr-login"
        );
    }

    #[test]
    fn parse_config_with_creds_store() {
        let json = r#"{
            "credsStore": "desktop"
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.creds_store.as_deref(), Some("desktop"));
    }

    #[test]
    fn empty_config() {
        let json = "{}";
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert!(config.auths.is_empty());
        assert!(config.cred_helpers.is_empty());
        assert!(config.creds_store.is_none());
    }

    #[test]
    fn entry_with_separate_username_password() {
        let json = r#"{
            "auths": {
                "custom.io": {
                    "username": "myuser",
                    "password": "mypass"
                }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "custom.io")
            .unwrap()
            .unwrap();
        assert!(
            matches!(&creds, Credentials::Basic { username, password } if username == "myuser" && password == "mypass")
        );
    }

    #[test]
    fn is_docker_hub_variants() {
        assert!(is_docker_hub("docker.io"));
        assert!(is_docker_hub("index.docker.io"));
        assert!(is_docker_hub("registry-1.docker.io"));
        assert!(is_docker_hub("https://index.docker.io/v1/"));
        assert!(!is_docker_hub("ghcr.io"));
        assert!(!is_docker_hub("quay.io"));
    }

    #[test]
    fn empty_auth_field_returns_none() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let result = resolve_from_docker_config(&config, "ghcr.io").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_auth_object_returns_none() {
        let json = r#"{
            "auths": {
                "ghcr.io": {}
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let result = resolve_from_docker_config(&config, "ghcr.io").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn decode_auth_empty_password() {
        let encoded = BASE64.encode("user:");
        let (user, pass) = decode_auth(&encoded).unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "");
    }

    #[test]
    fn decode_auth_empty_username() {
        let encoded = BASE64.encode(":password");
        let (user, pass) = decode_auth(&encoded).unwrap();
        assert_eq!(user, "");
        assert_eq!(pass, "password");
    }

    #[test]
    fn config_with_unknown_fields_ignored() {
        let json = r#"{
            "auths": {},
            "experimental": "enabled",
            "proxies": { "default": {} },
            "stackOrchestrator": "swarm"
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert!(config.auths.is_empty());
    }

    #[test]
    fn cred_helper_docker_hub_alias_lookup() {
        let json = r#"{
            "credHelpers": {
                "https://index.docker.io/v1/": "desktop"
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let helper = lookup_cred_helper(&config, "docker.io");
        assert_eq!(helper.as_deref(), Some("desktop"));
    }

    #[test]
    fn creds_store_not_checked_when_auths_match() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "dXNlcjpwYXNz" }
            },
            "credsStore": "desktop"
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "ghcr.io")
            .unwrap()
            .unwrap();
        assert!(matches!(creds, Credentials::Basic { username, .. } if username == "user"));
    }

    use crate::auth::Scope;

    #[tokio::test]
    async fn docker_config_auth_with_inline_creds() {
        let mock = wiremock::MockServer::start().await;

        let uri = mock.uri();
        let mock_host = uri.trim_start_matches("http://");
        let encoded = BASE64.encode("testuser:testpass");
        let config = DockerConfig {
            auths: {
                let mut m = HashMap::new();
                m.insert(
                    mock_host.to_string(),
                    AuthEntry {
                        auth: Some(encoded.clone()),
                        username: None,
                        password: None,
                    },
                );
                m
            },
            cred_helpers: HashMap::new(),
            creds_store: None,
        };

        // /v2/ returns 401 with Bearer challenge.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!("Bearer realm=\"{}/token\"", mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        // Token endpoint expects Basic auth.
        let expected_basic = format!("Basic {encoded}");
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .and(wiremock::matchers::header(
                "Authorization",
                expected_basic.as_str(),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"token": "docker-config-token", "expires_in": 300}),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        let auth = DockerConfigAuth::with_config_and_base_url(
            config,
            mock_host.to_string(),
            mock.uri(),
            reqwest::Client::new(),
        );
        let token = auth
            .get_token(&[Scope::pull("library/nginx")])
            .await
            .unwrap();
        assert_eq!(token.value(), "docker-config-token");
    }

    #[tokio::test]
    async fn docker_config_auth_no_creds_falls_back_to_anonymous() {
        let mock = wiremock::MockServer::start().await;
        let uri = mock.uri();
        let mock_host = uri.trim_start_matches("http://");

        let config = DockerConfig::default();

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!("Bearer realm=\"{}/token\"", mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "anon-token"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let auth = DockerConfigAuth::with_config_and_base_url(
            config,
            mock_host.to_string(),
            mock.uri(),
            reqwest::Client::new(),
        );
        let token = auth.get_token(&[Scope::pull("public/repo")]).await.unwrap();
        assert_eq!(token.value(), "anon-token");
    }

    #[test]
    fn docker_config_auth_name() {
        let auth = DockerConfigAuth::new(
            "example.com",
            DockerConfig::default(),
            reqwest::Client::new(),
        );
        assert_eq!(auth.name(), "docker-config");
    }
}
