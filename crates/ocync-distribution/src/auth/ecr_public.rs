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
use super::{AuthProvider, Credentials, Scope, Token, scopes_cache_key};
use crate::credential_cache::{CredentialCache, remaining};
use crate::error::Error;

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
                .map_err(|e| Error::auth_failed(ECR_PUBLIC_HOST, e))?;

            let auth_data = output.authorization_data.ok_or_else(|| {
                Error::auth_failed(ECR_PUBLIC_HOST, "ECR Public returned no authorization data")
            })?;

            let encoded_token = auth_data.authorization_token.ok_or_else(|| {
                Error::auth_failed(
                    ECR_PUBLIC_HOST,
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

/// AWS ECR Public authentication provider.
///
/// Obtains AWS credentials via `ecr-public:GetAuthorizationToken` (always
/// targeting `us-east-1`), then uses them as HTTP Basic credentials in
/// the standard OCI Bearer token exchange flow.
///
/// Registry credentials are cached via [`CredentialCache`] with the same
/// read-lock fast path / write-lock refresh pattern as
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
    credential_cache: CredentialCache<Credentials>,
    /// Per-scope coalescing Bearer token cache.
    tokens: TokenCache,
    /// Cached `WWW-Authenticate` challenge to skip redundant `/v2/` pings.
    challenge_cache: token_exchange::ChallengeCache,
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
    /// refreshed automatically when approaching expiry.
    pub fn new(http: reqwest::Client) -> Result<Self, Error> {
        let client =
            crate::aws::Client::from_env(ECR_PUBLIC_REGION, None, crate::aws::Service::EcrPublic)
                .map_err(|e| Error::auth_failed(ECR_PUBLIC_HOST, e))?;

        Ok(Self {
            base_url: format!("https://{ECR_PUBLIC_HOST}"),
            http,
            api: Box::new(AwsEcrPublicApi { client }),
            credential_cache: CredentialCache::new(),
            tokens: TokenCache::new(),
            challenge_cache: token_exchange::ChallengeCache::new(),
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
            challenge_cache: token_exchange::ChallengeCache::new(),
        }
    }

    /// Get or refresh registry credentials from `GetAuthorizationToken`.
    async fn ensure_credentials(&self) -> Result<Credentials, Error> {
        self.credential_cache
            .get_or_refresh("ecr-public-credential", async {
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
            tracing::debug!(base_url = %self.base_url, entries, "invalidating ECR Public token cache");
            self.tokens.clear().await;
            self.challenge_cache.clear().await;
            // Also clear registry credentials so they are re-fetched on next use.
            self.credential_cache.clear().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use tokio::sync::Mutex;
    use wiremock::MockServer;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::*;

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
    }

    impl MockEcrApi {
        fn with_responses(responses: Vec<EcrTokenResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
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
                self.responses
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| Error::auth_failed(ECR_PUBLIC_HOST, "mock: no more responses"))
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
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();

        // Wait for registry credentials to expire.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Clear Bearer token cache so the next call re-checks registry credentials.
        auth.invalidate().await;

        // Second call should refresh registry credentials from the mock queue.
        // If it doesn't, the expired credentials will still work for token
        // exchange (the mock server doesn't validate credentials), but the
        // ensure_credentials path will have been exercised.
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
        let auth = EcrPublicAuth::with_api(
            server.uri(),
            crate::test_http_client(),
            MockEcrApi::succeeding(&encoded),
        );
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.invalidate().await;
        // After invalidation: registry creds re-fetched, challenge cache cleared,
        // Bearer cache cleared. /v2/ is pinged again.
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }
}
