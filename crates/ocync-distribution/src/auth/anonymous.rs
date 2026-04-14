//! Anonymous auth provider using the Docker token-exchange flow.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::Mutex;

use super::token_exchange::{exchange_token, scope_cache_key};
use super::{AuthProvider, Scope, Token};
use crate::error::Error;

/// Anonymous auth provider that performs the Docker token-exchange flow.
///
/// When a registry responds with `401 Unauthorized` and a `WWW-Authenticate: Bearer ...`
/// header, this provider extracts the realm/service and exchanges them for an anonymous
/// token. Tokens are cached per-scope and coalesced under a mutex to prevent thundering
/// herd.
pub struct AnonymousAuth {
    /// The registry base URL (e.g. `https://registry-1.docker.io`).
    base_url: String,
    /// HTTP client for token requests.
    http: reqwest::Client,
    /// Cached tokens keyed by sorted scope strings.
    cache: Mutex<HashMap<String, Token>>,
}

impl fmt::Debug for AnonymousAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnonymousAuth")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl AnonymousAuth {
    /// Create a new anonymous auth provider for the given registry hostname.
    ///
    /// Uses HTTPS by default. For non-HTTPS registries (e.g. local development),
    /// use [`AnonymousAuth::with_base_url`].
    pub fn new(registry: impl Into<String>, http: reqwest::Client) -> Self {
        let registry = registry.into();
        Self {
            base_url: format!("https://{registry}"),
            http,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Create a new anonymous auth provider with an explicit base URL.
    ///
    /// Use this for registries that don't use HTTPS (e.g. `http://localhost:5000`).
    pub fn with_base_url(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl AuthProvider for AnonymousAuth {
    fn name(&self) -> &'static str {
        "anonymous"
    }

    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        let scopes = scopes.to_vec();
        Box::pin(async move {
            let key = scope_cache_key(&scopes);

            // Hold the mutex for the entire check-then-fetch to prevent thundering herd.
            let mut cache = self.cache.lock().await;

            if let Some(token) = cache.get(&key) {
                if !token.should_refresh() {
                    return Ok(token.clone());
                }
            }

            let token = exchange_token(&self.http, &self.base_url, &scopes, None).await?;
            cache.insert(key, token.clone());

            Ok(token)
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            cache.clear();
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn anonymous_auth_exchanges_token() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="test""#, server.uri()),
            ))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "anon-123", "expires_in": 300})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let auth = AnonymousAuth::with_base_url(server.uri(), reqwest::Client::new());
        let token = auth
            .get_token(&[Scope::pull("library/nginx")])
            .await
            .unwrap();
        assert_eq!(token.value(), "anon-123");
    }

    #[tokio::test]
    async fn anonymous_auth_caches_per_scope() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, server.uri()),
            ))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "cached", "expires_in": 3600})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let auth = AnonymousAuth::with_base_url(server.uri(), reqwest::Client::new());
        let t1 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(t1.value(), "cached");
        assert_eq!(t2.value(), "cached");
    }

    #[tokio::test]
    async fn anonymous_auth_invalidate_clears_cache() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, server.uri()),
            ))
            .expect(2)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "fresh", "expires_in": 3600})),
            )
            .expect(2)
            .mount(&server)
            .await;

        let auth = AnonymousAuth::with_base_url(server.uri(), reqwest::Client::new());
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.invalidate().await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    #[test]
    fn anonymous_auth_name() {
        let auth = AnonymousAuth::new("example.com", reqwest::Client::new());
        assert_eq!(auth.name(), "anonymous");
    }
}
