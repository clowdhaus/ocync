//! AWS ECR authentication provider with feature-gated SDK integration.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::error::Error;

/// Decode an ECR authorization token.
///
/// ECR tokens are base64-encoded strings in the format `AWS:<token>`.
/// Returns the password portion (everything after `AWS:`).
pub fn decode_ecr_token(encoded: &str) -> Result<String, Error> {
    let decoded = BASE64.decode(encoded).map_err(|e| Error::AuthFailed {
        registry: String::new(),
        reason: format!("invalid base64 in ECR token: {e}"),
    })?;

    let text = String::from_utf8(decoded).map_err(|e| Error::AuthFailed {
        registry: String::new(),
        reason: format!("ECR token is not valid UTF-8: {e}"),
    })?;

    let password = text.strip_prefix("AWS:").ok_or_else(|| Error::AuthFailed {
        registry: String::new(),
        reason: "ECR token does not start with 'AWS:'".into(),
    })?;

    Ok(password.to_owned())
}

/// Extract the AWS region from an ECR hostname.
///
/// Expected format: `<account>.dkr.ecr[-fips].<region>.amazonaws.com`
pub fn ecr_region(hostname: &str) -> Option<&str> {
    // Split: ["<account>", "dkr", "ecr" or "ecr-fips", "<region>", "amazonaws", "com"]
    let parts: Vec<&str> = hostname.split('.').collect();
    if parts.len() < 6 {
        return None;
    }

    // The segment after "ecr" or "ecr-fips" is the region.
    let ecr_idx = parts.iter().position(|p| *p == "ecr" || *p == "ecr-fips")?;
    parts.get(ecr_idx + 1).copied()
}

// -- Feature-gated implementation ---------------------------------------------

#[cfg(feature = "ecr")]
mod provider {
    use std::future::Future;
    use std::pin::Pin;
    use std::time::Duration;

    use aws_config::BehaviorVersion;
    use tokio::sync::Mutex;

    use super::{decode_ecr_token, ecr_region};
    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::Error;

    /// ECR token lifetime (12 hours).
    const ECR_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

    /// AWS ECR authentication provider.
    ///
    /// Uses the AWS SDK to obtain authorization tokens via `GetAuthorizationToken`.
    /// Tokens are cached with `Mutex` and refreshed when they approach expiry.
    pub struct EcrAuth {
        /// The ECR registry hostname.
        hostname: String,
        /// Cached bearer token.
        cached_token: Mutex<Option<Token>>,
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
        pub fn new(hostname: impl Into<String>) -> Self {
            Self {
                hostname: hostname.into(),
                cached_token: Mutex::new(None),
            }
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
                let mut cached = self.cached_token.lock().await;
                *cached = None;
            })
        }
    }

    impl EcrAuth {
        async fn get_token_inner(&self) -> Result<Token, Error> {
            // Hold the mutex for the entire check-then-fetch to prevent thundering herd.
            let mut cached = self.cached_token.lock().await;

            if let Some(ref token) = *cached {
                if !token.should_refresh() {
                    return Ok(token.clone());
                }
            }

            let region = ecr_region(&self.hostname).ok_or_else(|| Error::AuthFailed {
                registry: self.hostname.clone(),
                reason: "unable to extract AWS region from ECR hostname".into(),
            })?;

            let config = aws_config::defaults(BehaviorVersion::latest())
                .region(aws_config::Region::new(region.to_owned()))
                .load()
                .await;

            let ecr_client = aws_sdk_ecr::Client::new(&config);

            let auth_output = ecr_client
                .get_authorization_token()
                .send()
                .await
                .map_err(|e| Error::AuthFailed {
                    registry: self.hostname.clone(),
                    reason: format!("ECR GetAuthorizationToken failed: {e}"),
                })?;

            let auth_data =
                auth_output
                    .authorization_data()
                    .first()
                    .ok_or_else(|| Error::AuthFailed {
                        registry: self.hostname.clone(),
                        reason: "ECR returned empty authorization data".into(),
                    })?;

            let encoded = auth_data
                .authorization_token()
                .ok_or_else(|| Error::AuthFailed {
                    registry: self.hostname.clone(),
                    reason: "ECR authorization data missing token".into(),
                })?;

            let password = decode_ecr_token(encoded)?;
            let token = Token::with_ttl(password, ECR_TOKEN_TTL);

            *cached = Some(token.clone());

            Ok(token)
        }
    }
}

#[cfg(feature = "ecr")]
pub use provider::EcrAuth;

// -- Stub when feature is not compiled ----------------------------------------

#[cfg(not(feature = "ecr"))]
mod stub {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::Error;

    /// Stub ECR provider returned when the `ecr` feature is not enabled.
    #[derive(Debug)]
    pub struct EcrStub;

    impl AuthProvider for EcrStub {
        fn name(&self) -> &'static str {
            "ecr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
            Box::pin(async {
                Err(Error::ProviderNotCompiled {
                    provider: "ecr",
                    feature: "ecr",
                })
            })
        }

        fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }
}

#[cfg(not(feature = "ecr"))]
pub use stub::EcrStub;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_ecr_token_valid() {
        // "AWS:my-secret-token" base64-encoded
        let encoded = BASE64.encode("AWS:my-secret-token");
        let password = decode_ecr_token(&encoded).unwrap();
        assert_eq!(password, "my-secret-token");
    }

    #[test]
    fn decode_ecr_token_invalid_prefix() {
        // "NOTAWS:token" base64-encoded
        let encoded = BASE64.encode("NOTAWS:token");
        let result = decode_ecr_token(&encoded);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("AWS:"));
    }

    #[test]
    fn decode_ecr_token_invalid_base64() {
        let result = decode_ecr_token("not-valid-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn parse_ecr_region_standard() {
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        assert_eq!(ecr_region(host), Some("us-east-1"));
    }

    #[test]
    fn parse_ecr_region_fips() {
        let host = "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com";
        assert_eq!(ecr_region(host), Some("us-gov-west-1"));
    }

    #[test]
    fn parse_ecr_region_invalid_host() {
        assert_eq!(ecr_region("ghcr.io"), None);
        assert_eq!(ecr_region(""), None);
    }

    #[cfg(not(feature = "ecr"))]
    #[test]
    fn stub_returns_provider_not_compiled() {
        use crate::auth::AuthProvider;

        let stub = EcrStub;
        assert_eq!(stub.name(), "ecr");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(stub.get_token(&[]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(
            err,
            Error::ProviderNotCompiled {
                provider: "ecr",
                feature: "ecr"
            }
        ));
    }
}
