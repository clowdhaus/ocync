//! HTTP client for a single OCI registry endpoint.

use std::sync::Arc;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use tokio::sync::Semaphore;
use url::Url;

use crate::auth::{AuthProvider, Scope};
use crate::error::Error;

const DEFAULT_MAX_CONCURRENT: usize = 8;
const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024; // 8 MiB
const USER_AGENT_VALUE: &str = concat!("ocync/", env!("CARGO_PKG_VERSION"));

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
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Set the authentication provider.
    pub fn auth(mut self, auth: impl AuthProvider + 'static) -> Self {
        self.auth = Some(Box::new(auth));
        self
    }

    /// Set the authentication provider from a boxed trait object.
    pub fn auth_boxed(mut self, auth: Box<dyn AuthProvider>) -> Self {
        self.auth = Some(auth);
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

        Ok(RegistryClient {
            base_url: self.url,
            http,
            auth: self.auth,
            semaphore: Arc::new(Semaphore::new(self.max_concurrent)),
            chunk_size: self.chunk_size,
        })
    }
}

/// HTTP client for a single OCI registry.
///
/// Each `RegistryClient` targets one registry host and owns its auth provider
/// and concurrency semaphore. Construct via [`RegistryClientBuilder`].
pub struct RegistryClient {
    pub(crate) base_url: Url,
    pub(crate) http: reqwest::Client,
    pub(crate) auth: Option<Box<dyn AuthProvider>>,
    pub(crate) semaphore: Arc<Semaphore>,
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

    /// The base URL of this registry.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// The configured chunk size for blob uploads.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Ping the registry's `/v2/` endpoint.
    ///
    /// Returns `Ok(())` if the registry responds with 200 or 401 (which confirms
    /// the endpoint exists and speaks the OCI Distribution protocol).
    pub async fn ping(&self) -> Result<(), Error> {
        let url = build_v2_url(&self.base_url)?;
        let resp = self.http.get(url).send().await?;
        let status = resp.status();

        if status.is_success() || status.as_u16() == 401 {
            Ok(())
        } else {
            let message = resp.text().await.unwrap_or_default();
            Err(Error::RegistryError {
                status: status.as_u16(),
                message,
            })
        }
    }

    /// Perform an authenticated GET request.
    ///
    /// Acquires a semaphore permit, attaches auth headers, and retries once
    /// on 401 (invalidating the cached token first).
    pub async fn get(
        &self,
        repository: &str,
        path: &str,
        accept: Option<&str>,
    ) -> Result<reqwest::Response, Error> {
        let url = build_url(&self.base_url, repository, path)?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let scopes = [Scope::pull(repository)];

        // First attempt.
        let resp = self.send_get(&url, &scopes, accept).await?;
        if resp.status().as_u16() != 401 {
            return classify_response(resp, &self.base_url, repository).await;
        }

        // 401 — invalidate cached token and retry once.
        tracing::debug!(url = %url, "got 401, invalidating token and retrying");
        if let Some(ref auth) = self.auth {
            auth.invalidate().await;
        }
        let resp = self.send_get(&url, &scopes, accept).await?;
        classify_response(resp, &self.base_url, repository).await
    }

    /// Perform an authenticated HEAD request.
    ///
    /// Same retry logic as [`get`](Self::get).
    pub async fn head(&self, repository: &str, path: &str) -> Result<reqwest::Response, Error> {
        let url = build_url(&self.base_url, repository, path)?;
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        let scopes = [Scope::pull(repository)];

        let resp = self.send_head(&url, &scopes).await?;
        if resp.status().as_u16() != 401 {
            return classify_response(resp, &self.base_url, repository).await;
        }

        // 401 — invalidate cached token and retry once.
        tracing::debug!(url = %url, "got 401, invalidating token and retrying");
        if let Some(ref auth) = self.auth {
            auth.invalidate().await;
        }
        let resp = self.send_head(&url, &scopes).await?;
        classify_response(resp, &self.base_url, repository).await
    }

    /// Internal: send a GET with auth headers.
    async fn send_get(
        &self,
        url: &Url,
        scopes: &[Scope],
        accept: Option<&str>,
    ) -> Result<reqwest::Response, Error> {
        let mut headers = self.auth_headers(scopes).await?;
        if let Some(accept) = accept {
            if let Ok(val) = HeaderValue::from_str(accept) {
                headers.insert(ACCEPT, val);
            }
        }

        Ok(self.http.get(url.clone()).headers(headers).send().await?)
    }

    /// Internal: send a HEAD with auth headers.
    async fn send_head(&self, url: &Url, scopes: &[Scope]) -> Result<reqwest::Response, Error> {
        let headers = self.auth_headers(scopes).await?;
        Ok(self.http.head(url.clone()).headers(headers).send().await?)
    }

    /// Build auth headers for a request.
    pub(crate) async fn auth_headers(&self, scopes: &[Scope]) -> Result<HeaderMap, Error> {
        let mut headers = HeaderMap::new();

        if let Some(ref auth) = self.auth {
            let token = auth.get_token(scopes).await?;
            let value = token.value();
            if !value.is_empty() {
                let header_value = HeaderValue::from_str(&format!("Bearer {value}"))
                    .map_err(|e| Error::Other(format!("invalid auth header: {e}")))?;
                headers.insert(AUTHORIZATION, header_value);
            }
        }

        Ok(headers)
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

    let registry = base_url.host_str().unwrap_or("unknown").to_owned();

    match status.as_u16() {
        401 => Err(Error::Unauthorized { registry }),
        403 => Err(Error::Forbidden {
            registry,
            repository: repository.to_owned(),
        }),
        404 => {
            let message = resp.text().await.unwrap_or_default();
            Err(Error::NotFound(message))
        }
        _ => {
            let message = resp.text().await.unwrap_or_default();
            Err(Error::RegistryError {
                status: status.as_u16(),
                message,
            })
        }
    }
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
    use super::*;

    fn test_base_url() -> Url {
        Url::parse("https://registry-1.docker.io").unwrap()
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
        assert_eq!(client.base_url().as_str(), "https://registry-1.docker.io/");
        assert_eq!(client.chunk_size(), DEFAULT_CHUNK_SIZE);
        assert!(client.auth.is_none());
    }

    #[test]
    fn builder_custom_chunk_size() {
        let client = RegistryClient::builder(test_base_url())
            .chunk_size(4 * 1024 * 1024)
            .build()
            .unwrap();
        assert_eq!(client.chunk_size(), 4 * 1024 * 1024);
    }

    #[test]
    fn builder_custom_concurrency() {
        let client = RegistryClient::builder(test_base_url())
            .max_concurrent(4)
            .build()
            .unwrap();
        assert_eq!(client.semaphore.available_permits(), 4);
    }

    #[test]
    fn user_agent_value() {
        assert!(USER_AGENT_VALUE.starts_with("ocync/"));
    }
}
