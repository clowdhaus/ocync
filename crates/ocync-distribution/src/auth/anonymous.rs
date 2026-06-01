//! Anonymous auth provider using the Docker token-exchange flow.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use super::token_cache::TokenCache;
use super::token_exchange;
use super::{AuthProvider, Scope, Token, scopes_cache_key};
use crate::error::Error;

/// Anonymous auth provider that performs the Docker token-exchange flow.
///
/// When a registry responds with `401 Unauthorized` and a `WWW-Authenticate: Bearer ...`
/// header, this provider extracts the realm/service and exchanges them for an anonymous
/// token. Tokens are coalesced per scope: concurrent fetches for the same scope produce
/// one token exchange while distinct scopes run in parallel.
pub struct AnonymousAuth {
    /// The registry base URL (e.g. `https://registry-1.docker.io`).
    base_url: String,
    /// HTTP client for token requests.
    http: reqwest::Client,
    /// Per-scope coalescing token cache.
    tokens: TokenCache,
    /// Cached `WWW-Authenticate` challenge to skip redundant `/v2/` pings.
    challenge_cache: token_exchange::ChallengeCache,
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
            tokens: TokenCache::new(),
            challenge_cache: token_exchange::ChallengeCache::new(),
        }
    }

    /// Create a new anonymous auth provider with an explicit base URL.
    ///
    /// Use this for registries that don't use HTTPS (e.g. `http://localhost:5000`).
    pub fn with_base_url(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            tokens: TokenCache::new(),
            challenge_cache: token_exchange::ChallengeCache::new(),
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
            let key = scopes_cache_key(&scopes);
            self.tokens
                .get_or_fetch(key.clone(), || async {
                    tracing::debug!(base_url = %self.base_url, scope = %key, "token cache miss, exchanging");
                    let cached_challenge = self.challenge_cache.get().await;
                    let (token, challenge) = token_exchange::exchange(
                        &self.http,
                        &self.base_url,
                        &scopes,
                        None,
                        cached_challenge.as_ref(),
                    )
                    .await
                    .map_err(|e| {
                        tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                        e
                    })?;
                    self.challenge_cache.set(challenge).await;
                    Ok(token)
                })
                .await
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let entries = self.tokens.len().await;
            tracing::debug!(base_url = %self.base_url, entries, "invalidating token cache");
            self.tokens.clear().await;
            self.challenge_cache.clear().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

        let auth = AnonymousAuth::with_base_url(server.uri(), crate::test_http_client());
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

        let auth = AnonymousAuth::with_base_url(server.uri(), crate::test_http_client());
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

        let auth = AnonymousAuth::with_base_url(server.uri(), crate::test_http_client());
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.invalidate().await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    #[tokio::test]
    async fn anonymous_auth_challenge_cache_reuse() {
        let server = MockServer::start().await;

        // /v2/ should only be hit once -- second get_token reuses the cached challenge.
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="test""#, server.uri()),
            ))
            .expect(1)
            .mount(&server)
            .await;

        // Token endpoint is called twice (different scopes).
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "reused", "expires_in": 300})),
            )
            .expect(2)
            .mount(&server)
            .await;

        let auth = AnonymousAuth::with_base_url(server.uri(), crate::test_http_client());
        let t1 = auth.get_token(&[Scope::pull("repo-a")]).await.unwrap();
        let t2 = auth.get_token(&[Scope::pull("repo-b")]).await.unwrap();
        assert_eq!(t1.value(), "reused");
        assert_eq!(t2.value(), "reused");
        // expect(1) on /v2/ proves the challenge was cached and reused.
    }

    #[tokio::test(flavor = "current_thread")]
    async fn anonymous_auth_distinct_scopes_fetch_concurrently() {
        // Two distinct scopes must reach the token endpoint concurrently --
        // a shared provider mutex would serialize the second request behind
        // the first's 300ms response delay. We assert on the arrival-diff at
        // the mock (parallel: ~ms, serialized: >= 300ms) rather than total
        // wall-clock so CI runner jitter cannot flake the test.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="test""#, server.uri()),
            ))
            .mount(&server)
            .await;
        let arrivals: Arc<Mutex<Vec<std::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let slow_for = |scope: &str, token_value: &str| {
            let arrivals = Arc::clone(&arrivals);
            Mock::given(method("GET"))
                .and(path("/token"))
                .and(query_param("scope", scope))
                .and(move |_req: &Request| {
                    arrivals.lock().unwrap().push(std::time::Instant::now());
                    true
                })
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(std::time::Duration::from_millis(300))
                        .set_body_json(serde_json::json!({
                            "token": token_value,
                            "expires_in": 3600,
                        })),
                )
                .expect(1)
        };
        slow_for("repository:repo-a:pull", "tok-a")
            .mount(&server)
            .await;
        slow_for("repository:repo-b:pull", "tok-b")
            .mount(&server)
            .await;

        let auth = Arc::new(AnonymousAuth::with_base_url(
            server.uri(),
            crate::test_http_client(),
        ));
        let auth_a = Arc::clone(&auth);
        let auth_b = Arc::clone(&auth);

        let (t_a, t_b) = tokio::join!(
            async move { auth_a.get_token(&[Scope::pull("repo-a")]).await.unwrap() },
            async move { auth_b.get_token(&[Scope::pull("repo-b")]).await.unwrap() },
        );

        assert_eq!(t_a.value(), "tok-a");
        assert_eq!(t_b.value(), "tok-b");

        let times = arrivals.lock().unwrap();
        assert_eq!(
            times.len(),
            2,
            "expected 2 token requests, got {}",
            times.len()
        );
        let (first, second) = if times[0] <= times[1] {
            (times[0], times[1])
        } else {
            (times[1], times[0])
        };
        let diff = second.duration_since(first);
        assert!(
            diff < std::time::Duration::from_millis(250),
            "expected near-simultaneous arrival, got {diff:?}; distinct scopes must not serialize on a shared provider mutex",
        );
    }

    #[test]
    fn anonymous_auth_name() {
        let auth = AnonymousAuth::new("example.com", crate::test_http_client());
        assert_eq!(auth.name(), "anonymous");
    }
}
