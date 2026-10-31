//! Static bearer token authentication provider.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{AuthProvider, Presents, Scope, Token};
use crate::error::Error;

/// Auth provider that returns a pre-configured bearer token, such as a CI
/// token or personal access token.
///
/// Once refused after a 401, every later `get_token()` fails, so the client
/// cannot retry the same token forever.
pub struct StaticTokenAuth {
    token: String,
    /// For error messages.
    registry: String,
    invalidated: AtomicBool,
}

impl fmt::Debug for StaticTokenAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticTokenAuth")
            .field("token", &crate::REDACTED)
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

    fn presents(&self) -> Presents {
        Presents::Credentials
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

    fn refuse(&self, _refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
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
        auth.refuse(Token::new("revoked-token")).await;
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
