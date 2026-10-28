//! Google Artifact Registry / Container Registry authentication provider.
//!
//! Obtains `OAuth2` access tokens from Application Default Credentials,
//! resolved in [`crate::google`], then performs standard OCI Bearer token
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

use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::token_cache::TokenCache;
use super::token_exchange;
use super::{AuthProvider, Credentials, Scope, Token, registry_origin, scopes_cache_key};
use crate::aws::{AwsCredentialProviders, Clock};
use crate::credential_cache::{CredentialCache, remaining};
use crate::error::{Error, error_chain};
use crate::google::{self, AccessToken};
use crate::retry::{Bounded, retrying};

/// Username for GCP `OAuth2` token exchange.
///
/// GAR and GCR accept `oauth2accesstoken` as the username in HTTP Basic
/// auth, with the `OAuth2` access token as the password.
const GCP_USERNAME: &str = "oauth2accesstoken";

/// Abstraction over GCP token acquisition for testability.
///
/// The real implementation resolves Application Default Credentials. Tests
/// inject a mock that returns canned tokens without hitting GCP.
pub(crate) trait GcpTokenSource: Send + Sync + fmt::Debug {
    /// Obtain an `OAuth2` access token and its expiry.
    fn access_token(&self)
    -> Pin<Box<dyn Future<Output = Result<AccessToken, Error>> + Send + '_>>;

    /// Drop a token kept for reuse, so the next is fetched anew.
    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// The Google credentials one run's GAR and GCR providers share, so the
/// Application Default Credentials are loaded, and their token fetched,
/// once per run rather than once per registry host. Create one per run:
/// the next run, such as the next watch cycle, reads the credentials again.
#[derive(Default)]
pub struct GcpCredentials(RefCell<Option<Arc<Adc>>>);

impl fmt::Debug for GcpCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcpCredentials").finish_non_exhaustive()
    }
}

impl GcpCredentials {
    /// The credentials of `host`, loaded on first use. A failed load is not
    /// kept, so it fails only its caller.
    async fn adc(
        &self,
        host: &crate::aws::Host,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Arc<Adc>, google::Error> {
        if let Some(adc) = &*self.0.borrow() {
            return Ok(Arc::clone(adc));
        }
        let clients = google::Clients::new()?;
        let credentials = google::Credentials::load(host, &clients, aws_credentials).await?;
        let adc = Arc::new(Adc {
            credentials,
            clients,
            token: CredentialCache::new(),
        });
        *self.0.borrow_mut() = Some(Arc::clone(&adc));
        Ok(adc)
    }
}

/// Application Default Credentials and the token they last returned.
struct Adc {
    credentials: google::Credentials,
    clients: google::Clients,
    token: CredentialCache<AccessToken>,
}

impl fmt::Debug for Adc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Adc")
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

/// Default [`GcpTokenSource`]: the run's Application Default Credentials.
#[derive(Debug)]
struct AdcTokenSource {
    adc: Arc<Adc>,
    /// Registry hostname for error context.
    hostname: String,
}

impl GcpTokenSource for AdcTokenSource {
    fn access_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<AccessToken, Error>> + Send + '_>> {
        Box::pin(async move {
            let adc = &self.adc;
            adc.token
                .get_or_refresh(async {
                    // The prepare phase reaches this before the engine's
                    // retry exists, so a transient failure is sent again here.
                    let token = retrying(
                        "GCP access token",
                        &mut Bounded(google::Error::is_transient),
                        |_| adc.credentials.access_token(&adc.clients, Clock::System),
                    )
                    .await?;
                    let ttl = lifetime(&token);
                    Ok((token, ttl))
                })
                .await
                .map_err(|e: google::Error| {
                    Error::auth_failed(
                        &self.hostname,
                        format!("GCP access token request failed: {}", error_chain(&e)),
                    )
                })
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.adc.token.clear())
    }
}

/// How long `token` is kept. A token with no expiry is kept until a
/// registry refuses it, as upstream never refreshes one: the cache holds a
/// lifetime past what the clock can hold as never expiring.
fn lifetime(token: &AccessToken) -> Duration {
    token.expiry.map_or(Duration::MAX, remaining)
}

/// Google Artifact Registry / Container Registry authentication provider.
///
/// Obtains GCP `OAuth2` access tokens via Application Default Credentials,
/// then uses them as HTTP Basic credentials (`oauth2accesstoken:<token>`)
/// in the standard OCI Bearer token exchange flow.
///
/// SDK credentials are cached via [`CredentialCache`] with the same
/// read-lock fast path / write-lock refresh pattern as
/// [`super::ecr::EcrAuth`], supporting long-running processes (watch mode).
///
/// Bearer tokens are coalesced per scope via [`TokenCache`]: concurrent
/// fetches for the same scope produce one token exchange while distinct
/// scopes run in parallel.
pub struct GcpAuth {
    /// The registry base URL.
    base_url: String,
    /// HTTP client for OCI token exchange.
    http: reqwest::Client,
    /// GCP token source for credential refresh.
    api: Box<dyn GcpTokenSource>,
    /// Cached SDK credentials (username/password) with TTL-based refresh.
    credential_cache: CredentialCache<Credentials>,
    /// Per-scope coalescing Bearer token cache.
    tokens: TokenCache,
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
    /// `registry` is the base URL the registry's client uses.
    pub async fn new(
        registry: &url::Url,
        http: reqwest::Client,
        aws_credentials: &AwsCredentialProviders,
        gcp_credentials: &GcpCredentials,
    ) -> Result<Self, Error> {
        Self::on_host(
            registry,
            http,
            &crate::aws::Host::current(),
            aws_credentials,
            gcp_credentials,
        )
        .await
    }

    /// [`Self::new`] with the credentials of `host`.
    async fn on_host(
        registry: &url::Url,
        http: reqwest::Client,
        host: &crate::aws::Host,
        aws_credentials: &AwsCredentialProviders,
        gcp_credentials: &GcpCredentials,
    ) -> Result<Self, Error> {
        let hostname = registry.host_str().unwrap_or_default().to_owned();
        let adc = gcp_credentials
            .adc(host, aws_credentials)
            .await
            .map_err(|e| {
                Error::auth_failed(
                    &hostname,
                    format!("GCP credential setup failed: {}", error_chain(&e)),
                )
            })?;

        Ok(Self {
            base_url: registry_origin(registry),
            http,
            api: Box::new(AdcTokenSource { adc, hostname }),
            credential_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
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
            credential_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
            challenge_cache: token_exchange::ChallengeCache::new(),
        }
    }

    /// Get or refresh GCP `OAuth2` credentials.
    async fn ensure_credentials(&self) -> Result<Credentials, Error> {
        self.credential_cache
            .get_or_refresh(async {
                tracing::debug!("GCP OAuth2 credentials expired or missing, refreshing");
                let access_token = self.api.access_token().await.map_err(|e| {
                    tracing::warn!(error = %e, "GCP access token request failed");
                    e
                })?;
                let credentials = Credentials::Basic {
                    username: GCP_USERNAME.to_owned(),
                    password: access_token.token.expose().to_owned(),
                };
                match access_token.expiry {
                    Some(expiry) => tracing::debug!(
                        ttl_secs = remaining(expiry).as_secs(),
                        "GCP OAuth2 credentials refreshed"
                    ),
                    None => tracing::debug!("GCP OAuth2 credentials refreshed, with no expiry"),
                }
                Ok((credentials, lifetime(&access_token)))
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
            self.tokens
                .get_or_fetch(key.clone(), || async {
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
                    Ok(token)
                })
                .await
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let entries = self.tokens.len().await;
            tracing::debug!(base_url = %self.base_url, entries, "invalidating GCP token cache");
            self.tokens.clear().await;
            self.challenge_cache.clear().await;
            // Also clear the Google token, here and in the run's shared
            // cache, so it is fetched anew on next use.
            self.credential_cache.clear().await;
            self.api.invalidate().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tokio::sync::Mutex;
    use wiremock::MockServer;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::*;

    /// How long the mock's tokens last unless a test says otherwise.
    const HOUR: Option<Duration> = Some(Duration::from_secs(3600));

    /// Mock GCP token source that returns pre-configured access tokens,
    /// each lasting its own time from when it is handed out.
    #[derive(Debug)]
    struct MockGcpTokenSource {
        tokens: Mutex<VecDeque<(String, Option<Duration>)>>,
    }

    impl MockGcpTokenSource {
        fn succeeding(token: &str) -> Self {
            Self::with_tokens(vec![(token, HOUR); 10])
        }

        fn with_tokens(tokens: Vec<(&str, Option<Duration>)>) -> Self {
            Self {
                tokens: Mutex::new(
                    tokens
                        .into_iter()
                        .map(|(token, lasts)| (token.to_owned(), lasts))
                        .collect(),
                ),
            }
        }
    }

    impl GcpTokenSource for MockGcpTokenSource {
        fn access_token(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<AccessToken, Error>> + Send + '_>> {
            Box::pin(async move {
                let (token, lasts) =
                    self.tokens
                        .lock()
                        .await
                        .pop_front()
                        .ok_or_else(|| Error::AuthFailed {
                            registry: "mock-gcp".into(),
                            reason: "mock: no more tokens".into(),
                        })?;
                Ok(AccessToken {
                    token: token.into(),
                    expiry: lasts.map(|lasts| std::time::SystemTime::now() + lasts),
                })
            })
        }

        fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    /// The password of the credentials `auth` holds now.
    async fn password(auth: &GcpAuth) -> String {
        let Credentials::Basic { password, .. } = auth.ensure_credentials().await.unwrap();
        password
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
            MockGcpTokenSource::with_tokens(vec![("ya29.once", HOUR)]),
        );
        let t1 = auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();
        assert_eq!(t1.value(), "cached");
        assert_eq!(t2.value(), "cached");
    }

    /// A token that expires within the refresh window is fetched again on
    /// the next use. Breaks if the cache keeps a token past its own expiry.
    #[tokio::test]
    async fn a_short_lived_token_is_fetched_again() {
        let auth = GcpAuth::with_api(
            "https://us-docker.pkg.dev",
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![
                ("ya29.first", Some(Duration::from_secs(20))),
                ("ya29.second", HOUR),
            ]),
        );
        assert_eq!(password(&auth).await, "ya29.first");
        assert_eq!(password(&auth).await, "ya29.second");
    }

    /// A token with no expiry is kept, as upstream never refreshes one.
    /// Breaks if it is fetched again on each use.
    #[tokio::test]
    async fn a_token_without_expiry_is_kept() {
        let auth = GcpAuth::with_api(
            "https://us-docker.pkg.dev",
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![("ya29.only", None)]),
        );
        assert_eq!(password(&auth).await, "ya29.only");
        assert_eq!(password(&auth).await, "ya29.only");
    }

    /// A transient failure from Google is sent again before the provider
    /// gives up, as the prepare phase has no other retry; a refusal is not.
    /// Breaks if a 503 fails the load or a 401 is sent twice.
    #[tokio::test]
    async fn retries_a_transient_google_failure() {
        for (first, calls, succeeds) in [(503, 2, true), (401, 1, false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(first))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            mount_user_token(&server, calls - 1).await;
            let auth = adc_auth(
                "https://us-docker.pkg.dev",
                &user_host(&server),
                &GcpCredentials::default(),
            )
            .await;

            let credentials = auth.ensure_credentials().await;

            assert_eq!(credentials.is_ok(), succeeds, "{first}: {credentials:?}");
        }
    }

    /// The registries of one run share one load of the credentials and one
    /// Google token, and an invalidation fetches it anew.
    /// Breaks if each registry fetches its own token, or a refused token is
    /// handed out again.
    #[tokio::test]
    async fn registries_share_the_google_token() {
        let server = MockServer::start().await;
        mount_user_token(&server, 2).await;
        let host = user_host(&server);
        let shared = GcpCredentials::default();
        let us = adc_auth("https://us-docker.pkg.dev", &host, &shared).await;
        let europe = adc_auth("https://europe-docker.pkg.dev", &host, &shared).await;

        assert_eq!(password(&us).await, "ya29.user");
        assert_eq!(password(&europe).await, "ya29.user");
        us.invalidate().await;
        assert_eq!(password(&us).await, "ya29.user");
    }

    /// An authorized user's token endpoint at `server`, answering `calls`
    /// times.
    async fn mount_user_token(server: &MockServer, calls: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.user", "token_type": "Bearer", "expires_in": 3600
            })))
            .expect(calls)
            .mount(server)
            .await;
    }

    /// A host whose credentials are an authorized user refreshing at
    /// `server`.
    fn user_host(server: &MockServer) -> crate::aws::Host {
        let file = serde_json::json!({
            "type": "authorized_user",
            "client_id": "client",
            "client_secret": "secret",
            "refresh_token": "refresh",
            "token_uri": format!("{}/token", server.uri()),
        });
        google::tests::host(
            crate::aws::Platform::Unix,
            &[("GOOGLE_APPLICATION_CREDENTIALS", "/adc.json")],
            &[("/adc.json", file.to_string().into_bytes())],
        )
    }

    /// A provider for `registry` with `host`'s credentials, shared through
    /// `shared`.
    async fn adc_auth(registry: &str, host: &crate::aws::Host, shared: &GcpCredentials) -> GcpAuth {
        GcpAuth::on_host(
            &registry.parse().unwrap(),
            crate::test_http_client(),
            host,
            &AwsCredentialProviders::default(),
            shared,
        )
        .await
        .unwrap()
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
