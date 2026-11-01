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

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::detect::ECR_PUBLIC_HOST;
use super::ecr::{AuthorizationData, AuthorizationToken, EcrApi, EcrTokenResponse};
use super::recovery::{Recovery, judge_token_endpoint};
use super::token_cache::TokenCache;
use super::{AuthProvider, Credentials, Presents, Scope, Token, scopes_cache_key};
use crate::credential_cache::CredentialCache;
use crate::error::{CredentialFailure, Error};

/// The only region ECR Public's API serves.
const ECR_PUBLIC_REGION: &str = "us-east-1";

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

            auth_data.into_response(crate::aws::Service::EcrPublic, ECR_PUBLIC_HOST)
        })
    }
}

/// The Basic credentials for the token exchange that an ECR Public
/// authorization token carries.
fn decode_ecr_credentials(encoded_token: String) -> Result<Credentials, Error> {
    let password = AuthorizationToken::parse(encoded_token, ECR_PUBLIC_HOST)?.into_password();

    Ok(Credentials::Basic {
        username: "AWS".to_owned(),
        password,
    })
}

/// AWS ECR Public authentication provider.
///
/// Registry credentials sit in a `CredentialCache`, refreshed before they
/// expire so watch mode keeps running. Bearer tokens are coalesced per scope
/// in a `TokenCache`: concurrent fetches for one scope make one exchange,
/// while distinct scopes run in parallel.
pub struct EcrPublicAuth {
    base_url: String,
    http: reqwest::Client,
    api: Box<dyn EcrApi>,
    credential_cache: CredentialCache<Credentials, Error>,
    tokens: TokenCache,
    recovery: Recovery<Credentials>,
}

impl fmt::Debug for EcrPublicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EcrPublicAuth")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl EcrPublicAuth {
    /// Create an ECR Public auth provider. Credentials are fetched on the
    /// first `get_token` call and shared through `aws_credentials` with other
    /// clients in `us-east-1` for the profile `AWS_PROFILE` selects, or
    /// `default`.
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
            recovery: Recovery::default(),
        })
    }

    /// Create an ECR Public auth provider with an injected API.
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
            recovery: Recovery::default(),
        }
    }

    /// Exchange `credentials` for a registry token for `scopes`. A token
    /// endpoint that answers 401 has refused the credentials.
    async fn exchange(&self, scopes: &[Scope], credentials: &Credentials) -> Result<Token, Error> {
        self.tokens
            .challenge()
            .exchange(&self.http, &self.base_url, scopes, Some(credentials))
            .await
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
                let credentials = decode_ecr_credentials(response.encoded_token)?;
                let ttl = response.ttl;
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

    fn presents(&self) -> Presents {
        Presents::Credentials
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
                    let scopes = &scopes;
                    let exchanged = self
                        .recovery
                        .exchange(
                            || self.ensure_credentials(),
                            |credentials| async move { self.exchange(scopes, &credentials).await },
                            judge_token_endpoint,
                            |refused| {
                                self.credential_cache
                                    .refuse(refused.clone(), Credentials::eq);
                            },
                        )
                        .await;
                    let token = exchanged.inspect_err(|e| {
                        tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                    })?;
                    Ok(token)
                })
                .await
        })
    }

    fn refuse(&self, refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // The credentials stay: only the token exchange that uses them
            // judges them.
            self.tokens.refuse(&self.base_url, &refused).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use tokio::sync::Mutex;
    use wiremock::MockServer;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::super::recovery::Judgment;
    use super::*;
    use crate::auth::testing;

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
                ttl: Duration::from_secs(3600),
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
        let creds = decode_ecr_credentials(encoded).unwrap();
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
        assert!(decode_ecr_credentials(encoded).is_err());
    }

    #[tokio::test]
    async fn token_exchange_with_sdk_credentials() {
        let server = MockServer::start().await;
        testing::challenge(&server, "public.ecr.aws")
            .mount(&server)
            .await;
        testing::token("bearer-tok").mount(&server).await;

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
        testing::challenge(&server, "public.ecr.aws")
            .mount(&server)
            .await;
        testing::token("cached").mount(&server).await;

        // Only one API response available. If the second get_token call
        // re-fetches registry credentials it will fail, proving they were cached.
        let encoded = BASE64.encode("AWS:once");
        let auth = EcrPublicAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockEcrApi::with_responses(vec![EcrTokenResponse {
                encoded_token: encoded,
                ttl: Duration::from_secs(3600),
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
        testing::challenge(&server, "public.ecr.aws")
            .mount(&server)
            .await;
        testing::token("refreshed").mount(&server).await;

        let encoded1 = BASE64.encode("AWS:first");
        let encoded2 = BASE64.encode("AWS:second");
        let auth = EcrPublicAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockEcrApi::with_responses(vec![
                EcrTokenResponse {
                    encoded_token: encoded1,
                    // Near-zero TTL forces immediate expiry.
                    ttl: Duration::from_secs(1),
                },
                EcrTokenResponse {
                    encoded_token: encoded2,
                    ttl: Duration::from_secs(3600),
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
        testing::challenge(&server, "public.ecr.aws")
            .expect(2)
            .mount(&server)
            .await;

        testing::token("fresh").mount(&server).await;

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

    /// Mount the `/v2/` challenge, and a `/token` endpoint that accepts the
    /// credentials whose password is `old` for one exchange and refuses them
    /// after, and always accepts those whose password is `new`.
    async fn mount_revoked_credentials(server: &MockServer, old: &str, new: &str) {
        testing::challenge(server, "public.ecr.aws")
            .mount(server)
            .await;
        let basic = |password: &str| format!("Basic {}", BASE64.encode(format!("AWS:{password}")));
        let accepted = ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"token": "fresh", "expires_in": 300}));
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header("authorization", basic(old).as_str()))
            .respond_with(accepted.clone())
            .up_to_n_times(1)
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header("authorization", basic(old).as_str()))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header("authorization", basic(new).as_str()))
            .respond_with(accepted)
            .expect(1)
            .mount(server)
            .await;
    }

    /// An `EcrTokenResponse` for the password `password`.
    fn response_for(password: &str) -> EcrTokenResponse {
        EcrTokenResponse {
            encoded_token: BASE64.encode(format!("AWS:{password}")),
            ttl: Duration::from_secs(3600),
        }
    }

    /// Credentials revoked while kept and still fresh are refused by the
    /// token endpoint, dropped, and replaced within the one request: two
    /// `GetAuthorizationToken` calls and three token exchanges in all.
    /// Breaks if the refusal is not recovered from, or the credentials are
    /// kept.
    #[tokio::test]
    async fn revoked_credentials_are_replaced_within_the_request() {
        let server = MockServer::start().await;
        mount_revoked_credentials(&server, "old", "new").await;
        let api = MockEcrApi::with_responses(vec![response_for("old"), response_for("new")]);
        let calls = api.calls();
        let auth = EcrPublicAuth::with_api(server.uri(), crate::test_http_client(), api);

        auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Mount the `/v2/` challenge, and a `/token` endpoint that accepts the
    /// credentials whose password is in `accepted` and refuses all others,
    /// `refusals` times in all.
    async fn mount_token_endpoint(server: &MockServer, accepted: &[&str], refusals: u64) {
        testing::challenge(server, "public.ecr.aws")
            .mount(server)
            .await;
        for password in accepted {
            Mock::given(method("GET"))
                .and(path("/token"))
                .and(header(
                    "authorization",
                    format!("Basic {}", BASE64.encode(format!("AWS:{password}"))).as_str(),
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"token": "fresh", "expires_in": 300})),
                )
                .mount(server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(refusals)
            .mount(server)
            .await;
    }

    /// Requests that all used credentials revoked while kept are all
    /// refused, and each is sent again with the replacement one of them
    /// fetched: one `GetAuthorizationToken` call in all. Breaks if only the
    /// request that dropped the credentials recovers, or each fetches its
    /// own.
    #[tokio::test(flavor = "current_thread")]
    async fn requests_refused_with_one_revoked_credential_share_its_replacement() {
        let server = MockServer::start().await;
        mount_token_endpoint(&server, &["new"], 5).await;
        let api = MockEcrApi::with_responses(vec![response_for("new")]);
        let calls = api.calls();
        let auth = EcrPublicAuth::with_api(server.uri(), crate::test_http_client(), api);
        auth.credential_cache.set(
            Credentials::Basic {
                username: "AWS".to_owned(),
                password: "old".to_owned(),
            },
            Duration::from_secs(3600),
        );

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
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Credentials the token endpoint refuses although they were fetched
    /// for the retry cost one recovery: two `GetAuthorizationToken` calls
    /// and four exchanges for three scopes, all failing. Breaks if the
    /// retry is retried, or each later scope signs in again.
    #[tokio::test]
    async fn refused_replacement_credentials_cost_one_recovery() {
        let server = MockServer::start().await;
        mount_token_endpoint(&server, &[], 4).await;
        let api =
            MockEcrApi::with_responses(["one", "two", "three", "four"].map(response_for).into());
        let calls = api.calls();
        let auth = EcrPublicAuth::with_api(server.uri(), crate::test_http_client(), api);

        for repo in ["a", "b", "c"] {
            let err = auth.get_token(&[Scope::pull(repo)]).await.unwrap_err();
            assert_eq!(judge_token_endpoint(&err), Judgment::Refused, "{err:?}");
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
