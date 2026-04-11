//! GitHub Container Registry auth provider.

use std::future::Future;
use std::pin::Pin;

use crate::auth::{AuthProvider, Scope, Token};
use crate::error::Error;

/// GitHub Container Registry authentication provider.
///
/// Reads `GITHUB_TOKEN` from the environment. This is the standard
/// authentication mechanism for GHCR in CI and local development.
#[derive(Debug)]
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
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        Box::pin(async {
            let token = std::env::var("GITHUB_TOKEN").map_err(|_| Error::NoCredentials {
                registry: "ghcr.io".into(),
            })?;

            Ok(Token::new(token))
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}
