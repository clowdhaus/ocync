//! HTTP client for a single OCI registry endpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use http::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::aimd::{AimdController, RegistryAction};
use crate::auth::{AuthProvider, Scope, Token};
use crate::error::Error;
use crate::spec::{RegistryAuthority, RepositoryName};

const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 50;
const USER_AGENT_VALUE: &str = concat!("ocync/", env!("CARGO_PKG_VERSION"));

/// Build a no-redirect HTTP client for token-bearing requests.
///
/// Following a redirect would forward credentials (realm auth headers, ACR
/// exchange form bodies) to whatever target a malicious endpoint names.
pub(crate) fn no_redirect_http_client() -> reqwest::Client {
    no_redirect_builder()
        .build()
        .expect("no-redirect HTTP client builder should not fail")
}

/// The builder behind [`no_redirect_http_client`], for a client that also
/// sets its own timeouts, such as the AWS API client.
pub(crate) fn no_redirect_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT_VALUE)
        .redirect(reqwest::redirect::Policy::none())
}

/// Default cap on concurrent blob uploads per registry.
///
/// reqwest/hyper opens one HTTP/2 connection per origin and multiplexes
/// streams over it. Each blob upload (POST + streaming PUT) holds an h2
/// stream open for the duration of the transfer. Blob downloads go over
/// HTTP/1.1 and are not counted. Major container registries advertise
/// `SETTINGS_MAX_CONCURRENT_STREAMS` in the range 100-128
/// (registry-1.docker.io: 128, ghcr.io: 100, cgr.dev: 100,
/// us-docker.pkg.dev: 100, public.ecr.aws: 128, quay.io: 128,
/// mcr.microsoft.com: 100, probed 2026-06-01).
///
/// Capping concurrent blob uploads at 64 leaves 36-64 stream slots for
/// short-lived metadata operations (manifest HEAD/GET, blob mount POST,
/// auth refresh) on every registry we've probed. Without this cap, a
/// `max_concurrent_transfers * BLOB_CONCURRENCY` above the budget
/// overshoots it and the connection stalls indefinitely.
const DEFAULT_BLOB_UPLOAD_CONCURRENCY: usize = 64;

/// Default cap on concurrent blob downloads per registry. Each is its own
/// HTTP/1.1 connection while its body is open, so the cap bounds the
/// connections and TLS handshakes one sync opens to a source. Every upload
/// reads one download, so a lower cap would hold uploads below their own
/// cap, and a higher one would only open more bodies waiting on uploads.
const DEFAULT_BLOB_DOWNLOAD_CONCURRENCY: usize = DEFAULT_BLOB_UPLOAD_CONCURRENCY;

/// Default per-target byte budget for in-flight buffered blob bodies.
///
/// The fallback upload paths (GHCR single-PATCH, ACR chunked PATCH) buffer
/// the whole blob because the target cannot accept a streaming body. With
/// [`DEFAULT_BLOB_UPLOAD_CONCURRENCY`] uploads and no byte cap, 1 GB layers
/// (common for CUDA/ML images) would hold ~64 GB resident. 512 MB is ~32x a typical 16 MB
/// layer, so modest layers do not serialise, while a single oversized layer
/// (e.g. 2 GB GPU runtime) runs alone.
const DEFAULT_BUFFERED_BLOB_BYTES: usize = 512 * 1024 * 1024;

/// Fallback acquisition size when a fallback upload has no `known_size`
/// hint: the buffer's own initial allocation cap.
const BUFFERED_BLOB_BYTES_FALLBACK_RESERVE: u64 = crate::blob::MAX_BUFFER_PREALLOC as u64;

/// Sentinel value stored in [`RegistryClient::rate_limit_remaining`] when no
/// rate-limit header has been observed yet.
const RATE_LIMIT_UNKNOWN: u64 = u64::MAX;

use crate::auth::AuthScheme;

/// Builder for [`RegistryClient`].
pub struct RegistryClientBuilder {
    url: Url,
    auth: Option<Box<dyn AuthProvider>>,
    max_concurrent: usize,
    blob_upload_concurrency: usize,
    blob_download_concurrency: usize,
    buffered_blob_bytes: usize,
    dns_overrides: Vec<(String, std::net::SocketAddr)>,
    accept_invalid_certs: bool,
    force_http1: bool,
    h2_adaptive_window: bool,
}

impl std::fmt::Debug for RegistryClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryClientBuilder")
            .field("url", &self.url)
            .field("max_concurrent", &self.max_concurrent)
            .field("dns_overrides", &self.dns_overrides)
            .finish_non_exhaustive()
    }
}

impl RegistryClientBuilder {
    /// A builder for the base registry URL (e.g. `https://registry-1.docker.io`).
    pub fn new(url: Url) -> Self {
        Self {
            url,
            auth: None,
            max_concurrent: DEFAULT_MAX_CONCURRENT_REQUESTS,
            blob_upload_concurrency: DEFAULT_BLOB_UPLOAD_CONCURRENCY,
            blob_download_concurrency: DEFAULT_BLOB_DOWNLOAD_CONCURRENCY,
            buffered_blob_bytes: DEFAULT_BUFFERED_BLOB_BYTES,
            dns_overrides: Vec::new(),
            accept_invalid_certs: false,
            force_http1: false,
            h2_adaptive_window: true,
        }
    }

    /// Set the authentication provider.
    pub fn auth(mut self, auth: impl AuthProvider + 'static) -> Self {
        self.auth = Some(Box::new(auth));
        self
    }

    /// Set the maximum number of concurrent requests. `0` is clamped to `1`
    /// so the semaphore always makes progress.
    pub fn max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n.max(1);
        self
    }

    /// Set the cap on concurrent blob uploads. Each upload holds an HTTP/2
    /// stream open on the one connection per origin, so the cap must stay
    /// under the server's `SETTINGS_MAX_CONCURRENT_STREAMS` less headroom for
    /// manifest, mount and auth requests. Default: `DEFAULT_BLOB_UPLOAD_CONCURRENCY`
    /// (64).
    pub fn blob_upload_concurrency(mut self, n: usize) -> Self {
        self.blob_upload_concurrency = n.max(1);
        self
    }

    /// Set the cap on concurrent blob downloads. Default:
    /// `DEFAULT_BLOB_DOWNLOAD_CONCURRENCY` (64).
    #[cfg(test)]
    pub(crate) fn blob_download_concurrency(mut self, n: usize) -> Self {
        self.blob_download_concurrency = n.max(1);
        self
    }

    /// Set the byte budget for in-flight buffered blob bodies on the
    /// fallback upload paths (GHCR / ACR); the streaming PUT path does not
    /// buffer and is unaffected. Uploads that would exceed the budget queue.
    ///
    /// `n` is clamped to at least 1. A budget below the 16 MB an
    /// unknown-size upload requests still admits it, since each request is
    /// capped at the budget.
    ///
    /// Default: `DEFAULT_BUFFERED_BLOB_BYTES` (512 MB).
    pub fn buffered_blob_bytes(mut self, n: usize) -> Self {
        self.buffered_blob_bytes = n.max(1);
        self
    }

    // The three hooks below have no production caller and no CLI flag or
    // env var reaches them. They stay public for the perf harness,
    // `crates/ocync-sync/tests/perf_profile.rs`; do not delete as unused.

    /// Accept any server certificate without validation, for the perf
    /// harness's self-signed local registry. Never use in production.
    #[doc(hidden)]
    pub fn allow_invalid_certs(mut self, allow: bool) -> Self {
        self.accept_invalid_certs = allow;
        self
    }

    /// Refuse to negotiate HTTP/2 via ALPN (blob downloads always use
    /// HTTP/1.1), to isolate HTTP/2 multiplexing stalls in the perf harness.
    /// Never use in production.
    #[doc(hidden)]
    pub fn force_http1(mut self, force: bool) -> Self {
        self.force_http1 = force;
        self
    }

    /// Toggle HTTP/2 adaptive-window sizing (enabled by default), for A/B
    /// testing the disabled path.
    #[doc(hidden)]
    pub fn http2_adaptive_window(mut self, enable: bool) -> Self {
        self.h2_adaptive_window = enable;
        self
    }

    /// Pin DNS resolution for `host` to `addr`. Multiple calls stack.
    ///
    /// Lets tests route a real registry hostname at a mock server so
    /// provider detection runs as it would in production.
    pub fn resolve(mut self, host: impl Into<String>, addr: std::net::SocketAddr) -> Self {
        self.dns_overrides.push((host.into(), addr));
        self
    }

    /// Build the registry client.
    pub fn build(self) -> Result<RegistryClient, Error> {
        crate::install_crypto_provider();
        let base_builder = || {
            let mut builder = reqwest::Client::builder().user_agent(USER_AGENT_VALUE);
            for (host, addr) in &self.dns_overrides {
                builder = builder.resolve(host, *addr);
            }
            if self.accept_invalid_certs {
                builder = builder.danger_accept_invalid_certs(true);
            }
            builder
        };
        let mut http_builder = base_builder();
        if self.force_http1 {
            http_builder = http_builder.http1_only();
        }
        // Adaptive flow-control windows (off by default in reqwest) start at
        // 65,535 bytes and grow with measured throughput, instead of hyper's
        // fixed 2 MiB per stream and 5 MiB per connection.
        if self.h2_adaptive_window {
            http_builder = http_builder.http2_adaptive_window(true);
        }
        let http = http_builder.build()?;
        let blob_download_http = base_builder().http1_only().build()?;

        let aimd = AimdController::new(
            self.url.host_str().unwrap_or("unknown"),
            self.max_concurrent,
        );
        Ok(RegistryClient {
            base_url: self.url,
            http,
            blob_download_http,
            auth: self.auth,
            aimd,
            blob_upload_sem: Arc::new(Semaphore::new(self.blob_upload_concurrency)),
            blob_download_sem: Arc::new(Semaphore::new(self.blob_download_concurrency)),
            buffered_blob_bytes_sem: Arc::new(Semaphore::new(self.buffered_blob_bytes)),
            buffered_blob_bytes_budget: self.buffered_blob_bytes,
            rate_limit_remaining: AtomicU64::new(RATE_LIMIT_UNKNOWN),
        })
    }
}

/// HTTP client for a single OCI registry.
///
/// Each `RegistryClient` targets one registry host and owns its auth provider
/// and AIMD concurrency controller. Construct via [`RegistryClientBuilder`].
pub struct RegistryClient {
    pub(crate) base_url: Url,
    pub(crate) http: reqwest::Client,
    /// HTTP/1.1 only, for blob downloads. Over HTTP/2 a response body
    /// holds a share of its connection's receive window until it is read,
    /// and with adaptive window sizing that window starts at 65,535 bytes,
    /// so bodies opened ahead of uploads still waiting on a limit can fill
    /// it and starve the bodies being read. Over HTTP/1.1 each body holds
    /// back only its own connection.
    pub(crate) blob_download_http: reqwest::Client,
    pub(crate) auth: Option<Box<dyn AuthProvider>>,
    pub(crate) aimd: AimdController,
    /// Cap on concurrent blob uploads; see [`DEFAULT_BLOB_UPLOAD_CONCURRENCY`].
    pub(crate) blob_upload_sem: Arc<Semaphore>,
    /// Cap on concurrent blob downloads, held by [`Self::blob_pull`]'s
    /// stream until it is dropped. Never shared with uploads: a copy holds
    /// one while its upload waits for an upload permit, which would
    /// deadlock if both came from one semaphore.
    pub(crate) blob_download_sem: Arc<Semaphore>,
    /// Byte budget for buffered blob bodies, one permit per byte, held by
    /// the fallback upload paths from before `buffer_stream` until the
    /// upload completes.
    pub(crate) buffered_blob_bytes_sem: Arc<Semaphore>,
    /// Total permits in `buffered_blob_bytes_sem`, so one oversized blob
    /// can be capped at the whole budget instead of blocking forever.
    pub(crate) buffered_blob_bytes_budget: usize,
    /// Last observed rate-limit remaining value, [`RATE_LIMIT_UNKNOWN`] until
    /// a header carries one.
    rate_limit_remaining: AtomicU64,
}

/// Acquire a permit from one of the client's blob caps.
async fn acquire_permit(sem: &Arc<Semaphore>) -> OwnedSemaphorePermit {
    // The client never closes its semaphores.
    Arc::clone(sem)
        .acquire_owned()
        .await
        .expect("blob semaphore closed")
}

impl RegistryClient {
    /// Acquire a permit gating concurrent blob uploads, held for the whole
    /// upload body. See [`DEFAULT_BLOB_UPLOAD_CONCURRENCY`].
    pub(crate) async fn acquire_blob_upload_permit(&self) -> OwnedSemaphorePermit {
        acquire_permit(&self.blob_upload_sem).await
    }

    /// Acquire a permit gating concurrent blob downloads. See
    /// [`DEFAULT_BLOB_DOWNLOAD_CONCURRENCY`].
    pub(crate) async fn acquire_blob_download_permit(&self) -> OwnedSemaphorePermit {
        acquire_permit(&self.blob_download_sem).await
    }

    /// Acquire both the per-target count permit and the byte-budget
    /// permit for a fallback (buffered) blob upload, in the right order.
    ///
    /// Returns `(bytes_permit, upload_permit)`. Tuples drop in field order,
    /// so the contested byte budget is released before the count cap,
    /// letting queued buffered uploads admit at once.
    pub(crate) async fn acquire_buffered_upload_permits(
        &self,
        known_size: Option<u64>,
    ) -> (OwnedSemaphorePermit, OwnedSemaphorePermit) {
        let upload_permit = self.acquire_blob_upload_permit().await;
        let bytes_permit = self.acquire_buffered_bytes_permit(known_size).await;
        (bytes_permit, upload_permit)
    }

    /// Acquire a byte-budget permit for a fallback (buffered) blob upload.
    ///
    /// `expected` is the manifest-declared blob size, `None` for an opaque
    /// stream.
    ///
    /// - `Some(n)`: acquire `min(n, budget)` permits, at least one, so an
    ///   empty blob still counts against the budget. An oversized blob
    ///   takes the whole budget and others queue until it completes, though
    ///   its memory exceeds the budget.
    /// - `None`: acquire [`BUFFERED_BLOB_BYTES_FALLBACK_RESERVE`], also
    ///   capped at the budget. The blob may buffer more; the upload count
    ///   cap still bounds how many.
    pub(crate) async fn acquire_buffered_bytes_permit(
        &self,
        expected: Option<u64>,
    ) -> OwnedSemaphorePermit {
        let request = expected.unwrap_or(BUFFERED_BLOB_BYTES_FALLBACK_RESERVE);
        let budget = self.buffered_blob_bytes_budget as u64;
        // Above the budget the semaphore could never grant the request.
        // `acquire_many_owned` takes a u32.
        let permits: u32 = request.min(budget).min(u32::MAX as u64).max(1) as u32;
        Arc::clone(&self.buffered_blob_bytes_sem)
            .acquire_many_owned(permits)
            .await
            .expect("buffered blob bytes semaphore closed")
    }
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remaining = self.rate_limit_remaining.load(Ordering::Relaxed);
        f.debug_struct("RegistryClient")
            .field("base_url", &self.base_url)
            .field(
                "rate_limit_remaining",
                &if remaining == RATE_LIMIT_UNKNOWN {
                    None
                } else {
                    Some(remaining)
                },
            )
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// A builder for this client.
    pub fn builder(url: Url) -> RegistryClientBuilder {
        RegistryClientBuilder::new(url)
    }

    /// The configured auth provider's [`AuthProvider::name`], or `None`
    /// without auth.
    pub fn auth_name(&self) -> Option<&'static str> {
        self.auth.as_ref().map(|a| a.name())
    }

    /// Ping the registry's `/v2/` endpoint. A 200 or 401 is success: either
    /// confirms it speaks the OCI Distribution protocol.
    pub async fn ping(&self) -> Result<(), Error> {
        let url = build_v2_url(&self.base_url)?;
        let resp = self.http.get(url).send().await?;
        let status = resp.status();

        if status.is_success() || status == StatusCode::UNAUTHORIZED {
            Ok(())
        } else {
            let message = crate::json::error_body(resp).await;
            Err(Error::RegistryError { status, message })
        }
    }

    /// The registry authority as `host:port`, for use as a cache key.
    pub fn registry_authority(&self) -> Result<RegistryAuthority, Error> {
        let host = self
            .base_url
            .host_str()
            .ok_or_else(|| Error::UrlConstruction {
                path: self.base_url.to_string(),
                reason: "URL has no host".into(),
            })?;
        let port = self.base_url.port_or_known_default().unwrap_or(443);
        Ok(RegistryAuthority::new(format!("{host}:{port}")))
    }

    /// The last observed rate-limit remaining value, or `None` before any
    /// rate-limit header. The engine pauses discovery on a source when this
    /// runs low.
    pub fn rate_limit_remaining(&self) -> Option<u64> {
        let v = self.rate_limit_remaining.load(Ordering::Relaxed);
        if v == RATE_LIMIT_UNKNOWN {
            None
        } else {
            Some(v)
        }
    }

    /// Store the rate-limit remaining value from response headers.
    fn record_rate_limit(&self, headers: &HeaderMap) {
        if let Some(remaining) = parse_rate_limit_remaining(headers) {
            self.rate_limit_remaining
                .store(remaining, Ordering::Relaxed);
        }
    }

    /// Perform an authenticated GET under an AIMD permit, retrying once on
    /// 401 with the sent token refused.
    pub async fn get(
        &self,
        repository: &RepositoryName,
        path: &str,
        accept: Option<&str>,
        op: RegistryAction,
    ) -> Result<reqwest::Response, Error> {
        let http = self.http_for(op);
        let url = build_url(&self.base_url, repository, path)?;
        let scopes = [Scope::pull(repository.as_str())];

        let resp = self
            .send_with_aimd(op, &scopes, "GET", |mut headers| {
                if let Some(accept) = accept {
                    if let Ok(val) = HeaderValue::from_str(accept) {
                        headers.insert(ACCEPT, val);
                    }
                }
                http.get(url.clone()).headers(headers)
            })
            .await?;

        classify_response(resp, &self.base_url, repository).await
    }

    /// The HTTP client for `op`: [`Self::blob_download_http`] for blob downloads,
    /// [`Self::http`], the default client, for everything else.
    fn http_for(&self, op: RegistryAction) -> &reqwest::Client {
        match op {
            RegistryAction::BlobRead => &self.blob_download_http,
            RegistryAction::ManifestHead
            | RegistryAction::ManifestRead
            | RegistryAction::ManifestWrite
            | RegistryAction::BlobHead
            | RegistryAction::BlobUploadInit
            | RegistryAction::BlobUploadChunk
            | RegistryAction::BlobUploadComplete
            | RegistryAction::TagList => &self.http,
        }
    }

    /// Perform an authenticated HEAD, as [`get`](Self::get). Pass the same
    /// `accept` a GET would use so content negotiation matches.
    pub async fn head(
        &self,
        repository: &RepositoryName,
        path: &str,
        accept: Option<&str>,
        op: RegistryAction,
    ) -> Result<reqwest::Response, Error> {
        let url = build_url(&self.base_url, repository, path)?;
        let scopes = [Scope::pull(repository.as_str())];

        let resp = self
            .send_with_aimd(op, &scopes, "HEAD", |mut headers| {
                if let Some(accept) = accept {
                    if let Ok(val) = HeaderValue::from_str(accept) {
                        headers.insert(ACCEPT, val);
                    }
                }
                self.http.head(url.clone()).headers(headers)
            })
            .await?;

        classify_response(resp, &self.base_url, repository).await
    }

    /// Send a request under an AIMD permit with one 401 retry, reporting the
    /// outcome to AIMD and recording the rate-limit header.
    pub(crate) async fn send_with_aimd(
        &self,
        action: RegistryAction,
        scopes: &[Scope],
        context: &str,
        build_request: impl Fn(HeaderMap) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        let permit = self.aimd.acquire(action).await;
        let result = self.send_with_retry(scopes, context, build_request).await;
        if let Ok(resp) = &result {
            self.record_rate_limit(resp.headers());
        }
        report_permit(permit, &result);
        result
    }

    /// Send a request, and on 401 refuse the sent token and retry once.
    /// `build_request` is called with fresh auth headers each time. Takes no
    /// AIMD permit; callers hold their own.
    async fn send_with_retry(
        &self,
        scopes: &[Scope],
        context: &str,
        build_request: impl Fn(HeaderMap) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        let sent = self.auth_token(scopes).await?;
        let resp = build_request(auth_headers(sent.as_ref())?).send().await?;

        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        tracing::debug!(context, "got 401, refusing the token and retrying");
        self.refuse_auth(sent).await;
        let retry = self.auth_token(scopes).await?;
        Ok(build_request(auth_headers(retry.as_ref())?).send().await?)
    }

    /// Tell the auth provider the registry refused `refused` with 401, if a
    /// token was sent.
    ///
    /// Call before retrying after a 401 so the provider fetches a fresh
    /// token. The provider keeps any token that has replaced `refused`.
    pub(crate) async fn refuse_auth(&self, refused: Option<Token>) {
        if let (Some(auth), Some(refused)) = (&self.auth, refused) {
            auth.refuse(refused).await;
        }
    }

    /// The token to send with a request, or `None` with no auth provider.
    pub(crate) async fn auth_token(&self, scopes: &[Scope]) -> Result<Option<Token>, Error> {
        match &self.auth {
            Some(auth) => auth.get_token(scopes).await.map(Some),
            None => Ok(None),
        }
    }
}

/// The request headers that carry `token`, none when there is no token or
/// its value is empty.
pub(crate) fn auth_headers(token: Option<&Token>) -> Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();
    if let Some(token) = token.filter(|token| !token.value().is_empty()) {
        let prefix = match token.scheme() {
            AuthScheme::Bearer => "Bearer",
            AuthScheme::Basic => "Basic",
        };
        let header_value =
            HeaderValue::from_str(&format!("{prefix} {}", token.value())).map_err(|e| {
                Error::InvalidHeaderValue {
                    header: "Authorization".into(),
                    reason: e.to_string(),
                }
            })?;
        headers.insert(AUTHORIZATION, header_value);
    }
    Ok(headers)
}

/// Report the AIMD permit outcome: [`AimdPermit::throttled`] on 429,
/// [`AimdPermit::success`] otherwise. The permit's drop treats a transport
/// error as success.
pub(crate) fn report_permit(
    permit: crate::aimd::AimdPermit<'_>,
    result: &Result<reqwest::Response, Error>,
) {
    match result {
        Ok(resp) if resp.status() == StatusCode::TOO_MANY_REQUESTS => permit.throttled(),
        _ => permit.success(),
    }
}

/// The rate-limit remaining value from response headers, preferring
/// Docker Hub's `ratelimit-remaining: N;w=21600` over GHCR's
/// `x-ratelimit-remaining: N`.
fn parse_rate_limit_remaining(headers: &HeaderMap) -> Option<u64> {
    if let Some(val) = headers.get("ratelimit-remaining") {
        if let Ok(s) = val.to_str() {
            let num_part = s.split(';').next().unwrap_or(s).trim();
            if let Ok(n) = num_part.parse::<u64>() {
                return Some(n);
            }
        }
    }
    if let Some(val) = headers.get("x-ratelimit-remaining") {
        if let Ok(s) = val.to_str() {
            if let Ok(n) = s.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// Classify an HTTP response into success/error.
async fn classify_response(
    resp: reqwest::Response,
    base_url: &Url,
    repository: &str,
) -> Result<reqwest::Response, Error> {
    let status = resp.status();

    if status.is_success() {
        return Ok(resp);
    }

    let registry = base_url.host_str().unwrap_or("unknown");
    let body = crate::json::error_body(resp).await;
    let message = if body.is_empty() {
        format!("{registry}/{repository}")
    } else {
        format!("{registry}/{repository}: {body}")
    };

    Err(Error::RegistryError { status, message })
}

/// Construct a URL for the `/v2/` endpoint.
fn build_v2_url(base: &Url) -> Result<Url, Error> {
    base.join("/v2/").map_err(|e| Error::UrlConstruction {
        path: "/v2/".into(),
        reason: e.to_string(),
    })
}

/// Construct a URL for `/v2/{repository}/{path}`.
///
/// `repository` is interpolated without percent-encoding, so callers must
/// pass a validated name ([`RepositoryName`] or a parsed
/// [`Reference`](crate::Reference)) that cannot contain `?`, `#` or
/// whitespace.
pub(crate) fn build_url(base: &Url, repository: &str, path: &str) -> Result<Url, Error> {
    let full_path = format!("/v2/{repository}/{path}");
    base.join(&full_path).map_err(|e| Error::UrlConstruction {
        path: full_path,
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::Token;

    fn test_base_url() -> Url {
        Url::parse("https://registry-1.docker.io").unwrap()
    }

    /// Minimal auth provider for tests that always returns a fixed token.
    struct StubAuth;

    impl AuthProvider for StubAuth {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
            Box::pin(async { Ok(Token::new("stub-token".to_owned())) })
        }

        fn refuse(&self, _refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    #[test]
    fn registry_authority_https_default_port() {
        let client = RegistryClient::builder(test_base_url()).build().unwrap();
        assert_eq!(
            client.registry_authority().unwrap().as_str(),
            "registry-1.docker.io:443"
        );
    }

    #[test]
    fn registry_authority_explicit_port() {
        let base = Url::parse("http://localhost:5000").unwrap();
        let client = RegistryClient::builder(base).build().unwrap();
        assert_eq!(
            client.registry_authority().unwrap().as_str(),
            "localhost:5000"
        );
    }

    #[test]
    fn registry_authority_http_default_port() {
        let base = Url::parse("http://registry.example.com").unwrap();
        let client = RegistryClient::builder(base).build().unwrap();
        assert_eq!(
            client.registry_authority().unwrap().as_str(),
            "registry.example.com:80"
        );
    }

    /// A request path joins the base, `/v2/`, the repository and the
    /// endpoint. Breaks if a part is dropped or reordered.
    #[test]
    fn build_url_joins_repository_and_endpoint() {
        for (repository, endpoint, expected) in [
            (
                "library/nginx",
                "manifests/latest",
                "https://registry-1.docker.io/v2/library/nginx/manifests/latest",
            ),
            (
                "library/nginx",
                "blobs/sha256:abc123",
                "https://registry-1.docker.io/v2/library/nginx/blobs/sha256:abc123",
            ),
            (
                "clowdhaus/ocync",
                "tags/list",
                "https://registry-1.docker.io/v2/clowdhaus/ocync/tags/list",
            ),
        ] {
            let url = build_url(&test_base_url(), repository, endpoint).unwrap();
            assert_eq!(url.as_str(), expected);
        }
    }

    #[test]
    fn build_url_nested_repository() {
        let url = build_url(
            &Url::parse("https://ghcr.io").unwrap(),
            "clowdhaus/ocync/subpath",
            "manifests/v1.0",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://ghcr.io/v2/clowdhaus/ocync/subpath/manifests/v1.0"
        );
    }

    #[test]
    fn build_url_with_port() {
        let base = Url::parse("http://localhost:5000").unwrap();
        let url = build_url(&base, "myrepo", "manifests/latest").unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:5000/v2/myrepo/manifests/latest"
        );
    }

    #[test]
    fn build_v2_url_basic() {
        let url = build_v2_url(&test_base_url()).unwrap();
        assert_eq!(url.as_str(), "https://registry-1.docker.io/v2/");
    }

    #[test]
    fn builder_defaults() {
        let client = RegistryClient::builder(test_base_url()).build().unwrap();
        assert_eq!(client.base_url.as_str(), "https://registry-1.docker.io/");
        assert!(client.auth.is_none());
    }

    #[test]
    fn builder_custom_concurrency() {
        let client = RegistryClient::builder(test_base_url())
            .max_concurrent(4)
            .build()
            .unwrap();
        // The AIMD controller initialises per-action windows lazily. Before any
        // requests, the window limit equals ceil(DEFAULT_INITIAL_WINDOW) capped
        // at max_concurrent. DEFAULT_INITIAL_WINDOW is 1.0, so ceil(1.0) = 1.
        assert_eq!(client.aimd.window_limit(RegistryAction::BlobRead), 1);
    }

    #[test]
    fn user_agent_value() {
        assert!(USER_AGENT_VALUE.starts_with("ocync/"));
    }

    #[test]
    fn parse_rate_limit_docker_hub() {
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-remaining", "99;w=21600".parse().unwrap());
        assert_eq!(parse_rate_limit_remaining(&headers), Some(99));
    }

    #[test]
    fn parse_rate_limit_ghcr() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "42".parse().unwrap());
        assert_eq!(parse_rate_limit_remaining(&headers), Some(42));
    }

    #[test]
    fn parse_rate_limit_docker_hub_preferred_over_ghcr() {
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-remaining", "10;w=21600".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "999".parse().unwrap());
        // Docker Hub header takes precedence.
        assert_eq!(parse_rate_limit_remaining(&headers), Some(10));
    }

    #[test]
    fn parse_rate_limit_no_header() {
        let headers = HeaderMap::new();
        assert_eq!(parse_rate_limit_remaining(&headers), None);
    }

    #[test]
    fn parse_rate_limit_zero() {
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-remaining", "0;w=21600".parse().unwrap());
        assert_eq!(parse_rate_limit_remaining(&headers), Some(0));
    }

    #[test]
    fn rate_limit_remaining_none_before_any_response() {
        let client = RegistryClient::builder(test_base_url()).build().unwrap();
        assert_eq!(client.rate_limit_remaining(), None);
    }

    #[tokio::test]
    async fn rate_limit_remaining_updated_from_response_header() {
        let server = MockServer::start().await;
        let base_url = Url::parse(&server.uri()).unwrap();

        let client = RegistryClient::builder(base_url)
            .auth(StubAuth)
            .max_concurrent(50)
            .build()
            .unwrap();

        Mock::given(method("GET"))
            .and(path("/v2/repo/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ratelimit-remaining", "42;w=21600")
                    .set_body_string("ok"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let repo = RepositoryName::new("repo").unwrap();
        let _ = client
            .get(
                &repo,
                "manifests/latest",
                None,
                RegistryAction::ManifestRead,
            )
            .await;

        assert_eq!(
            client.rate_limit_remaining(),
            Some(42),
            "rate_limit_remaining should be updated from response header"
        );
    }

    /// Negative test: `rate_limit_remaining` must NOT be updated when the
    /// response has no rate-limit header. A missing header means the
    /// registry does not expose budget info, so the value should stay at
    /// whatever it was before (not be reset to None or 0).
    #[tokio::test]
    async fn rate_limit_remaining_not_reset_without_header() {
        let server = MockServer::start().await;
        let base_url = Url::parse(&server.uri()).unwrap();

        let client = RegistryClient::builder(base_url)
            .auth(StubAuth)
            .max_concurrent(50)
            .build()
            .unwrap();

        // First response: has rate-limit header.
        Mock::given(method("GET"))
            .and(path("/v2/repo/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ratelimit-remaining", "50;w=21600")
                    .set_body_string("ok"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let repo = RepositoryName::new("repo").unwrap();
        let _ = client
            .get(
                &repo,
                "manifests/latest",
                None,
                RegistryAction::ManifestRead,
            )
            .await;
        assert_eq!(client.rate_limit_remaining(), Some(50));

        // Second response: no rate-limit header.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/v2/repo/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = client
            .get(
                &repo,
                "manifests/latest",
                None,
                RegistryAction::ManifestRead,
            )
            .await;

        // Value should still be 50, NOT reset.
        assert_eq!(
            client.rate_limit_remaining(),
            Some(50),
            "rate_limit_remaining must not be reset when header is absent"
        );
    }

    #[tokio::test]
    async fn get_429_shrinks_aimd_window() {
        let server = MockServer::start().await;
        let base_url = Url::parse(&server.uri()).unwrap();

        let client = RegistryClient::builder(base_url)
            .auth(StubAuth)
            .max_concurrent(50)
            .build()
            .unwrap();

        // Seed the AIMD window for ManifestRead with a success so the window
        // has a known starting value we can compare against after the 429.
        Mock::given(method("GET"))
            .and(path("/v2/repo/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let repo = RepositoryName::new("repo").unwrap();
        let _ = client
            .get(
                &repo,
                "manifests/latest",
                None,
                RegistryAction::ManifestRead,
            )
            .await;

        let limit_before = client.aimd.window_limit(RegistryAction::ManifestRead);

        // Reset the mock to return 429 on the next request.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/v2/repo/manifests/latest"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .expect(1)
            .mount(&server)
            .await;

        let result = client
            .get(
                &repo,
                "manifests/latest",
                None,
                RegistryAction::ManifestRead,
            )
            .await;

        // The 429 response is classified as a RegistryError after report_permit
        // has already called permit.throttled().
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().status_code(),
            Some(StatusCode::TOO_MANY_REQUESTS),
        );

        let limit_after = client.aimd.window_limit(RegistryAction::ManifestRead);
        assert!(
            limit_after < limit_before,
            "AIMD window should shrink after 429: before={limit_before}, after={limit_after}",
        );
    }
}
