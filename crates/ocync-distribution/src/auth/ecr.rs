//! AWS ECR authentication provider.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
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
/// Implementations are pre-configured with a specific registry. The default
/// ([`AwsEcrApi`]) holds a client constructed once at [`EcrAuth::new`] time.
pub(super) trait EcrApi: Send + Sync {
    /// Fetch an authorization token from ECR.
    fn get_authorization_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>>;
}

/// One `authorizationData` entry of a `GetAuthorizationToken` output, the
/// same shape in ECR and ECR Public.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AuthorizationData {
    /// The base64 `AWS:<password>` token.
    pub(super) authorization_token: Option<String>,
    /// When the token expires.
    #[serde(default, deserialize_with = "epoch_seconds")]
    pub(super) expires_at: Option<SystemTime>,
}

/// Read an expiry sent as seconds since the UNIX epoch. One before the
/// epoch has passed; one too far ahead to represent reads as no expiry, so
/// the default lifetime applies rather than the token failing.
fn epoch_seconds<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SystemTime>, D::Error> {
    let Some(secs) = Option::<f64>::deserialize(deserializer)? else {
        return Ok(None);
    };
    Ok(Duration::try_from_secs_f64(secs.max(0.0))
        .ok()
        .and_then(|since_epoch| SystemTime::UNIX_EPOCH.checked_add(since_epoch)))
}

/// Time until `until`, or zero once it has passed.
pub(crate) fn remaining(until: SystemTime) -> Duration {
    until
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// ECR's `GetAuthorizationToken`, which takes no input.
#[derive(serde::Serialize)]
struct GetAuthorizationToken {}

/// ECR answers with a list of authorization data, one per registry.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetAuthorizationTokenOutput {
    #[serde(default, deserialize_with = "crate::aws::null_as_default")]
    authorization_data: Vec<AuthorizationData>,
}

impl crate::aws::Operation for GetAuthorizationToken {
    const NAME: &'static str = "GetAuthorizationToken";
    type Output = GetAuthorizationTokenOutput;
}

/// Default [`EcrApi`], calling ECR through [`crate::aws::Client`].
#[derive(Debug)]
struct AwsEcrApi {
    client: crate::aws::Client,
    registry: String,
}

impl EcrApi for AwsEcrApi {
    fn get_authorization_token(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<EcrTokenResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            let output = self
                .client
                .call(&GetAuthorizationToken {})
                .await
                .map_err(|e| Error::auth_failed(&self.registry, e))?;

            let auth_data = output
                .authorization_data
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::auth_failed(&self.registry, "ECR returned empty authorization data")
                })?;

            let encoded_token = auth_data.authorization_token.ok_or_else(|| {
                Error::auth_failed(&self.registry, "ECR authorization data missing token")
            })?;

            // Prefer the API-provided expiry over the hardcoded default.
            let expires_in = auth_data.expires_at.map(remaining);

            Ok(EcrTokenResponse {
                encoded_token,
                expires_in,
            })
        })
    }
}

/// Cached credential with TTL tracking.
///
/// Generic cache for credentials that uses a read-lock fast path
/// for concurrent cache hits and a write-lock with double-check for
/// refreshes. Used by [`EcrAuth`] (caching `Token`),
/// [`super::ecr_public::EcrPublicAuth`] (caching `Credentials`) and the AWS
/// API client (caching AWS keys).
pub(crate) struct CredentialCache<T> {
    cached: RwLock<Option<CacheEntry<T>>>,
}

struct CacheEntry<T> {
    value: T,
    /// Tracks TTL via `is_valid()`.
    lifetime: Token,
}

impl<T: Clone + Send + Sync> CredentialCache<T> {
    /// Create an empty cache.
    pub(crate) fn new() -> Self {
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
    pub(crate) async fn get_or_refresh<E>(
        &self,
        label: &str,
        refresh: impl Future<Output = Result<(T, Duration), E>>,
    ) -> Result<T, E> {
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

        *cached = Some(CacheEntry {
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
        *self.cached.write().await = Some(CacheEntry {
            value,
            lifetime: Token::with_ttl("test-lifetime", ttl),
        });
    }
}

/// An ECR authorization token: base64 of `AWS:<password>`.
pub(super) struct AuthorizationToken {
    /// The token as ECR sent it, which ECR takes back as
    /// `Authorization: Basic <encoded>`.
    encoded: String,
    /// The password the token carries, which ECR Public's token exchange
    /// takes.
    password: String,
}

impl AuthorizationToken {
    /// Read `encoded`, naming `registry` in any error.
    pub(super) fn parse(encoded: String, registry: &str) -> Result<Self, Error> {
        let decoded = BASE64.decode(&encoded).map_err(|e| {
            Error::auth_failed(registry, format!("invalid base64 in ECR token: {e}"))
        })?;
        let text = String::from_utf8(decoded).map_err(|e| {
            Error::auth_failed(registry, format!("ECR token is not valid UTF-8: {e}"))
        })?;
        let password = text
            .strip_prefix("AWS:")
            .ok_or_else(|| Error::auth_failed(registry, "ECR token does not start with 'AWS:'"))?
            .to_owned();
        Ok(Self { encoded, password })
    }

    /// The token as ECR sent it.
    pub(super) fn into_encoded(self) -> String {
        self.encoded
    }

    /// The password the token carries.
    pub(super) fn into_password(self) -> String {
        self.password
    }
}

/// AWS ECR authentication provider.
///
/// Obtains authorization tokens via `GetAuthorizationToken`, through a
/// client created once at construction time. Tokens are cached via
/// [`CredentialCache`] (read-lock fast path for concurrent cache hits,
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
    credential_cache: CredentialCache<Token>,
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
    /// Loads the AWS config and constructs the ECR client once. Returns
    /// an error if the region cannot be extracted from the hostname, or if
    /// a configured endpoint URL is combined with FIPS or dual-stack.
    ///
    /// When `profile` is `Some`, credential resolution is scoped to that
    /// named profile in the shared credentials/config file. When `None`,
    /// the ambient default credential chain is used unchanged. Each
    /// `EcrAuth` instance holds its own [`aws_config::SdkConfig`], so
    /// per-instance profile overrides do not leak across registries.
    pub async fn new(hostname: impl Into<String>, profile: Option<&str>) -> Result<Self, Error> {
        let hostname = hostname.into();
        let config = crate::ecr::load_sdk_config(&hostname, profile)
            .await
            .map_err(|e| {
                Error::auth_failed(&hostname, format!("failed to load AWS config: {e}"))
            })?;

        let client = crate::aws::Client::new(&config, crate::aws::Service::Ecr)
            .map_err(|e| Error::auth_failed(&hostname, e))?;
        let registry = hostname.clone();

        Ok(Self {
            hostname,
            credential_cache: CredentialCache::new(),
            api: Box::new(AwsEcrApi { client, registry }),
        })
    }

    /// Create an ECR auth provider with an injected API implementation.
    #[cfg(test)]
    fn with_api(hostname: impl Into<String>, api: impl EcrApi + 'static) -> Self {
        Self {
            hostname: hostname.into(),
            credential_cache: CredentialCache::new(),
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
                let token = AuthorizationToken::parse(response.encoded_token, &self.hostname)?;
                let ttl = response.expires_in.unwrap_or(ECR_DEFAULT_TOKEN_TTL);
                tracing::debug!(registry = %self.hostname, ttl_secs = ttl.as_secs(), "ECR token refreshed");
                // ECR expects `Authorization: Basic <base64(AWS:password)>`. The encoded
                // token from GetAuthorizationToken is already in the right format.
                let token = Token::with_ttl(token.into_encoded(), ttl).with_scheme(AuthScheme::Basic);
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
                    _ => Err(Error::auth_failed("mock", "mock: no token available")),
                }
            })
        }
    }

    // --- AuthorizationToken tests ---

    /// ECR answers with a list of authorization data. The expiry is read
    /// as epoch seconds. A negative one has passed, and one too large to
    /// represent or null is no expiry; none of them fails the token.
    #[test]
    fn reads_the_get_authorization_token_output() {
        let output: GetAuthorizationTokenOutput = serde_json::from_str(
            r#"{"authorizationData":[{"authorizationToken":"QVdTOnB3","expiresAt":4102444800.5,"proxyEndpoint":"https://x"}]}"#,
        )
        .unwrap();
        let data = &output.authorization_data[0];
        assert_eq!(data.authorization_token.as_deref(), Some("QVdTOnB3"));
        assert_eq!(
            data.expires_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(4_102_444_800_500))
        );
        for (sent, expected) in [
            ("-1", Some(SystemTime::UNIX_EPOCH)),
            ("1e300", None),
            ("null", None),
        ] {
            let body = format!(r#"{{"authorizationData":[{{"expiresAt":{sent}}}]}}"#);
            let output: GetAuthorizationTokenOutput = serde_json::from_str(&body).unwrap();
            assert_eq!(output.authorization_data[0].expires_at, expected, "{sent}");
        }
        let output: GetAuthorizationTokenOutput =
            serde_json::from_str(r#"{"authorizationData":null}"#).unwrap();
        assert!(output.authorization_data.is_empty());
    }

    #[test]
    fn an_expiry_in_the_past_leaves_no_time() {
        assert_eq!(remaining(SystemTime::UNIX_EPOCH), Duration::ZERO);
    }

    /// A token keeps its encoded form for ECR and yields its password for
    /// ECR Public.
    #[test]
    fn parses_a_token() {
        let encoded = BASE64.encode("AWS:my-secret-token");
        let parse = || AuthorizationToken::parse(encoded.clone(), "test-registry").unwrap();
        assert_eq!(parse().into_encoded(), encoded);
        assert_eq!(parse().into_password(), "my-secret-token");
    }

    #[test]
    fn parse_token_invalid_prefix() {
        let encoded = BASE64.encode("NOTAWS:token");
        let err = AuthorizationToken::parse(encoded, "test-registry")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("AWS:"));
        assert!(err.contains("test-registry"));
    }

    #[test]
    fn parse_token_invalid_base64() {
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        let err = AuthorizationToken::parse("not-valid-base64!!!".into(), host)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("invalid base64"));
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
        let result = EcrAuth::new("ghcr.io", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("region"));
    }

    #[tokio::test]
    async fn new_accepts_optional_profile() {
        // Profile name does not validate here; SDK config load is permissive
        // about unknown profile names. This test asserts the signature and that
        // construction itself does not fail.
        let auth = EcrAuth::new(
            "123456789012.dkr.ecr.us-east-1.amazonaws.com",
            Some("nonexistent-test-profile"),
        )
        .await;
        assert!(
            auth.is_ok(),
            "EcrAuth::new with profile should succeed: {:?}",
            auth.err()
        );
    }
}
