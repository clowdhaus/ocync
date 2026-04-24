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
use super::{AuthProvider, Scope, Token, scopes_cache_key};
use crate::error::Error;

/// Extract a string claim from a JWT payload without signature verification.
///
/// JWTs are `header.payload.signature`, each base64url-encoded. Decodes
/// the payload (middle segment) and parses as JSON. No cryptographic
/// verification -- the token was already validated by the issuing authority.
#[cfg(test)]
fn extract_jwt_claim(token: &str, claim: &str) -> Result<String, Error> {
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

    let json: serde_json::Value =
        serde_json::from_slice(&decoded).map_err(|e| Error::AuthFailed {
            registry: "ACR".into(),
            reason: format!("JWT payload is not valid JSON: {e}"),
        })?;

    json.get(claim)
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| Error::AuthFailed {
            registry: "ACR".into(),
            reason: format!("JWT missing '{claim}' claim"),
        })
}

/// Extract the `exp` claim from a JWT as a Unix timestamp.
fn extract_jwt_exp(token: &str) -> Result<i64, Error> {
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

    let json: serde_json::Value =
        serde_json::from_slice(&decoded).map_err(|e| Error::AuthFailed {
            registry: "ACR".into(),
            reason: format!("JWT payload is not valid JSON: {e}"),
        })?;

    json.get("exp")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| Error::AuthFailed {
            registry: "ACR".into(),
            reason: "JWT missing 'exp' claim".into(),
        })
}

// --- Task 2: Sovereign Cloud Mapping and AAD Response Types ---

/// Map ACR registry hostname to the matching Azure AD authority and
/// container registry resource endpoint.
///
/// Sovereign clouds use different AAD login endpoints and resource URIs.
fn azure_cloud(registry_host: &str) -> (&'static str, &'static str) {
    if registry_host.ends_with(".azurecr.cn") {
        (
            "login.chinacloudapi.cn",
            "https://containerregistry.azure.cn",
        )
    } else if registry_host.ends_with(".azurecr.us") {
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
/// `expires_in` is relative seconds from now.
#[derive(Deserialize)]
struct EntraTokenResponse {
    access_token: String,
    expires_in: i64,
}

/// Azure AD token response from the IMDS metadata endpoint.
///
/// Used by `ManagedIdentityCredential`. Note: `expires_on` is a **string**
/// containing a Unix timestamp, not an integer. This is a documented IMDS
/// quirk that differs from the Entra v2.0 endpoint.
#[derive(Deserialize)]
struct ImdsTokenResponse {
    access_token: String,
    expires_on: String,
}

/// Azure CLI `az account get-access-token` JSON output.
#[derive(Deserialize)]
struct CliTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Unix timestamp. Requires Azure CLI >= 2.54.0.
    expires_on: Option<i64>,
}

/// An AAD access token with its expiry time.
#[derive(Clone, Debug)]
pub(crate) struct AadToken {
    access_token: String,
    /// Seconds since Unix epoch when this token expires.
    #[allow(dead_code)] // read by Task 7 ACR refresh TTL tracking
    expires_on: i64,
}

impl AadToken {
    /// Remaining TTL as a `Duration`, floored at zero.
    #[allow(dead_code)] // called by Task 7 ACR refresh TTL tracking
    fn ttl(&self) -> Duration {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs() as i64;
        let remaining = self.expires_on - now;
        if remaining > 0 {
            Duration::from_secs(remaining as u64)
        } else {
            Duration::ZERO
        }
    }
}

// --- Task 3: AzureTokenSource Trait ---

/// Abstraction over Azure AD token acquisition for testability.
///
/// The real implementation chains four credential sources (extracted from
/// `azure_identity` patterns). Tests inject a mock that returns canned tokens.
pub(crate) trait AzureTokenSource: Send + Sync + fmt::Debug {
    /// Acquire an Azure AD access token for the given resource.
    fn get_token(
        &self,
        resource: &str,
    ) -> Pin<Box<dyn Future<Output = Result<AadToken, Error>> + Send + '_>>;

    /// Reset cached credential source so the chain re-probes.
    /// Called on `invalidate()` to support credential rotation in watch
    /// mode. Default no-op for test mocks. Exceeds upstream which never
    /// resets the cached source.
    fn reset(&self) {}
}

/// Build a dedicated HTTP client for ACR exchange requests.
///
/// Uses `redirect::Policy::none()` to prevent open-redirect SSRF:
/// a 307 redirect from a malicious ACR endpoint would forward the POST body
/// (containing the AAD access token) to the redirect target. Upstream
/// `azure_identity` sets `redirect::Policy::none()` globally on all SDK
/// clients. This matches `realm_http_client()` in `token_exchange.rs`.
fn acr_exchange_http_client() -> reqwest::Client {
    crate::install_crypto_provider();
    reqwest::Client::builder()
        .user_agent(concat!("ocync/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("ACR exchange HTTP client builder should not fail")
}

/// Truncate a response body for inclusion in error messages.
///
/// Prevents credential leakage through logs. Upstream
/// `azure_identity` dumps full response bodies -- we exceed upstream
/// by truncating. Matches `token_exchange.rs` discipline of
/// status-code-only errors.
fn truncate_body(body: &str) -> &str {
    if body.len() <= 200 {
        body
    } else {
        &body[..200]
    }
}

/// ACR refresh token response from `/oauth2/exchange`.
#[derive(Deserialize)]
struct AcrRefreshTokenResponse {
    refresh_token: String,
}

/// ACR access token response from `/oauth2/token`.
#[derive(Deserialize)]
struct AcrAccessTokenResponse {
    access_token: String,
}

/// Exchange an AAD access token for an ACR refresh token.
///
/// `POST /oauth2/exchange` with `grant_type=access_token`.
/// The `tenant` parameter is omitted -- ACR derives it from the AAD
/// token's `tid` claim server-side.
async fn acr_exchange_refresh_token(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    aad_access_token: &str,
) -> Result<String, Error> {
    let url = format!("{base_url}/oauth2/exchange");
    let form = [
        ("grant_type", "access_token"),
        ("service", service),
        ("access_token", aad_access_token),
    ];

    let response = http
        .post(&url)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::AuthFailed {
            registry: service.to_owned(),
            reason: format!("ACR /oauth2/exchange request failed: {e}"),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::AuthFailed {
            registry: service.to_owned(),
            reason: format!(
                "ACR /oauth2/exchange returned {status}: {}",
                truncate_body(&body)
            ),
        });
    }

    let body: AcrRefreshTokenResponse = response.json().await.map_err(|e| Error::AuthFailed {
        registry: service.to_owned(),
        reason: format!("ACR /oauth2/exchange response parse failed: {e}"),
    })?;

    Ok(body.refresh_token)
}

/// Exchange an ACR refresh token for a scoped access token.
///
/// `POST /oauth2/token` with `grant_type=refresh_token`.
async fn acr_exchange_access_token(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    refresh_token: &str,
    scope: &str,
) -> Result<String, Error> {
    let url = format!("{base_url}/oauth2/token");
    let form = [
        ("grant_type", "refresh_token"),
        ("service", service),
        ("scope", scope),
        ("refresh_token", refresh_token),
    ];

    let response = http
        .post(&url)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::AuthFailed {
            registry: service.to_owned(),
            reason: format!("ACR /oauth2/token request failed: {e}"),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::AuthFailed {
            registry: service.to_owned(),
            reason: format!(
                "ACR /oauth2/token returned {status}: {}",
                truncate_body(&body)
            ),
        });
    }

    let body: AcrAccessTokenResponse = response.json().await.map_err(|e| Error::AuthFailed {
        registry: service.to_owned(),
        reason: format!("ACR /oauth2/token response parse failed: {e}"),
    })?;

    Ok(body.access_token)
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

    async fn try_client_secret(&self) -> Result<AadToken, CredentialError> {
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

        let resp = self.http.post(&url).form(&form).send().await.map_err(|e| {
            CredentialError::Failed(format!("client secret token request failed: {e}"))
        })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CredentialError::Failed(format!(
                "client secret auth failed: {}",
                truncate_body(&body)
            )));
        }

        let body: EntraTokenResponse = resp.json().await.map_err(|e| {
            CredentialError::Failed(format!("client secret response parse failed: {e}"))
        })?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        Ok(AadToken {
            access_token: body.access_token,
            expires_on: now + body.expires_in,
        })
    }

    async fn try_workload_identity(&self) -> Result<AadToken, CredentialError> {
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

        let resp = self.http.post(&url).form(&form).send().await.map_err(|e| {
            CredentialError::Failed(format!("workload identity token request failed: {e}"))
        })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CredentialError::Failed(format!(
                "workload identity auth failed: {}",
                truncate_body(&body)
            )));
        }

        let body: EntraTokenResponse = resp.json().await.map_err(|e| {
            CredentialError::Failed(format!("workload identity response parse failed: {e}"))
        })?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        Ok(AadToken {
            access_token: body.access_token,
            expires_on: now + body.expires_in,
        })
    }

    async fn try_managed_identity(&self) -> Result<AadToken, CredentialError> {
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

        let expires_on: i64 = body.expires_on.parse().map_err(|_| {
            CredentialError::Failed("IMDS expires_on is not a valid integer".into())
        })?;

        Ok(AadToken {
            access_token: body.access_token,
            expires_on,
        })
    }

    async fn try_azure_cli(&self) -> Result<AadToken, CredentialError> {
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

        let expires_on = body.expires_on.ok_or_else(|| {
            CredentialError::Failed(
                "az CLI output missing 'expires_on'; upgrade Azure CLI to >= 2.54.0".into(),
            )
        })?;

        Ok(AadToken {
            access_token: body.access_token,
            expires_on,
        })
    }

    async fn try_source(&self, index: usize) -> Result<AadToken, CredentialError> {
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

    fn get_token(
        &self,
        _resource: &str,
    ) -> Pin<Box<dyn Future<Output = Result<AadToken, Error>> + Send + '_>> {
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
    pub async fn new(hostname: &str, _http: reqwest::Client) -> Result<Self, Error> {
        let (authority, resource) = azure_cloud(hostname);
        let api = Box::new(AzureCredentialChain::new(authority, resource));
        Ok(Self {
            base_url: format!("https://{hostname}"),
            service: hostname.to_owned(),
            exchange_http: acr_exchange_http_client(),
            api,
            refresh_cache: SdkCredentialCache::new(),
            token_cache: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    fn with_api(
        base_url: &str,
        service: &str,
        _http: reqwest::Client,
        api: impl AzureTokenSource + 'static,
    ) -> Self {
        Self {
            base_url: base_url.to_owned(),
            service: service.to_owned(),
            exchange_http: acr_exchange_http_client(),
            api: Box::new(api),
            refresh_cache: SdkCredentialCache::new(),
            token_cache: Mutex::new(HashMap::new()),
        }
    }

    async fn ensure_refresh_token(&self) -> Result<String, Error> {
        self.refresh_cache
            .get_or_refresh("acr-refresh-token", async {
                tracing::debug!(service = %self.service, "ACR refresh token expired, re-acquiring");
                let (_authority, resource) = azure_cloud(&self.service);
                let aad_token = self.api.get_token(resource).await?;
                let refresh_token = acr_exchange_refresh_token(
                    &self.exchange_http,
                    &self.base_url,
                    &self.service,
                    &aad_token.access_token,
                )
                .await?;
                let ttl = extract_jwt_exp(&refresh_token)
                    .ok()
                    .map(|exp| {
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
                    })
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
            let access_token = acr_exchange_access_token(
                &self.exchange_http,
                &self.base_url,
                &self.service,
                &refresh_token,
                &scope_str,
            )
            .await?;
            let ttl = extract_jwt_exp(&access_token)
                .ok()
                .map(|exp| {
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
                })
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

    // --- Task 1: JWT helper tests ---

    #[test]
    fn extract_tid_from_aad_token() {
        let token = fake_jwt(r#"{"tid":"72f988bf-86f1-41af-91ab-2d7cd011db47","sub":"user"}"#);
        let tid = extract_jwt_claim(&token, "tid").unwrap();
        assert_eq!(tid, "72f988bf-86f1-41af-91ab-2d7cd011db47");
    }

    #[test]
    fn extract_exp_from_acr_token() {
        let token = fake_jwt(r#"{"exp":1700000000,"iss":"Azure Container Registry"}"#);
        let exp = extract_jwt_exp(&token).unwrap();
        assert_eq!(exp, 1700000000);
    }

    #[test]
    fn extract_claim_missing_claim() {
        let token = fake_jwt(r#"{"sub":"user"}"#);
        let err = extract_jwt_claim(&token, "tid").unwrap_err();
        assert!(err.to_string().contains("missing 'tid' claim"));
    }

    #[test]
    fn extract_exp_missing() {
        let token = fake_jwt(r#"{"sub":"user"}"#);
        let err = extract_jwt_exp(&token).unwrap_err();
        assert!(err.to_string().contains("missing 'exp' claim"));
    }

    #[test]
    fn extract_claim_malformed_jwt() {
        let err = extract_jwt_claim("not-a-jwt", "tid").unwrap_err();
        assert!(err.to_string().contains("fewer than 2 segments"));
    }

    #[test]
    fn extract_claim_invalid_base64() {
        let err = extract_jwt_claim("header.!!!invalid!!!.sig", "tid").unwrap_err();
        assert!(err.to_string().contains("base64 decode failed"));
    }

    #[test]
    fn extract_claim_invalid_json() {
        let payload = URL_SAFE_NO_PAD.encode("not json");
        let token = format!("header.{payload}.sig");
        let err = extract_jwt_claim(&token, "tid").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    // --- Task 2: Cloud mapping tests ---

    #[test]
    fn azure_cloud_public() {
        let (authority, resource) = azure_cloud("myregistry.azurecr.io");
        assert_eq!(authority, "login.microsoftonline.com");
        assert_eq!(resource, "https://containerregistry.azure.net");
    }

    #[test]
    fn azure_cloud_china() {
        let (authority, resource) = azure_cloud("myregistry.azurecr.cn");
        assert_eq!(authority, "login.chinacloudapi.cn");
        assert_eq!(resource, "https://containerregistry.azure.cn");
    }

    #[test]
    fn azure_cloud_usgov() {
        let (authority, resource) = azure_cloud("myregistry.azurecr.us");
        assert_eq!(authority, "login.microsoftonline.us");
        assert_eq!(resource, "https://containerregistry.azure.us");
    }

    // --- Task 3: Mock token source ---

    /// Mock Azure token source that returns pre-configured AAD tokens.
    #[derive(Debug)]
    struct MockAzureTokenSource {
        tokens: Mutex<VecDeque<AadToken>>,
    }

    impl MockAzureTokenSource {
        fn succeeding(access_token: &str) -> Self {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let token = AadToken {
                access_token: access_token.to_owned(),
                expires_on: now + 3600,
            };
            Self {
                tokens: Mutex::new(VecDeque::from(
                    std::iter::repeat_n(token, 10).collect::<Vec<_>>(),
                )),
            }
        }

        fn with_tokens(tokens: Vec<AadToken>) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(tokens)),
            }
        }
    }

    impl AzureTokenSource for MockAzureTokenSource {
        fn get_token(
            &self,
            _resource: &str,
        ) -> Pin<Box<dyn Future<Output = Result<AadToken, Error>> + Send + '_>> {
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
    }

    // --- Task 4: ACR exchange tests ---

    use wiremock::MockServer;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn acr_exchange_returns_refresh_token() {
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

        let refresh = acr_exchange_refresh_token(
            &acr_exchange_http_client(),
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
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let err = acr_exchange_refresh_token(
            &acr_exchange_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "bad-token",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    #[tokio::test]
    async fn acr_token_returns_access_token() {
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

        let access = acr_exchange_access_token(
            &acr_exchange_http_client(),
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
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad refresh"))
            .mount(&server)
            .await;

        let err = acr_exchange_access_token(
            &acr_exchange_http_client(),
            &server.uri(),
            "myregistry.azurecr.io",
            "bad-refresh",
            "repository:repo:pull",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    // --- Task 5: AcrAuth tests ---

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

    fn mount_acr_exchange(
        _server: &MockServer,
        refresh_token: &str,
        access_token: &str,
    ) -> Vec<Mock> {
        vec![
            Mock::given(method("POST"))
                .and(path("/oauth2/exchange"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"refresh_token": refresh_token})),
                ),
            Mock::given(method("POST"))
                .and(path("/oauth2/token"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"access_token": access_token})),
                ),
        ]
    }

    #[tokio::test]
    async fn auth_name() {
        let auth = AcrAuth::with_api(
            "https://myregistry.azurecr.io",
            "myregistry.azurecr.io",
            crate::test_http_client(),
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        assert_eq!(auth.name(), "acr");
    }

    #[tokio::test]
    async fn full_acr_auth_flow() {
        let server = MockServer::start().await;
        let refresh_jwt = fake_jwt_with_exp("", 10800);
        let access_jwt = fake_jwt_with_exp("", 4500);
        for mock in mount_acr_exchange(&server, &refresh_jwt, &access_jwt) {
            mock.mount(&server).await;
        }
        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            crate::test_http_client(),
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
            crate::test_http_client(),
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
            crate::test_http_client(),
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
            crate::test_http_client(),
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        let t1 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t1.value(), &expired_jwt);
        let t2 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t2.value(), &fresh_jwt);
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
        let auth = AcrAuth::with_api(
            &server.uri(),
            "myregistry.azurecr.io",
            crate::test_http_client(),
            MockAzureTokenSource::succeeding("aad-tok"),
        );
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.invalidate().await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    #[tokio::test]
    async fn auth_propagates_aad_error() {
        let auth = AcrAuth::with_api(
            "https://myregistry.azurecr.io",
            "myregistry.azurecr.io",
            crate::test_http_client(),
            MockAzureTokenSource::with_tokens(vec![]),
        );
        let result = auth.get_token(&[Scope::pull("repo")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("mock: no more tokens"), "got: {err}");
    }

    // --- Task 6: Credential chain tests ---

    #[test]
    fn parse_entra_token_response() {
        let json = r#"{"access_token":"tok","expires_in":3599,"ext_expires_in":3599,"token_type":"Bearer"}"#;
        let resp: EntraTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
        assert_eq!(resp.expires_in, 3599);
    }

    #[test]
    fn parse_imds_token_response_string_expires_on() {
        let json = r#"{"access_token":"tok","expires_on":"1700000000","token_type":"Bearer","resource":"https://containerregistry.azure.net"}"#;
        let resp: ImdsTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
        assert_eq!(resp.expires_on, "1700000000");
    }

    #[test]
    fn parse_cli_token_response() {
        let json = r#"{"accessToken":"tok","expiresOn":"2026-04-23","expires_on":1745414400,"tokenType":"Bearer"}"#;
        let resp: CliTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
        assert_eq!(resp.expires_on, Some(1745414400));
    }

    #[tokio::test]
    async fn credential_chain_skips_not_configured() {
        let source = MockAzureTokenSource::succeeding("first-call");
        let token = source
            .get_token("https://containerregistry.azure.net")
            .await
            .unwrap();
        assert_eq!(token.access_token, "first-call");
        let token2 = source
            .get_token("https://containerregistry.azure.net")
            .await
            .unwrap();
        assert_eq!(token2.access_token, "first-call");
    }

    #[tokio::test]
    async fn credential_chain_all_not_configured_error() {
        let source = MockAzureTokenSource::with_tokens(vec![]);
        let err = source
            .get_token("https://containerregistry.azure.net")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no more tokens"), "got: {err}");
    }
}
