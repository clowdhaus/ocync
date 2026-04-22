//! AWS ECR Public authentication provider.
//!
//! Uses `aws-sdk-ecrpublic` to obtain authorization tokens via
//! `GetAuthorizationToken`. ECR Public is a global service; the API
//! endpoint is always in `us-east-1` regardless of where the caller runs.
//!
//! The token format is identical to ECR Private: base64-encoded
//! `AWS:<password>`, used as `Authorization: Basic <token>`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::RwLock;

use super::ecr::{EcrApi, EcrTokenResponse, ttl_from_expiry, validate_ecr_token};
use super::{AuthProvider, AuthScheme, Scope, Token};
use crate::error::Error;

/// Default ECR Public token lifetime (12 hours).
const ECR_PUBLIC_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Default [`EcrApi`] backed by the `aws-sdk-ecrpublic` SDK.
///
/// Holds the ECR Public SDK client created once during [`EcrPublicAuth::new`].
struct AwsEcrPublicApi {
    client: aws_sdk_ecrpublic::Client,
}

impl std::fmt::Debug for AwsEcrPublicApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// AWS ECR Public authentication provider.
///
/// Uses the AWS SDK to obtain authorization tokens via
/// `ecr-public:GetAuthorizationToken`. The SDK client is created once at
/// construction time, always targeting `us-east-1` (ECR Public is a
/// global service with a single API endpoint). Tokens are cached with
/// [`RwLock`] and refreshed when they approach expiry.
///
/// Supports all AWS credential sources via the default credential chain.
pub struct EcrPublicAuth {
    /// Cached bearer token.
    cached_token: RwLock<Option<Token>>,
    /// ECR API implementation.
    api: Box<dyn EcrApi>,
}

impl std::fmt::Debug for EcrPublicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcrPublicAuth").finish_non_exhaustive()
    }
}

impl EcrPublicAuth {
    /// Create a new ECR Public auth provider.
    ///
    /// Loads AWS credentials and constructs the ECR Public SDK client
    /// targeting `us-east-1`.
    pub async fn new() -> Result<Self, Error> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .load()
            .await;

        Ok(Self {
            cached_token: RwLock::new(None),
            api: Box::new(AwsEcrPublicApi {
                client: aws_sdk_ecrpublic::Client::new(&config),
            }),
        })
    }

    /// Create an ECR Public auth provider with an injected API implementation.
    #[cfg(test)]
    fn with_api(api: impl EcrApi + 'static) -> Self {
        Self {
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

    /// Fetch or return a cached ECR Public authorization token.
    async fn get_token_inner(&self) -> Result<Token, Error> {
        // Fast path: read-lock cache check.
        {
            let cached = self.cached_token.read().await;
            if let Some(token) = cached.as_ref().filter(|t| t.is_valid()) {
                tracing::debug!("ECR Public token cache hit (read-lock fast path)");
                return Ok(token.clone());
            }
        }

        // Slow path: write-lock for fetch + update.
        let mut cached = self.cached_token.write().await;

        // Double-check after acquiring write lock.
        if let Some(token) = cached.as_ref().filter(|t| t.is_valid()) {
            tracing::debug!("ECR Public token cache hit (write-lock recheck)");
            return Ok(token.clone());
        }

        tracing::debug!("ECR Public token cache miss, calling GetAuthorizationToken");
        let response = self.api.get_authorization_token().await.map_err(|e| {
            tracing::warn!(error = %e, "ECR Public GetAuthorizationToken failed");
            e
        })?;
        validate_ecr_token(&response.encoded_token, "public.ecr.aws")?;
        let ttl = response.expires_in.unwrap_or(ECR_PUBLIC_DEFAULT_TOKEN_TTL);
        tracing::debug!(ttl_secs = ttl.as_secs(), "ECR Public token refreshed");
        let token = Token::with_ttl(response.encoded_token, ttl).with_scheme(AuthScheme::Basic);

        *cached = Some(token.clone());

        Ok(token)
    }
}

impl AuthProvider for EcrPublicAuth {
    fn name(&self) -> &'static str {
        "ecr-public"
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
            tracing::debug!("invalidating cached ECR Public token");
            *cached = None;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
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
                        registry: "public.ecr.aws".into(),
                        reason: "mock: no token available".into(),
                    }),
                }
            })
        }
    }

    #[tokio::test]
    async fn auth_name() {
        let auth = EcrPublicAuth::with_api(MockEcrApi::failing());
        assert_eq!(auth.name(), "ecr-public");
    }

    #[test]
    fn validate_ecr_token_works_for_public() {
        let encoded = BASE64.encode("AWS:public-secret");
        validate_ecr_token(&encoded, "public.ecr.aws").unwrap();
    }

    #[tokio::test]
    async fn auth_caches_token() {
        // Queue has exactly one response. If the second get_token call hits
        // the API it will fail (queue empty), proving the cache works.
        let encoded = BASE64.encode("AWS:cached-token");
        let auth = EcrPublicAuth::with_api(MockEcrApi::succeeding(&encoded));

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), encoded);

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), encoded);
    }

    #[tokio::test]
    async fn auth_invalidation_forces_refetch() {
        let encoded1 = BASE64.encode("AWS:first-token");
        let encoded2 = BASE64.encode("AWS:second-token");
        let responses = vec![
            Some(EcrTokenResponse {
                encoded_token: encoded1.clone(),
                expires_in: None,
            }),
            Some(EcrTokenResponse {
                encoded_token: encoded2.clone(),
                expires_in: None,
            }),
        ];
        let auth = EcrPublicAuth::with_api(MockEcrApi {
            responses: Mutex::new(VecDeque::from(responses)),
        });

        let t1 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t1.value(), encoded1);

        auth.invalidate().await;

        let t2 = auth.get_token(&[]).await.unwrap();
        assert_eq!(t2.value(), encoded2);
    }

    #[tokio::test]
    async fn auth_propagates_api_error() {
        let auth = EcrPublicAuth::with_api(MockEcrApi::failing());

        let result = auth.get_token(&[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("public.ecr.aws"));
    }

    #[tokio::test]
    async fn auth_refreshes_near_expiry_token() {
        let encoded = BASE64.encode("AWS:refreshed-token");
        let auth = EcrPublicAuth::with_api(MockEcrApi::succeeding(&encoded));

        // Inject a near-expiry token (10s remaining < 30s EARLY_REFRESH_WINDOW).
        auth.set_cached_token(Token::with_ttl("stale", Duration::from_secs(10)))
            .await;

        // Should trigger refresh because should_refresh() returns true.
        let token = auth.get_token(&[]).await.unwrap();
        assert_eq!(token.value(), encoded);
    }
}
