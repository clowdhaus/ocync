//! Azure Container Registry authentication provider.
//!
//! Acquires Azure AD credentials via a four-source credential chain, then
//! exchanges them through ACR's proprietary OAuth2 endpoints:
//!
//! 1. AAD credential -> access token (client secret, workload identity,
//!    managed identity, or Azure CLI)
//! 2. `POST /oauth2/exchange` -> ACR refresh token (~3h)
//! 3. `POST /oauth2/token` -> scoped ACR access token (~75min)
//!
//! Sovereign clouds (China `.azurecr.cn`, US Gov `.azurecr.us`) are
//! supported via AAD authority and resource endpoint mapping.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use super::recovery::{Judgment, Recovery, judge_status};
use super::token_cache::TokenCache;
use super::{AuthProvider, Refusal, Scope, Token, registry_origin, scopes_cache_key};
use crate::client::{no_redirect_builder, no_redirect_http_client};
use crate::credential_cache::CredentialCache;
use crate::error::{CredentialFailure, Error, OAuthCode, error_chain, truncate_quoted};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

/// Decode the payload segment of a JWT token as JSON.
fn decode_jwt_payload(token: &str) -> Result<serde_json::Value, Error> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| acr_failed("JWT has fewer than 2 segments"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| acr_failed(format_args!("JWT payload base64 decode failed: {e}")))?;
    crate::json::parse(&decoded)
        .map_err(|e| acr_failed(format_args!("JWT payload is not valid JSON: {e}")))
}

/// Extract the `exp` claim from a JWT as a Unix timestamp.
fn extract_jwt_exp(token: &str) -> Result<i64, Error> {
    let json = decode_jwt_payload(token)?;
    json.get("exp")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| acr_failed("JWT missing 'exp' claim"))
}

/// An ACR token that does not read: [`CredentialFailure::Failed`].
fn acr_failed(reason: impl fmt::Display) -> Error {
    Error::cloud_credentials("ACR", CredentialFailure::Failed, reason)
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
/// Expects a DNS hostname, as the ACR regex in `detect_provider_kind()`
/// enforces.
fn azure_endpoints(host: &str) -> (&'static str, &'static str) {
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
#[derive(Deserialize)]
struct EntraTokenResponse {
    access_token: String,
}

/// Azure AD token response from the IMDS metadata endpoint.
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

/// Azure AD token acquisition, injectable for tests.
trait AzureTokenSource: Send + Sync + fmt::Debug {
    /// Acquire an Azure AD access token string.
    fn get_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>>;

    /// Reset the cached credential source so the chain re-probes, when ACR
    /// refuses what the chain gave: the Azure AD token at
    /// `/oauth2/exchange`, or a refresh token made from it at
    /// `/oauth2/token`. Upstream `azure_identity` never resets, which would
    /// miss a credential rotated during watch mode.
    fn reset(&self) {}
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

/// A failed ACR exchange: the error to report, and the status the endpoint
/// answered with when it answered with an error status.
#[derive(Debug)]
struct ExchangeError {
    error: Error,
    status: Option<http::StatusCode>,
}

impl ExchangeError {
    /// What the failure says of the credential sent, by the endpoint's
    /// answer. A failure with no answer judges nothing.
    fn judgment(&self) -> Judgment {
        self.status.map_or(Judgment::NotJudged, judge_status)
    }
}

impl From<Error> for ExchangeError {
    fn from(error: Error) -> Self {
        Self {
            error,
            status: None,
        }
    }
}

/// POST a form to an ACR `OAuth2` endpoint and deserialize the response.
/// `http` must not follow redirects, or a 307 forwards the form's
/// credentials.
async fn exchange_post<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<T, ExchangeError> {
    let url = format!("{base_url}{endpoint}");

    // A failure here fails every mapping on this registry, and nothing
    // above the prepare phase re-sends it. The last response is returned
    // rather than raised, so the branch below reads the body, where ACR
    // names an exhausted quota.
    let response =
        crate::retry::send_retrying(http.post(&url).form(form), &format!("ACR {endpoint}"))
            .await
            // Unwrapped: `Error::Http`'s own prefix would double this one.
            .map_err(|e| match &e {
                Error::Http(inner) => Error::cloud_credentials(
                    service,
                    CredentialFailure::of_transport(inner),
                    format_args!("ACR {endpoint} request failed: {}", error_chain(inner)),
                ),
                other => Error::cloud_credentials(
                    service,
                    CredentialFailure::Failed,
                    format_args!("ACR {endpoint} request failed: {other}"),
                ),
            })?;

    let status = response.status();
    if !status.is_success() {
        let body = crate::json::error_body(response).await;
        return Err(ExchangeError {
            error: Error::cloud_credentials(
                service,
                CredentialFailure::of_status(status),
                format_args!(
                    "ACR {endpoint} returned {status}: {}",
                    truncate_quoted(&body)
                ),
            ),
            status: Some(status),
        });
    }

    crate::json::answer(response, crate::json::MAX_SMALL_ANSWER)
        .await
        .map_err(|e| {
            Error::cloud_credentials(
                service,
                CredentialFailure::of_unreadable(&e),
                format_args!("ACR {endpoint}: {}", error_chain(&e)),
            )
            .into()
        })
}

/// Exchange an AAD access token for an ACR refresh token. `tenant` is
/// omitted: ACR reads it from the AAD token's `tid` claim.
async fn exchange_refresh_token(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    aad_access_token: &str,
) -> Result<String, ExchangeError> {
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
async fn exchange_access_token(
    http: &reqwest::Client,
    base_url: &str,
    service: &str,
    refresh_token: &str,
    scope: &str,
) -> Result<String, ExchangeError> {
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

/// The access token in the Azure CLI's `stdout`. A refused value is left
/// out of the error, as it can be the token.
fn cli_access_token(stdout: &[u8]) -> Result<String, CredentialError> {
    let body: CliTokenResponse = crate::json::parse(stdout).map_err(|e| {
        CredentialError::Failed(
            CredentialFailure::Failed,
            format!("az CLI output parse failed: {e}"),
        )
    })?;
    Ok(body.access_token)
}

/// What an Azure CLI that gave no token says by `stderr`: refused when it
/// asks for `az login`, as it does for a login that is missing, expired or
/// revoked; any other failure, such as an unreachable Entra ID, failed.
fn cli_failure(stderr: &str) -> CredentialFailure {
    if stderr.contains("az login") {
        CredentialFailure::Refused
    } else {
        CredentialFailure::Failed
    }
}

/// The failure of a chain whose sources failed with `failures`: refused
/// when any refused the identity, as that source needs a new sign-in
/// whatever the others did; else transient when any may pass; else failed.
fn chain_failure(failures: &[CredentialFailure]) -> CredentialFailure {
    [CredentialFailure::Refused, CredentialFailure::Transient]
        .into_iter()
        .find(|failure| failures.contains(failure))
        .unwrap_or(CredentialFailure::Failed)
}

/// Internal error for credential chain control flow.
enum CredentialError {
    /// Credential source is not configured (skip to next).
    NotConfigured,
    /// Credential source is configured but failed (stop and report).
    Failed(CredentialFailure, String),
}

/// Source indexes in the credential chain.
const SOURCE_CLIENT_SECRET: usize = 0;
const SOURCE_WORKLOAD_IDENTITY: usize = 1;
const SOURCE_MANAGED_IDENTITY: usize = 2;
const SOURCE_AZURE_CLI: usize = 3;
/// No cached source, in an `AtomicUsize` that cannot hold an `Option`.
const SOURCE_NONE: usize = usize::MAX;

/// Azure credential chain that tries four credential sources in order and
/// keeps using the first that succeeds until reset.
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
        let http = no_redirect_builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(10))
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
        let resp = http.post(url).form(form).send().await.map_err(|e| {
            CredentialError::Failed(
                CredentialFailure::of_transport(&e),
                format!("{source} token request failed: {}", error_chain(&e)),
            )
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = crate::json::error_body(resp).await;
            return Err(CredentialError::Failed(
                CredentialFailure::of_oauth(status, OAuthCode::read(body.as_bytes())),
                format!("{source} auth failed: {}", truncate_quoted(&body)),
            ));
        }

        let body: EntraTokenResponse = crate::json::answer(resp, crate::json::MAX_SMALL_ANSWER)
            .await
            .map_err(|e| {
                CredentialError::Failed(
                    CredentialFailure::of_unreadable(&e),
                    format!("{source}: {}", error_chain(&e)),
                )
            })?;

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
            CredentialError::Failed(
                CredentialFailure::Failed,
                format!("failed to read federated token file '{token_file}': {e}"),
            )
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
        // `resource` is one of `azure_endpoints()`'s constants, none holding
        // `&` or `#`, so it needs no encoding.
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
            let body = crate::json::error_body(resp).await;
            return Err(CredentialError::Failed(
                CredentialFailure::Failed,
                format!(
                    "managed identity not configured: {}",
                    truncate_quoted(&body)
                ),
            ));
        }
        // Another cloud's metadata server can answer at this address, and
        // Azure's answers 404 or 410 while it updates: no status here
        // refuses an Azure identity.
        if !status.is_success() {
            return Err(CredentialError::Failed(
                CredentialFailure::of_unjudged(status),
                format!("IMDS returned {status}"),
            ));
        }

        let body: ImdsTokenResponse = crate::json::answer(resp, crate::json::MAX_SMALL_ANSWER)
            .await
            .map_err(|e| {
                CredentialError::Failed(
                    CredentialFailure::of_unreadable(&e),
                    format!("IMDS: {}", error_chain(&e)),
                )
            })?;

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
                    CredentialError::Failed(
                        CredentialFailure::Failed,
                        format!("az CLI execution failed: {e}"),
                    )
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CredentialError::Failed(
                cli_failure(&stderr),
                format!("az CLI failed: {}", truncate_quoted(&stderr)),
            ));
        }

        cli_access_token(&output.stdout)
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
                return self.try_source(cached).await.map_err(|e| {
                    let (failure, reason) = match e {
                        CredentialError::NotConfigured => {
                            (CredentialFailure::Failed, "not configured".to_owned())
                        }
                        CredentialError::Failed(failure, reason) => (failure, reason),
                    };
                    Error::cloud_credentials(
                        "ACR",
                        failure,
                        format_args!("{name} credential failed: {reason}"),
                    )
                });
            }

            let mut errors = Vec::new();
            let mut failures = Vec::new();
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
                    Err(CredentialError::Failed(failure, reason)) => {
                        tracing::warn!(source = name, "credential failed");
                        errors.push(format!("{name}: {reason}"));
                        failures.push(failure);
                    }
                }
            }

            Err(Error::cloud_credentials(
                "ACR",
                chain_failure(&failures),
                if errors.is_empty() {
                    "no Azure credential sources configured (set AZURE_CLIENT_ID + AZURE_CLIENT_SECRET + AZURE_TENANT_ID, or run on Azure with managed identity, or install Azure CLI)".into()
                } else {
                    format!(
                        "all Azure credential sources failed:\n  {}",
                        errors.join("\n  ")
                    )
                },
            ))
        })
    }
}

/// Default ACR refresh token TTL fallback (3 hours).
const ACR_REFRESH_TOKEN_DEFAULT_TTL: Duration = Duration::from_secs(3 * 60 * 60);

/// Default ACR access token TTL fallback (60 minutes).
const ACR_ACCESS_TOKEN_DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);

/// ACR authentication provider.
///
/// The ACR refresh token is cached and refreshed before it expires. Access
/// tokens are coalesced per scope in a `TokenCache`: concurrent fetches for
/// one scope make one exchange, while distinct scopes run in parallel.
pub struct AcrAuth {
    base_url: String,
    service: String,
    exchange_http: reqwest::Client,
    api: Box<dyn AzureTokenSource>,
    refresh_cache: CredentialCache<String, Error>,
    tokens: TokenCache,
    /// What the token endpoint's refusals of the refresh token come to.
    recovery: Recovery<String>,
    /// How many times the credential chain has been reset. A chain answer
    /// is still the current one if the count has not moved since it was
    /// asked for.
    chain_resets: AtomicU64,
}

impl fmt::Debug for AcrAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcrAuth")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AcrAuth {
    /// Construct an `AcrAuth` for the registry at `registry`, the base URL
    /// its client uses.
    pub async fn new(registry: &url::Url) -> Result<Self, Error> {
        let hostname = registry.host_str().unwrap_or_default().to_owned();
        let (authority, resource) = azure_endpoints(&hostname);
        let api = Box::new(AzureCredentialChain::new(authority, resource));
        Ok(Self {
            base_url: registry_origin(registry),
            service: hostname,
            exchange_http: no_redirect_http_client(),
            api,
            refresh_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
            recovery: Recovery::default(),
            chain_resets: AtomicU64::new(0),
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
            refresh_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
            recovery: Recovery::default(),
            chain_resets: AtomicU64::new(0),
        }
    }

    /// Re-probe the credential chain on its next use.
    fn reset_chain(&self) {
        self.chain_resets.fetch_add(1, Ordering::Relaxed);
        self.api.reset();
    }

    /// The token endpoint refused the Azure AD token the chain gave after
    /// `resets` resets. Re-probe the chain unless it has been reset since,
    /// as the answer is then no longer the chain's current one.
    fn reset_chain_after(&self, resets: u64) {
        if self
            .chain_resets
            .compare_exchange(resets, resets + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.api.reset();
        }
    }

    /// The token endpoint refused `refresh_token`: drop it if it is still
    /// the one kept, and then re-probe the credential chain it came from.
    fn refuse_refresh_token(&self, refresh_token: &str) {
        let refused = refresh_token.to_owned();
        if self.refresh_cache.refuse(refused, String::eq) == Refusal::Current {
            self.reset_chain();
        }
    }

    async fn ensure_refresh_token(&self) -> Result<String, Error> {
        self.refresh_cache
            .get_or_refresh_sharing_failure(async {
                tracing::debug!(service = %self.service, "ACR refresh token expired, re-acquiring");
                let resets = self.chain_resets.load(Ordering::Relaxed);
                let aad_access_token = self.api.get_token().await?;
                let refresh_token = exchange_refresh_token(
                    &self.exchange_http,
                    &self.base_url,
                    &self.service,
                    &aad_access_token,
                )
                .await
                .map_err(|failure| {
                    if failure.judgment() == Judgment::Refused {
                        self.reset_chain_after(resets);
                    }
                    failure.error
                })?;
                let ttl = extract_jwt_exp(&refresh_token)
                    .ok()
                    .map(ttl_from_unix_exp)
                    .unwrap_or(ACR_REFRESH_TOKEN_DEFAULT_TTL);
                tracing::debug!(service = %self.service, ttl_secs = ttl.as_secs(), "ACR refresh token acquired");
                Ok((refresh_token, ttl))
            })
            .await
    }

    /// Exchange the refresh token for an access token for `scope`.
    async fn access_token_for(&self, scope: &str) -> Result<String, Error> {
        self.recovery
            .exchange(
                || self.ensure_refresh_token(),
                |refresh_token| async move {
                    exchange_access_token(
                        &self.exchange_http,
                        &self.base_url,
                        &self.service,
                        &refresh_token,
                        scope,
                    )
                    .await
                },
                ExchangeError::judgment,
                |refused| self.refuse_refresh_token(refused),
            )
            .await
            .map_err(|failure| failure.error)
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
            self.tokens
                .get_or_fetch(key.clone(), || async {
                    let scope_str = scopes
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(" ");
                    tracing::debug!(service = %self.service, scope = %key, "ACR token cache miss, exchanging");
                    let access_token = self.access_token_for(&scope_str).await?;
                    let ttl = extract_jwt_exp(&access_token)
                        .ok()
                        .map(ttl_from_unix_exp)
                        .unwrap_or(ACR_ACCESS_TOKEN_DEFAULT_TTL);
                    Ok(Token::with_ttl(access_token, ttl))
                })
                .await
        })
    }

    fn refuse(&self, refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // The refresh token and credential chain stay: only the
            // exchanges that use them judge them.
            self.tokens.refuse(&self.base_url, &refused).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain is refused when any source refused, whichever failed first,
    /// else transient when any may pass. Breaks if a refused CLI login is
    /// hidden by a metadata probe that failed before it, or a refusal
    /// stops counting as one.
    #[test]
    fn a_chain_is_refused_when_any_source_was() {
        use CredentialFailure::{Failed, Refused, Transient};
        for (failures, expected) in [
            (&[][..], Failed),
            (&[Failed, Refused], Refused),
            (&[Refused, Transient], Refused),
            (&[Failed, Transient], Transient),
            (&[Failed], Failed),
        ] {
            assert_eq!(chain_failure(failures), expected, "{failures:?}");
        }
    }

    /// An Azure CLI asking for `az login` refuses, and one that cannot
    /// reach Entra ID failed. Breaks if every CLI failure exits as refused
    /// credentials, or an expired login stops counting as refused.
    #[test]
    fn a_cli_asking_for_login_is_refused() {
        for (stderr, expected) in [
            (
                "ERROR: AADSTS700082: The refresh token has expired. To re-authenticate, please run:\naz login",
                CredentialFailure::Refused,
            ),
            (
                "ERROR: Please run 'az login' to setup account.",
                CredentialFailure::Refused,
            ),
            (
                "ERROR: HTTPSConnectionPool(host='login.microsoftonline.com', port=443): Max retries exceeded",
                CredentialFailure::Failed,
            ),
        ] {
            assert_eq!(cli_failure(stderr), expected, "{stderr}");
        }
    }

    /// Azure CLI output with a mistyped token is refused without the
    /// value. Breaks if the output is read with the raw parser.
    #[test]
    fn mistyped_cli_output_leaves_the_token_out() {
        let Err(CredentialError::Failed(_, reason)) =
            cli_access_token(br#"{"accessToken": 735017}"#)
        else {
            panic!("accepted");
        };
        assert!(
            reason.contains("invalid type: integer, expected a string"),
            "{reason}"
        );
        assert!(!reason.contains("735017"), "{reason}");
    }

    use std::collections::VecDeque;

    use tokio::sync::Mutex;

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

    /// Mock Azure token source that returns pre-configured access token strings.
    #[derive(Debug)]
    struct MockAzureTokenSource {
        tokens: Mutex<VecDeque<String>>,
        /// Shared counter so tests can verify a refused refresh token calls
        /// `api.reset()` even after the mock is moved into `AcrAuth`.
        reset_count: std::sync::Arc<AtomicUsize>,
        /// How many times `get_token` was called.
        get_count: std::sync::Arc<AtomicUsize>,
    }

    impl MockAzureTokenSource {
        fn succeeding(access_token: &str) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(
                    std::iter::repeat_n(access_token.to_owned(), 10).collect::<Vec<_>>(),
                )),
                reset_count: std::sync::Arc::new(AtomicUsize::new(0)),
                get_count: std::sync::Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_tokens(tokens: Vec<String>) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(tokens)),
                reset_count: std::sync::Arc::new(AtomicUsize::new(0)),
                get_count: std::sync::Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AzureTokenSource for MockAzureTokenSource {
        fn get_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
            Box::pin(async move {
                self.get_count.fetch_add(1, Ordering::Relaxed);
                // Yield so callers that ask meanwhile find the call in flight.
                tokio::task::yield_now().await;
                self.tokens.lock().await.pop_front().ok_or_else(|| {
                    Error::cloud_credentials(
                        "mock-acr",
                        CredentialFailure::Failed,
                        "mock: no more tokens",
                    )
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

    /// A request that cannot connect names the refusal and may pass, for
    /// the ACR exchange and the Entra ID token request alike. Breaks if
    /// either reason keeps only the top line, "error sending request", or
    /// either exits as refused credentials.
    #[tokio::test]
    async fn a_refused_connection_is_named() {
        crate::install_crypto_provider();
        let address = crate::REFUSING;
        let refused = crate::refusal();
        let base_url = format!("http://{address}");
        let http = no_redirect_http_client();

        let err = exchange_refresh_token(&http, &base_url, "myregistry.azurecr.io", "aad-tok")
            .await
            .unwrap_err()
            .error;
        let Error::CloudCredentials {
            reason, failure, ..
        } = err
        else {
            panic!("{err:?}");
        };
        assert!(reason.ends_with(&refused), "{reason}");
        assert_eq!(failure, CredentialFailure::Transient);

        let Err(CredentialError::Failed(failure, reason)) = AzureCredentialChain::post_entra_token(
            &http,
            &format!("{base_url}/token"),
            &[],
            "client secret",
        )
        .await
        else {
            panic!("expected a failed token request");
        };
        assert!(reason.ends_with(&refused), "{reason}");
        assert_eq!(failure, CredentialFailure::Transient);
    }

    /// A failed exchange names its cause, and only a 401 judges the
    /// credential refused. A redirect is never followed, so a 307 cannot
    /// replay the credential elsewhere. Breaks if a redirect is followed,
    /// an unreadable answer is taken as a refusal, or a 401 is not.
    #[tokio::test]
    async fn a_failed_exchange_names_its_cause() {
        crate::install_crypto_provider();
        let elsewhere = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&elsewhere)
            .await;
        let cases = [
            (
                ResponseTemplate::new(401).set_body_string("unauthorized"),
                "401",
                Judgment::Refused,
            ),
            (
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/steal", elsewhere.uri())),
                "307",
                Judgment::NotJudged,
            ),
            (
                ResponseTemplate::new(200).set_body_string("not json"),
                "unreadable answer from",
                Judgment::NotJudged,
            ),
        ];
        for (answer, cause, judged) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/oauth2/exchange"))
                .respond_with(answer)
                .expect(1)
                .mount(&server)
                .await;

            let failure = exchange_refresh_token(
                &no_redirect_http_client(),
                &server.uri(),
                "myregistry.azurecr.io",
                "aad-tok",
            )
            .await
            .unwrap_err();
            let err = &failure.error;
            assert!(err.to_string().contains(cause), "{cause}: {err}");
            assert_eq!(failure.judgment(), judged, "{cause}: {err}");
            assert_eq!(
                err.is_auth_error(),
                judged == Judgment::Refused,
                "{cause}: {err}"
            );
        }
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

    /// A refusal of the registry token drops the token and keeps the
    /// refresh token and the credential chain, so the next token costs one
    /// access-token exchange and nothing else. Breaks if a registry 401
    /// judges the credentials behind the token, or a refusal of another
    /// token resets the chain.
    #[tokio::test]
    async fn a_registry_refusal_drops_only_the_access_token() {
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

        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let get_count = mock.get_count.clone();

        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);
        let refused = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(Token::new("other")).await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(refused).await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(reset_count.load(Ordering::Relaxed), 0);
        assert_eq!(get_count.load(Ordering::Relaxed), 1);
    }

    /// Mount `/oauth2/exchange` answering the refresh tokens `first` then
    /// `second`, and `/oauth2/token` answering 200 for a refresh token it
    /// accepts. `first` is accepted for one exchange and refused after.
    /// Each mock expects the calls the test makes.
    async fn mount_revoked_refresh_token(server: &MockServer, first: &str, second: &str) {
        for (refresh, calls) in [(first, 1), (second, 1)] {
            Mock::given(method("POST"))
                .and(path("/oauth2/exchange"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "refresh_token": refresh })),
                )
                .up_to_n_times(calls)
                .expect(calls)
                .mount(server)
                .await;
        }
        let access = fake_jwt_with_exp("", 4500);
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains(format!("refresh_token={first}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access})),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains(format!("refresh_token={first}")))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains(format!("refresh_token={second}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access})),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    /// A refresh token revoked while kept and still fresh is refused by the
    /// token endpoint, dropped, and replaced within the one request: two
    /// refresh-token exchanges, three access-token exchanges, and the chain
    /// reset once. Breaks if the refusal is not recovered from, the refresh
    /// token is kept, or the chain is left on the source that produced it.
    #[tokio::test]
    async fn a_revoked_refresh_token_is_replaced_within_the_request() {
        let server = MockServer::start().await;
        // The two refresh tokens differ in a claim, so the mocks tell them apart.
        let first = fake_jwt_with_exp(r#""n":1"#, 10800);
        let second = fake_jwt_with_exp(r#""n":2"#, 10800);
        mount_revoked_refresh_token(&server, &first, &second).await;
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let get_count = mock.get_count.clone();
        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);

        auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();

        assert_eq!(reset_count.load(Ordering::Relaxed), 1);
        assert_eq!(get_count.load(Ordering::Relaxed), 2);
    }

    /// Requests that all used a refresh token revoked while kept are all
    /// refused, and each is sent again with the replacement one of them
    /// fetched: one Azure AD token and one `/oauth2/exchange` in all, and
    /// the chain reset once. Breaks if only the request that dropped the
    /// refresh token recovers, each fetches its own, or each resets the
    /// chain.
    #[tokio::test(flavor = "current_thread")]
    async fn requests_refused_with_one_revoked_refresh_token_share_its_replacement() {
        let server = MockServer::start().await;
        let revoked = fake_jwt_with_exp(r#""n":1"#, 10800);
        let replacement = fake_jwt_with_exp(r#""n":2"#, 10800);
        let access = fake_jwt_with_exp("", 4500);
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "refresh_token": &replacement })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains(format!("refresh_token={replacement}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": &access})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(5)
            .mount(&server)
            .await;
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let get_count = mock.get_count.clone();
        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);
        auth.refresh_cache.set(revoked, Duration::from_secs(3600));

        let results = tokio::join!(
            auth.get_token(&[Scope::pull("a")]),
            auth.get_token(&[Scope::pull("b")]),
            auth.get_token(&[Scope::pull("c")]),
            auth.get_token(&[Scope::pull("d")]),
            auth.get_token(&[Scope::pull("e")]),
        );

        for result in [results.0, results.1, results.2, results.3, results.4] {
            result.unwrap();
        }
        assert_eq!(get_count.load(Ordering::Relaxed), 1);
        assert_eq!(reset_count.load(Ordering::Relaxed), 1);
    }

    /// A refresh token the token endpoint refuses although it was fetched
    /// for the retry costs one recovery: two refresh tokens, two Azure AD
    /// tokens and four access-token exchanges for three scopes, all
    /// failing. Breaks if the retry is retried, or each later scope signs in
    /// again.
    #[tokio::test]
    async fn a_refused_replacement_costs_one_recovery() {
        let server = MockServer::start().await;
        for n in [1, 2] {
            Mock::given(method("POST"))
                .and(path("/oauth2/exchange"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                    {"refresh_token": fake_jwt_with_exp(&format!(r#""n":{n}"#), 10800)}
                )))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(4)
            .mount(&server)
            .await;
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let get_count = mock.get_count.clone();
        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);

        for repo in ["a", "b", "c"] {
            let err = auth.get_token(&[Scope::pull(repo)]).await.unwrap_err();
            assert!(err.to_string().contains("401"), "{err}");
        }

        assert_eq!(get_count.load(Ordering::Relaxed), 2);
    }

    /// A 403 or 404 from the token endpoint, as for one repository the
    /// identity cannot reach, judges nothing: the refresh token is kept, the
    /// chain is not reset, and the next scope uses the same refresh token.
    /// Breaks if any refused status judges the refresh token.
    #[tokio::test]
    async fn only_a_401_judges_the_refresh_token() {
        for status in [403, 404] {
            let server = MockServer::start().await;
            let access = fake_jwt_with_exp("", 4500);
            Mock::given(method("POST"))
                .and(path("/oauth2/exchange"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"refresh_token": fake_jwt_with_exp("", 10800)}),
                ))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/oauth2/token"))
                .respond_with(ResponseTemplate::new(status))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/oauth2/token"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"access_token": &access})),
                )
                .mount(&server)
                .await;
            let mock = MockAzureTokenSource::succeeding("aad-tok");
            let reset_count = mock.reset_count.clone();
            let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);

            assert!(
                auth.get_token(&[Scope::pull("private")]).await.is_err(),
                "{status}"
            );
            auth.get_token(&[Scope::pull("public")]).await.unwrap();
            assert_eq!(reset_count.load(Ordering::Relaxed), 0, "{status}");
        }
    }

    /// A 401 from `/oauth2/exchange`, ACR refusing the Azure AD token,
    /// resets the credential chain. Breaks if the chain stays on the source
    /// whose token was refused.
    #[tokio::test]
    async fn a_refused_aad_token_resets_the_chain() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);

        assert!(auth.get_token(&[Scope::pull("repo")]).await.is_err());

        assert_eq!(reset_count.load(Ordering::Relaxed), 1);
    }

    /// An Azure AD token refused after the chain was reset is not the
    /// chain's current answer, so the refusal resets nothing more. Breaks if
    /// a late refusal resets the chain a second time.
    #[tokio::test]
    async fn a_refused_aad_token_from_a_reset_chain_resets_nothing_more() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);

        let (result, ()) = tokio::join!(auth.get_token(&[Scope::pull("repo")]), async {
            tokio::task::yield_now().await;
            auth.reset_chain();
        });

        assert!(result.is_err());
        assert_eq!(reset_count.load(Ordering::Relaxed), 1);
    }

    /// A refusal of a refresh token that was replaced keeps the replacement
    /// and the chain, and one of the kept refresh token drops it and resets
    /// the chain. Breaks if a refusal of a replaced token drops the kept one
    /// or resets the chain.
    #[tokio::test]
    async fn only_a_refusal_of_the_kept_refresh_token_resets_the_chain() {
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let get_count = mock.get_count.clone();
        let auth = AcrAuth::with_api(
            "https://myregistry.azurecr.io",
            "myregistry.azurecr.io",
            mock,
        );
        auth.refresh_cache
            .set("new".to_owned(), Duration::from_secs(3600));

        auth.refuse_refresh_token("old");
        assert_eq!(auth.ensure_refresh_token().await.unwrap(), "new");
        assert_eq!(reset_count.load(Ordering::Relaxed), 0);
        assert_eq!(get_count.load(Ordering::Relaxed), 0);

        auth.refuse_refresh_token("new");
        assert_eq!(reset_count.load(Ordering::Relaxed), 1);
    }

    /// A refusal of the refresh token kept, made while the refresh that
    /// replaces it in the early-refresh window is in flight, resets the
    /// chain: the token stays kept until that refresh lands. Breaks if a
    /// refresh in flight makes the refusal read as one of a replaced token.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_during_the_early_refresh_window_resets_the_chain() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/exchange"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(
                        serde_json::json!({"refresh_token": fake_jwt_with_exp(r#""n":2"#, 10800)}),
                    )
                    .set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;
        let mock = MockAzureTokenSource::succeeding("aad-tok");
        let reset_count = mock.reset_count.clone();
        let auth = AcrAuth::with_api(&server.uri(), "myregistry.azurecr.io", mock);
        auth.refresh_cache
            .set("old".to_owned(), Duration::from_secs(10));

        let (refreshed, ()) = tokio::join!(auth.ensure_refresh_token(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            auth.refuse_refresh_token("old");
        });

        refreshed.unwrap();
        assert_eq!(reset_count.load(Ordering::Relaxed), 1);
    }

    /// Callers waiting on a credential refresh that fails share its error,
    /// across scopes. Breaks if each waiter repeats the failed acquisition.
    #[tokio::test(flavor = "current_thread")]
    async fn waiters_on_a_failed_refresh_share_its_error() {
        let mock = MockAzureTokenSource::with_tokens(vec![]);
        let get_count = mock.get_count.clone();
        let auth = AcrAuth::with_api(
            "https://myregistry.azurecr.io",
            "myregistry.azurecr.io",
            mock,
        );

        let (first, second, third) = tokio::join!(
            auth.get_token(&[Scope::pull("repo-a")]),
            auth.get_token(&[Scope::pull("repo-b")]),
            auth.get_token(&[Scope::pull("repo-c")]),
        );

        for result in [first, second, third] {
            let err = result.unwrap_err().to_string();
            assert!(err.contains("mock: no more tokens"), "got: {err}");
        }
        assert_eq!(get_count.load(Ordering::Relaxed), 1);
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
