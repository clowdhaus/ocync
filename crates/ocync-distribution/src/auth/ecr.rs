//! AWS ECR authentication provider.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::sync::RwLock;

use crate::auth::{AuthProvider, AuthScheme, Scope, Token};
use crate::error::Error;

/// Default ECR token lifetime (12 hours).
///
/// Used as a fallback when the API response does not include an expiry
/// timestamp. The actual `GetAuthorizationToken` response includes an
/// `expiresAt` field that is preferred when available.
const ECR_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Response from an ECR `GetAuthorizationToken` call.
#[derive(Clone)]
pub(super) struct EcrTokenResponse {
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
pub(super) trait EcrApi: Send + Sync {
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
            let expires_in = auth_data.expires_at().map(ttl_from_expiry);

            Ok(EcrTokenResponse {
                encoded_token,
                expires_in,
            })
        })
    }
}

/// Compute remaining TTL from an AWS SDK `DateTime` expiry.
///
/// Returns the duration between now and the expiry instant. If the expiry
/// is in the past, returns [`Duration::ZERO`].
pub(super) fn ttl_from_expiry(exp: &aws_sdk_ecr::primitives::DateTime) -> Duration {
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
}

/// Cached SDK credential with TTL tracking.
///
/// Generic cache for ECR SDK credentials that uses a read-lock fast path
/// for concurrent cache hits and a write-lock with double-check for
/// refreshes. Used by both [`EcrAuth`] (caching `Token`) and
/// [`super::ecr_public::EcrPublicAuth`] (caching `Credentials`).
pub(super) struct SdkCredentialCache<T> {
    cached: RwLock<Option<SdkCacheEntry<T>>>,
}

struct SdkCacheEntry<T> {
    value: T,
    /// Tracks TTL via `is_valid()`.
    lifetime: Token,
}

impl<T: Clone + Send + Sync> SdkCredentialCache<T> {
    /// Create an empty cache.
    pub(super) fn new() -> Self {
        Self {
            cached: RwLock::new(None),
        }
    }

    /// Return the cached value if still valid, otherwise await `refresh` to
    /// obtain a new one.
    ///
    /// `label` is used as the `Token` value for TTL tracking (not for auth).
    /// The refresh future is created at the call site but only polled on
    /// cache miss, so the API call is never issued when the cache is warm.
    pub(super) async fn get_or_refresh(
        &self,
        label: &str,
        refresh: impl Future<Output = Result<(T, Duration), Error>>,
    ) -> Result<T, Error> {
        // Fast path: read-lock check allows concurrent readers.
        {
            let cached = self.cached.read().await;
            if let Some(ref entry) = *cached {
                if entry.lifetime.is_valid() {
                    return Ok(entry.value.clone());
                }
            }
        }

        // Slow path: write-lock for refresh.
        let mut cached = self.cached.write().await;

        // Double-check after acquiring write lock -- another task may have
        // already refreshed while we waited.
        if let Some(ref entry) = *cached {
            if entry.lifetime.is_valid() {
                return Ok(entry.value.clone());
            }
        }

        let (value, ttl) = refresh.await?;

        *cached = Some(SdkCacheEntry {
            value: value.clone(),
            lifetime: Token::with_ttl(label, ttl),
        });

        Ok(value)
    }

    /// Clear the cached value, forcing the next `get_or_refresh` to call
    /// the refresh function.
    pub(super) async fn clear(&self) {
        *self.cached.write().await = None;
    }

    /// Replace the cached value (test helper).
    #[cfg(test)]
    pub(super) async fn set(&self, value: T, ttl: Duration) {
        *self.cached.write().await = Some(SdkCacheEntry {
            value,
            lifetime: Token::with_ttl("test-lifetime", ttl),
        });
    }
}

/// Validate an ECR authorization token.
///
/// ECR tokens are base64-encoded strings in the format `AWS:<token>`.
/// Validates the format and returns the original encoded string unchanged --
/// ECR expects it sent as `Authorization: Basic <encoded>`.
pub(super) fn validate_ecr_token(encoded: &str, registry: &str) -> Result<(), Error> {
    let decoded = BASE64.decode(encoded).map_err(|e| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: format!("invalid base64 in ECR token: {e}"),
    })?;

    let text = String::from_utf8(decoded).map_err(|e| Error::AuthFailed {
        registry: registry.to_owned(),
        reason: format!("ECR token is not valid UTF-8: {e}"),
    })?;

    if !text.starts_with("AWS:") {
        return Err(Error::AuthFailed {
            registry: registry.to_owned(),
            reason: "ECR token does not start with 'AWS:'".into(),
        });
    }

    Ok(())
}

/// AWS ECR authentication provider.
///
/// Uses the AWS SDK to obtain authorization tokens via `GetAuthorizationToken`.
/// The SDK client is created once at construction time. Tokens are cached via
/// [`SdkCredentialCache`] (read-lock fast path for concurrent cache hits,
/// write-lock with double-check for fetches) and refreshed when they approach
/// expiry. The actual token lifetime is read from the API response, falling
/// back to 12 hours if not provided.
///
/// Supports all AWS credential sources via the default credential chain:
/// environment variables, shared config/credential files, IMDS/ECS
/// container credentials, IRSA (IAM Roles for Service Accounts), and
/// EKS Pod Identity.
pub struct EcrAuth {
    /// The ECR registry hostname.
    hostname: String,
    /// Cached ECR token with TTL-based refresh.
    credential_cache: SdkCredentialCache<Token>,
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
            credential_cache: SdkCredentialCache::new(),
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
            credential_cache: SdkCredentialCache::new(),
            api: Box::new(api),
        }
    }

    /// Replace the cached token (test helper).
    #[cfg(test)]
    async fn set_cached_token(&self, token: Token) {
        self.credential_cache
            .set(token, Duration::from_secs(10))
            .await;
    }

    async fn get_token_inner(&self) -> Result<Token, Error> {
        self.credential_cache
            .get_or_refresh("ecr-token", async {
                tracing::debug!(registry = %self.hostname, "token cache miss, calling GetAuthorizationToken");
                let response = self.api.get_authorization_token().await.map_err(|e| {
                    tracing::warn!(registry = %self.hostname, error = %e, "ECR GetAuthorizationToken failed");
                    e
                })?;
                validate_ecr_token(&response.encoded_token, &self.hostname)?;
                let ttl = response.expires_in.unwrap_or(ECR_DEFAULT_TOKEN_TTL);
                tracing::debug!(registry = %self.hostname, ttl_secs = ttl.as_secs(), "ECR token refreshed");
                // ECR expects `Authorization: Basic <base64(AWS:password)>`. The encoded
                // token from GetAuthorizationToken is already in the right format.
                let token =
                    Token::with_ttl(response.encoded_token, ttl).with_scheme(AuthScheme::Basic);
                Ok((token, ttl))
            })
            .await
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
            tracing::debug!(registry = %self.hostname, "invalidating cached ECR token");
            self.credential_cache.clear().await;
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

    // --- validate_ecr_token tests ---

    #[test]
    fn validate_valid_token() {
        let encoded = BASE64.encode("AWS:my-secret-token");
        validate_ecr_token(&encoded, "test-registry").unwrap();
    }

    #[test]
    fn validate_token_invalid_prefix() {
        let encoded = BASE64.encode("NOTAWS:token");
        let result = validate_ecr_token(&encoded, "test-registry");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("AWS:"));
        assert!(err.contains("test-registry"));
    }

    #[test]
    fn validate_token_invalid_base64() {
        let result = validate_ecr_token("not-valid-base64!!!", "test-registry");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test-registry"));
    }

    #[test]
    fn validate_token_includes_registry_in_error() {
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        let err = validate_ecr_token("!!!", host).unwrap_err().to_string();
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
    async fn auth_returns_basic_scheme_with_encoded_token() {
        let encoded = BASE64.encode("AWS:my-secret");
        let auth = EcrAuth::with_api(TEST_HOST, MockEcrApi::succeeding(&encoded));

        let token = auth.get_token(&[]).await.unwrap();
        // Token value is the original base64-encoded string, not the decoded password.
        assert_eq!(token.value(), encoded);
        assert_eq!(*token.scheme(), AuthScheme::Basic);
    }

    #[tokio::test]
    async fn auth_caches_token() {
        // Queue has exactly one response. If the second get_token call hits
        // the API it will fail (queue empty), proving the cache works.
        let encoded = BASE64.encode("AWS:cached-token");
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded.clone())]),
        );

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), encoded);

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), encoded);
    }

    #[tokio::test]
    async fn auth_refreshes_near_expiry_token() {
        let encoded = BASE64.encode("AWS:refreshed-token");
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded.clone())]),
        );

        // Inject a near-expiry token (10s remaining < 30s EARLY_REFRESH_WINDOW).
        auth.set_cached_token(Token::with_ttl("stale", Duration::from_secs(10)))
            .await;

        // Should trigger refresh because should_refresh() returns true.
        let token = auth.get_token(&[]).await.unwrap();
        assert_eq!(token.value(), encoded);
    }

    #[tokio::test]
    async fn auth_respects_api_provided_expiry() {
        // First response has a short TTL (20s < 30s EARLY_REFRESH_WINDOW).
        let short_encoded = BASE64.encode("AWS:short-lived");
        let fresh_encoded = BASE64.encode("AWS:refreshed");
        let short = EcrTokenResponse {
            encoded_token: short_encoded.clone(),
            expires_in: Some(Duration::from_secs(20)),
        };
        let fresh = EcrTokenResponse {
            encoded_token: fresh_encoded.clone(),
            expires_in: None,
        };
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_responses(vec![Some(short), Some(fresh)]),
        );

        // First call returns the short-lived token.
        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), short_encoded);

        // Second call triggers refresh because the cached token is near expiry.
        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), fresh_encoded);
    }

    #[tokio::test]
    async fn auth_expired_api_token_triggers_immediate_refresh() {
        let expired_encoded = BASE64.encode("AWS:expired");
        let fresh_encoded = BASE64.encode("AWS:fresh");
        let expired = EcrTokenResponse {
            encoded_token: expired_encoded.clone(),
            expires_in: Some(Duration::ZERO),
        };
        let fresh = EcrTokenResponse {
            encoded_token: fresh_encoded.clone(),
            expires_in: None,
        };
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_responses(vec![Some(expired), Some(fresh)]),
        );

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), expired_encoded);

        // Token has Duration::ZERO TTL -- next call must refresh.
        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), fresh_encoded);
    }

    #[tokio::test]
    async fn auth_invalidation_forces_refetch() {
        let encoded1 = BASE64.encode("AWS:first-token");
        let encoded2 = BASE64.encode("AWS:second-token");
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded1.clone()), Some(encoded2.clone())]),
        );

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), encoded1);

        auth.invalidate().await;

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), encoded2);
    }

    #[tokio::test]
    async fn auth_concurrent_access() {
        // One response in the queue. 10 concurrent tasks all call get_token.
        // If locking is broken, some tasks would fail (queue exhausted).
        let encoded = BASE64.encode("AWS:concurrent");
        let auth = Arc::new(EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded.clone())]),
        ));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let auth = Arc::clone(&auth);
            handles.push(tokio::spawn(async move { auth.get_token(&[]).await }));
        }

        for handle in handles {
            let token = handle.await.unwrap().unwrap();
            assert_eq!(token.value(), encoded);
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
        assert_eq!(token.value(), encoded);
    }

    #[tokio::test]
    async fn new_rejects_non_ecr_hostname() {
        let result = EcrAuth::new("ghcr.io").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("region"));
    }
}
