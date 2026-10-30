//! AWS ECR authentication provider.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use crate::auth::{AuthProvider, AuthScheme, Scope, Token};
use crate::credential_cache::{CredentialCache, remaining};
use crate::error::{CredentialFailure, Error};

/// ECR token lifetime when the answer carries no `expiresAt`.
const ECR_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Response from an ECR `GetAuthorizationToken` call.
#[derive(Clone)]
pub(super) struct EcrTokenResponse {
    /// The raw base64-encoded authorization token (`AWS:<password>`).
    pub encoded_token: String,
    /// Time until the token expires, if provided by the API.
    pub expires_in: Option<Duration>,
}

/// ECR's `GetAuthorizationToken` for one registry, injectable for tests.
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

/// ECR's `GetAuthorizationToken`, which takes no input.
#[derive(serde::Serialize)]
struct GetAuthorizationToken {}

/// ECR answers with a list of authorization data, one per registry.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetAuthorizationTokenOutput {
    #[serde(default, deserialize_with = "crate::json::null_as_default")]
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
                .map_err(|e| Error::cloud_credentials(&self.registry, e.credential_failure(), e))?;

            let auth_data = output
                .authorization_data
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::cloud_credentials(
                        &self.registry,
                        CredentialFailure::Failed,
                        "ECR returned empty authorization data",
                    )
                })?;

            let encoded_token = auth_data.authorization_token.ok_or_else(|| {
                Error::cloud_credentials(
                    &self.registry,
                    CredentialFailure::Failed,
                    "ECR authorization data missing token",
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
            Error::cloud_credentials(
                registry,
                CredentialFailure::Failed,
                format!("invalid base64 in ECR token: {e}"),
            )
        })?;
        let text = String::from_utf8(decoded).map_err(|e| {
            Error::cloud_credentials(
                registry,
                CredentialFailure::Failed,
                format!("ECR token is not valid UTF-8: {e}"),
            )
        })?;
        let password = text
            .strip_prefix("AWS:")
            .ok_or_else(|| {
                Error::cloud_credentials(
                    registry,
                    CredentialFailure::Failed,
                    "ECR token does not start with 'AWS:'",
                )
            })?
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

/// AWS ECR authentication provider: a `GetAuthorizationToken` token, cached
/// until it nears expiry and sent as Basic auth.
pub struct EcrAuth {
    hostname: String,
    credential_cache: CredentialCache<Token, Error>,
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
    /// Create an ECR auth provider for the registry hostname. Fails under
    /// the conditions listed on `BatchChecker::from_hostname`.
    ///
    /// `profile` scopes credential resolution to that named profile, `None`
    /// to the default chain. Credentials come from `aws_credentials`, shared
    /// only with clients for the same profile and region, so a profile
    /// override does not leak across registries.
    pub fn new(
        hostname: impl Into<String>,
        profile: Option<&str>,
        aws_credentials: &crate::AwsCredentialProviders,
    ) -> Result<Self, Error> {
        let hostname = hostname.into();
        let client = crate::ecr::api_client(&hostname, profile, aws_credentials)?;
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

    /// Replace the cached token.
    #[cfg(test)]
    fn set_cached_token(&self, token: Token) {
        self.credential_cache.set(token, Duration::from_secs(10));
    }

    async fn get_token_inner(&self) -> Result<Token, Error> {
        self.credential_cache
            .get_or_refresh_sharing_failure(async {
                tracing::debug!(registry = %self.hostname, "token cache miss, calling GetAuthorizationToken");
                let response = self.api.get_authorization_token().await.map_err(|e| {
                    tracing::warn!(registry = %self.hostname, error = %e, "ECR GetAuthorizationToken failed");
                    e
                })?;
                let token = AuthorizationToken::parse(response.encoded_token, &self.hostname)?;
                let ttl = response.expires_in.unwrap_or(ECR_DEFAULT_TOKEN_TTL);
                tracing::debug!(registry = %self.hostname, ttl_secs = ttl.as_secs(), "ECR token refreshed");
                // The encoded token is already ECR's `Basic` credential.
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

    fn refuse(&self, refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let refusal = self
                .credential_cache
                .refuse(refused, Token::sends_same_header);
            tracing::debug!(registry = %self.hostname, ?refusal, "registry refused a token");
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Mutex;

    use super::*;

    /// A throttle that outlasts its retries is not an auth error, and an
    /// access denial is. Breaks if every AWS failure exits as refused
    /// credentials, or a denial stops counting as one.
    /// A hostname that names no region fails construction with the config
    /// error as the source. Breaks if `new` flattens the cause to text.
    #[test]
    fn new_keeps_the_config_error_as_source() {
        let error =
            EcrAuth::new("localhost", None, &crate::AwsCredentialProviders::default()).unwrap_err();
        let source = std::error::Error::source(&error).expect("source");
        assert_eq!(
            source.to_string(),
            "cannot extract AWS region from ECR hostname 'localhost'"
        );
    }

    #[tokio::test]
    async fn only_an_aws_refusal_is_an_auth_error() {
        for (answer, auth_error) in [
            (crate::aws::tests::throttle(), false),
            (crate::aws::tests::access_denied(), true),
        ] {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(answer)
                .mount(&server)
                .await;
            let api = AwsEcrApi {
                client: crate::aws::tests::client(&server, std::num::NonZeroU32::MIN),
                registry: "ecr".to_owned(),
            };
            let Err(err) = api.get_authorization_token().await else {
                panic!("accepted");
            };
            assert_eq!(err.is_auth_error(), auth_error, "{err}");
        }
    }

    /// Mock ECR API that returns pre-configured responses in order.
    ///
    /// Uses a response queue: each call pops the next response. An empty
    /// queue returns an error, which lets tests verify caching by providing
    /// exactly N responses for N expected fetches.
    struct MockEcrApi {
        responses: Mutex<VecDeque<Option<EcrTokenResponse>>>,
        /// How many times `get_authorization_token` was called.
        calls: Arc<AtomicUsize>,
    }

    impl MockEcrApi {
        /// A handle on the call count, usable after the mock has moved
        /// into the provider.
        fn calls(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.calls)
        }

        /// Create a mock that returns the given encoded token on every call.
        fn succeeding(encoded_token: &str) -> Self {
            let response = EcrTokenResponse {
                encoded_token: encoded_token.to_owned(),
                expires_in: None,
            };
            let responses = std::iter::repeat_n(Some(response), 10).collect();
            Self {
                responses: Mutex::new(responses),
                calls: Arc::default(),
            }
        }

        /// Create a mock with an empty queue (always fails).
        fn failing() -> Self {
            Self {
                responses: Mutex::new(VecDeque::new()),
                calls: Arc::default(),
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
                calls: Arc::default(),
            }
        }

        /// Create a mock with an explicit [`EcrTokenResponse`] sequence.
        fn with_responses(responses: Vec<Option<EcrTokenResponse>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: Arc::default(),
            }
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
                let mut responses = self.responses.lock().await;
                match responses.pop_front() {
                    Some(Some(response)) => Ok(response),
                    _ => Err(Error::cloud_credentials(
                        "mock",
                        CredentialFailure::Failed,
                        "mock: no token available",
                    )),
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
        auth.set_cached_token(Token::with_ttl("stale", Duration::from_secs(10)));

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
    async fn auth_refusal_forces_refetch() {
        let encoded1 = BASE64.encode("AWS:first-token");
        let encoded2 = BASE64.encode("AWS:second-token");
        let auth = EcrAuth::with_api(
            TEST_HOST,
            MockEcrApi::with_tokens(vec![Some(encoded1.clone()), Some(encoded2.clone())]),
        );

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), encoded1);

        auth.refuse(t1).await;

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), encoded2);
    }

    /// N requests that carry one stale token are each refused, some before
    /// the refresh lands and some after, and all cost one
    /// `GetAuthorizationToken` call. Breaks if a refusal for a token that
    /// has been replaced drops the replacement, or one that arrives while
    /// the refresh runs starts another.
    #[tokio::test(flavor = "current_thread")]
    async fn refusals_of_one_stale_token_cost_one_refresh() {
        let fresh = BASE64.encode("AWS:fresh");
        let api = MockEcrApi::with_tokens(vec![Some(fresh.clone())]);
        let calls = api.calls();
        let auth = EcrAuth::with_api(TEST_HOST, api);
        let stale =
            Token::with_ttl("stale", Duration::from_secs(10)).with_scheme(AuthScheme::Basic);
        auth.set_cached_token(stale.clone());

        let refused_after = |yields: usize| {
            let (auth, stale) = (&auth, &stale);
            async move {
                for _ in 0..yields {
                    tokio::task::yield_now().await;
                }
                auth.refuse(stale.clone()).await;
                auth.get_token(&[]).await.unwrap()
            }
        };
        let tokens = tokio::join!(
            refused_after(0),
            refused_after(0),
            refused_after(1),
            refused_after(5),
            refused_after(5),
        );

        for token in [&tokens.0, &tokens.1, &tokens.2, &tokens.3, &tokens.4] {
            assert_eq!(token.value(), fresh);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Callers waiting on a refresh that fails share its error. Breaks if
    /// each waiter repeats the failed call against the same endpoint.
    #[tokio::test(flavor = "current_thread")]
    async fn waiters_on_a_failed_refresh_share_its_error() {
        let api = MockEcrApi::failing();
        let calls = api.calls();
        let auth = EcrAuth::with_api(TEST_HOST, api);

        let (first, second, third) = tokio::join!(
            auth.get_token(&[]),
            auth.get_token(&[]),
            auth.get_token(&[]),
        );

        for result in [first, second, third] {
            assert!(result.unwrap_err().to_string().contains("mock"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
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

    #[test]
    fn new_rejects_non_ecr_hostname() {
        let result = EcrAuth::new("ghcr.io", None, &crate::AwsCredentialProviders::default());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("region"));
    }

    #[test]
    fn new_accepts_optional_profile() {
        // Profile name does not validate here: credentials load lazily, so
        // an unknown profile surfaces on the first call. This test asserts
        // the signature and that construction itself does not fail.
        let auth = EcrAuth::new(
            "123456789012.dkr.ecr.us-east-1.amazonaws.com",
            Some("nonexistent-test-profile"),
            &crate::AwsCredentialProviders::default(),
        );
        assert!(
            auth.is_ok(),
            "EcrAuth::new with profile should succeed: {:?}",
            auth.err()
        );
    }
}
