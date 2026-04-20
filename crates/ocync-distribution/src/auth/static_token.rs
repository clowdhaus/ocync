//! Static bearer token authentication provider.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{AuthProvider, Scope, Token};
use crate::error::Error;

/// Auth provider that returns a pre-configured bearer token.
///
/// Used for CI tokens, personal access tokens (PATs), and other
/// scenarios where the caller already has a valid bearer token
/// that doesn't require a token-exchange flow.
///
/// When `invalidate()` is called (after a 401 from the registry),
/// the token is marked as revoked and subsequent `get_token()` calls
/// return an error, breaking the infinite retry loop.
pub struct StaticTokenAuth {
    /// The bearer token value.
    token: String,
    /// The registry this token authenticates against (for error messages).
    registry: String,
    /// Whether the token has been invalidated by a 401 response.
    invalidated: AtomicBool,
}

impl fmt::Debug for StaticTokenAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticTokenAuth")
            .field("token", &"[REDACTED]")
            .field("registry", &self.registry)
            .field("invalidated", &self.invalidated.load(Ordering::Relaxed))
            .finish()
    }
}

impl StaticTokenAuth {
    /// Create a new static token auth provider for the given registry.
    pub fn new(registry: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            registry: registry.into(),
            invalidated: AtomicBool::new(false),
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
        Box::pin(async move {
            if self.invalidated.load(Ordering::Acquire) {
                return Err(Error::AuthFailed {
                    registry: self.registry.clone(),
                    reason: "static token was invalidated (rejected by registry)".into(),
                });
            }
            tracing::trace!("using static token (no refresh possible)");
            Ok(Token::new(&self.token))
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            tracing::warn!(
                registry = %self.registry,
                "static token invalidated; subsequent requests will fail"
            );
            self.invalidated.store(true, Ordering::Release);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Scope;

    #[tokio::test]
    async fn static_token_returns_configured_value() {
        let auth = StaticTokenAuth::new("ghcr.io", "my-pat-token-123");
        let scopes = [Scope::pull("library/nginx")];
        let token = auth.get_token(&scopes).await.unwrap();
        assert_eq!(token.value(), "my-pat-token-123");
    }

    #[tokio::test]
    async fn static_token_ignores_scopes() {
        let auth = StaticTokenAuth::new("ghcr.io", "token");
        let t1 = auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull_push("repo-b")]).await.unwrap();
        assert_eq!(t1.value(), "token");
        assert_eq!(t2.value(), "token");
    }

    #[tokio::test]
    async fn static_token_errors_after_invalidate() {
        let auth = StaticTokenAuth::new("ghcr.io", "revoked-token");
        auth.invalidate().await;
        let err = auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert!(err.to_string().contains("invalidated"), "error: {err}");
        assert!(err.to_string().contains("ghcr.io"), "error: {err}");
    }

    #[test]
    fn static_token_name() {
        let auth = StaticTokenAuth::new("ghcr.io", "t");
        assert_eq!(auth.name(), "static-token");
    }

    #[test]
    fn static_token_debug_redacts() {
        let auth = StaticTokenAuth::new("ghcr.io", "super-secret-token");
        let debug = format!("{auth:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(debug.contains("[REDACTED]"));
    }
}
