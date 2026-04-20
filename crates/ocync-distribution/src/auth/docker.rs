//! Docker `config.json` credential resolution and helper execution.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::token_exchange;
use super::{AuthProvider, Credentials, Scope, Token, scopes_cache_key};
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
    auths: HashMap<String, AuthEntry>,
    /// Credential helpers keyed by registry hostname.
    #[serde(rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
    /// Default credential store (e.g. "desktop", "osxkeychain").
    #[serde(rename = "credsStore")]
    creds_store: Option<String>,
}

/// A single auth entry from the `auths` map.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AuthEntry {
    /// Base64-encoded `username:password`.
    auth: Option<String>,
    /// Username (when stored separately).
    username: Option<String>,
    /// Password (when stored separately).
    password: Option<String>,
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
    fn load_from(path: &Path) -> Result<Self, Error> {
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
pub async fn resolve_from_docker_config(
    config: &DockerConfig,
    registry: &str,
) -> Result<Option<Credentials>, Error> {
    // Try static auths first.
    if let Some(creds) = lookup_auths(config, registry)? {
        return Ok(Some(creds));
    }

    // Try credential helpers.
    if let Some(helper) = lookup_cred_helper(config, registry) {
        let creds = run_credential_helper(&helper, registry).await?;
        return Ok(Some(creds));
    }

    // Try default credential store.
    if let Some(ref store) = config.creds_store {
        match run_credential_helper(store, registry).await {
            Ok(creds) => return Ok(Some(creds)),
            Err(_) => {
                // Default store may not have creds for this registry -- not an error.
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
        if let Some(creds) = entry_to_credentials(entry, registry)? {
            return Ok(Some(creds));
        }
    }

    // Try with/without https:// prefix.
    let with_scheme = format!("https://{registry}");
    if let Some(entry) = config.auths.get(&with_scheme) {
        if let Some(creds) = entry_to_credentials(entry, registry)? {
            return Ok(Some(creds));
        }
    }

    // Docker Hub aliases -- if the requested registry is any Docker Hub alias,
    // try all other aliases.
    if is_docker_hub(registry) {
        for alias in DOCKER_HUB_ALIASES {
            if let Some(entry) = config.auths.get(*alias) {
                if let Some(creds) = entry_to_credentials(entry, registry)? {
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
fn entry_to_credentials(entry: &AuthEntry, registry: &str) -> Result<Option<Credentials>, Error> {
    // Prefer the `auth` field (base64 encoded).
    if let Some(ref encoded) = entry.auth {
        if !encoded.is_empty() {
            let (username, password) = decode_auth(encoded, registry)?;
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
fn decode_auth(encoded: &str, registry: &str) -> Result<(String, String), Error> {
    let decoded = BASE64.decode(encoded).map_err(|e| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: format!("invalid base64 in auth field: {e}"),
    })?;

    let text = String::from_utf8(decoded).map_err(|e| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: format!("auth field is not valid UTF-8: {e}"),
    })?;

    let (username, password) = text.split_once(':').ok_or_else(|| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: "auth field does not contain ':'".into(),
    })?;

    Ok((username.to_owned(), password.to_owned()))
}

/// Execute a Docker credential helper and return the credentials.
///
/// Runs `docker-credential-{helper} get` with the registry on stdin.
/// Uses `tokio::process::Command` to avoid blocking the single-threaded
/// tokio runtime.
async fn run_credential_helper(helper: &str, registry: &str) -> Result<Credentials, Error> {
    // Validate helper name to prevent command injection via crafted
    // `credsStore` / `credHelpers` values in Docker config.json.
    if !helper
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(Error::CredentialHelperFailed {
            helper: helper.to_owned(),
            reason: format!(
                "invalid credential helper name: {helper:?} \
                 (must be alphanumeric and hyphens only)"
            ),
        });
    }

    let program = format!("docker-credential-{helper}");

    let mut child = tokio::process::Command::new(&program)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            tracing::warn!(helper = %program, registry, error = %e, "credential helper failed to execute");
            Error::CredentialHelperFailed {
                helper: program.clone(),
                reason: format!("failed to execute: {e}"),
            }
        })?;

    // Write registry name to stdin, then drop to signal EOF.
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(registry.as_bytes())
            .await
            .map_err(|e| Error::CredentialHelperFailed {
                helper: program.clone(),
                reason: format!("failed to write to stdin: {e}"),
            })?;
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| {
        tracing::warn!(helper = %program, registry, "credential helper timed out after 30s");
        Error::CredentialHelperFailed {
            helper: program.clone(),
            reason: "timed out after 30s (helper may be waiting for interactive input)".into(),
        }
    })?
    .map_err(|e| {
        tracing::warn!(helper = %program, registry, error = %e, "credential helper failed to execute");
        Error::CredentialHelperFailed {
            helper: program.clone(),
            reason: format!("failed to execute: {e}"),
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(helper = %program, registry, status = %output.status, "credential helper exited with error");
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
/// Resolves credentials for the configured registry hostname on construction,
/// then uses the shared token-exchange flow with those credentials (or without,
/// for anonymous access when no credentials are found). Tokens are cached
/// per-scope with the same pattern as [`super::basic::BasicAuth`] and
/// [`super::anonymous::AnonymousAuth`].
pub struct DockerConfigAuth {
    /// The registry base URL for token exchange.
    base_url: String,
    /// HTTP client for token requests.
    http: reqwest::Client,
    /// Resolved credentials (None = anonymous).
    credentials: Option<Credentials>,
    /// Cached tokens keyed by sorted scope strings.
    cache: Mutex<HashMap<String, Token>>,
}

impl fmt::Debug for DockerConfigAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DockerConfigAuth")
            .field("base_url", &self.base_url)
            .field("has_credentials", &self.credentials.is_some())
            .finish_non_exhaustive()
    }
}

impl DockerConfigAuth {
    /// Create a new Docker config auth provider for the given hostname.
    ///
    /// Resolves credentials from `config`. Uses HTTPS by default.
    pub async fn new(
        registry: impl Into<String>,
        config: &DockerConfig,
        http: reqwest::Client,
    ) -> Result<Self, Error> {
        let registry = registry.into();
        let credentials = resolve_from_docker_config(config, &registry).await?;
        if credentials.is_some() {
            tracing::debug!(registry = %registry, "docker config: resolved credentials");
        } else {
            tracing::debug!(registry = %registry, "docker config: no credentials, using anonymous");
        }
        Ok(Self {
            base_url: format!("https://{registry}"),
            http,
            credentials,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Create a Docker config auth provider with an explicit base URL (for testing).
    #[cfg(test)]
    fn with_base_url(
        base_url: impl Into<String>,
        http: reqwest::Client,
        credentials: Option<Credentials>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            credentials,
            cache: Mutex::new(HashMap::new()),
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
            let key = scopes_cache_key(&scopes);

            let mut cache = self.cache.lock().await;

            if let Some(token) = cache.get(&key).filter(|t| t.is_valid()) {
                tracing::debug!(base_url = %self.base_url, scope = %key, "token cache hit");
                return Ok(token.clone());
            }

            tracing::debug!(base_url = %self.base_url, scope = %key, "token cache miss, exchanging");
            let token = token_exchange::exchange(
                &self.http,
                &self.base_url,
                &scopes,
                self.credentials.as_ref(),
            )
            .await
            .map_err(|e| {
                tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                e
            })?;
            cache.insert(key, token.clone());

            Ok(token)
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            tracing::debug!(base_url = %self.base_url, entries = cache.len(), "invalidating token cache");
            cache.clear();
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
        let (user, pass) = decode_auth("dXNlcjpwYXNz", "test.io").unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "pass");
    }

    #[test]
    fn decode_auth_with_colon_in_password() {
        // "user:pa:ss" base64-encoded
        let encoded = BASE64.encode("user:pa:ss");
        let (user, pass) = decode_auth(&encoded, "test.io").unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "pa:ss");
    }

    #[test]
    fn decode_auth_invalid_base64() {
        let result = decode_auth("not-valid-base64!!!", "test.io");
        assert!(result.is_err());
    }

    #[test]
    fn decode_auth_no_colon() {
        // "userpass" (no colon) base64-encoded
        let encoded = BASE64.encode("userpass");
        let result = decode_auth(&encoded, "test.io");
        assert!(result.is_err());
    }

    #[test]
    fn decode_auth_error_includes_registry() {
        let result = decode_auth("not-valid-base64!!!", "ghcr.io");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("ghcr.io"),
            "error should include registry name: {err}"
        );
    }

    #[tokio::test]
    async fn lookup_exact_match() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "dXNlcjpwYXNz" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "ghcr.io")
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(creds, Credentials::Basic { username, password } if username == "user" && password == "pass")
        );
    }

    #[tokio::test]
    async fn lookup_with_scheme_prefix() {
        let json = r#"{
            "auths": {
                "https://ghcr.io": { "auth": "dXNlcjpwYXNz" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "ghcr.io")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(creds, Credentials::Basic { .. }));
    }

    #[tokio::test]
    async fn lookup_not_found() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "dXNlcjpwYXNz" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let result = resolve_from_docker_config(&config, "quay.io")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn lookup_docker_hub_alias() {
        let json = r#"{
            "auths": {
                "https://index.docker.io/v1/": { "auth": "ZG9ja2VyOnNlY3JldA==" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();

        // Look up via "docker.io" -- should find the creds stored under the full URL alias.
        let creds = resolve_from_docker_config(&config, "docker.io")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&creds, Credentials::Basic { username, .. } if username == "docker"));

        // Also via "registry-1.docker.io".
        let creds = resolve_from_docker_config(&config, "registry-1.docker.io")
            .await
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

    #[tokio::test]
    async fn entry_with_separate_username_password() {
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
            .await
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

    #[tokio::test]
    async fn empty_auth_field_returns_none() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "" }
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let result = resolve_from_docker_config(&config, "ghcr.io")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_auth_object_returns_none() {
        let json = r#"{
            "auths": {
                "ghcr.io": {}
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let result = resolve_from_docker_config(&config, "ghcr.io")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn decode_auth_empty_password() {
        let encoded = BASE64.encode("user:");
        let (user, pass) = decode_auth(&encoded, "test.io").unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "");
    }

    #[test]
    fn decode_auth_empty_username() {
        let encoded = BASE64.encode(":password");
        let (user, pass) = decode_auth(&encoded, "test.io").unwrap();
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

    #[tokio::test]
    async fn creds_store_not_checked_when_auths_match() {
        let json = r#"{
            "auths": {
                "ghcr.io": { "auth": "dXNlcjpwYXNz" }
            },
            "credsStore": "desktop"
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let creds = resolve_from_docker_config(&config, "ghcr.io")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(creds, Credentials::Basic { username, .. } if username == "user"));
    }

    use crate::auth::Scope;

    #[tokio::test]
    async fn docker_config_auth_with_inline_creds() {
        let mock = wiremock::MockServer::start().await;
        let encoded = BASE64.encode("testuser:testpass");

        let creds = Credentials::Basic {
            username: "testuser".into(),
            password: "testpass".into(),
        };

        // /v2/ returns 401 with Bearer challenge.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
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

        let auth =
            DockerConfigAuth::with_base_url(mock.uri(), crate::test_http_client(), Some(creds));
        let token = auth
            .get_token(&[Scope::pull("library/nginx")])
            .await
            .unwrap();
        assert_eq!(token.value(), "docker-config-token");
    }

    #[tokio::test]
    async fn docker_config_auth_no_creds_falls_back_to_anonymous() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
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

        let auth = DockerConfigAuth::with_base_url(mock.uri(), crate::test_http_client(), None);
        let token = auth.get_token(&[Scope::pull("public/repo")]).await.unwrap();
        assert_eq!(token.value(), "anon-token");
    }

    #[tokio::test]
    async fn docker_config_auth_caches_tokens() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "cached", "expires_in": 3600})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let auth = DockerConfigAuth::with_base_url(mock.uri(), crate::test_http_client(), None);
        let t1 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t1.value(), "cached");
        assert_eq!(t2.value(), "cached");
        // expect(1) on both endpoints proves the second call hit cache.
    }

    #[tokio::test]
    async fn docker_config_auth_invalidate_clears_cache() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(2)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "fresh", "expires_in": 3600})),
            )
            .expect(2)
            .mount(&mock)
            .await;

        let auth = DockerConfigAuth::with_base_url(mock.uri(), crate::test_http_client(), None);
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.invalidate().await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        // expect(2) proves cache was cleared.
    }

    #[tokio::test]
    async fn docker_config_auth_name() {
        let auth = DockerConfigAuth::new(
            "example.com",
            &DockerConfig::default(),
            crate::test_http_client(),
        )
        .await
        .unwrap();
        assert_eq!(auth.name(), "docker-config");
    }

    #[tokio::test]
    async fn docker_config_auth_resolves_from_config() {
        let config = DockerConfig {
            auths: {
                let mut m = HashMap::new();
                m.insert(
                    "ghcr.io".to_string(),
                    AuthEntry {
                        auth: Some(BASE64.encode("user:pass")),
                        username: None,
                        password: None,
                    },
                );
                m
            },
            cred_helpers: HashMap::new(),
            creds_store: None,
        };
        let auth = DockerConfigAuth::new("ghcr.io", &config, crate::test_http_client())
            .await
            .unwrap();
        // Verify credentials were resolved (has_credentials in Debug output).
        let debug = format!("{auth:?}");
        assert!(debug.contains("has_credentials: true"));
    }

    #[tokio::test]
    async fn credential_helper_rejects_path_traversal() {
        let err = run_credential_helper("../../evil", "registry.example.com")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid credential helper name"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn credential_helper_rejects_shell_metacharacters() {
        let err = run_credential_helper("helper;rm -rf /", "registry.example.com")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid credential helper name"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn credential_helper_accepts_valid_names() {
        // These should pass validation but fail to execute (no such binary).
        // Verify the error is CredentialHelperFailed with "failed to execute",
        // NOT the name-validation error.
        for name in ["gcloud", "ecr-login", "osxkeychain", "desktop"] {
            let err = run_credential_helper(name, "registry.example.com")
                .await
                .unwrap_err();
            assert!(
                !err.to_string().contains("invalid credential helper name"),
                "valid name {name:?} was rejected: {err}"
            );
        }
    }

    #[tokio::test]
    async fn docker_config_auth_no_match_is_anonymous() {
        let config = DockerConfig {
            auths: {
                let mut m = HashMap::new();
                m.insert(
                    "ghcr.io".to_string(),
                    AuthEntry {
                        auth: Some(BASE64.encode("user:pass")),
                        username: None,
                        password: None,
                    },
                );
                m
            },
            cred_helpers: HashMap::new(),
            creds_store: None,
        };
        // quay.io not in config -- should resolve as anonymous.
        let auth = DockerConfigAuth::new("quay.io", &config, crate::test_http_client())
            .await
            .unwrap();
        let debug = format!("{auth:?}");
        assert!(debug.contains("has_credentials: false"));
    }
}
