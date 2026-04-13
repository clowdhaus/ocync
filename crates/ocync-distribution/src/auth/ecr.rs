//! AWS ECR authentication provider.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::sync::RwLock;

use crate::auth::{AuthProvider, Scope, Token};
use crate::error::Error;

/// Default ECR token lifetime (12 hours).
///
/// Used as a fallback when the API response does not include an expiry
/// timestamp. The actual `GetAuthorizationToken` response includes an
/// `expiresAt` field that is preferred when available.
const ECR_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Response from an ECR `GetAuthorizationToken` call.
#[derive(Clone)]
pub(crate) struct EcrTokenResponse {
    /// The raw base64-encoded authorization token (`AWS:<password>`).
    pub encoded_token: String,
    /// Time until the token expires, if provided by the API.
    pub expires_in: Option<Duration>,
}

/// Abstraction over ECR `GetAuthorizationToken` for testability.
///
/// Implementations are pre-configured with a specific registry and SDK client.
/// The default ([`AwsEcrApi`]) holds a cached AWS SDK client constructed once
/// at [`EcrAuth::new`] time.
pub(crate) trait EcrApi: Send + Sync {
    /// Fetch an authorization token from ECR.
    fn get_authorization_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>>;
}

/// Default [`EcrApi`] backed by the AWS SDK.
///
/// Holds the ECR SDK client created once during [`EcrAuth::new`].
struct AwsEcrApi {
    ecr_client: aws_sdk_ecr::Client,
    registry: String,
}

impl std::fmt::Debug for AwsEcrApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsEcrApi")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl EcrApi for AwsEcrApi {
    fn get_authorization_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            let auth_output = self
                .ecr_client
                .get_authorization_token()
                .send()
                .await
                .map_err(|e| Error::AuthFailed {
                    registry: self.registry.clone(),
                    reason: format!("ECR GetAuthorizationToken failed: {e}"),
                })?;

            let auth_data =
                auth_output
                    .authorization_data()
                    .first()
                    .ok_or_else(|| Error::AuthFailed {
                        registry: self.registry.clone(),
                        reason: "ECR returned empty authorization data".into(),
                    })?;

            let encoded_token = auth_data
                .authorization_token()
                .map(|s| s.to_owned())
                .ok_or_else(|| Error::AuthFailed {
                    registry: self.registry.clone(),
                    reason: "ECR authorization data missing token".into(),
                })?;

            // Prefer the API-provided expiry over the hardcoded default.
            let expires_in = auth_data.expires_at().map(|exp| {
                let now_secs = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("system clock before UNIX epoch")
                    .as_secs() as i64;
                let remaining = exp.secs() - now_secs;
                if remaining > 0 {
                    Duration::from_secs(remaining as u64)
                } else {
                    Duration::ZERO
                }
            });

            Ok(EcrTokenResponse {
                encoded_token,
                expires_in,
            })
        })
    }
}

/// Decode an ECR authorization token.
///
/// ECR tokens are base64-encoded strings in the format `AWS:<token>`.
/// Returns the password portion (everything after `AWS:`).
pub(crate) fn decode_ecr_token(encoded: &str, registry: &str) -> Result<String, Error> {
    let decoded = BASE64.decode(encoded).map_err(|e| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: format!("invalid base64 in ECR token: {e}"),
    })?;

    let text = String::from_utf8(decoded).map_err(|e| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: format!("ECR token is not valid UTF-8: {e}"),
    })?;

    let password = text.strip_prefix("AWS:").ok_or_else(|| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: "ECR token does not start with 'AWS:'".into(),
    })?;

    Ok(password.to_owned())
}

/// AWS ECR authentication provider.
///
/// Uses the AWS SDK to obtain authorization tokens via `GetAuthorizationToken`.
/// The SDK client is created once at construction time. Tokens are cached with
/// [`RwLock`] (read-fast-path for concurrent cache hits, write-lock for fetches)
/// and refreshed when they approach expiry. The actual token lifetime is read
/// from the API response, falling back to 12 hours if not provided.
///
/// Supports all AWS credential sources via the default credential chain:
/// environment variables, shared config/credential files, SSO, IMDS/ECS
/// container credentials, IRSA (IAM Roles for Service Accounts), and
/// EKS Pod Identity.
pub struct EcrAuth {
    /// The ECR registry hostname.
    hostname: String,
    /// Cached bearer token. Uses `RwLock` so concurrent readers can check
    /// the cache without blocking each other.
    cached_token: RwLock<Option<Token>>,
    /// ECR API implementation.
    api: Box<dyn EcrApi>,
}

impl std::fmt::Debug for EcrAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcrAuth")
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

impl EcrAuth {
    /// Create a new ECR auth provider for the given registry hostname.
    ///
    /// Loads AWS credentials and constructs the ECR SDK client once. Returns
    /// an error if the region cannot be extracted from the hostname.
    pub async fn new(hostname: impl Into<String>) -> Result<Self, Error> {
        let hostname = hostname.into();
        let config =
            crate::ecr::load_sdk_config(&hostname)
                .await
                .map_err(|e| Error::AuthFailed {
                    registry: hostname.clone(),
                    reason: format!("failed to load AWS SDK config: {e}"),
                })?;

        let ecr_client = aws_sdk_ecr::Client::new(&config);
        let registry = hostname.clone();

        Ok(Self {
            hostname,
            cached_token: RwLock::new(None),
            api: Box::new(AwsEcrApi {
                ecr_client,
                registry,
            }),
        })
    }

    /// Create an ECR auth provider with an injected API implementation.
    #[cfg(test)]
    fn with_api(hostname: impl Into<String>, api: impl EcrApi + 'static) -> Self {
        Self {
            hostname: hostname.into(),
            cached_token: RwLock::new(None),
            api: Box::new(api),
        }
    }

    /// Replace the cached token (test helper).
    #[cfg(test)]
    async fn set_cached_token(&self, token: Token) {
        let mut cached = self.cached_token.write().await;
        *cached = Some(token);
    }

    async fn get_token_inner(&self) -> Result<Token, Error> {
        // Fast path: read-lock cache check allows concurrent readers.
        {
            let cached = self.cached_token.read().await;
            if let Some(ref token) = *cached {
                if !token.should_refresh() {
                    return Ok(token.clone());
                }
            }
        }

        // Slow path: write-lock for fetch + update.
        let mut cached = self.cached_token.write().await;

        // Double-check after acquiring write lock — another task may have
        // already refreshed the token while we waited.
        if let Some(ref token) = *cached {
            if !token.should_refresh() {
                return Ok(token.clone());
            }
        }

        let response = self.api.get_authorization_token().await?;
        let password = decode_ecr_token(&response.encoded_token, &self.hostname)?;
        let ttl = response.expires_in.unwrap_or(ECR_DEFAULT_TOKEN_TTL);
        let token = Token::with_ttl(password, ttl);

        *cached = Some(token.clone());

        Ok(token)
    }
}

impl AuthProvider for EcrAuth {
    fn name(&self) -> &'static str {
        "ecr"
    }

    fn get_token(
        &self,
        _scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        Box::pin(async move { self.get_token_inner().await })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cached = self.cached_token.write().await;
            *cached = None;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;

    /// Mock ECR API that returns pre-configured responses in order.
    ///
    /// Uses a response queue: each call pops the next response. An empty
    /// queue returns an error, which lets tests verify caching by providing
    /// exactly N responses for N expected fetches.
    struct MockEcrApi {
        responses: Mutex<VecDeque<Option<EcrTokenResponse>>>,
    }

    impl MockEcrApi {
        /// Create a mock that returns the given encoded token on every call.
        fn succeeding(encoded_token: &str) -> Self {
            let response = EcrTokenResponse {
                encoded_token: encoded_token.to_owned(),
                expires_in: None,
            };
            let responses = std::iter::repeat_n(Some(response), 10).collect();
            Self {
                responses: Mutex::new(responses),
            }
        }

        /// Create a mock with an empty queue (always fails).
        fn failing() -> Self {
            Self {
                responses: Mutex::new(VecDeque::new()),
            }
        }

        /// Create a mock with an explicit encoded-token sequence.
        fn with_tokens(tokens: Vec<Option<String>>) -> Self {
            let responses = tokens
                .into_iter()
                .map(|t| {
                    t.map(|encoded_token| EcrTokenResponse {
                        encoded_token,
                        expires_in: None,
                    })
                })
                .collect();
            Self {
                responses: Mutex::new(responses),
            }
        }

        /// Create a mock with an explicit [`EcrTokenResponse`] sequence.
        fn with_responses(responses: Vec<Option<EcrTokenResponse>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl EcrApi for MockEcrApi {
        fn get_authorization_token(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>> {
            Box::pin(async move {
                let mut responses = self.responses.lock().await;
                match responses.pop_front() {
                    Some(Some(response)) => Ok(response),
                    _ => Err(Error::AuthFailed {
                        registry: "mock".into(),
                        reason: "mock: no token available".into(),
                    }),
                }
            })
        }
    }

    // --- decode_ecr_token tests ---

    #[test]
    fn decode_valid_token() {
        let encoded = BASE64.encode("AWS:my-secret-token");
        let password = decode_ecr_token(&encoded, "test-registry").unwrap();
        assert_eq!(password, "my-secret-token");
    }

    #[test]
    fn decode_token_invalid_prefix() {
        let encoded = BASE64.encode("NOTAWS:token");
        let result = decode_ecr_token(&encoded, "test-registry");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("AWS:"));
        assert!(err.contains("test-registry"));
    }

    #[test]
    fn decode_token_invalid_base64() {
        let result = decode_ecr_token("not-valid-base64!!!", "test-registry");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test-registry"));
    }

    #[test]
    fn decode_token_includes_registry_in_error() {
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        let err = decode_ecr_token("!!!", host).unwrap_err().to_string();
        assert!(err.contains(host));
    }

    // --- EcrAuth async tests ---

    const TEST_HOST: &str = "123456789012.dkr.ecr.us-east-1.amazonaws.com";

    #[tokio::test]
    async fn auth_name() {
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::failing());
        assert_eq!(auth.name(), "ecr");
    }

    #[tokio::test]
    async fn auth_returns_decoded_token() {
        let encoded = BASE64.encode("AWS:my-secret");
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::succeeding(&encoded));

        let token = auth.get_token(&[]).await.unwrap();
        assert_eq!(token.value(), "my-secret");
    }

    #[tokio::test]
    async fn auth_caches_token() {
        // Queue has exactly one response. If the second get_token call hits
        // the API it will fail (queue empty), proving the cache works.
        let encoded = BASE64.encode("AWS:cached-token");
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::with_tokens(vec![Some(encoded)]));

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), "cached-token");

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), "cached-token");
    }

    #[tokio::test]
    async fn auth_refreshes_near_expiry_token() {
        let encoded = BASE64.encode("AWS:refreshed-token");
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::with_tokens(vec![Some(encoded)]));

        // Inject a near-expiry token (1 min remaining < 15 min threshold).
        auth.set_cached_token(Token::with_ttl("stale", Duration::from_secs(60)))
            .await;

        // Should trigger refresh because should_refresh() returns true.
        let token = auth.get_token(&[]).await.unwrap();
        assert_eq!(token.value(), "refreshed-token");
    }

    #[tokio::test]
    async fn auth_respects_api_provided_expiry() {
        // First response has a short TTL (5 min < 15 min threshold).
        let short = EcrTokenResponse {
            encoded_token: BASE64.encode("AWS:short-lived"),
            expires_in: Some(Duration::from_secs(5 * 60)),
        };
        let fresh = EcrTokenResponse {
            encoded_token: BASE64.encode("AWS:refreshed"),
            expires_in: None,
        };
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_responses(vec![Some(short), Some(fresh)]),
        );

        // First call returns the short-lived token.
        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), "short-lived");

        // Second call triggers refresh because the cached token is near expiry.
        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), "refreshed");
    }

    #[tokio::test]
    async fn auth_expired_api_token_triggers_immediate_refresh() {
        let expired = EcrTokenResponse {
            encoded_token: BASE64.encode("AWS:expired"),
            expires_in: Some(Duration::ZERO),
        };
        let fresh = EcrTokenResponse {
            encoded_token: BASE64.encode("AWS:fresh"),
            expires_in: None,
        };
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_responses(vec![Some(expired), Some(fresh)]),
        );

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), "expired");

        // Token has Duration::ZERO TTL — next call must refresh.
        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), "fresh");
    }

    #[tokio::test]
    async fn auth_invalidation_forces_refetch() {
        let encoded1 = BASE64.encode("AWS:first-token");
        let encoded2 = BASE64.encode("AWS:second-token");
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded1), Some(encoded2)]),
        );

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), "first-token");

        auth.invalidate().await;

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), "second-token");
    }

    #[tokio::test]
    async fn auth_concurrent_access() {
        // One response in the queue. 10 concurrent tasks all call get_token.
        // If locking is broken, some tasks would fail (queue exhausted).
        let encoded = BASE64.encode("AWS:concurrent");
        let auth = Arc::new(EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded)]),
        ));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let auth = Arc::clone(&auth);
            handles.push(tokio::spawn(async move { auth.get_token(&[]).await }));
        }

        for handle in handles {
            let token = handle.await.unwrap().unwrap();
            assert_eq!(token.value(), "concurrent");
        }
    }

    #[tokio::test]
    async fn auth_propagates_api_error() {
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::failing());

        let result = auth.get_token(&[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mock"));
    }

    #[tokio::test]
    async fn auth_propagates_decode_error() {
        // Valid base64 that doesn't start with "AWS:" prefix.
        let bad_token = BASE64.encode("INVALID:format");
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::succeeding(&bad_token));

        let result = auth.get_token(&[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("AWS:"));
    }

    #[tokio::test]
    async fn auth_ignores_scopes() {
        // ECR tokens are registry-wide, not scope-specific.
        let encoded = BASE64.encode("AWS:scoped-token");
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::succeeding(&encoded));

        let scopes = [Scope::pull("library/nginx"), Scope::pull_push("myrepo")];
        let token = auth.get_token(&scopes).await.unwrap();
        assert_eq!(token.value(), "scoped-token");
    }

    #[tokio::test]
    async fn new_rejects_non_ecr_hostname() {
        let result = EcrAuth::new("ghcr.io").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("region"));
    }
}
