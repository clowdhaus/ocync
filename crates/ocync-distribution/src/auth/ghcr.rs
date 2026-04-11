//! GitHub Container Registry auth provider.

// ── Feature-gated implementation ──────────────────────────────────────────

#[cfg(feature = "ghcr")]
mod provider {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::DistributionError;

    /// GitHub Container Registry authentication provider.
    ///
    /// Reads `GITHUB_TOKEN` from the environment. This is the standard
    /// authentication mechanism for GHCR in CI and local development.
    pub struct GhcrAuth;

    impl GhcrAuth {
        /// Create a new GHCR auth provider.
        pub fn new() -> Self {
            Self
        }
    }

    impl Default for GhcrAuth {
        fn default() -> Self {
            Self::new()
        }
    }

    impl AuthProvider for GhcrAuth {
        fn name(&self) -> &'static str {
            "ghcr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, DistributionError>> + Send + '_>> {
            Box::pin(async {
                let token = std::env::var("GITHUB_TOKEN").map_err(|_| {
                    DistributionError::NoCredentials {
                        registry: "ghcr.io".into(),
                    }
                })?;

                Ok(Token::new(token))
            })
        }
    }
}

#[cfg(feature = "ghcr")]
pub use provider::GhcrAuth;

// ── Stub when feature is not compiled ─────────────────────────────────────

#[cfg(not(feature = "ghcr"))]
mod stub {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::DistributionError;

    /// Stub GHCR provider returned when the `ghcr` feature is not enabled.
    pub struct GhcrStub;

    impl AuthProvider for GhcrStub {
        fn name(&self) -> &'static str {
            "ghcr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, DistributionError>> + Send + '_>> {
            Box::pin(async {
                Err(DistributionError::ProviderNotCompiled {
                    provider: "ghcr",
                    feature: "ghcr",
                })
            })
        }
    }
}

#[cfg(not(feature = "ghcr"))]
pub use stub::GhcrStub;

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "ghcr"))]
    #[test]
    fn stub_returns_provider_not_compiled() {
        use super::GhcrStub;
        use crate::auth::AuthProvider;
        use crate::error::DistributionError;

        let stub = GhcrStub;
        assert_eq!(stub.name(), "ghcr");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(stub.get_token(&[]));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DistributionError::ProviderNotCompiled {
                provider: "ghcr",
                feature: "ghcr"
            }
        ));
    }
}
