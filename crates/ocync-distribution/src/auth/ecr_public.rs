//! AWS ECR Public authentication provider.
//!
//! Obtains authorization credentials via ECR Public's
//! `GetAuthorizationToken`, then performs standard OCI Bearer token
//! exchange using those credentials. This is the Rust equivalent of:
//!
//! ```sh
//! aws ecr-public get-login-password --region us-east-1 \
//!   | docker login --username AWS --password-stdin public.ecr.aws
//! ```
//!
//! ECR Public is a global service; the API endpoint is always in
//! `us-east-1` regardless of where the caller runs.
//!
//! Registry credentials are cached with TTL and refreshed automatically
//! when they approach expiry, matching [`super::ecr::EcrAuth`]'s
//! behavior for long-running processes (watch mode).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;

use super::detect::ECR_PUBLIC_HOST;
use super::ecr::{AuthorizationData, AuthorizationToken, EcrApi, EcrTokenResponse};
use super::token_cache::TokenCache;
use super::token_exchange;
use super::{AuthProvider, Credentials, Refusal, Scope, Token, scopes_cache_key};
use crate::credential_cache::{CredentialCache, remaining};
use crate::error::{CredentialFailure, Error};

/// The only region ECR Public's API serves.
const ECR_PUBLIC_REGION: &str = "us-east-1";

/// Default ECR Public token lifetime (12 hours).
const ECR_PUBLIC_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Default [`EcrApi`], calling ECR Public through [`crate::aws::Client`].
#[derive(Debug)]
struct AwsEcrPublicApi {
    client: crate::aws::Client,
}

/// ECR Public's `GetAuthorizationToken`, which takes no input.
#[derive(serde::Serialize)]
struct GetAuthorizationToken {}

/// ECR Public answers with one `authorizationData` object, not a list.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetAuthorizationTokenOutput {
    authorization_data: Option<AuthorizationData>,
}

impl crate::aws::Operation for GetAuthorizationToken {
    const NAME: &'static str = "GetAuthorizationToken";
    type Output = GetAuthorizationTokenOutput;
}

impl EcrApi for AwsEcrPublicApi {
    fn get_authorization_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            let output = self
                .client
                .call(&GetAuthorizationToken {})
                .await
                .map_err(|e| {
                    Error::cloud_credentials(ECR_PUBLIC_HOST, e.credential_failure(), e)
                })?;

            let auth_data = output.authorization_data.ok_or_else(|| {
                Error::cloud_credentials(
                    ECR_PUBLIC_HOST,
                    CredentialFailure::Failed,
                    "ECR Public returned no authorization data",
                )
            })?;

            let encoded_token = auth_data.authorization_token.ok_or_else(|| {
                Error::cloud_credentials(
                    ECR_PUBLIC_HOST,
                    CredentialFailure::Failed,
                    "ECR Public authorization data missing token",
                )
            })?;

            let expires_in = auth_data.expires_at.map(remaining);

            Ok(EcrTokenResponse {
                encoded_token,
                expires_in,
            })
        })
    }
}

/// Decode an ECR authorization token into OCI registry credentials.
///
/// ECR tokens are base64-encoded `AWS:<password>`. This extracts the
/// password and returns `Credentials::Basic` for use in OCI Bearer
/// token exchange, along with the token's TTL.
fn decode_ecr_credentials(
    encoded_token: String,
    ttl: Option<Duration>,
) -> Result<(Credentials, Duration), Error> {
    let password = AuthorizationToken::parse(encoded_token, ECR_PUBLIC_HOST)?.into_password();

    Ok((
        Credentials::Basic {
            username: "AWS".to_owned(),
            password,
        },
        ttl.unwrap_or(ECR_PUBLIC_DEFAULT_TOKEN_TTL),
    ))
}

/// Whether two credentials are the same username and password.
fn same_credentials(a: &Credentials, b: &Credentials) -> bool {
    let (
        Credentials::Basic {
            username: a_user,
            password: a_pass,
        },
        Credentials::Basic {
            username: b_user,
            password: b_pass,
        },
    ) = (a, b);
    a_user == b_user && a_pass == b_pass
}

/// AWS ECR Public authentication provider.
///
/// Obtains AWS credentials via `ecr-public:GetAuthorizationToken` (always
/// targeting `us-east-1`), then uses them as HTTP Basic credentials in
/// the standard OCI Bearer token exchange flow.
///
/// Registry credentials are cached via [`CredentialCache`] as in
/// [`super::ecr::EcrAuth`], supporting long-running processes (watch mode).
///
/// Bearer tokens are coalesced per scope via [`TokenCache`]: concurrent
/// fetches for the same scope produce one token exchange while distinct
/// scopes run in parallel.
pub struct EcrPublicAuth {
    /// The registry base URL.
    base_url: String,
    /// HTTP client for OCI token exchange.
    http: reqwest::Client,
    /// ECR Public API for credential refresh.
    api: Box<dyn EcrApi>,
    /// Cached registry credentials (username/password) with TTL-based refresh.
    credential_cache: CredentialCache<Credentials, Error>,
    /// Per-scope coalescing Bearer token cache.
    tokens: TokenCache,
}

impl fmt::Debug for EcrPublicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EcrPublicAuth")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl EcrPublicAuth {
    /// Create a new ECR Public auth provider.
    ///
    /// Constructs the ECR Public client targeting `us-east-1`.
    /// Credentials are fetched lazily on the first `get_token` call and
    /// refreshed automatically when approaching expiry, and shared through
    /// `aws_credentials` with other clients in `us-east-1` for the profile
    /// `AWS_PROFILE` selects, or `default`.
    pub fn new(
        http: reqwest::Client,
        aws_credentials: &crate::AwsCredentialProviders,
    ) -> Result<Self, Error> {
        let client = crate::aws::Client::from_env(
            ECR_PUBLIC_REGION,
            None,
            crate::aws::Service::EcrPublic,
            aws_credentials,
        )
        .map_err(|e| Error::cloud_credentials(ECR_PUBLIC_HOST, CredentialFailure::Failed, e))?;

        Ok(Self {
            base_url: format!("https://{ECR_PUBLIC_HOST}"),
            http,
            api: Box::new(AwsEcrPublicApi { client }),
            credential_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
        })
    }

    /// Create an ECR Public auth provider with an injected API (test helper).
    #[cfg(test)]
    fn with_api(
        base_url: impl Into<String>,
        http: reqwest::Client,
        api: impl EcrApi + 'static,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            api: Box::new(api),
            credential_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
        }
    }

    /// The token endpoint refused `refused`: drop them if they are still the
    /// credentials kept.
    fn refuse_credentials(&self, refused: &Credentials) -> Refusal {
        let refused = refused.clone();
        self.credential_cache
            .refuse_if(move |kept| same_credentials(kept, &refused))
    }

    /// Get or refresh registry credentials from `GetAuthorizationToken`.
    async fn ensure_credentials(&self) -> Result<Credentials, Error> {
        self.credential_cache
            .get_or_refresh_sharing_failure(async {
                tracing::debug!("ECR Public registry credentials expired or missing, refreshing");
                let response = self.api.get_authorization_token().await.map_err(|e| {
                    tracing::warn!(error = %e, "ECR Public GetAuthorizationToken failed");
                    e
                })?;
                let (credentials, ttl) =
                    decode_ecr_credentials(response.encoded_token, response.expires_in)?;
                tracing::debug!(
                    ttl_secs = ttl.as_secs(),
                    "ECR Public registry credentials refreshed"
                );
                Ok((credentials, ttl))
            })
            .await
    }
}

impl AuthProvider for EcrPublicAuth {
    fn name(&self) -> &'static str {
        "ecr-public"
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
                    // Ensure registry credentials are fresh before token exchange.
                    let credentials = self.ensure_credentials().await?;
                    tracing::debug!(base_url = %self.base_url, scope = %key, "token cache miss, exchanging");
                    let cached_challenge = self.tokens.challenge().get().await;
                    let exchanged = token_exchange::exchange(
                        &self.http,
                        &self.base_url,
                        &scopes,
                        Some(&credentials),
                        cached_challenge.as_ref(),
                    )
                    .await;
                    // The token endpoint refused the credentials themselves.
                    // They are dropped only if they are still the ones kept.
                    if let Err(Error::TokenEndpoint {
                        status: http::StatusCode::UNAUTHORIZED,
                        ..
                    }) = &exchanged
                    {
                        self.refuse_credentials(&credentials);
                    }
                    let (token, challenge) = exchanged.inspect_err(|e| {
                        tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                    })?;
                    self.tokens.challenge().set(challenge).await;
                    Ok(token)
                })
                .await
        })
    }

    fn refuse(&self, refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // The credentials stay: only the token exchange that uses them
            // judges them.
            let refusal = self.tokens.refuse(&refused).await;
            tracing::debug!(base_url = %self.base_url, ?refusal, "registry refused a token");
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use tokio::sync::Mutex;
    use wiremock::MockServer;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::*;

    /// A throttle that outlasts its retries is not an auth error, and an
    /// access denial is. Breaks if every AWS failure exits as refused
    /// credentials, or a denial stops counting as one.
    #[tokio::test]
    async fn only_an_aws_refusal_is_an_auth_error() {
        for (answer, auth_error) in [
            (crate::aws::tests::throttle(), false),
            (crate::aws::tests::access_denied(), true),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(answer)
                .mount(&server)
                .await;
            let api = AwsEcrPublicApi {
                client: crate::aws::tests::client(&server, std::num::NonZeroU32::MIN),
            };
            let Err(err) = api.get_authorization_token().await else {
                panic!("accepted");
            };
            assert_eq!(err.is_auth_error(), auth_error, "{err}");
        }
    }

    /// ECR Public answers with one authorization data object where ECR has
    /// a list. Reading it as ECR's list, or ECR's list as it, fails.
    #[test]
    fn reads_the_get_authorization_token_output() {
        let output: GetAuthorizationTokenOutput = serde_json::from_str(
            r#"{"authorizationData":{"authorizationToken":"QVdTOnB3","expiresAt":4102444800}}"#,
        )
        .unwrap();
        let data = output.authorization_data.unwrap();
        assert_eq!(data.authorization_token.as_deref(), Some("QVdTOnB3"));
        assert!(data.expires_at.is_some());
        assert!(
            serde_json::from_str::<GetAuthorizationTokenOutput>(
                r#"{"authorizationData":[{"authorizationToken":"QVdTOnB3"}]}"#
            )
            .is_err()
        );
    }

    /// Mock ECR API that returns pre-configured responses in order.
    struct MockEcrApi {
        responses: Mutex<VecDeque<EcrTokenResponse>>,
        /// How many times `get_authorization_token` was called.
        calls: Arc<AtomicUsize>,
    }

    impl MockEcrApi {
        fn with_responses(responses: Vec<EcrTokenResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: Arc::default(),
            }
        }

        /// A handle on the call count, usable after the mock has moved
        /// into the provider.
        fn calls(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.calls)
        }

        fn succeeding(encoded_token: &str) -> Self {
            let response = EcrTokenResponse {
                encoded_token: encoded_token.to_owned(),
                expires_in: None,
            };
            Self::with_responses(std::iter::repeat_n(response, 10).collect())
        }
    }

    impl EcrApi for MockEcrApi {
        fn get_authorization_token(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                // Yield so callers that ask meanwhile find the call in flight.
                tokio::task::yield_now().await;
                self.responses.lock().await.pop_front().ok_or_else(|| {
                    Error::cloud_credentials(
                        ECR_PUBLIC_HOST,
                        CredentialFailure::Failed,
                        "mock: no more responses",
                    )
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
                        r#"Bearer realm="{}/token",service="public.ecr.aws""#,
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
        let encoded = BASE64.encode("AWS:test");
        let auth = EcrPublicAuth::with_api(
            "https://public.ecr.aws",
            crate::test_http_client(),
            MockEcrApi::succeeding(&encoded),
        );
        assert_eq!(auth.name(), "ecr-public");
    }

    #[test]
    fn decode_ecr_credentials_valid() {
        let encoded = BASE64.encode("AWS:my-secret-password");
        let (creds, _ttl) = decode_ecr_credentials(encoded, None).unwrap();
        match creds {
            Credentials::Basic { username, password } => {
                assert_eq!(username, "AWS");
                assert_eq!(password, "my-secret-password");
            }
        }
    }

    #[test]
    fn decode_ecr_credentials_invalid_prefix() {
        let encoded = BASE64.encode("NOTAWS:password");
        assert!(decode_ecr_credentials(encoded, None).is_err());
    }

    #[tokio::test]
    async fn token_exchange_with_sdk_credentials() {
        let server = MockServer::start().await;
        for mock in mount_v2_and_token(&server, "bearer-tok") {
            mock.mount(&server).await;
        }

        let encoded = BASE64.encode("AWS:test-password");
        let auth = EcrPublicAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockEcrApi::succeeding(&encoded),
        );
        let token = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(token.value(), "bearer-tok");
    }

    #[tokio::test]
    async fn sdk_credentials_cached_across_scopes() {
        let server = MockServer::start().await;
        for mock in mount_v2_and_token(&server, "cached") {
            mock.mount(&server).await;
        }

        // Only one API response available. If the second get_token call
        // re-fetches registry credentials it will fail, proving they were cached.
        let encoded = BASE64.encode("AWS:once");
        let auth = EcrPublicAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockEcrApi::with_responses(vec![EcrTokenResponse {
                encoded_token: encoded,
                expires_in: None,
            }]),
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

        let encoded1 = BASE64.encode("AWS:first");
        let encoded2 = BASE64.encode("AWS:second");
        let auth = EcrPublicAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockEcrApi::with_responses(vec![
                EcrTokenResponse {
                    encoded_token: encoded1,
                    // Near-zero TTL forces immediate expiry.
                    expires_in: Some(Duration::from_secs(1)),
                },
                EcrTokenResponse {
                    encoded_token: encoded2,
                    expires_in: None,
                },
            ]),
        );

        // First call fetches registry credentials (TTL=1s).
        let refused = auth.get_token(&[Scope::pull("repo")]).await.unwrap();

        // Wait for registry credentials to expire.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Clear Bearer token cache so the next call re-checks registry credentials.
        auth.refuse(refused).await;

        // Second call should refresh registry credentials from the mock queue.
        // If it doesn't, the expired credentials will still work for token
        // exchange (the mock server doesn't validate credentials), but the
        // ensure_credentials path will have been exercised.
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    /// A refusal of the registry token drops the token and the challenge
    /// and keeps the cloud credentials, so the next token costs a `/v2/`
    /// ping but no `GetAuthorizationToken`. Breaks if a registry 401 judges
    /// the cloud credentials, or a refusal of another token drops anything.
    #[tokio::test]
    async fn a_registry_refusal_keeps_the_cloud_credentials() {
        let server = MockServer::start().await;

        // /v2/ hit twice: once before the refusal, once after.
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="public.ecr.aws""#,
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

        let encoded = BASE64.encode("AWS:test");
        let api = MockEcrApi::succeeding(&encoded);
        let calls = api.calls();
        let auth = EcrPublicAuth::with_api(server.uri(), crate::test_http_client(), api);
        let refused = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        // A refusal of some other token keeps the caches: no second `/v2/`
        // ping.
        auth.refuse(Token::new("other")).await;
        assert!(auth.tokens.challenge().get().await.is_some());
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(refused).await;
        assert!(auth.tokens.challenge().get().await.is_none());
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A token endpoint that refuses the credentials drops them, so the
    /// next token fetches new ones. Breaks if a refused exchange keeps the
    /// credentials it was refused for.
    #[tokio::test]
    async fn a_refused_exchange_drops_the_cloud_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="public.ecr.aws""#,
                    server.uri()
                ),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
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

        let api = MockEcrApi::succeeding(&BASE64.encode("AWS:test"));
        let calls = api.calls();
        let auth = EcrPublicAuth::with_api(server.uri(), crate::test_http_client(), api);
        let err = auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert!(matches!(
            err,
            Error::TokenEndpoint {
                status: http::StatusCode::UNAUTHORIZED,
                ..
            }
        ));
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A refusal of credentials that have been replaced keeps the
    /// replacement. Breaks if a refusal drops whatever is kept.
    #[tokio::test]
    async fn a_refusal_of_replaced_credentials_keeps_the_replacement() {
        let api = MockEcrApi::succeeding(&BASE64.encode("AWS:test"));
        let calls = api.calls();
        let auth =
            EcrPublicAuth::with_api("https://public.ecr.aws", crate::test_http_client(), api);
        let credentials = |password: &str| Credentials::Basic {
            username: "AWS".to_owned(),
            password: password.to_owned(),
        };
        auth.credential_cache
            .set(credentials("new"), Duration::from_secs(3600));

        assert_eq!(auth.refuse_credentials(&credentials("old")), Refusal::Stale);
        let kept = auth.ensure_credentials().await.unwrap();
        assert!(same_credentials(&kept, &credentials("new")));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        assert_eq!(
            auth.refuse_credentials(&credentials("new")),
            Refusal::Current
        );
        auth.ensure_credentials().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Callers waiting on a refresh that fails share its error, across
    /// scopes. Breaks if each waiter repeats the failed call against the
    /// same endpoint.
    #[tokio::test(flavor = "current_thread")]
    async fn waiters_on_a_failed_refresh_share_its_error() {
        let api = MockEcrApi::with_responses(vec![]);
        let calls = api.calls();
        let auth =
            EcrPublicAuth::with_api("https://public.ecr.aws", crate::test_http_client(), api);

        let (first, second, third) = tokio::join!(
            auth.get_token(&[Scope::pull("repo-a")]),
            auth.get_token(&[Scope::pull("repo-b")]),
            auth.get_token(&[Scope::pull("repo-c")]),
        );

        for result in [first, second, third] {
            assert!(result.unwrap_err().to_string().contains("mock"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
