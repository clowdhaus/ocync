//! Google Container Registry / Artifact Registry auth provider.

use std::future::Future;
use std::pin::Pin;

use crate::auth::{AuthProvider, Scope, Token};
use crate::error::Error;

/// Google Container Registry / Artifact Registry authentication provider.
///
/// Placeholder -- full implementation will use Application Default Credentials.
#[derive(Debug)]
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
