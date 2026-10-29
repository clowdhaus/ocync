//! Docker `config.json` credential resolution and helper execution.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use super::Credentials;
use crate::error::Error;

/// The server address the Docker CLI keys Docker Hub credentials under.
const DOCKER_HUB_SERVER: &str = "https://index.docker.io/v1/";

/// Docker Hub hostnames that all resolve to the same credential entry.
const DOCKER_HUB_ALIASES: &[&str] = &[
    "docker.io",
    "index.docker.io",
    "registry-1.docker.io",
    DOCKER_HUB_SERVER,
    "https://index.docker.io/v2/",
];

/// What a credential helper prints when it holds nothing for a server, per
/// the docker-credential-helpers protocol.
const CREDENTIALS_NOT_FOUND: &str = "credentials not found in native keychain";

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

/// Resolve credentials for a registry from a Docker config, as the Docker
/// CLI does: a credential helper, the registry's own `credHelpers` entry or
/// else the default `credsStore`, answers in place of `auths`, so a stale
/// `auths` entry left by `docker login` never shadows it. A helper holding
/// nothing for the registry means none (anonymous), and any other helper
/// failure is an error. Handles Docker Hub hostname normalization.
pub async fn resolve_from_docker_config(
    config: &DockerConfig,
    registry: &str,
) -> Result<Option<Credentials>, Error> {
    match helper_for(config, registry) {
        Some(helper) => run_credential_helper(helper, server_address(registry)).await,
        None => lookup_auths(config, registry),
    }
}

/// The helper Docker asks for `registry`: its `credHelpers` entry, else
/// the default `credsStore`. An empty name is no helper, and an empty
/// `credHelpers` entry also turns off the default store for its registry.
fn helper_for<'a>(config: &'a DockerConfig, registry: &str) -> Option<&'a str> {
    entry_keys(registry)
        .find_map(|key| config.cred_helpers.get(key.as_ref()))
        .or(config.creds_store.as_ref())
        .map(String::as_str)
        .filter(|helper| !helper.is_empty())
}

/// The address a helper is asked about: Docker keys Docker Hub under its
/// index server, and every other registry under its host.
fn server_address(registry: &str) -> &str {
    if is_docker_hub(registry) {
        DOCKER_HUB_SERVER
    } else {
        registry
    }
}

/// The keys a registry's entry may sit under in a Docker config map, most
/// specific first: the host, then the host with a scheme, or every alias
/// for a Docker Hub host.
fn entry_keys(registry: &str) -> impl Iterator<Item = Cow<'_, str>> {
    let rest: Vec<Cow<'_, str>> = if is_docker_hub(registry) {
        DOCKER_HUB_ALIASES
            .iter()
            .filter(|alias| **alias != registry)
            .map(|alias| Cow::Borrowed(*alias))
            .collect()
    } else {
        vec![Cow::Owned(format!("https://{registry}"))]
    };
    std::iter::once(Cow::Borrowed(registry)).chain(rest)
}

/// Look up static credentials from the `auths` map.
fn lookup_auths(config: &DockerConfig, registry: &str) -> Result<Option<Credentials>, Error> {
    for key in entry_keys(registry) {
        if let Some(entry) = config.auths.get(key.as_ref()) {
            if let Some(creds) = entry_to_credentials(entry, registry)? {
                return Ok(Some(creds));
            }
        }
    }
    Ok(None)
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
async fn run_credential_helper(helper: &str, registry: &str) -> Result<Option<Credentials>, Error> {
    let program = helper_program(helper)?;

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

/// The program a helper name runs. The name comes from a config file, so
/// one that could reach outside the `docker-credential-` prefix, through a
/// path separator or a shell character, is refused.
fn helper_program(helper: &str) -> Result<String, Error> {
    if helper
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Ok(format!("docker-credential-{helper}"));
    }
    Err(Error::CredentialHelperFailed {
        helper: helper.to_owned(),
        reason: format!(
            "invalid credential helper name: {helper:?} \
             (must be alphanumeric, hyphens and dots only)"
        ),
    })
}

/// The credentials a helper's `output` holds, or none when it reports it
/// holds nothing for the server. Its stderr is cut to the most of a
/// program's output an error keeps, and a refused value from its stdout is
/// left out.
fn helper_credentials(
    program: String,
    registry: &str,
    output: &std::process::Output,
) -> Result<Option<Credentials>, Error> {
    if !output.status.success() {
        if String::from_utf8_lossy(&output.stdout).trim() == CREDENTIALS_NOT_FOUND {
            tracing::debug!(helper = %program, registry, "credential helper holds no credentials");
            return Ok(None);
        }
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

    Ok(Some(Credentials::Basic {
        username: resp.username,
        password: resp.secret,
    }))
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

    /// A helper that holds nothing for the server answers none, one that
    /// fails has its stderr cut to what can be quoted, and one whose answer
    /// is mistyped is refused without the refused value. Breaks if a
    /// not-found answer is taken as a failure, the stderr is kept past the
    /// cap, or the value is quoted.
    #[cfg(unix)]
    #[test]
    fn a_helper_failure_is_cut_to_what_can_be_quoted() {
        use std::os::unix::process::ExitStatusExt;

        let output = |code: i32, stdout: &str, stderr: String| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.into_bytes(),
        };
        let not_found = output(
            1,
            "credentials not found in native keychain\n",
            String::new(),
        );
        assert!(
            helper_credentials("helper".into(), "r", &not_found)
                .unwrap()
                .is_none()
        );
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

    /// Credentials resolve from the `auths` entry for the registry, with or
    /// without a scheme, under any Docker Hub alias, from an encoded `auth`
    /// or a separate username and password; an empty entry or no entry
    /// resolves to none. Breaks if a lookup misses an entry it should find
    /// or finds one it should not.
    #[tokio::test]
    async fn credentials_resolve_from_the_auths_entry() {
        let user_pass = r#"{"auths": {"ghcr.io": {"auth": "dXNlcjpwYXNz"}}}"#;
        let hub = r#"{"auths": {"https://index.docker.io/v1/": {"auth": "ZG9ja2VyOnNlY3JldA=="}}}"#;
        let cases = [
            (user_pass, "ghcr.io", Some(("user", "pass"))),
            (user_pass, "quay.io", None),
            (
                r#"{"auths": {"https://ghcr.io": {"auth": "dXNlcjpwYXNz"}}}"#,
                "ghcr.io",
                Some(("user", "pass")),
            ),
            (hub, "docker.io", Some(("docker", "secret"))),
            (hub, "registry-1.docker.io", Some(("docker", "secret"))),
            (
                r#"{"auths": {"custom.io": {"username": "myuser", "password": "mypass"}}}"#,
                "custom.io",
                Some(("myuser", "mypass")),
            ),
            (r#"{"auths": {"ghcr.io": {"auth": ""}}}"#, "ghcr.io", None),
            (r#"{"auths": {"ghcr.io": {}}}"#, "ghcr.io", None),
        ];
        for (json, registry, expected) in cases {
            let config: DockerConfig = serde_json::from_str(json).unwrap();
            let expected = expected.map(|(username, password)| Credentials::Basic {
                username: username.into(),
                password: password.into(),
            });
            assert!(
                resolve_from_docker_config(&config, registry).await.unwrap() == expected,
                "{registry} in {json}"
            );
        }
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
    fn is_docker_hub_variants() {
        assert!(is_docker_hub("docker.io"));
        assert!(is_docker_hub("index.docker.io"));
        assert!(is_docker_hub("registry-1.docker.io"));
        assert!(is_docker_hub("https://index.docker.io/v1/"));
        assert!(!is_docker_hub("ghcr.io"));
        assert!(!is_docker_hub("quay.io"));
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

    /// A registry's own helper, under any Docker Hub alias, is asked
    /// before the default store, an empty name is no helper, and a helper
    /// is asked about Docker Hub under its index server. Breaks if the
    /// store shadows a registry's helper, an empty name is run, or Docker
    /// Hub is asked about under a name Docker never stores it by.
    #[test]
    fn the_helper_docker_would_ask() {
        let cases = [
            (
                r#"{"credHelpers": {"ghcr.io": "gh"}, "credsStore": "desktop"}"#,
                "ghcr.io",
                Some("gh"),
                "ghcr.io",
            ),
            (
                r#"{"credHelpers": {"https://index.docker.io/v1/": "hub"}}"#,
                "docker.io",
                Some("hub"),
                "https://index.docker.io/v1/",
            ),
            (
                r#"{"credHelpers": {"quay.io": "quay"}, "credsStore": "desktop"}"#,
                "registry-1.docker.io",
                Some("desktop"),
                "https://index.docker.io/v1/",
            ),
            (
                r#"{"auths": {"ghcr.io": {"auth": "dXNlcjpwYXNz"}}}"#,
                "ghcr.io",
                None,
                "ghcr.io",
            ),
            (r#"{"credsStore": ""}"#, "ghcr.io", None, "ghcr.io"),
            (
                r#"{"credHelpers": {"ghcr.io": ""}, "credsStore": "desktop"}"#,
                "ghcr.io",
                None,
                "ghcr.io",
            ),
        ];
        for (json, registry, helper, server) in cases {
            let config: DockerConfig = serde_json::from_str(json).unwrap();
            assert_eq!(
                helper_for(&config, registry),
                helper,
                "{registry} in {json}"
            );
            assert_eq!(server_address(registry), server, "{registry}");
        }
    }

    /// A helper, the registry's own or the default store, answers in place
    /// of the `auths` entry, and its failure is the answer. Breaks if a
    /// stale `auths` entry is sent in place of the helper's credentials.
    #[tokio::test]
    async fn a_helper_shadows_the_auths_entry() {
        for json in [
            r#"{"auths": {"ghcr.io": {"auth": "dXNlcjpwYXNz"}}, "credHelpers": {"ghcr.io": "ocync-absent"}}"#,
            r#"{"auths": {"ghcr.io": {"auth": "dXNlcjpwYXNz"}}, "credsStore": "ocync-absent"}"#,
        ] {
            let config: DockerConfig = serde_json::from_str(json).unwrap();
            let err = resolve_from_docker_config(&config, "ghcr.io")
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::CredentialHelperFailed { helper, .. } if helper == "docker-credential-ocync-absent"),
                "{json}: {err}"
            );
        }
    }

    /// A helper name runs its `docker-credential-` program, and one that
    /// could reach another program is refused. Breaks if a path or shell
    /// character gets through, or a real helper name, WSL's `desktop.exe`
    /// among them, is refused.
    #[test]
    fn a_helper_name_runs_only_its_own_program() {
        for name in ["gcloud", "ecr-login", "osxkeychain", "desktop.exe"] {
            assert_eq!(
                helper_program(name).unwrap(),
                format!("docker-credential-{name}")
            );
        }
        for name in ["../../evil", "helper;rm -rf /", "a/b"] {
            let err = helper_program(name).unwrap_err();
            assert!(
                err.to_string().contains("invalid credential helper name"),
                "{name}: {err}"
            );
        }
    }
}
