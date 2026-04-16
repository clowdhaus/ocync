//! HTTP client for a single OCI registry endpoint.

use http::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use url::Url;

use crate::aimd::{AimdController, RegistryAction};
use crate::auth::{AuthProvider, Scope};
use crate::error::Error;
use crate::spec::{RegistryAuthority, RepositoryName};

const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 50;
const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024; // 8 MiB
const USER_AGENT_VALUE: &str = concat!("ocync/", env!("CARGO_PKG_VERSION"));

use crate::auth::AuthScheme;

/// Builder for [`RegistryClient`].
pub struct RegistryClientBuilder {
    url: Url,
    auth: Option<Box<dyn AuthProvider>>,
    max_concurrent: usize,
    chunk_size: usize,
}

impl std::fmt::Debug for RegistryClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryClientBuilder")
            .field("url", &self.url)
            .field("max_concurrent", &self.max_concurrent)
            .field("chunk_size", &self.chunk_size)
            .finish_non_exhaustive()
    }
}

impl RegistryClientBuilder {
    /// Create a new builder for the given registry URL.
    ///
    /// The URL should be the base registry URL (e.g. `https://registry-1.docker.io`).
    pub fn new(url: Url) -> Self {
        Self {
            url,
            auth: None,
            max_concurrent: DEFAULT_MAX_CONCURRENT_REQUESTS,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Set the authentication provider.
    pub fn auth(mut self, auth: impl AuthProvider + 'static) -> Self {
        self.auth = Some(Box::new(auth));
        self
    }

    /// Set the maximum number of concurrent requests.
    pub fn max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n;
        self
    }

    /// Set the chunk size for blob uploads (in bytes).
    pub fn chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size;
        self
    }

    /// Build the registry client.
    pub fn build(self) -> Result<RegistryClient, Error> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT_VALUE)
            .build()?;

        let aimd = AimdController::new(
            self.url.host_str().unwrap_or("unknown"),
            self.max_concurrent,
        );
        Ok(RegistryClient {
            base_url: self.url,
            http,
            auth: self.auth,
            aimd,
            chunk_size: self.chunk_size,
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
    pub(crate) auth: Option<Box<dyn AuthProvider>>,
    pub(crate) aimd: AimdController,
    pub(crate) chunk_size: usize,
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryClient")
            .field("base_url", &self.base_url)
            .field("chunk_size", &self.chunk_size)
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// Create a builder for this client.
    pub fn builder(url: Url) -> RegistryClientBuilder {
        RegistryClientBuilder::new(url)
    }

    /// Ping the registry's `/v2/` endpoint.
    ///
    /// Returns `Ok(())` if the registry responds with 200 or 401 (which confirms
    /// the endpoint exists and speaks the OCI Distribution protocol).
    pub async fn ping(&self) -> Result<(), Error> {
        let url = build_v2_url(&self.base_url)?;
        let resp = self.http.get(url).send().await?;
        let status = resp.status();

        if status.is_success() || status == StatusCode::UNAUTHORIZED {
            Ok(())
        } else {
            let message = resp.text().await.unwrap_or_default();
            Err(Error::RegistryError { status, message })
        }
    }

    /// Return the registry authority as `host:port` for use as a cache key.
    ///
    /// Returns `Err` if the URL has no host (a bug in client construction).
    pub fn registry_authority(&self) -> Result<RegistryAuthority, Error> {
        let host = self
            .base_url
            .host_str()
            .ok_or_else(|| Error::Other(format!("registry URL has no host: {}", self.base_url)))?;
        let port = self.base_url.port_or_known_default().unwrap_or(443);
        Ok(RegistryAuthority::new(format!("{host}:{port}")))
    }

    /// Perform an authenticated GET request.
    ///
    /// Acquires an AIMD permit for the given operation, attaches auth headers,
    /// and retries once on 401 (invalidating the cached token first). Reports
    /// throttle on 429 so the AIMD window adapts.
    pub async fn get(
        &self,
        repository: &RepositoryName,
        path: &str,
        accept: Option<&str>,
        op: RegistryAction,
    ) -> Result<reqwest::Response, Error> {
        let url = build_url(&self.base_url, repository, path)?;
        let scopes = [Scope::pull(repository.as_str())];

        let resp = self
            .send_with_aimd(op, &scopes, "GET", |mut headers| {
                if let Some(accept) = accept {
                    if let Ok(val) = HeaderValue::from_str(accept) {
                        headers.insert(ACCEPT, val);
                    }
                }
                self.http.get(url.clone()).headers(headers)
            })
            .await?;

        classify_response(resp, &self.base_url, repository).await
    }

    /// Perform an authenticated HEAD request.
    ///
    /// Same AIMD and retry logic as [`get`](Self::get). Pass `accept` to
    /// set an `Accept` header, ensuring content negotiation matches what
    /// [`get`](Self::get) would return for the same resource.
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

    /// Send a request with AIMD permit tracking and 401 retry.
    ///
    /// Combines permit acquisition, the authenticated retry loop, and AIMD
    /// feedback reporting into a single call. Callers no longer need to
    /// manually acquire/report permits.
    pub(crate) async fn send_with_aimd(
        &self,
        action: RegistryAction,
        scopes: &[Scope],
        context: &str,
        build_request: impl Fn(HeaderMap) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        let permit = self.aimd.acquire(action).await;
        let result = self.send_with_retry(scopes, context, build_request).await;
        report_permit(permit, &result);
        result
    }

    /// Send a request with 401 retry and token invalidation.
    ///
    /// Builds the request via `build_request` (called with fresh auth headers),
    /// sends it, and if a 401 is returned, invalidates the cached token and
    /// retries once. Does NOT acquire an AIMD permit — callers manage their
    /// own permits.
    async fn send_with_retry(
        &self,
        scopes: &[Scope],
        context: &str,
        build_request: impl Fn(HeaderMap) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        let headers = self.auth_headers(scopes).await?;
        let resp = build_request(headers).send().await?;

        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        tracing::debug!(context, "got 401, invalidating token and retrying");
        self.invalidate_auth().await;
        let headers = self.auth_headers(scopes).await?;
        Ok(build_request(headers).send().await?)
    }

    /// Invalidate the cached auth token.
    ///
    /// Call this before retrying a request after a 401 response so the auth
    /// provider fetches a fresh token instead of returning the stale one.
    async fn invalidate_auth(&self) {
        if let Some(ref auth) = self.auth {
            auth.invalidate().await;
        }
    }

    /// Build auth headers for a request.
    async fn auth_headers(&self, scopes: &[Scope]) -> Result<HeaderMap, Error> {
        let mut headers = HeaderMap::new();

        if let Some(ref auth) = self.auth {
            let token = auth.get_token(scopes).await?;
            let value = token.value();
            if !value.is_empty() {
                let prefix = match token.scheme() {
                    AuthScheme::Bearer => "Bearer",
                    AuthScheme::Basic => "Basic",
                };
                let header_value = HeaderValue::from_str(&format!("{prefix} {value}"))
                    .map_err(|e| Error::Other(format!("invalid auth header: {e}")))?;
                headers.insert(AUTHORIZATION, header_value);
            }
        }

        Ok(headers)
    }
}

/// Report AIMD permit outcome based on the HTTP response.
///
/// Calls [`AimdPermit::throttled`] on 429, [`AimdPermit::success`] otherwise.
/// Transport errors (no response at all) are treated as success by the permit's
/// drop impl, so we only need to handle the `Ok` case explicitly.
fn report_permit(permit: crate::aimd::AimdPermit<'_>, result: &Result<reqwest::Response, Error>) {
    match result {
        Ok(resp) if resp.status() == StatusCode::TOO_MANY_REQUESTS => permit.throttled(),
        _ => permit.success(),
    }
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
    let body = resp.text().await.unwrap_or_default();
    let message = if body.is_empty() {
        format!("{registry}/{repository}")
    } else {
        format!("{registry}/{repository}: {body}")
    };

    Err(Error::RegistryError { status, message })
}

/// Construct a URL for the `/v2/` endpoint.
fn build_v2_url(base: &Url) -> Result<Url, Error> {
    base.join("/v2/")
        .map_err(|e| Error::Other(format!("failed to build /v2/ URL: {e}")))
}

/// Construct a URL for `/v2/{repository}/{path}`.
pub(crate) fn build_url(base: &Url, repository: &str, path: &str) -> Result<Url, Error> {
    let full_path = format!("/v2/{repository}/{path}");
    base.join(&full_path)
        .map_err(|e| Error::Other(format!("failed to build URL '{full_path}': {e}")))
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

        fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
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

    #[test]
    fn build_url_manifests() {
        let url = build_url(&test_base_url(), "library/nginx", "manifests/latest").unwrap();
        assert_eq!(
            url.as_str(),
            "https://registry-1.docker.io/v2/library/nginx/manifests/latest"
        );
    }

    #[test]
    fn build_url_blobs() {
        let url = build_url(&test_base_url(), "library/nginx", "blobs/sha256:abc123").unwrap();
        assert_eq!(
            url.as_str(),
            "https://registry-1.docker.io/v2/library/nginx/blobs/sha256:abc123"
        );
    }

    #[test]
    fn build_url_tags_list() {
        let url = build_url(&test_base_url(), "clowdhaus/ocync", "tags/list").unwrap();
        assert_eq!(
            url.as_str(),
            "https://registry-1.docker.io/v2/clowdhaus/ocync/tags/list"
        );
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
        assert_eq!(client.chunk_size, DEFAULT_CHUNK_SIZE);
        assert!(client.auth.is_none());
    }

    #[test]
    fn builder_custom_chunk_size() {
        let client = RegistryClient::builder(test_base_url())
            .chunk_size(4 * 1024 * 1024)
            .build()
            .unwrap();
        assert_eq!(client.chunk_size, 4 * 1024 * 1024);
    }

    #[test]
    fn builder_custom_concurrency() {
        let client = RegistryClient::builder(test_base_url())
            .max_concurrent(4)
            .build()
            .unwrap();
        // The AIMD controller initialises per-action windows lazily, so verify
        // via the window_limit accessor (which returns the default initial value
        // capped at max_concurrent before any requests are made).
        assert_eq!(client.aimd.window_limit(RegistryAction::BlobRead), 4);
    }

    #[test]
    fn user_agent_value() {
        assert!(USER_AGENT_VALUE.starts_with("ocync/"));
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

        let repo = RepositoryName::new("repo");
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
