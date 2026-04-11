//! Azure Container Registry auth provider.
//!
//! Placeholder — full implementation will use Azure Identity credentials.

use std::future::Future;
use std::pin::Pin;

use crate::auth::{AuthProvider, Scope, Token};
use crate::error::Error;

/// Azure Container Registry authentication provider (not yet implemented).
#[derive(Debug)]
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
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        Box::pin(async {
            Err(Error::AuthFailed {
                registry: self.hostname.clone(),
                reason: "ACR auth provider not yet implemented".into(),
            })
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_auth_failed() {
        let provider = AcrAuth::new("myregistry.azurecr.io");
        assert_eq!(provider.name(), "acr");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(provider.get_token(&[]));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::AuthFailed { .. }));
    }
}
