//! Azure Container Registry auth provider.

// ── Feature-gated implementation ──────────────────────────────────────────

#[cfg(feature = "acr")]
mod provider {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::DistributionError;

    /// Azure Container Registry authentication provider.
    ///
    /// Placeholder — full implementation will use Azure Identity credentials.
    pub struct AcrAuth {
        /// The registry hostname.
        hostname: String,
    }

    impl AcrAuth {
        /// Create a new ACR auth provider for the given registry hostname.
        pub fn new(hostname: impl Into<String>) -> Self {
            Self {
                hostname: hostname.into(),
            }
        }
    }

    impl AuthProvider for AcrAuth {
        fn name(&self) -> &'static str {
            "acr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, DistributionError>> + Send + '_>> {
            Box::pin(async {
                Err(DistributionError::AuthFailed {
                    registry: self.hostname.clone(),
                    reason: "ACR auth provider not yet implemented".into(),
                })
            })
        }
    }
}

#[cfg(feature = "acr")]
pub use provider::AcrAuth;

// ── Stub when feature is not compiled ─────────────────────────────────────

#[cfg(not(feature = "acr"))]
mod stub {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::DistributionError;

    /// Stub ACR provider returned when the `acr` feature is not enabled.
    pub struct AcrStub;

    impl AuthProvider for AcrStub {
        fn name(&self) -> &'static str {
            "acr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, DistributionError>> + Send + '_>> {
            Box::pin(async {
                Err(DistributionError::ProviderNotCompiled {
                    provider: "acr",
                    feature: "acr",
                })
            })
        }
    }
}

#[cfg(not(feature = "acr"))]
pub use stub::AcrStub;

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "acr"))]
    #[test]
    fn stub_returns_provider_not_compiled() {
        use super::AcrStub;
        use crate::auth::AuthProvider;
        use crate::error::DistributionError;

        let stub = AcrStub;
        assert_eq!(stub.name(), "acr");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(stub.get_token(&[]));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DistributionError::ProviderNotCompiled {
                provider: "acr",
                feature: "acr"
            }
        ));
    }
}
