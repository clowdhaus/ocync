//! HTTP Basic auth provider using the Docker token-exchange flow.
//!
//! Performs the same challenge-response token exchange as [`super::anonymous::AnonymousAuth`],
//! but includes an `Authorization: Basic base64(user:pass)` header on the token request.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::Mutex;

use super::token_exchange;
use super::{AuthProvider, Credentials, Scope, Token, scopes_cache_key};
use crate::error::Error;

/// Auth provider that performs the Docker token-exchange flow with HTTP Basic credentials.
///
/// When a registry responds with `401 Unauthorized` and a `WWW-Authenticate: Bearer ...`
/// header, this provider extracts the realm/service and exchanges them for a token using
/// HTTP Basic authentication. Tokens are cached per-scope and coalesced under a mutex to
/// prevent thundering herd.
pub struct BasicAuth {
    /// The registry base URL (e.g. `https://registry-1.docker.io`).
    base_url: String,
    /// HTTP client for token requests.
    http: reqwest::Client,
    /// Credentials for the Basic auth header.
    credentials: Credentials,
    /// Cached tokens keyed by sorted scope strings.
    cache: Mutex<HashMap<String, Token>>,
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasicAuth")
            .field("base_url", &self.base_url)
            .field("credentials", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl BasicAuth {
    /// Create a new Basic auth provider for the given registry hostname.
    ///
    /// Uses HTTPS by default. For non-HTTPS registries (e.g. local development),
    /// use [`BasicAuth::with_base_url`].
    pub fn new(
        registry: impl Into<String>,
        http: reqwest::Client,
        credentials: Credentials,
    ) -> Self {
        let registry = registry.into();
        Self {
            base_url: format!("https://{registry}"),
            http,
            credentials,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Create a new Basic auth provider with an explicit base URL.
    ///
    /// Use this for registries that don't use HTTPS (e.g. `http://localhost:5000`).
    pub fn with_base_url(
        base_url: impl Into<String>,
        http: reqwest::Client,
        credentials: Credentials,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            credentials,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl AuthProvider for BasicAuth {
    fn name(&self) -> &'static str {
        "basic"
    }

    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>> {
        let scopes = scopes.to_vec();
        Box::pin(async move {
            let key = scopes_cache_key(&scopes);

            // Hold the mutex for the entire check-then-fetch to prevent thundering herd.
            let mut cache = self.cache.lock().await;

            if let Some(token) = cache.get(&key).filter(|t| t.is_valid()) {
                tracing::debug!(base_url = %self.base_url, scope = %key, "token cache hit");
                return Ok(token.clone());
            }

            tracing::debug!(base_url = %self.base_url, scope = %key, "token cache miss, exchanging");
            let token = token_exchange::exchange(
                &self.http,
                &self.base_url,
                &scopes,
                Some(&self.credentials),
            )
            .await
            .map_err(|e| {
                tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                e
            })?;
            cache.insert(key, token.clone());

            Ok(token)
        })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            tracing::debug!(base_url = %self.base_url, entries = cache.len(), "invalidating token cache");
            cache.clear();
        })
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn test_credentials() -> Credentials {
        Credentials::Basic {
            username: "testuser".into(),
            password: "testpass".into(),
        }
    }

    fn expected_basic_header() -> String {
        let encoded = BASE64.encode("testuser:testpass");
        format!("Basic {encoded}")
    }

    async fn mount_v2_challenge(server: &MockServer, expect: u64) {
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="test-registry""#,
                    server.uri()
                ),
            ))
            .expect(expect)
            .mount(server)
            .await;
    }

    async fn mount_token_endpoint(server: &MockServer, token_value: &str, expect: u64) {
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header("Authorization", expected_basic_header().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": token_value,
                "expires_in": 3600
            })))
            .expect(expect)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn basic_auth_sends_authorization_header() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 1).await;
        mount_token_endpoint(&server, "tok123", 1).await;

        let auth =
            BasicAuth::with_base_url(server.uri(), crate::test_http_client(), test_credentials());
        let token = auth
            .get_token(&[Scope::pull("library/nginx")])
            .await
            .unwrap();
        assert_eq!(token.value(), "tok123");

        // Verify query params on the token request.
        let requests = server.received_requests().await.unwrap();
        let token_req = requests
            .iter()
            .find(|r| r.url.path() == "/token")
            .expect("token request not found");
        let query_pairs: HashMap<String, String> = token_req
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            query_pairs.get("service").map(String::as_str),
            Some("test-registry")
        );
        assert_eq!(
            query_pairs.get("scope").map(String::as_str),
            Some("repository:library/nginx:pull")
        );
    }

    #[tokio::test]
    async fn basic_auth_caches_tokens_per_scope() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 1).await;
        mount_token_endpoint(&server, "cached-tok", 1).await;

        let auth =
            BasicAuth::with_base_url(server.uri(), crate::test_http_client(), test_credentials());
        let scopes = [Scope::pull("library/nginx")];
        let t1 = auth.get_token(&scopes).await.unwrap();
        let t2 = auth.get_token(&scopes).await.unwrap();
        assert_eq!(t1.value(), "cached-tok");
        assert_eq!(t2.value(), "cached-tok");
    }

    #[tokio::test]
    async fn basic_auth_invalidate_clears_cache() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 2).await;
        mount_token_endpoint(&server, "fresh-tok", 2).await;

        let auth =
            BasicAuth::with_base_url(server.uri(), crate::test_http_client(), test_credentials());
        let scopes = [Scope::pull("library/nginx")];
        auth.get_token(&scopes).await.unwrap();
        auth.invalidate().await;
        auth.get_token(&scopes).await.unwrap();
    }

    #[tokio::test]
    async fn basic_auth_no_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let auth =
            BasicAuth::with_base_url(server.uri(), crate::test_http_client(), test_credentials());
        let token = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        assert_eq!(token.value(), "");
    }

    #[tokio::test]
    async fn basic_auth_token_endpoint_error() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;

        let auth =
            BasicAuth::with_base_url(server.uri(), crate::test_http_client(), test_credentials());
        let err = auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert!(matches!(err, Error::Http(_)));
    }

    #[tokio::test]
    async fn basic_auth_missing_www_authenticate_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let auth =
            BasicAuth::with_base_url(server.uri(), crate::test_http_client(), test_credentials());
        let err = auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert!(err.to_string().contains("WWW-Authenticate"));
    }

    #[test]
    fn basic_auth_name() {
        let auth = BasicAuth::new("example.com", crate::test_http_client(), test_credentials());
        assert_eq!(auth.name(), "basic");
    }

    #[test]
    fn basic_auth_debug_redacts_credentials() {
        let auth = BasicAuth::new("example.com", crate::test_http_client(), test_credentials());
        let debug = format!("{auth:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("testuser"));
        assert!(!debug.contains("testpass"));
    }
}
