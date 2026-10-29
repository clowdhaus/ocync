//! Docker `config.json` credential resolution and helper execution.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use super::Credentials;
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
            .field("auth", &crate::REDACTED)
            .field("username", &self.username)
            .field("password", &crate::REDACTED)
            .finish()
    }
}

impl DockerConfig {
    /// Load from the default Docker config path (`~/.docker/config.json`).
    pub fn load_default() -> Result<Self, Error> {
        let path = default_config_path().ok_or_else(|| Error::CredentialConfig {
            reason: "unable to determine home directory".into(),
        })?;
        Self::load_from(&path)
    }

    /// Load from a specific file path.
    fn load_from(path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path).map_err(|e| Error::CredentialConfig {
            reason: format!("failed to read config at {}: {e}", path.display()),
        })?;
        crate::json::parse(&contents).map_err(|e| Error::CredentialConfig {
            reason: format!("failed to parse config at {}: {e}", path.display()),
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
            tracing::debug!(helper = %program, registry, error = %e, "credential helper failed to execute");
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
        // Timeout is a real operational problem (hung helper, locked keychain).
        // Distinct from the noisy fallback paths around it -- keep at WARN.
        tracing::warn!(helper = %program, registry, "credential helper timed out after 30s");
        Error::CredentialHelperFailed {
            helper: program.clone(),
            reason: "timed out after 30s (helper may be waiting for interactive input)".into(),
        }
    })?
    .map_err(|e| {
        tracing::debug!(helper = %program, registry, error = %e, "credential helper failed to execute");
        Error::CredentialHelperFailed {
            helper: program.clone(),
            reason: format!("failed to execute: {e}"),
        }
    })?;

    helper_credentials(program, registry, &output)
}

/// The credentials a helper's `output` holds. Its stderr is cut to the
/// most of a program's output an error keeps, and a refused value from its
/// stdout is left out.
fn helper_credentials(
    program: String,
    registry: &str,
    output: &std::process::Output,
) -> Result<Credentials, Error> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(helper = %program, registry, status = %output.status, "credential helper exited with error");
        return Err(Error::CredentialHelperFailed {
            helper: program,
            reason: format!(
                "exited with {}: {}",
                output.status,
                crate::error::truncate_quoted(stderr.trim())
            ),
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
        crate::json::parse(&output.stdout).map_err(|e| Error::CredentialHelperFailed {
            helper: program,
            reason: format!("invalid JSON response: {e}"),
        })?;

    Ok(Credentials::Basic {
        username: resp.username,
        password: resp.secret,
    })
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

    /// A helper that fails has its stderr cut to what can be quoted, and one
    /// whose answer is mistyped is refused without the refused value.
    /// Breaks if the stderr is kept past the cap or the value is quoted.
    #[cfg(unix)]
    #[test]
    fn a_helper_failure_is_cut_to_what_can_be_quoted() {
        use std::os::unix::process::ExitStatusExt;

        let output = |code: i32, stdout: &str, stderr: String| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.into_bytes(),
        };
        let failed = output(1, "", format!("{}s3cret", "x".repeat(200)));
        let error = helper_credentials("helper".into(), "r", &failed).unwrap_err();
        assert!(!error.to_string().contains("s3cret"), "{error}");
        let mistyped = output(0, r#"{"Username": "u", "Secret": 7}"#, String::new());
        let error = helper_credentials("helper".into(), "r", &mistyped).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("invalid type: integer, expected a string"),
            "{text}"
        );
        assert!(!text.contains("`7`"), "{text}");
    }

    /// A config whose auth entry is a string, not an object, is refused
    /// without quoting the string, which holds the encoded password. Breaks
    /// if the parser's message reaches the error.
    #[test]
    fn a_mistyped_config_leaves_the_password_out() {
        let path = std::env::temp_dir().join(format!("ocync-docker-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"auths": {"ghcr.io": "dXNlcjpzM2NyZXQ="}}"#).unwrap();
        let result = DockerConfig::load_from(&path);
        std::fs::remove_file(&path).unwrap();
        let error = result.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("expected struct AuthEntry"), "{text}");
        assert!(!text.contains("dXNlcjpzM2NyZXQ="), "{text}");
    }

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
}
