//! Google Artifact Registry / Container Registry authentication provider.
//!
//! Uses `google-cloud-auth` to obtain `OAuth2` access tokens via Application
//! Default Credentials (ADC), then performs standard OCI Bearer token
//! exchange using `oauth2accesstoken` as the username. This is the Rust
//! equivalent of:
//!
//! ```sh
//! gcloud auth print-access-token \
//!   | docker login -u oauth2accesstoken --password-stdin us-docker.pkg.dev
//! ```
//!
//! ADC resolves credentials in order: `GOOGLE_APPLICATION_CREDENTIALS` env
//! var, `~/.config/gcloud/application_default_credentials.json`, GCE/GKE
//! metadata server. GKE Workload Identity is transparent.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::Mutex;

use super::ecr::SdkCredentialCache;
use super::token_exchange;
use super::{AuthProvider, Credentials, Scope, Token, scopes_cache_key};
use crate::error::Error;

/// Username for GCP `OAuth2` token exchange.
///
/// GAR and GCR accept `oauth2accesstoken` as the username in HTTP Basic
/// auth, with the `OAuth2` access token as the password.
const GCP_USERNAME: &str = "oauth2accesstoken";

/// Default GCP `OAuth2` token lifetime (10 minutes).
///
/// `google-cloud-auth` does not expose the actual `expires_in` from the
/// metadata server or token endpoint response. Standard GCP tokens last
/// 1 hour, but Workload Identity Federation can issue tokens as short as
/// 15 minutes. A 10-minute default ensures we proactively re-fetch
/// before WIF tokens expire, avoiding unnecessary 401 -> invalidate ->
/// re-fetch cycles in watch mode. The cost is ~6 local ADC refreshes
/// per hour instead of 1 -- cheap since the metadata server and file
/// reads are sub-millisecond.
const GCP_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(600);

/// Abstraction over GCP token acquisition for testability.
///
/// The real implementation wraps `google-cloud-auth`. Tests inject a mock
/// that returns canned tokens without hitting GCP.
pub(crate) trait GcpTokenSource: Send + Sync + fmt::Debug {
    /// Obtain a raw `OAuth2` access token string.
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>>;
}

/// Default [`GcpTokenSource`] backed by `google-cloud-auth` ADC.
struct GoogleCloudTokenSource {
    credentials: google_cloud_auth::credentials::AccessTokenCredentials,
    /// Registry hostname for error context.
    hostname: String,
}

impl fmt::Debug for GoogleCloudTokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCloudTokenSource")
            .finish_non_exhaustive()
    }
}

impl GcpTokenSource for GoogleCloudTokenSource {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
        Box::pin(async move {
            let token = self
                .credentials
                .access_token()
                .await
                .map_err(|e| Error::AuthFailed {
                    registry: self.hostname.clone(),
                    reason: format!("GCP access token request failed: {e}"),
                })?;
            Ok(token.token)
        })
    }
}

/// Google Artifact Registry / Container Registry authentication provider.
///
/// Obtains GCP `OAuth2` access tokens via Application Default Credentials,
/// then uses them as HTTP Basic credentials (`oauth2accesstoken:<token>`)
/// in the standard OCI Bearer token exchange flow.
///
/// SDK credentials are cached via [`SdkCredentialCache`] with the same
/// read-lock fast path / write-lock refresh pattern as
/// [`super::ecr::EcrAuth`], supporting long-running processes (watch mode).
///
/// Bearer tokens are cached per-scope with the same challenge-cache
/// optimization as [`super::basic::BasicAuth`].
pub struct GcpAuth {
    /// The registry base URL.
    base_url: String,
    /// HTTP client for OCI token exchange.
    http: reqwest::Client,
    /// GCP token source for credential refresh.
    api: Box<dyn GcpTokenSource>,
    /// Cached SDK credentials (username/password) with TTL-based refresh.
    sdk_credential_cache: SdkCredentialCache<Credentials>,
    /// Cached Bearer tokens keyed by sorted scope strings.
    cache: Mutex<HashMap<String, Token>>,
    /// Cached `WWW-Authenticate` challenge to skip redundant `/v2/` pings.
    challenge_cache: token_exchange::ChallengeCache,
}

impl fmt::Debug for GcpAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcpAuth")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl GcpAuth {
    /// Create a new GCP auth provider using Application Default Credentials.
    ///
    /// Credentials are fetched lazily on the first `get_token` call and
    /// refreshed automatically when approaching expiry.
    pub async fn new(hostname: &str, http: reqwest::Client) -> Result<Self, Error> {
        let hostname = hostname.to_owned();
        let credentials = google_cloud_auth::credentials::Builder::default()
            .with_scopes(["https://www.googleapis.com/auth/devstorage.read_write"])
            .build_access_token_credentials()
            .map_err(|e| Error::AuthFailed {
                registry: hostname.clone(),
                reason: format!("GCP credential setup failed: {e}"),
            })?;

        Ok(Self {
            base_url: format!("https://{hostname}"),
            http,
            api: Box::new(GoogleCloudTokenSource {
                credentials,
                hostname,
            }),
            sdk_credential_cache: SdkCredentialCache::new(),
            cache: Mutex::new(HashMap::new()),
            challenge_cache: token_exchange::ChallengeCache::new(),
        })
    }

    /// Create a GCP auth provider with an injected token source (test helper).
    #[cfg(test)]
    fn with_api(
        base_url: impl Into<String>,
        http: reqwest::Client,
        api: impl GcpTokenSource + 'static,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            api: Box::new(api),
            sdk_credential_cache: SdkCredentialCache::new(),
            cache: Mutex::new(HashMap::new()),
            challenge_cache: token_exchange::ChallengeCache::new(),
        }
    }

    /// Get or refresh GCP `OAuth2` credentials.
    async fn ensure_credentials(&self) -> Result<Credentials, Error> {
        self.sdk_credential_cache
            .get_or_refresh("gcp-oauth2-credential", async {
                tracing::debug!("GCP OAuth2 credentials expired or missing, refreshing");
                let access_token = self.api.access_token().await.map_err(|e| {
                    tracing::warn!(error = %e, "GCP access token request failed");
                    e
                })?;
                let credentials = Credentials::Basic {
                    username: GCP_USERNAME.to_owned(),
                    password: access_token,
                };
                tracing::debug!(
                    ttl_secs = GCP_DEFAULT_TOKEN_TTL.as_secs(),
                    "GCP OAuth2 credentials refreshed"
                );
                Ok((credentials, GCP_DEFAULT_TOKEN_TTL))
            })
            .await
    }
}

impl AuthProvider for GcpAuth {
    fn name(&self) -> &'static str {
        "gcp"
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

            // Ensure GCP credentials are fresh before token exchange.
            let credentials = self.ensure_credentials().await?;

            tracing::debug!(base_url = %self.base_url, scope = %key, "token cache miss, exchanging");
            let cached_challenge = self.challenge_cache.get().await;
            let (token, challenge) = token_exchange::exchange(
                &self.http,
                &self.base_url,
                &scopes,
                Some(&credentials),
                cached_challenge.as_ref(),
            )
            .await
            .map_err(|e| {
                tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                e
            })?;

            self.challenge_cache.set(challenge).await;

            cache.insert(key, token.clone());

            Ok(token)
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            tracing::debug!(base_url = %self.base_url, entries = cache.len(), "invalidating GCP token cache");
            cache.clear();
            drop(cache);

            self.challenge_cache.clear().await;

            // Also clear SDK credentials so they are re-fetched on next use.
            self.sdk_credential_cache.clear().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use wiremock::MockServer;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::*;

    /// Mock GCP token source that returns pre-configured access tokens.
    #[derive(Debug)]
    struct MockGcpTokenSource {
        tokens: Mutex<VecDeque<String>>,
    }

    impl MockGcpTokenSource {
        fn succeeding(token: &str) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(
                    std::iter::repeat_n(token.to_owned(), 10).collect::<Vec<_>>(),
                )),
            }
        }

        fn with_tokens(tokens: Vec<String>) -> Self {
            Self {
                tokens: Mutex::new(VecDeque::from(tokens)),
            }
        }
    }

    impl GcpTokenSource for MockGcpTokenSource {
        fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
            Box::pin(async move {
                self.tokens
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| Error::AuthFailed {
                        registry: "mock-gcp".into(),
                        reason: "mock: no more tokens".into(),
                    })
            })
        }
    }

    fn mount_v2_and_token(server: &MockServer, token_value: &str) -> Vec<Mock> {
        vec![
            Mock::given(method("GET")).and(path("/v2/")).respond_with(
                ResponseTemplate::new(401).insert_header(
                    "WWW-Authenticate",
                    format!(
                        r#"Bearer realm="{}/token",service="us-docker.pkg.dev""#,
                        server.uri()
                    ),
                ),
            ),
            Mock::given(method("GET")).and(path("/token")).respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": token_value, "expires_in": 300})),
            ),
        ]
    }

    #[tokio::test]
    async fn auth_name() {
        let auth = GcpAuth::with_api(
            "https://us-docker.pkg.dev",
            crate::test_http_client(),
            MockGcpTokenSource::succeeding("ya29.test"),
        );
        assert_eq!(auth.name(), "gcp");
    }

    #[tokio::test]
    async fn credentials_use_oauth2accesstoken_username() {
        let auth = GcpAuth::with_api(
            "https://us-docker.pkg.dev",
            crate::test_http_client(),
            MockGcpTokenSource::succeeding("ya29.test-token"),
        );
        let creds = auth.ensure_credentials().await.unwrap();
        match creds {
            Credentials::Basic { username, password } => {
                assert_eq!(username, GCP_USERNAME);
                assert_eq!(password, "ya29.test-token");
            }
        }
    }

    #[tokio::test]
    async fn token_exchange_with_gcp_credentials() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="us-docker.pkg.dev""#,
                    server.uri()
                ),
            ))
            .expect(1)
            .mount(&server)
            .await;

        // Verify the token endpoint receives Basic auth with the
        // oauth2accesstoken username and the GCP access token as password.
        // "oauth2accesstoken:ya29.gcp-token" base64 = "b2F1dGgyYWNjZXNzdG9rZW46eWEyOS5nY3AtdG9rZW4="
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header(
                "Authorization",
                "Basic b2F1dGgyYWNjZXNzdG9rZW46eWEyOS5nY3AtdG9rZW4=",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "bearer-tok", "expires_in": 300})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::succeeding("ya29.gcp-token"),
        );
        let token = auth
            .get_token(&[Scope::pull("project/repo")])
            .await
            .unwrap();
        assert_eq!(token.value(), "bearer-tok");
    }

    #[tokio::test]
    async fn sdk_credentials_cached_across_scopes() {
        let server = MockServer::start().await;
        for mock in mount_v2_and_token(&server, "cached") {
            mock.mount(&server).await;
        }

        // Only one token available. If the second get_token call re-fetches
        // credentials it will fail, proving they were cached.
        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec!["ya29.once".to_owned()]),
        );
        let t1 = auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();
        assert_eq!(t1.value(), "cached");
        assert_eq!(t2.value(), "cached");
    }

    #[tokio::test]
    async fn sdk_credentials_refresh_on_expiry() {
        let server = MockServer::start().await;
        for mock in mount_v2_and_token(&server, "refreshed") {
            mock.mount(&server).await;
        }

        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![
                "ya29.first".to_owned(),
                "ya29.second".to_owned(),
            ]),
        );

        // First call fetches "ya29.first" with GCP_DEFAULT_TOKEN_TTL.
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();

        // Overwrite the SDK cache with a near-zero TTL (10s < 30s
        // EARLY_REFRESH_WINDOW). This exercises the TTL-driven refresh
        // path in SdkCredentialCache, simulating what happens when the
        // underlying access token expires before our cache TTL.
        let stale = Credentials::Basic {
            username: GCP_USERNAME.to_owned(),
            password: "ya29.first".to_owned(),
        };
        auth.sdk_credential_cache
            .set(stale, Duration::from_secs(10))
            .await;

        // Clear the Bearer token cache so get_token() re-enters
        // ensure_credentials(). SDK creds are near-expiry, so
        // SdkCredentialCache triggers a refresh -> "ya29.second".
        auth.cache.lock().await.clear();

        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    #[tokio::test]
    async fn invalidation_clears_all_caches() {
        let server = MockServer::start().await;

        // /v2/ hit twice: once before invalidation, once after.
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="us-docker.pkg.dev""#,
                    server.uri()
                ),
            ))
            .expect(2)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "fresh", "expires_in": 300})),
            )
            .mount(&server)
            .await;

        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::succeeding("ya29.test"),
        );
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.invalidate().await;
        // After invalidation: SDK creds re-fetched, challenge cache cleared,
        // Bearer cache cleared. /v2/ is pinged again.
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    #[tokio::test]
    async fn challenge_cache_skips_v2_on_second_scope() {
        let server = MockServer::start().await;

        // /v2/ exactly once; challenge cached for second call.
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="us-docker.pkg.dev""#,
                    server.uri()
                ),
            ))
            .expect(1)
            .mount(&server)
            .await;

        // /token hit twice: different scopes produce separate Bearer tokens.
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "tok", "expires_in": 300})),
            )
            .expect(2)
            .mount(&server)
            .await;

        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::succeeding("ya29.test"),
        );
        auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();
    }

    #[tokio::test]
    async fn auth_propagates_access_token_error() {
        // Empty token queue -- access_token() returns an error immediately.
        let auth = GcpAuth::with_api(
            "https://us-docker.pkg.dev",
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![]),
        );
        let result = auth.get_token(&[Scope::pull("repo")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("mock: no more tokens"),
            "expected mock error, got: {err}"
        );
    }
}
