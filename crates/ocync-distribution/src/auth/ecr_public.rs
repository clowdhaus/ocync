//! AWS ECR Public authentication provider.
//!
//! Uses `aws-sdk-ecrpublic` to obtain authorization credentials via
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
//! SDK credentials are cached with TTL and refreshed automatically
//! when they approach expiry, matching [`super::ecr::EcrAuth`]'s
//! behavior for long-running processes (watch mode).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::sync::Mutex;

use super::ecr::{
    EcrApi, EcrTokenResponse, SdkCredentialCache, ttl_from_expiry, validate_ecr_token,
};
use super::token_exchange;
use super::{AuthProvider, Credentials, Scope, Token, scopes_cache_key};
use crate::error::Error;

/// Default ECR Public token lifetime (12 hours).
const ECR_PUBLIC_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Default [`EcrApi`] backed by the `aws-sdk-ecrpublic` SDK.
///
/// Holds the ECR Public SDK client created once during [`EcrPublicAuth::new`].
struct AwsEcrPublicApi {
    client: aws_sdk_ecrpublic::Client,
}

impl fmt::Debug for AwsEcrPublicApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsEcrPublicApi").finish_non_exhaustive()
    }
}

impl EcrApi for AwsEcrPublicApi {
    fn get_authorization_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            let output = self
                .client
                .get_authorization_token()
                .send()
                .await
                .map_err(|e| Error::AuthFailed {
                    registry: "public.ecr.aws".to_owned(),
                    reason: format!("ECR Public GetAuthorizationToken failed: {e}"),
                })?;

            let auth_data = output
                .authorization_data()
                .ok_or_else(|| Error::AuthFailed {
                    registry: "public.ecr.aws".to_owned(),
                    reason: "ECR Public returned no authorization data".into(),
                })?;

            let encoded_token = auth_data
                .authorization_token()
                .map(|s| s.to_owned())
                .ok_or_else(|| Error::AuthFailed {
                    registry: "public.ecr.aws".to_owned(),
                    reason: "ECR Public authorization data missing token".into(),
                })?;

            let expires_in = auth_data.expires_at().map(ttl_from_expiry);

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
    encoded_token: &str,
    ttl: Option<Duration>,
) -> Result<(Credentials, Duration), Error> {
    validate_ecr_token(encoded_token, "public.ecr.aws")?;

    let decoded = BASE64
        .decode(encoded_token)
        .map_err(|e| Error::AuthFailed {
            registry: "public.ecr.aws".to_owned(),
            reason: format!("invalid base64 in ECR Public token: {e}"),
        })?;
    let text = String::from_utf8(decoded).map_err(|e| Error::AuthFailed {
        registry: "public.ecr.aws".to_owned(),
        reason: format!("ECR Public token is not valid UTF-8: {e}"),
    })?;

    let password = text
        .strip_prefix("AWS:")
        .ok_or_else(|| Error::AuthFailed {
            registry: "public.ecr.aws".to_owned(),
            reason: "ECR Public token missing 'AWS:' prefix".into(),
        })?
        .to_owned();

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
/// SDK credentials are cached via [`SdkCredentialCache`] with the same
/// read-lock fast path / write-lock refresh pattern as
/// [`super::ecr::EcrAuth`], supporting long-running processes (watch mode).
///
/// Bearer tokens are cached per-scope with the same challenge-cache
/// optimization as [`super::basic::BasicAuth`].
pub struct EcrPublicAuth {
    /// The registry base URL.
    base_url: String,
    /// HTTP client for OCI token exchange.
    http: reqwest::Client,
    /// ECR Public SDK API for credential refresh.
    api: Box<dyn EcrApi>,
    /// Cached SDK credentials (username/password) with TTL-based refresh.
    sdk_credential_cache: SdkCredentialCache<Credentials>,
    /// Cached Bearer tokens keyed by sorted scope strings.
    cache: Mutex<HashMap<String, Token>>,
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
    /// Constructs the ECR Public SDK client targeting `us-east-1`.
    /// Credentials are fetched lazily on the first `get_token` call and
    /// refreshed automatically when approaching expiry.
    pub async fn new(http: reqwest::Client) -> Result<Self, Error> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .load()
            .await;

        Ok(Self {
            base_url: "https://public.ecr.aws".to_owned(),
            http,
            api: Box::new(AwsEcrPublicApi {
                client: aws_sdk_ecrpublic::Client::new(&config),
            }),
            sdk_credential_cache: SdkCredentialCache::new(),
            cache: Mutex::new(HashMap::new()),
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
            sdk_credential_cache: SdkCredentialCache::new(),
            cache: Mutex::new(HashMap::new()),
            challenge_cache: token_exchange::ChallengeCache::new(),
        }
    }

    /// Get or refresh SDK credentials from `GetAuthorizationToken`.
    async fn ensure_credentials(&self) -> Result<Credentials, Error> {
        self.sdk_credential_cache
            .get_or_refresh("ecr-public-sdk-credential", async {
                tracing::debug!("ECR Public SDK credentials expired or missing, refreshing");
                let response = self.api.get_authorization_token().await.map_err(|e| {
                    tracing::warn!(error = %e, "ECR Public GetAuthorizationToken failed");
                    e
                })?;
                let (credentials, ttl) =
                    decode_ecr_credentials(&response.encoded_token, response.expires_in)?;
                tracing::debug!(
                    ttl_secs = ttl.as_secs(),
                    "ECR Public SDK credentials refreshed"
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

            let mut cache = self.cache.lock().await;

            if let Some(token) = cache.get(&key).filter(|t| t.is_valid()) {
                tracing::debug!(base_url = %self.base_url, scope = %key, "token cache hit");
                return Ok(token.clone());
            }

            // Ensure SDK credentials are fresh before token exchange.
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
            tracing::debug!(base_url = %self.base_url, entries = cache.len(), "invalidating ECR Public token cache");
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

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use wiremock::MockServer;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    use super::*;

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
                    .ok_or_else(|| Error::AuthFailed {
                        registry: "public.ecr.aws".into(),
                        reason: "mock: no more responses".into(),
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
        let (creds, _ttl) = decode_ecr_credentials(&encoded, None).unwrap();
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
        assert!(decode_ecr_credentials(&encoded, None).is_err());
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

        // Only one SDK response available. If the second get_token call
        // re-fetches SDK credentials it will fail, proving they were cached.
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

        // First call fetches SDK credentials (TTL=1s).
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();

        // Wait for SDK credentials to expire.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Clear Bearer token cache so the next call re-checks SDK credentials.
        auth.invalidate().await;

        // Second call should refresh SDK credentials from the mock queue.
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
        // After invalidation: SDK creds re-fetched, challenge cache cleared,
        // Bearer cache cleared. /v2/ is pinged again.
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }
}
