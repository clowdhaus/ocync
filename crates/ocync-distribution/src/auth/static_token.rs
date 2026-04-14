//! Static bearer token authentication provider.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use super::{AuthProvider, Scope, Token};
use crate::error::Error;

/// Auth provider that returns a pre-configured bearer token.
///
/// Used for CI tokens, personal access tokens (PATs), and other
/// scenarios where the caller already has a valid bearer token
/// that doesn't require a token-exchange flow.
///
/// `invalidate()` is a no-op because there is no cached exchange
/// to clear — the token is the source of truth.
pub struct StaticTokenAuth {
    /// The bearer token value.
    token: String,
}

impl fmt::Debug for StaticTokenAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticTokenAuth")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl StaticTokenAuth {
    /// Create a new static token auth provider.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl AuthProvider for StaticTokenAuth {
    fn name(&self) -> &'static str {
        "static-token"
    }

    fn get_token(
        &self,
        _scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        Box::pin(async move { Ok(Token::new(&self.token)) })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Scope;

    #[tokio::test]
    async fn static_token_returns_configured_value() {
        let auth = StaticTokenAuth::new("my-pat-token-123");
        let scopes = [Scope::pull("library/nginx")];
        let token = auth.get_token(&scopes).await.unwrap();
        assert_eq!(token.value(), "my-pat-token-123");
    }

    #[tokio::test]
    async fn static_token_ignores_scopes() {
        let auth = StaticTokenAuth::new("token");
        let t1 = auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull_push("repo-b")]).await.unwrap();
        assert_eq!(t1.value(), "token");
        assert_eq!(t2.value(), "token");
    }

    #[tokio::test]
    async fn static_token_survives_invalidate() {
        let auth = StaticTokenAuth::new("persistent");
        auth.invalidate().await;
        let token = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(token.value(), "persistent");
    }

    #[test]
    fn static_token_name() {
        let auth = StaticTokenAuth::new("t");
        assert_eq!(auth.name(), "static-token");
    }

    #[test]
    fn static_token_debug_redacts() {
        let auth = StaticTokenAuth::new("super-secret-token");
        let debug = format!("{auth:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(debug.contains("[REDACTED]"));
    }
}
