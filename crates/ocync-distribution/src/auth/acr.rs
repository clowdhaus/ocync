//! Azure Container Registry authentication provider.
//!
//! Acquires Azure AD credentials via a four-source credential chain
//! (extracted from `azure_identity` patterns, zero external deps), then
//! exchanges them through ACR's proprietary OAuth2 endpoints:
//!
//! 1. AAD credential -> access token (client secret, workload identity,
//!    managed identity, or Azure CLI)
//! 2. `POST /oauth2/exchange` -> ACR refresh token (~3h)
//! 3. `POST /oauth2/token` -> scoped ACR access token (~75min)
//!
//! Sovereign clouds (China `.azurecr.cn`, US Gov `.azurecr.us`) are
//! supported via AAD authority and resource endpoint mapping.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::ecr::SdkCredentialCache;
use super::token_exchange::no_redirect_http_client;
use super::{AuthProvider, Scope, Token, scopes_cache_key};
use crate::error::Error;

/// Decode the payload segment of a JWT token as JSON.
fn decode_jwt_payload(token: &str) -> Result<serde_json::Value, Error> {
    let payload = token.split('.').nth(1).ok_or_else(|| Error::AuthFailed {
        registry: "ACR".into(),
        reason: "JWT has fewer than 2 segments".into(),
    })?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| Error::AuthFailed {
            registry: "ACR".into(),
            reason: format!("JWT payload base64 decode failed: {e}"),
        })?;
    serde_json::from_slice(&decoded).map_err(|e| Error::AuthFailed {
        registry: "ACR".into(),
        reason: format!("JWT payload is not valid JSON: {e}"),
    })
}

/// Extract the `exp` claim from a JWT as a Unix timestamp.
fn extract_jwt_exp(token: &str) -> Result<i64, Error> {
    let json = decode_jwt_payload(token)?;
    json.get("exp")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| Error::AuthFailed {
            registry: "ACR".into(),
            reason: "JWT missing 'exp' claim".into(),
        })
}

/// Compute remaining TTL from a Unix epoch timestamp.
fn ttl_from_unix_exp(exp: i64) -> Duration {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs() as i64;
    let remaining = exp - now;
    if remaining > 0 {
        Duration::from_secs(remaining as u64)
    } else {
        Duration::ZERO
    }
}

/// Map ACR registry hostname to the matching Azure AD authority and
/// container registry resource endpoint.
///
/// Sovereign clouds use different AAD login endpoints and resource URIs.
/// Expects a DNS hostname (not an IP address) -- enforced by the ACR
/// regex in `detect_provider_kind()`.
fn azure_endpoints(registry_host: &str) -> (&'static str, &'static str) {
    // Strip port if present -- bare_hostname() preserves ports
    // (e.g. "myregistry.azurecr.cn:443") which would break the
    // suffix match for sovereign clouds.
    let host = registry_host
        .rsplit_once(':')
        .map_or(registry_host, |(h, _)| h);
    if host.ends_with(".azurecr.cn") {
        (
            "login.chinacloudapi.cn",
            "https://containerregistry.azure.cn",
        )
    } else if host.ends_with(".azurecr.us") {
        (
            "login.microsoftonline.us",
            "https://containerregistry.azure.us",
        )
    } else {
        (
            "login.microsoftonline.com",
            "https://containerregistry.azure.net",
        )
    }
}

/// Azure AD token response from the Entra ID v2.0 token endpoint.
///
/// Used by `ClientSecretCredential` and `WorkloadIdentityCredential`.
#[derive(Deserialize)]
struct EntraTokenResponse {
    access_token: String,
}

/// Azure AD token response from the IMDS metadata endpoint.
///
/// Used by `ManagedIdentityCredential`.
#[derive(Deserialize)]
struct ImdsTokenResponse {
    access_token: String,
}

/// Azure CLI `az account get-access-token` JSON output.
#[derive(Deserialize)]
struct CliTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
}

/// Abstraction over Azure AD token acquisition for testability.
///
/// The real implementation chains four credential sources (extracted from
/// `azure_identity` patterns). Tests inject a mock that returns canned tokens.
trait AzureTokenSource: Send + Sync + fmt::Debug {
    /// Acquire an Azure AD access token string.
    fn get_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>>;

    /// Reset cached credential source so the chain re-probes.
    /// Called on `invalidate()` to support credential rotation in watch
    /// mode. Default no-op for test mocks. Exceeds upstream which never
    /// resets the cached source.
    fn reset(&self) {}
}

/// Prevents credential leakage through error messages. Upstream
/// `azure_identity` dumps full response bodies -- we exceed upstream
/// by truncating. Matches `token_exchange.rs` discipline of
/// status-code-only errors.
fn truncate_body(body: &str) -> &str {
    if body.len() <= 200 {
        return body;
    }
    // Find the last char boundary at or before byte 200 to avoid
    // panicking on multi-byte UTF-8 sequences.
    let mut end = 200;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// ACR refresh token response from `/oauth2/exchange`.
#[derive(Deserialize)]
struct RefreshTokenResponse {
    refresh_token: String,
}

/// ACR access token response from `/oauth2/token`.
#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: String,
}

/// POST a form to an ACR `OAuth2` endpoint and deserialize the response.
///
/// Shared by both `/oauth2/exchange` (AAD -> refresh) and `/oauth2/token`
/// (refresh -> access). Uses the no-redirect HTTP client to prevent
/// credential forwarding via 307 redirects.
async fn exchange_post<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<T, Error> {
    let url = format!("{base_url}{endpoint}");
    let response = http
        .post(&url)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::AuthFailed {
            registry: service.to_owned(),
            reason: format!("ACR {endpoint} request failed: {e}"),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::AuthFailed {
            registry: service.to_owned(),
            reason: format!("ACR {endpoint} returned {status}: {}", truncate_body(&body)),
        });
    }

    response.json().await.map_err(|e| Error::AuthFailed {
        registry: service.to_owned(),
        reason: format!("ACR {endpoint} response parse failed: {e}"),
    })
}

/// Exchange an AAD access token for an ACR refresh token.
///
/// `POST /oauth2/exchange` with `grant_type=access_token`.
/// The `tenant` parameter is omitted -- ACR derives it from the AAD
/// token's `tid` claim server-side.
async fn exchange_refresh_token(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    aad_access_token: &str,
) -> Result<String, Error> {
    let form = [
        ("grant_type", "access_token"),
        ("service", service),
        ("access_token", aad_access_token),
    ];
    let resp: RefreshTokenResponse =
        exchange_post(http, base_url, service, "/oauth2/exchange", &form).await?;
    Ok(resp.refresh_token)
}

/// Exchange an ACR refresh token for a scoped access token.
///
/// `POST /oauth2/token` with `grant_type=refresh_token`.
async fn exchange_access_token(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    refresh_token: &str,
    scope: &str,
) -> Result<String, Error> {
    let form = [
        ("grant_type", "refresh_token"),
        ("service", service),
        ("scope", scope),
        ("refresh_token", refresh_token),
    ];
    let resp: AccessTokenResponse =
        exchange_post(http, base_url, service, "/oauth2/token", &form).await?;
    Ok(resp.access_token)
}

/// Internal error for credential chain control flow.
enum CredentialError {
    /// Credential source is not configured (skip to next).
    NotConfigured,
    /// Credential source is configured but failed (stop and report).
    Failed(String),
}

/// Source index constants for the credential chain.
const SOURCE_CLIENT_SECRET: usize = 0;
const SOURCE_WORKLOAD_IDENTITY: usize = 1;
const SOURCE_MANAGED_IDENTITY: usize = 2;
const SOURCE_AZURE_CLI: usize = 3;
/// Sentinel for "no cached source". Uses `usize::MAX` rather than
/// `Option<usize>` because `AtomicUsize` cannot hold `Option`.
const SOURCE_NONE: usize = usize::MAX;

/// Azure credential chain that tries four credential sources in order.
///
/// Extracted from `azure_identity` crate patterns (zero external deps).
/// On first success, caches the winning source index and uses only that
/// source for subsequent calls.
///
/// Unlike upstream `azure_identity` which never resets the cached source,
/// `reset_cached_source()` allows re-probing after `invalidate()` --
/// needed for credential rotation in watch mode.
struct AzureCredentialChain {
    authority: &'static str,
    resource: &'static str,
    cached_source: AtomicUsize,
    http: reqwest::Client,
}

impl fmt::Debug for AzureCredentialChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureCredentialChain")
            .field("authority", &self.authority)
            .finish_non_exhaustive()
    }
}

impl AzureCredentialChain {
    fn new(authority: &'static str, resource: &'static str) -> Self {
        crate::install_crypto_provider();
        let http = reqwest::Client::builder()
            .user_agent(concat!("ocync/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build Azure credential HTTP client");
        Self {
            authority,
            resource,
            cached_source: AtomicUsize::new(SOURCE_NONE),
            http,
        }
    }

    fn reset_cached_source(&self) {
        self.cached_source.store(SOURCE_NONE, Ordering::Relaxed);
    }

    /// POST to the Entra ID v2.0 token endpoint and parse the response.
    async fn post_entra_token(
        http: &reqwest::Client,
        url: &str,
        form: &[(&str, &str)],
        source: &str,
    ) -> Result<String, CredentialError> {
        let resp =
            http.post(url).form(form).send().await.map_err(|e| {
                CredentialError::Failed(format!("{source} token request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CredentialError::Failed(format!(
                "{source} auth failed: {}",
                truncate_body(&body)
            )));
        }

        let body: EntraTokenResponse = resp
            .json()
            .await
            .map_err(|e| CredentialError::Failed(format!("{source} response parse failed: {e}")))?;

        Ok(body.access_token)
    }

    async fn try_client_secret(&self) -> Result<String, CredentialError> {
        let tenant =
            std::env::var("AZURE_TENANT_ID").map_err(|_| CredentialError::NotConfigured)?;
        let client_id =
            std::env::var("AZURE_CLIENT_ID").map_err(|_| CredentialError::NotConfigured)?;
        let client_secret =
            std::env::var("AZURE_CLIENT_SECRET").map_err(|_| CredentialError::NotConfigured)?;

        let url = format!("https://{}/{tenant}/oauth2/v2.0/token", self.authority);
        let scope = format!("{resource}/.default", resource = self.resource);
        let form = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("grant_type", "client_credentials"),
            ("scope", scope.as_str()),
        ];

        Self::post_entra_token(&self.http, &url, &form, "client secret").await
    }

    async fn try_workload_identity(&self) -> Result<String, CredentialError> {
        let tenant =
            std::env::var("AZURE_TENANT_ID").map_err(|_| CredentialError::NotConfigured)?;
        let client_id =
            std::env::var("AZURE_CLIENT_ID").map_err(|_| CredentialError::NotConfigured)?;
        let token_file = std::env::var("AZURE_FEDERATED_TOKEN_FILE")
            .map_err(|_| CredentialError::NotConfigured)?;

        let assertion = tokio::fs::read_to_string(&token_file).await.map_err(|e| {
            CredentialError::Failed(format!(
                "failed to read federated token file '{token_file}': {e}"
            ))
        })?;

        let url = format!("https://{}/{tenant}/oauth2/v2.0/token", self.authority);
        let scope = format!("{resource}/.default", resource = self.resource);
        let form = [
            ("client_id", client_id.as_str()),
            ("client_assertion", assertion.trim()),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
            ),
            ("grant_type", "client_credentials"),
            ("scope", scope.as_str()),
        ];

        Self::post_entra_token(&self.http, &url, &form, "workload identity").await
    }

    async fn try_managed_identity(&self) -> Result<String, CredentialError> {
        // Safety: `resource` is always one of three hardcoded `&'static str`
        // values from `azure_endpoints()` -- none contain `&` or `#`, so
        // string interpolation into the query string is safe. IMDS accepts
        // the unencoded form (matching the Azure SDK's behavior).
        let resource = self.resource;
        let url = format!(
            "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2019-08-01&resource={resource}"
        );

        let resp = self
            .http
            .get(&url)
            .header("metadata", "true")
            .send()
            .await
            .map_err(|_| CredentialError::NotConfigured)?;

        let status = resp.status();
        if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED
        {
            let body = resp.text().await.unwrap_or_default();
            return Err(CredentialError::Failed(format!(
                "managed identity not configured: {}",
                truncate_body(&body)
            )));
        }
        if !status.is_success() {
            return Err(CredentialError::Failed(format!("IMDS returned {status}")));
        }

        let body: ImdsTokenResponse = resp
            .json()
            .await
            .map_err(|_| CredentialError::Failed("IMDS response parse failed".into()))?;

        Ok(body.access_token)
    }

    async fn try_azure_cli(&self) -> Result<String, CredentialError> {
        let output = tokio::process::Command::new("az")
            .args([
                "account",
                "get-access-token",
                "--resource",
                self.resource,
                "-o",
                "json",
            ])
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CredentialError::NotConfigured
                } else {
                    CredentialError::Failed(format!("az CLI execution failed: {e}"))
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CredentialError::Failed(format!(
                "az CLI failed: {}",
                truncate_body(&stderr)
            )));
        }

        let body: CliTokenResponse = serde_json::from_slice(&output.stdout)
            .map_err(|e| CredentialError::Failed(format!("az CLI output parse failed: {e}")))?;

        Ok(body.access_token)
    }

    async fn try_source(&self, index: usize) -> Result<String, CredentialError> {
        match index {
            SOURCE_CLIENT_SECRET => self.try_client_secret().await,
            SOURCE_WORKLOAD_IDENTITY => self.try_workload_identity().await,
            SOURCE_MANAGED_IDENTITY => self.try_managed_identity().await,
            SOURCE_AZURE_CLI => self.try_azure_cli().await,
            _ => unreachable!("invalid source index"),
        }
    }

    fn source_name(index: usize) -> &'static str {
        match index {
            SOURCE_CLIENT_SECRET => "ClientSecret",
            SOURCE_WORKLOAD_IDENTITY => "WorkloadIdentity",
            SOURCE_MANAGED_IDENTITY => "ManagedIdentity",
            SOURCE_AZURE_CLI => "AzureCli",
            _ => "Unknown",
        }
    }
}

impl AzureTokenSource for AzureCredentialChain {
    fn reset(&self) {
        self.reset_cached_source();
    }

    fn get_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
        Box::pin(async move {
            let cached = self.cached_source.load(Ordering::Relaxed);

            if cached != SOURCE_NONE {
                let name = Self::source_name(cached);
                return self
                    .try_source(cached)
                    .await
                    .map_err(|_| Error::AuthFailed {
                        registry: "ACR".into(),
                        reason: format!("{name} credential failed"),
                    });
            }

            let mut errors = Vec::new();
            for index in [
                SOURCE_CLIENT_SECRET,
                SOURCE_WORKLOAD_IDENTITY,
                SOURCE_MANAGED_IDENTITY,
                SOURCE_AZURE_CLI,
            ] {
                let name = Self::source_name(index);
                match self.try_source(index).await {
                    Ok(token) => {
                        tracing::debug!(source = name, "Azure credential acquired");
                        self.cached_source.store(index, Ordering::Relaxed);
                        return Ok(token);
                    }
                    Err(CredentialError::NotConfigured) => {
                        tracing::debug!(source = name, "not configured, skipping");
                    }
                    Err(CredentialError::Failed(reason)) => {
                        tracing::warn!(source = name, "credential failed");
                        errors.push(format!("{name}: {reason}"));
                    }
                }
            }

            Err(Error::AuthFailed {
                registry: "ACR".into(),
                reason: if errors.is_empty() {
                    "no Azure credential sources configured (set AZURE_CLIENT_ID + AZURE_CLIENT_SECRET + AZURE_TENANT_ID, or run on Azure with managed identity, or install Azure CLI)".into()
                } else {
                    format!(
                        "all Azure credential sources failed:\n  {}",
                        errors.join("\n  ")
                    )
                },
            })
        })
    }
}

/// Default ACR refresh token TTL fallback (3 hours).
const ACR_REFRESH_TOKEN_DEFAULT_TTL: Duration = Duration::from_secs(3 * 60 * 60);

/// Default ACR access token TTL fallback (60 minutes).
const ACR_ACCESS_TOKEN_DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);

/// ACR authentication provider.
///
/// Acquires an Azure AD token via [`AzureTokenSource`], exchanges it for an
/// ACR refresh token (~3h TTL), then exchanges the refresh token for a
/// scoped ACR access token per registry scope.
pub struct AcrAuth {
    base_url: String,
    service: String,
    exchange_http: reqwest::Client,
    api: Box<dyn AzureTokenSource>,
    refresh_cache: SdkCredentialCache<String>,
    token_cache: Mutex<HashMap<String, Token>>,
}

impl fmt::Debug for AcrAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcrAuth")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AcrAuth {
    /// Construct a new `AcrAuth` for the given ACR hostname.
    pub async fn new(hostname: &str) -> Result<Self, Error> {
        let (authority, resource) = azure_endpoints(hostname);
        let api = Box::new(AzureCredentialChain::new(authority, resource));
        Ok(Self {
            base_url: format!("https://{hostname}"),
            service: hostname.to_owned(),
            exchange_http: no_redirect_http_client(),
            api,
            refresh_cache: SdkCredentialCache::new(),
            token_cache: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    fn with_api(base_url: &str, service: &str, api: impl AzureTokenSource + 'static) -> Self {
        crate::install_crypto_provider();
        Self {
            base_url: base_url.to_owned(),
            service: service.to_owned(),
            exchange_http: no_redirect_http_client(),
            api: Box::new(api),
            refresh_cache: SdkCredentialCache::new(),
            token_cache: Mutex::new(HashMap::new()),
        }
    }

    async fn ensure_refresh_token(&self) -> Result<String, Error> {
        self.refresh_cache
            .get_or_refresh("acr-refresh-token", async {
                tracing::debug!(service = %self.service, "ACR refresh token expired, re-acquiring");
                let aad_access_token = self.api.get_token().await?;
                let refresh_token = exchange_refresh_token(
                    &self.exchange_http,
                    &self.base_url,
                    &self.service,
                    &aad_access_token,
                )
                .await?;
                let ttl = extract_jwt_exp(&refresh_token)
                    .ok()
                    .map(ttl_from_unix_exp)
                    .unwrap_or(ACR_REFRESH_TOKEN_DEFAULT_TTL);
                tracing::debug!(service = %self.service, ttl_secs = ttl.as_secs(), "ACR refresh token acquired");
                Ok((refresh_token, ttl))
            })
            .await
    }
}

impl AuthProvider for AcrAuth {
    fn name(&self) -> &'static str {
        "acr"
    }

    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        let scopes = scopes.to_vec();
        Box::pin(async move {
            let key = scopes_cache_key(&scopes);
            let mut cache = self.token_cache.lock().await;
            if let Some(token) = cache.get(&key).filter(|t| t.is_valid()) {
                tracing::debug!(service = %self.service, scope = %key, "ACR token cache hit");
                return Ok(token.clone());
            }
            let refresh_token = self.ensure_refresh_token().await?;
            let scope_str = scopes
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            tracing::debug!(service = %self.service, scope = %key, "ACR token cache miss, exchanging");
            let access_token = exchange_access_token(
                &self.exchange_http,
                &self.base_url,
                &self.service,
                &refresh_token,
                &scope_str,
            )
            .await?;
            let ttl = extract_jwt_exp(&access_token)
                .ok()
                .map(ttl_from_unix_exp)
                .unwrap_or(ACR_ACCESS_TOKEN_DEFAULT_TTL);
            let token = Token::with_ttl(access_token, ttl);
            cache.insert(key, token.clone());
            Ok(token)
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cache = self.token_cache.lock().await;
            tracing::debug!(service = %self.service, entries = cache.len(), "invalidating ACR token cache");
            cache.clear();
            drop(cache);
            self.refresh_cache.clear().await;
            self.api.reset();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;

    /// Build a fake JWT with a given JSON payload (no signature verification).
    fn fake_jwt(payload_json: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload_json);
        format!("{header}.{payload}.fake-signature")
    }

    #[test]
    fn decode_jwt_payload_extracts_claims() {
        let token = fake_jwt(r#"{"tid":"72f988bf-86f1-41af-91ab-2d7cd011db47","sub":"user"}"#);
        let json = decode_jwt_payload(&token).unwrap();
        assert_eq!(
            json.get("tid").and_then(|v| v.as_str()),
            Some("72f988bf-86f1-41af-91ab-2d7cd011db47")
        );
    }

    #[test]
    fn extract_exp_from_acr_token() {
        let token = fake_jwt(r#"{"exp":1700000000,"iss":"Azure Container Registry"}"#);
        let exp = extract_jwt_exp(&token).unwrap();
        assert_eq!(exp, 1700000000);
    }

    #[test]
    fn extract_exp_missing() {
        let token = fake_jwt(r#"{"sub":"user"}"#);
        let err = extract_jwt_exp(&token).unwrap_err();
        assert!(err.to_string().contains("missing 'exp' claim"));
    }

    #[test]
    fn decode_jwt_malformed() {
        let err = decode_jwt_payload("not-a-jwt").unwrap_err();
        assert!(err.to_string().contains("fewer than 2 segments"));
    }

    #[test]
    fn decode_jwt_invalid_base64() {
        let err = decode_jwt_payload("header.!!!invalid!!!.sig").unwrap_err();
        assert!(err.to_string().contains("base64 decode failed"));
    }

    #[test]
    fn decode_jwt_invalid_json() {
        let payload = URL_SAFE_NO_PAD.encode("not json");
        let token = format!("header.{payload}.sig");
        let err = decode_jwt_payload(&token).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn azure_endpoints_public() {
        let (authority, resource) = azure_endpoints("myregistry.azurecr.io");
        assert_eq!(authority, "login.microsoftonline.com");
        assert_eq!(resource, "https://containerregistry.azure.net");
    }

    #[test]
    fn azure_endpoints_china() {
        let (authority, resource) = azure_endpoints("myregistry.azurecr.cn");
        assert_eq!(authority, "login.chinacloudapi.cn");
        assert_eq!(resource, "https://containerregistry.azure.cn");
    }

    #[test]
    fn azure_endpoints_usgov() {
        let (authority, resource) = azure_endpoints("myregistry.azurecr.us");
        assert_eq!(authority, "login.microsoftonline.us");
        assert_eq!(resource, "https://containerregistry.azure.us");
    }

    #[test]
    fn azure_endpoints_strips_port_for_sovereign_match() {
        let (authority, _) = azure_endpoints("myregistry.azurecr.cn:443");
        assert_eq!(authority, "login.chinacloudapi.cn");
        let (authority, _) = azure_endpoints("myregistry.azurecr.us:443");
        assert_eq!(authority, "login.microsoftonline.us");
    }

    #[test]
    fn truncate_body_short() {
        assert_eq!(truncate_body("short"), "short");
    }

    #[test]
    fn truncate_body_at_multibyte_boundary() {
        // 198 ASCII chars + a 3-byte UTF-8 char (euro sign) = 201 bytes.
        // Naive &body[..200] would slice mid-char and panic.
        let body = format!("{}x{}", "a".repeat(197), '\u{20AC}');
        assert_eq!(body.len(), 201);
        let truncated = truncate_body(&body);
        assert!(truncated.len() <= 200);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    /// Mock Azure token source that returns pre-configured access token strings.
    #[derive(Debug)]
    struct MockAzureTokenSource {
        tokens: Mutex<VecDeque<String>>,
        /// Shared counter so tests can verify `invalidate()` calls
        /// `api.reset()` even after the mock is moved into `AcrAuth`.
        reset_count: std::sync::Arc<AtomicUsize>,
    }

    impl MockAzureTokenSource {
        fn succeeding(access_token: &str) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(
                    std::iter::repeat_n(access_token.to_owned(), 10).collect::<Vec<_>>(),
                )),
                reset_count: std::sync::Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_tokens(tokens: Vec<String>) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(tokens)),
                reset_count: std::sync::Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AzureTokenSource for MockAzureTokenSource {
        fn get_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
            Box::pin(async move {
                self.tokens
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| Error::AuthFailed {
                        registry: "mock-acr".into(),
                        reason: "mock: no more tokens".into(),
                    })
            })
        }

        fn reset(&self) {
            self.reset_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    use wiremock::MockServer;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn acr_exchange_returns_refresh_token() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .and(body_string_contains("grant_type=access_token"))
            .and(body_string_contains("service=myregistry.azurecr.io"))
            .and(body_string_contains("access_token=aad-tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"refresh_token": "acr-refresh-tok"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let refresh = exchange_refresh_token(
            &no_redirect_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "aad-tok",
        )
        .await
        .unwrap();
        assert_eq!(refresh, "acr-refresh-tok");
    }

    #[tokio::test]
    async fn acr_exchange_propagates_401() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let err = exchange_refresh_token(
            &no_redirect_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "bad-token",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    /// A redirect from an exchange endpoint must be treated as an error,
    /// not followed -- prevents credential forwarding via 307 replay.
    #[tokio::test]
    async fn exchange_rejects_redirect() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", "http://evil.example.com/steal"),
            )
            .mount(&server)
            .await;

        let err = exchange_refresh_token(
            &no_redirect_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "aad-tok",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("307"), "got: {err}");
    }

    /// A 200 response with malformed JSON must produce a parse error,
    /// not panic or return garbage.
    #[tokio::test]
    async fn exchange_rejects_malformed_json() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = exchange_refresh_token(
            &no_redirect_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "aad-tok",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("response parse failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn acr_token_returns_access_token() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("scope=repository%3Amyrepo%3Apull"))
            .and(body_string_contains("refresh_token=acr-refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "acr-access-tok"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let access = exchange_access_token(
            &no_redirect_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "acr-refresh",
            "repository:myrepo:pull",
        )
        .await
        .unwrap();
        assert_eq!(access, "acr-access-tok");
    }

    /// Negative test for /oauth2/token error path.
    #[tokio::test]
    async fn acr_token_propagates_401() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad refresh"))
            .mount(&server)
            .await;

        let err = exchange_access_token(
            &no_redirect_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "bad-refresh",
            "repository:repo:pull",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    /// Build a fake JWT with an `exp` claim set to `now + ttl_secs`.
    fn fake_jwt_with_exp(payload_extra: &str, ttl_secs: i64) -> String {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let exp = now + ttl_secs;
        let payload = format!(
            r#"{{"exp":{exp}{}}}"#,
            if payload_extra.is_empty() {
                String::new()
            } else {
                format!(",{payload_extra}")
            }
        );
        fake_jwt(&payload)
    }

    #[tokio::test]
    async fn auth_name() {
        let auth = AcrAuth::with_api(
            "https://myregistry.azurecr.io",
            "myregistry.azurecr.io",
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        assert_eq!(auth.name(), "acr");
    }

    #[tokio::test]
    async fn full_acr_auth_flow() {
        let server = MockServer::start().await;
        let refresh_jwt = fake_jwt_with_exp("", 10800);
        let access_jwt = fake_jwt_with_exp("", 4500);

        // Verify the AAD token flows through to the /oauth2/exchange body.
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .and(body_string_contains("access_token=aad-tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"refresh_token": &refresh_jwt})),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Verify the scope format flows through to the /oauth2/token body.
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains("scope=repository%3Amyrepo%3Apull"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access_jwt})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        let token = auth.get_token(&[Scope::pull("myrepo")]).await.unwrap();
        assert_eq!(token.value(), &access_jwt);
    }

    #[tokio::test]
    async fn access_token_cached_within_same_scope() {
        let server = MockServer::start().await;
        let refresh_jwt = fake_jwt_with_exp("", 10800);
        let access_jwt = fake_jwt_with_exp("", 4500);
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"refresh_token": &refresh_jwt})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access_jwt})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        let t1 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t1.value(), t2.value());
    }

    #[tokio::test]
    async fn refresh_token_cached_across_scopes() {
        let server = MockServer::start().await;
        let refresh_jwt = fake_jwt_with_exp("", 10800);
        let access_jwt = fake_jwt_with_exp("", 4500);
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"refresh_token": &refresh_jwt})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access_jwt})),
            )
            .expect(2)
            .mount(&server)
            .await;
        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();
    }

    #[tokio::test]
    async fn expired_access_token_triggers_re_exchange() {
        let server = MockServer::start().await;
        let refresh_jwt = fake_jwt_with_exp("", 10800);
        let expired_jwt = fake_jwt_with_exp("", 0);
        let fresh_jwt = fake_jwt_with_exp("", 4500);
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"refresh_token": &refresh_jwt})),
            )
            .mount(&server)
            .await;
        let call_count = std::sync::Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let expired_clone = expired_jwt.clone();
        let fresh_clone = fresh_jwt.clone();
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(move |_: &wiremock::Request| {
                let count = cc.fetch_add(1, Ordering::Relaxed);
                let tok = if count == 0 {
                    &expired_clone
                } else {
                    &fresh_clone
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": tok}))
            })
            .expect(2)
            .mount(&server)
            .await;
        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        let t1 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t1.value(), &expired_jwt);
        let t2 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t2.value(), &fresh_jwt);
    }

    /// When the cached refresh token expires, the next `get_token` must
    /// re-acquire from AAD (full chain: AAD -> exchange -> token).
    #[tokio::test]
    async fn expired_refresh_token_triggers_full_reacquisition() {
        let server = MockServer::start().await;
        let expired_refresh = fake_jwt_with_exp("", 0);
        let fresh_refresh = fake_jwt_with_exp("", 10800);
        let access_jwt = fake_jwt_with_exp("", 4500);

        let exchange_count = std::sync::Arc::new(AtomicUsize::new(0));
        let ec = exchange_count.clone();
        let expired_clone = expired_refresh.clone();
        let fresh_clone = fresh_refresh.clone();
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(move |_: &wiremock::Request| {
                let count = ec.fetch_add(1, Ordering::Relaxed);
                let tok = if count == 0 {
                    &expired_clone
                } else {
                    &fresh_clone
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"refresh_token": tok}))
            })
            .expect(2)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access_jwt})),
            )
            .mount(&server)
            .await;

        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        // First call: gets expired refresh token (TTL=0), then access token.
        auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        // Second call: refresh token expired in cache, must re-acquire from AAD.
        auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();
        // .expect(2) on exchange mock verifies full re-acquisition happened.
    }

    #[tokio::test]
    async fn invalidation_clears_all_caches() {
        let server = MockServer::start().await;
        let refresh_jwt = fake_jwt_with_exp("", 10800);
        let access_jwt = fake_jwt_with_exp("", 4500);
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"refresh_token": &refresh_jwt})),
            )
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access_jwt})),
            )
            .mount(&server)
            .await;

        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();

        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(reset_count.load(Ordering::Relaxed), 0);
        auth.invalidate().await;
        assert_eq!(
            reset_count.load(Ordering::Relaxed),
            1,
            "invalidate() must call api.reset()"
        );
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    #[tokio::test]
    async fn auth_propagates_aad_error() {
        let auth = AcrAuth::with_api(
            "https://myregistry.azurecr.io",
            "myregistry.azurecr.io",
            MockAzureTokenSource::with_tokens(vec![]),
        );
        let result = auth.get_token(&[Scope::pull("repo")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("mock: no more tokens"), "got: {err}");
    }

    #[test]
    fn parse_entra_token_response() {
        let json = r#"{"access_token":"tok","expires_in":3599,"ext_expires_in":3599,"token_type":"Bearer"}"#;
        let resp: EntraTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
    }

    #[test]
    fn parse_imds_token_response() {
        let json = r#"{"access_token":"tok","expires_on":"1700000000","token_type":"Bearer","resource":"https://containerregistry.azure.net"}"#;
        let resp: ImdsTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
    }

    #[test]
    fn parse_cli_token_response() {
        let json = r#"{"accessToken":"tok","expiresOn":"2026-04-23","expires_on":1745414400,"tokenType":"Bearer"}"#;
        let resp: CliTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
    }

    #[tokio::test]
    async fn mock_source_returns_sequential_tokens() {
        let source = MockAzureTokenSource::succeeding("first-call");
        let token = source.get_token().await.unwrap();
        assert_eq!(token, "first-call");
        let token2 = source.get_token().await.unwrap();
        assert_eq!(token2, "first-call");
    }

    #[tokio::test]
    async fn mock_source_empty_returns_error() {
        let source = MockAzureTokenSource::with_tokens(vec![]);
        let err = source.get_token().await.unwrap_err();
        assert!(err.to_string().contains("no more tokens"), "got: {err}");
    }
}
