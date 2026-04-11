//! Google Container Registry / Artifact Registry auth provider.

// -- Feature-gated implementation ---------------------------------------------

#[cfg(feature = "gcr")]
mod provider {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::Error;

    /// Google Container Registry / Artifact Registry authentication provider.
    ///
    /// Placeholder -- full implementation will use Application Default Credentials.
    pub struct GcrAuth {
        /// The registry hostname.
        hostname: String,
    }

    impl GcrAuth {
        /// Create a new GCR auth provider for the given registry hostname.
        pub fn new(hostname: impl Into<String>) -> Self {
            Self {
                hostname: hostname.into(),
            }
        }
    }

    impl AuthProvider for GcrAuth {
        fn name(&self) -> &'static str {
            "gcr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
            Box::pin(async {
                Err(Error::AuthFailed {
                    registry: self.hostname.clone(),
                    reason: "GCR auth provider not yet implemented".into(),
                })
            })
        }

        fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }
}

#[cfg(feature = "gcr")]
pub use provider::GcrAuth;

// -- Stub when feature is not compiled ----------------------------------------

#[cfg(not(feature = "gcr"))]
mod stub {
    use std::future::Future;
    use std::pin::Pin;

    use crate::auth::{AuthProvider, Scope, Token};
    use crate::error::Error;

    /// Stub GCR provider returned when the `gcr` feature is not enabled.
    #[derive(Debug)]
    pub struct GcrStub;

    impl AuthProvider for GcrStub {
        fn name(&self) -> &'static str {
            "gcr"
        }

        fn get_token(
            &self,
            _scopes: &[Scope],
        ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
            Box::pin(async {
                Err(Error::ProviderNotCompiled {
                    provider: "gcr",
                    feature: "gcr",
                })
            })
        }

        fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }
}

#[cfg(not(feature = "gcr"))]
pub use stub::GcrStub;

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "gcr"))]
    #[test]
    fn stub_returns_provider_not_compiled() {
        use super::GcrStub;
        use crate::auth::AuthProvider;
        use crate::error::Error;

        let stub = GcrStub;
        assert_eq!(stub.name(), "gcr");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(stub.get_token(&[]));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::ProviderNotCompiled {
                provider: "gcr",
                feature: "gcr"
            }
        ));
    }
}
