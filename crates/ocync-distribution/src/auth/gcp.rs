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

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, OnceCell};

use super::token_cache::TokenCache;
use super::token_exchange;
use super::{AuthProvider, Credentials, Scope, Token, registry_origin, scopes_cache_key};
use crate::aws::{AwsCredentialProviders, Clock};
use crate::credential_cache::{CredentialCache, remaining};
use crate::error::{Error, error_chain};
use crate::google::{self, AccessToken, Expiry};

/// Username for GCP `OAuth2` token exchange.
///
/// GAR and GCR accept `oauth2accesstoken` as the username in HTTP Basic
/// auth, with the `OAuth2` access token as the password.
const GCP_USERNAME: &str = "oauth2accesstoken";

/// Abstraction over GCP token acquisition for testability.
///
/// The real implementation resolves Application Default Credentials. Tests
/// inject a mock that returns canned tokens without hitting GCP.
trait GcpTokenSource: Send + Sync + fmt::Debug {
    /// Obtain an `OAuth2` access token and its expiry.
    fn access_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<AccessToken, google::Error>> + Send + '_>>;
}

/// The Google credentials one run's GAR and GCR providers share, so the
/// Application Default Credentials are loaded, and their token fetched,
/// once per run rather than once per registry host, and hosts that ask at
/// once share the one load. A failed load is kept too: a load already sends
/// a transient failure again, so another host would only repeat the same
/// doomed requests. Create one per run: the next run, such as the next
/// watch cycle, reads the credentials again.
#[derive(Default)]
pub struct SharedGoogleToken(OnceCell<Result<Arc<CachedGoogleToken>, Arc<google::Error>>>);

impl fmt::Debug for SharedGoogleToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedGoogleToken").finish_non_exhaustive()
    }
}

impl SharedGoogleToken {
    /// The cached token of `host`'s credentials, loaded on first use.
    async fn cached_token(
        &self,
        host: &crate::aws::Host,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Arc<CachedGoogleToken>, Arc<google::Error>> {
        self.0
            .get_or_init(|| async {
                AdcTokenSource::load(host, aws_credentials)
                    .await
                    .map(|source| Arc::new(CachedGoogleToken::new(source)))
                    .map_err(Arc::new)
            })
            .await
            .clone()
    }
}

/// A token source and the token it last returned, kept until it is about
/// to expire or a registry's token exchange refuses it with 401.
struct CachedGoogleToken {
    source: Box<dyn GcpTokenSource>,
    token: CredentialCache<AccessToken, Arc<google::Error>>,
}

impl fmt::Debug for CachedGoogleToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedGoogleToken")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl CachedGoogleToken {
    fn new(source: impl GcpTokenSource + 'static) -> Self {
        Self {
            source: Box::new(source),
            token: CredentialCache::new(),
        }
    }

    /// The kept token, or a new one. Providers waiting on a fetch that
    /// failed get its error rather than each asking again.
    async fn get(&self) -> Result<AccessToken, Arc<google::Error>> {
        self.token
            .get_or_refresh_sharing_failure(async {
                tracing::debug!("GCP OAuth2 token expired or missing, refreshing");
                let token = self.source.access_token().await.map_err(Arc::new)?;
                // A token with no expiry is kept until a token exchange
                // refuses it, as upstream never refreshes one: the cache
                // holds a lifetime past what the clock can hold as never
                // expiring.
                let ttl = match token.expiry {
                    Expiry::At(expiry) => {
                        let ttl = remaining(expiry);
                        tracing::debug!(ttl_secs = ttl.as_secs(), "GCP OAuth2 token refreshed");
                        ttl
                    }
                    Expiry::Never => {
                        tracing::debug!("GCP OAuth2 token refreshed, with no expiry");
                        Duration::MAX
                    }
                };
                Ok((token, ttl))
            })
            .await
    }

    /// Drop the kept token if it is `refused`, so one registry's refusal
    /// does not discard a token another has fetched since.
    fn refuse(&self, refused: &google::Secret) {
        self.token.clear_if(|kept| kept.token == *refused);
    }
}

/// Where a Google credential failure happened.
#[derive(Debug, Clone, Copy)]
enum Stage {
    /// Reading and parsing the credentials, when a registry is set up.
    Setup,
    /// Fetching a token with them.
    TokenRequest,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Setup => "GCP credential setup failed",
            Self::TokenRequest => "GCP access token request failed",
        })
    }
}

/// The error for `registry` when Google's credentials failed at `stage`.
fn credentials_failed(registry: &str, stage: Stage, error: &google::Error) -> Error {
    Error::cloud_credentials(
        registry,
        error.credential_failure(),
        format_args!("{stage}: {}", error_chain(error)),
    )
}

/// Default [`GcpTokenSource`]: the run's Application Default Credentials.
#[derive(Debug)]
struct AdcTokenSource {
    credentials: google::Credentials,
    clients: google::Clients,
}

impl AdcTokenSource {
    /// The Application Default Credentials of `host`.
    async fn load(
        host: &crate::aws::Host,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Self, google::Error> {
        let clients = google::Clients::new()?;
        let credentials = google::Credentials::load(host, &clients, aws_credentials).await?;
        Ok(Self {
            credentials,
            clients,
        })
    }
}

impl GcpTokenSource for AdcTokenSource {
    fn access_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<AccessToken, google::Error>> + Send + '_>> {
        // Each request of the chain retries a transient failure itself.
        Box::pin(self.credentials.access_token(&self.clients, Clock::System))
    }
}

/// Google Artifact Registry / Container Registry authentication provider.
///
/// Obtains GCP `OAuth2` access tokens via Application Default Credentials,
/// then uses them as HTTP Basic credentials (`oauth2accesstoken:<token>`)
/// in the standard OCI Bearer token exchange flow.
///
/// The Google token is kept in a [`CredentialCache`] the run's GAR and GCR
/// providers share, and refreshed before it expires, supporting
/// long-running processes (watch mode).
///
/// Bearer tokens are coalesced per scope via [`TokenCache`]: concurrent
/// fetches for the same scope produce one token exchange while distinct
/// scopes run in parallel.
pub struct GcpAuth {
    /// The registry base URL.
    base_url: String,
    /// Registry hostname for error context.
    hostname: String,
    /// HTTP client for OCI token exchange.
    http: reqwest::Client,
    /// The Google token, shared by the run's GAR and GCR providers.
    cached_token: Arc<CachedGoogleToken>,
    /// Per-scope coalescing Bearer token cache.
    tokens: TokenCache,
    /// Cached `WWW-Authenticate` challenge to skip redundant `/v2/` pings.
    challenge_cache: token_exchange::ChallengeCache,
    /// A Google token the registry refused although it was fetched fresh
    /// for it: the identity, not the token, is refused, so another refusal
    /// of the same token fetches no other. A different token kept later,
    /// such as one refreshed at expiry, is tried again.
    refused_fresh: Mutex<Option<google::Secret>>,
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
    /// The run's credentials are read and parsed on the first call, which
    /// for an external account with an AWS source may also ask the EC2
    /// instance metadata service for the region, so a bad credentials file
    /// fails here. The token is fetched on the first `get_token` call and
    /// refreshed when approaching expiry. `registry` is the base URL the
    /// registry's client uses.
    pub async fn new(
        registry: &url::Url,
        http: reqwest::Client,
        aws_credentials: &AwsCredentialProviders,
        shared_token: &SharedGoogleToken,
    ) -> Result<Self, Error> {
        Self::on_host(
            registry,
            http,
            &crate::aws::Host::current(),
            aws_credentials,
            shared_token,
        )
        .await
    }

    /// [`Self::new`] with the credentials of `host`.
    async fn on_host(
        registry: &url::Url,
        http: reqwest::Client,
        host: &crate::aws::Host,
        aws_credentials: &AwsCredentialProviders,
        shared_token: &SharedGoogleToken,
    ) -> Result<Self, Error> {
        let cached_token = shared_token
            .cached_token(host, aws_credentials)
            .await
            .map_err(|e| {
                credentials_failed(registry.host_str().unwrap_or_default(), Stage::Setup, &e)
            })?;
        Ok(Self::with_token(registry, http, cached_token))
    }

    fn with_token(
        registry: &url::Url,
        http: reqwest::Client,
        cached_token: Arc<CachedGoogleToken>,
    ) -> Self {
        Self {
            base_url: registry_origin(registry),
            hostname: registry.host_str().unwrap_or_default().to_owned(),
            http,
            cached_token,
            tokens: TokenCache::new(),
            challenge_cache: token_exchange::ChallengeCache::new(),
            refused_fresh: Mutex::new(None),
        }
    }

    /// Create a GCP auth provider with an injected token source (test helper).
    #[cfg(test)]
    fn with_api(
        base_url: impl Into<String>,
        http: reqwest::Client,
        api: impl GcpTokenSource + 'static,
    ) -> Self {
        Self::with_token(
            &base_url.into().parse().expect("a registry URL"),
            http,
            Arc::new(CachedGoogleToken::new(api)),
        )
    }

    /// The registry token for `scopes`, exchanged with `google_token`.
    async fn exchange(
        &self,
        scopes: &[Scope],
        google_token: &google::Secret,
    ) -> Result<(Token, Option<token_exchange::WwwAuthenticate>), Error> {
        let credentials = Credentials::Basic {
            username: GCP_USERNAME.to_owned(),
            password: google_token.expose().to_owned(),
        };
        let cached_challenge = self.challenge_cache.get().await;
        token_exchange::exchange(
            &self.http,
            &self.base_url,
            scopes,
            Some(&credentials),
            cached_challenge.as_ref(),
        )
        .await
    }

    /// The current Google token.
    async fn google_token(&self) -> Result<google::Secret, Error> {
        let token = self
            .cached_token
            .get()
            .await
            .map_err(|e| credentials_failed(&self.hostname, Stage::TokenRequest, &e))?;
        Ok(token.token)
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
                    tracing::debug!(base_url = %self.base_url, scope = %key, "token cache miss, exchanging");
                    let google_token = self.google_token().await?;
                    let identity_refused =
                        self.refused_fresh.lock().await.as_ref() == Some(&google_token);
                    let exchanged = match self.exchange(&scopes, &google_token).await {
                        // The Google token itself was refused, as when it was
                        // revoked or Google's clock has passed it: one more
                        // try with a fresh one. Any other failure, such as a
                        // scope the identity cannot reach, keeps it.
                        Err(Error::TokenEndpoint {
                            status: http::StatusCode::UNAUTHORIZED,
                            ..
                        }) if !identity_refused => {
                            self.cached_token.refuse(&google_token);
                            let fresh = self.google_token().await?;
                            let exchanged = self.exchange(&scopes, &fresh).await;
                            if let Err(Error::TokenEndpoint {
                                status: http::StatusCode::UNAUTHORIZED,
                                ..
                            }) = exchanged
                            {
                                *self.refused_fresh.lock().await = Some(fresh);
                            }
                            exchanged
                        }
                        exchanged => exchanged,
                    };
                    let (token, challenge) = exchanged.inspect_err(|e| {
                        tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                    })?;
                    self.challenge_cache.set(challenge).await;
                    Ok(token)
                })
                .await
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let entries = self.tokens.len();
            tracing::debug!(base_url = %self.base_url, entries, "invalidating GCP token cache");
            self.tokens.clear();
            // The Google token stays: a refusal here is of the registry's
            // token, and the kept Google token exchanges for a new one
            // without a request to Google. An exchange that refuses the
            // Google token drops it.
            self.challenge_cache.clear().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Mutex;
    use wiremock::MockServer;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::*;

    /// Only a refusal, by Google or by AWS for an AWS subject token, is an
    /// auth error: a failure that may pass, a token exchange for a pool that
    /// does not exist, a metadata server with no
    /// service account, an AWS region lookup refused, an AWS subject token
    /// whose credentials timed out, or credentials that do not read are not.
    /// Breaks if any of these exits as refused credentials, or a refusal
    /// stops counting as one.
    #[test]
    fn only_a_google_refusal_is_an_auth_error() {
        let answer = |call, status, body: &str| google::Error::Status {
            call,
            status,
            body: body.to_owned(),
            code: crate::error::OAuthCode::read(body.as_bytes()),
        };
        let status = |call, status| answer(call, status, "");
        let metadata = google::Call::Metadata;
        for (failure, auth_error) in [
            (
                status(metadata, http::StatusCode::SERVICE_UNAVAILABLE),
                false,
            ),
            (status(metadata, http::StatusCode::UNAUTHORIZED), true),
            (status(metadata, http::StatusCode::NOT_FOUND), false),
            (
                answer(
                    google::Call::StsExchange,
                    http::StatusCode::BAD_REQUEST,
                    r#"{"error": "invalid_grant"}"#,
                ),
                true,
            ),
            (
                answer(
                    google::Call::StsExchange,
                    http::StatusCode::BAD_REQUEST,
                    r#"{"error": "invalid_target"}"#,
                ),
                false,
            ),
            (
                status(google::Call::AwsRegion, http::StatusCode::UNAUTHORIZED),
                false,
            ),
            (
                google::Error::AwsSigning(crate::aws::SigningError::timed_out()),
                false,
            ),
            (
                google::Error::AwsSigning(crate::aws::SigningError::refused()),
                true,
            ),
            (google::Error::NoSubjectField, false),
        ] {
            let error = credentials_failed("gcr.io", Stage::TokenRequest, &failure);
            assert_eq!(error.is_auth_error(), auth_error, "{failure}");
        }
    }

    /// A token source whose every fetch fails after a second, counted.
    #[derive(Debug)]
    struct SlowlyFailing(Arc<AtomicUsize>);

    impl GcpTokenSource for SlowlyFailing {
        fn access_token(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<AccessToken, google::Error>> + Send + '_>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(google::Error::Status {
                    call: google::Call::Metadata,
                    status: http::StatusCode::SERVICE_UNAVAILABLE,
                    body: String::new(),
                    code: None,
                })
            })
        }
    }

    /// Registries waiting on a token fetch that fails get its error rather
    /// than each fetching again. Breaks if the Google token stops sharing a
    /// failed fetch.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn waiting_registries_share_a_failed_token_fetch() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let cached = CachedGoogleToken::new(SlowlyFailing(Arc::clone(&fetches)));

        let (first, second) = tokio::join!(cached.get(), cached.get());

        assert!(first.is_err() && second.is_err());
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
    }

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
        ) -> Pin<Box<dyn Future<Output = Result<AccessToken, google::Error>> + Send + '_>> {
            Box::pin(async move {
                let (token, lasts) =
                    self.tokens
                        .lock()
                        .await
                        .pop_front()
                        .ok_or_else(|| google::Error::Status {
                            call: google::Call::Metadata,
                            status: http::StatusCode::UNAUTHORIZED,
                            body: "mock: no more tokens".to_owned(),
                            code: None,
                        })?;
                Ok(AccessToken {
                    token: token.into(),
                    expiry: lasts.map_or(Expiry::Never, |lasts| {
                        Expiry::At(std::time::SystemTime::now() + lasts)
                    }),
                })
            })
        }
    }

    /// The password of the credentials `auth` holds now.
    async fn password(auth: &GcpAuth) -> String {
        auth.google_token().await.unwrap().expose().to_owned()
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

    /// The exchange sends the Google token as the `oauth2accesstoken`
    /// user's Basic password. Breaks if the user name or password changes.
    #[tokio::test]
    async fn exchanges_as_oauth2accesstoken() {
        let server = MockServer::start().await;
        mount_v2_and_token(&server, "registry")
            .remove(0)
            .mount(&server)
            .await;
        // base64 of `oauth2accesstoken:ya29.test-token`.
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header(
                "authorization",
                "Basic b2F1dGgyYWNjZXNzdG9rZW46eWEyOS50ZXN0LXRva2Vu",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "registry", "expires_in": 300})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::succeeding("ya29.test-token"),
        );

        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    /// An exchange that refuses the Google token with 401 drops it and tries
    /// once more with a fresh one, as one revoked or past Google's clock
    /// would never exchange again. Breaks if the refused token is used
    /// again or the exchange is not tried again.
    #[tokio::test]
    async fn a_refused_exchange_drops_the_google_token() {
        let server = MockServer::start().await;
        mount_v2_and_token(&server, "registry")
            .remove(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "registry", "expires_in": 300})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![("ya29.revoked", None), ("ya29.fresh", None)]),
        );

        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(password(&auth).await, "ya29.fresh");
    }

    /// A registry that refuses a fresh Google token too costs one more
    /// Google token for the host, not one per scope. Breaks if every scope
    /// refused with 401 fetches its own Google token.
    #[tokio::test]
    async fn a_refused_identity_fetches_one_more_google_token() {
        let server = MockServer::start().await;
        mount_v2_and_token(&server, "registry")
            .remove(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(4)
            .mount(&server)
            .await;
        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![
                ("ya29.first", None),
                ("ya29.second", None),
                ("ya29.third", None),
            ]),
        );

        for repo in ["a", "b", "c"] {
            auth.get_token(&[Scope::pull(repo)]).await.unwrap_err();
        }
        assert_eq!(password(&auth).await, "ya29.second");
    }

    /// A refused identity holds back only the token refused: one refreshed
    /// at expiry may again be dropped for a fresh one, as when the
    /// identity's access arrived since. Breaks if one double refusal stops
    /// the host from ever fetching a fresh token again.
    #[tokio::test]
    async fn a_later_token_may_be_refused_and_replaced() {
        let server = MockServer::start().await;
        mount_v2_and_token(&server, "registry")
            .remove(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(3)
            .expect(3)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "registry", "expires_in": 300})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![
                ("ya29.first", None),
                ("ya29.expiring", Some(Duration::from_secs(10))),
                ("ya29.refreshed", None),
                ("ya29.fresh", None),
            ]),
        );

        auth.get_token(&[Scope::pull("a")]).await.unwrap_err();
        auth.get_token(&[Scope::pull("b")]).await.unwrap();
        assert_eq!(password(&auth).await, "ya29.fresh");
    }

    /// An exchange that refuses the scope (403), as for a repository the
    /// identity cannot reach, keeps the Google token. Breaks if any failed
    /// exchange fetches another Google token.
    #[tokio::test]
    async fn a_refused_scope_keeps_the_google_token() {
        let server = MockServer::start().await;
        mount_v2_and_token(&server, "registry")
            .remove(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let auth = GcpAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockGcpTokenSource::with_tokens(vec![("ya29.kept", None)]),
        );

        auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert_eq!(password(&auth).await, "ya29.kept");
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

    /// The registries of one run share one load of the credentials and one
    /// Google token, and a registry's refusal of its own token does not
    /// fetch another from Google. Breaks if each registry fetches its own
    /// Google token or a refusal drops it.
    #[tokio::test]
    async fn registries_share_the_google_token() {
        let server = MockServer::start().await;
        mount_user_token(&server, 1).await;
        let host = user_host(&server);
        let shared = SharedGoogleToken::default();
        let us = adc_auth("https://us-docker.pkg.dev", &host, &shared).await;
        let europe = adc_auth("https://europe-docker.pkg.dev", &host, &shared).await;

        assert_eq!(password(&us).await, "ya29.user");
        assert_eq!(password(&europe).await, "ya29.user");
        us.invalidate().await;
        europe.invalidate().await;
        assert_eq!(password(&us).await, "ya29.user");
        assert_eq!(password(&europe).await, "ya29.user");
    }

    /// A failed load is kept for the run, so a second registry fails at
    /// once with the same error. Breaks if each registry repeats
    /// the load, here the region lookup of an AWS subject token source.
    #[tokio::test]
    async fn a_failed_load_is_kept_for_the_run() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let host = aws_source_host(&server);
        let shared = SharedGoogleToken::default();
        let aws_credentials = AwsCredentialProviders::default();

        for registry in ["https://us-docker.pkg.dev", "https://europe-docker.pkg.dev"] {
            let error = GcpAuth::on_host(
                &registry.parse().unwrap(),
                crate::test_http_client(),
                &host,
                &aws_credentials,
                &shared,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("status 404"), "{error}");
        }
    }

    /// Registries set up at once share one load. Breaks if each starts its
    /// own, here two region lookups.
    #[tokio::test(flavor = "current_thread")]
    async fn hosts_asking_at_once_share_one_load() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zone"))
            .respond_with(ResponseTemplate::new(200).set_body_string("us-east-1a"))
            .expect(1)
            .mount(&server)
            .await;
        let host = aws_source_host(&server);
        let shared = SharedGoogleToken::default();
        let aws_credentials = AwsCredentialProviders::default();
        let load = |registry: &str| {
            let registry: url::Url = registry.parse().unwrap();
            let (host, aws_credentials, shared) = (&host, &aws_credentials, &shared);
            async move {
                GcpAuth::on_host(
                    &registry,
                    crate::test_http_client(),
                    host,
                    aws_credentials,
                    shared,
                )
                .await
            }
        };

        let (us, europe) = tokio::join!(
            load("https://us-docker.pkg.dev"),
            load("https://europe-docker.pkg.dev")
        );
        us.unwrap();
        europe.unwrap();
    }

    /// A host whose credentials are an external account with an AWS
    /// source, its region looked up at `server`.
    fn aws_source_host(server: &MockServer) -> crate::aws::Host {
        adc_host(&serde_json::json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/aws",
            "subject_token_type": "urn:ietf:params:aws:token-type:aws4_request",
            "token_url": "https://sts.googleapis.com/v1/token",
            "credential_source": {
                "environment_id": "aws1",
                "region_url": format!("{}/zone", server.uri()),
            },
        }))
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
        adc_host(&serde_json::json!({
            "type": "authorized_user",
            "client_id": "client",
            "client_secret": "secret",
            "refresh_token": "refresh",
            "token_uri": format!("{}/token", server.uri()),
        }))
    }

    /// A host whose `GOOGLE_APPLICATION_CREDENTIALS` file is `file`.
    fn adc_host(file: &serde_json::Value) -> crate::aws::Host {
        google::tests::host(
            crate::aws::Platform::Unix,
            &[("GOOGLE_APPLICATION_CREDENTIALS", "/adc.json")],
            &[("/adc.json", file.to_string().into_bytes())],
        )
    }

    /// A provider for `registry` with `host`'s credentials, shared through
    /// `shared`.
    async fn adc_auth(
        registry: &str,
        host: &crate::aws::Host,
        shared: &SharedGoogleToken,
    ) -> GcpAuth {
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

    /// Invalidation clears the registry's token and challenge, so both are
    /// fetched again. Breaks if either is kept.
    #[tokio::test]
    async fn invalidation_clears_the_registry_caches() {
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
        // The challenge and Bearer token are fetched again: /v2/ is pinged
        // a second time.
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
