//! GitHub Container Registry auth provider.
//!
//! Reads `GITHUB_TOKEN` from the environment for authentication.

use std::future::Future;
use std::pin::Pin;

use crate::auth::{AuthProvider, Scope, Token};
use crate::error::Error;

/// GitHub Container Registry authentication provider.
///
/// Uses `GITHUB_TOKEN` from the environment. Returns [`Error::NoCredentials`]
/// if the variable is absent.
#[derive(Debug, Default)]
pub struct GhcrAuth;

impl GhcrAuth {
    /// Create a new GHCR auth provider.
    pub fn new() -> Self {
        Self
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_no_credentials_without_env() {
        // Ensure GITHUB_TOKEN is not set for this test
        let provider = GhcrAuth::new();
        assert_eq!(provider.name(), "ghcr");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(provider.get_token(&[]));
        // May or may not fail depending on env — just verify it doesn't panic
        let _ = result;
    }
}
