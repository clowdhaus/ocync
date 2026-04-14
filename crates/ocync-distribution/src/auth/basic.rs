//! HTTP Basic auth provider using the Docker token-exchange flow.
//!
//! Performs the same challenge-response token exchange as [`super::anonymous::AnonymousAuth`],
//! but includes an `Authorization: Basic base64(user:pass)` header on the token request.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::header::WWW_AUTHENTICATE;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::{AuthProvider, Credentials, Scope, Token};
use crate::error::Error;

/// The `Bearer` auth scheme prefix used in `WWW-Authenticate` challenges.
const BEARER_SCHEME: &str = "bearer";

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

    /// Build a cache key from a set of scopes.
    fn cache_key(scopes: &[Scope]) -> String {
        let mut parts: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        parts.sort();
        parts.join(" ")
    }

    /// Build the `Authorization: Basic ...` header value from credentials.
    fn basic_header_value(&self) -> String {
        let Credentials::Basic {
            ref username,
            ref password,
        } = self.credentials;
        let encoded = BASE64.encode(format!("{username}:{password}"));
        format!("Basic {encoded}")
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
        Box::pin(async move { self.get_token_inner(&scopes).await })
    }

    fn invalidate(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            cache.clear();
        })
    }
}

impl BasicAuth {
    async fn get_token_inner(&self, scopes: &[Scope]) -> Result<Token, Error> {
        let key = Self::cache_key(scopes);

        // Hold the mutex for the entire check-then-fetch to prevent thundering herd.
        let mut cache = self.cache.lock().await;

        // Check cache with scope awareness.
        if let Some(token) = cache.get(&key) {
            if !token.should_refresh() {
                return Ok(token.clone());
            }
        }

        // Need a fresh token — perform the exchange.
        let token = self.exchange_token(scopes).await?;
        cache.insert(key, token.clone());

        Ok(token)
    }

    /// Ping the registry's `/v2/` endpoint, parse the WWW-Authenticate header,
    /// then exchange for a token with Basic auth credentials.
    async fn exchange_token(&self, scopes: &[Scope]) -> Result<Token, Error> {
        let v2_url = format!("{}/v2/", self.base_url);
        let resp = self.http.get(&v2_url).send().await?;

        if resp.status().is_success() {
            // No auth required — return a dummy token.
            return Ok(Token::new(""));
        }

        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::AuthFailed {
                registry: self.base_url.clone(),
                reason: "401 response missing WWW-Authenticate header".into(),
            })?;

        let challenge = WwwAuthenticate::parse(www_auth).map_err(|reason| Error::AuthFailed {
            registry: self.base_url.clone(),
            reason,
        })?;

        // Build token request URL.
        let mut url = reqwest::Url::parse(&challenge.realm).map_err(|e| Error::AuthFailed {
            registry: self.base_url.clone(),
            reason: format!("invalid realm URL: {e}"),
        })?;

        {
            let mut query = url.query_pairs_mut();
            if let Some(ref service) = challenge.service {
                query.append_pair("service", service);
            }
            for scope in scopes {
                query.append_pair("scope", &scope.to_string());
            }
        }

        let token_resp = self
            .http
            .get(url)
            .header("Authorization", self.basic_header_value())
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;

        let token_value = token_resp
            .token
            .or(token_resp.access_token)
            .ok_or_else(|| Error::AuthFailed {
                registry: self.base_url.clone(),
                reason: "token response missing both 'token' and 'access_token' fields".into(),
            })?;

        let token = match token_resp.expires_in {
            Some(secs) if secs > 0 => Token::with_ttl(token_value, Duration::from_secs(secs)),
            _ => Token::new(token_value),
        };

        Ok(token)
    }
}

/// Parsed `WWW-Authenticate: Bearer realm="...",service="..."` header.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WwwAuthenticate {
    /// The token endpoint URL.
    realm: String,
    /// The service name (optional).
    service: Option<String>,
}

impl WwwAuthenticate {
    /// Parse a `WWW-Authenticate` header value.
    ///
    /// Only `Bearer` challenges are supported. Returns an error string on failure.
    fn parse(header: &str) -> Result<Self, String> {
        let header = header.trim();

        // Must start with "Bearer " (case-insensitive).
        let scheme_len = BEARER_SCHEME.len();
        let prefix_len = scheme_len + 1; // "bearer" + space
        if header.len() < prefix_len
            || !header[..scheme_len].eq_ignore_ascii_case(BEARER_SCHEME)
            || header.as_bytes()[scheme_len] != b' '
        {
            return Err(format!(
                "unsupported WWW-Authenticate scheme (expected Bearer): {header}"
            ));
        }

        let params = &header[prefix_len..];
        let mut realm = None;
        let mut service = None;

        for part in split_params(params) {
            let part = part.trim();
            if let Some((key, value)) = part.split_once('=') {
                let key = key.trim().to_ascii_lowercase();
                let value = value.trim().trim_matches('"');
                match key.as_str() {
                    "realm" => realm = Some(value.to_owned()),
                    "service" => service = Some(value.to_owned()),
                    _ => {} // Ignore unknown parameters.
                }
            }
        }

        let realm = realm.ok_or("WWW-Authenticate Bearer missing 'realm' parameter")?;

        Ok(Self { realm, service })
    }
}

/// Split parameter string on commas, respecting quoted strings.
fn split_params(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;

    for (i, ch) in s.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Token response from a registry auth endpoint.
#[derive(Deserialize)]
struct TokenResponse {
    /// The token (Docker Hub uses this field).
    token: Option<String>,
    /// Alternative field name (some registries use this).
    access_token: Option<String>,
    /// Token lifetime in seconds.
    expires_in: Option<u64>,
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Create test credentials with known values.
    fn test_credentials() -> Credentials {
        Credentials::Basic {
            username: "testuser".into(),
            password: "testpass".into(),
        }
    }

    /// Expected base64 encoding of "testuser:testpass".
    fn expected_basic_header() -> String {
        let encoded = BASE64.encode("testuser:testpass");
        format!("Basic {encoded}")
    }

    /// Mount a `/v2/` endpoint that returns 401 with a Bearer challenge.
    async fn mount_v2_challenge(server: &MockServer, expect: u64) {
        let realm = format!("{}/token", server.uri());
        let www_auth = format!(r#"Bearer realm="{}",service="test-registry""#, realm);
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(
                ResponseTemplate::new(401).insert_header("WWW-Authenticate", www_auth.as_str()),
            )
            .expect(expect)
            .mount(server)
            .await;
    }

    /// Mount a `/token` endpoint that returns a token and validates the Basic auth header.
    async fn mount_token_endpoint(server: &MockServer, token_value: &str, expect: u64) {
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(wiremock::matchers::header(
                "Authorization",
                expected_basic_header().as_str(),
            ))
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

        // Also verify query params by adding a more specific mock
        // (the mount_token_endpoint already matched, but let's verify the params
        // are present by checking the received requests after)
        let auth =
            BasicAuth::with_base_url(server.uri(), reqwest::Client::new(), test_credentials());
        let scopes = vec![Scope::pull("library/nginx")];

        let token = auth.get_token(&scopes).await.unwrap();
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
        // expect(1) — the /v2/ and /token endpoints should only be hit once.
        mount_v2_challenge(&server, 1).await;
        mount_token_endpoint(&server, "cached-tok", 1).await;

        let auth =
            BasicAuth::with_base_url(server.uri(), reqwest::Client::new(), test_credentials());
        let scopes = vec![Scope::pull("library/nginx")];

        let token1 = auth.get_token(&scopes).await.unwrap();
        let token2 = auth.get_token(&scopes).await.unwrap();

        assert_eq!(token1.value(), "cached-tok");
        assert_eq!(token2.value(), "cached-tok");
        // wiremock expect(1) enforces the endpoint was called exactly once.
    }

    #[tokio::test]
    async fn basic_auth_invalidate_clears_cache() {
        let server = MockServer::start().await;
        // expect(2) — one before invalidate, one after.
        mount_v2_challenge(&server, 2).await;
        mount_token_endpoint(&server, "fresh-tok", 2).await;

        let auth =
            BasicAuth::with_base_url(server.uri(), reqwest::Client::new(), test_credentials());
        let scopes = vec![Scope::pull("library/nginx")];

        let token1 = auth.get_token(&scopes).await.unwrap();
        assert_eq!(token1.value(), "fresh-tok");

        auth.invalidate().await;

        let token2 = auth.get_token(&scopes).await.unwrap();
        assert_eq!(token2.value(), "fresh-tok");
        // wiremock expect(2) enforces both endpoints were called exactly twice.
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
            BasicAuth::with_base_url(server.uri(), reqwest::Client::new(), test_credentials());
        let scopes = vec![Scope::pull("library/nginx")];

        let token = auth.get_token(&scopes).await.unwrap();
        assert_eq!(token.value(), "");
    }

    #[tokio::test]
    async fn basic_auth_token_endpoint_error() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 1).await;

        // Token endpoint returns 403.
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .expect(1)
            .mount(&server)
            .await;

        let auth =
            BasicAuth::with_base_url(server.uri(), reqwest::Client::new(), test_credentials());
        let scopes = vec![Scope::pull("library/nginx")];

        let err = auth.get_token(&scopes).await.unwrap_err();
        // reqwest translates non-success status from error_for_status() into an Http error.
        assert!(
            matches!(err, Error::Http(_)),
            "expected Http error, got: {err:?}"
        );
    }

    #[test]
    fn basic_auth_name() {
        let auth = BasicAuth::new(
            "registry.example.com",
            reqwest::Client::new(),
            test_credentials(),
        );
        assert_eq!(auth.name(), "basic");
    }

    #[tokio::test]
    async fn basic_auth_missing_www_authenticate_header() {
        let server = MockServer::start().await;

        // /v2/ returns 401 without WWW-Authenticate header.
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let creds = Credentials::Basic {
            username: "u".into(),
            password: "p".into(),
        };
        let auth = BasicAuth::with_base_url(server.uri(), reqwest::Client::new(), creds);
        let result = auth.get_token(&[Scope::pull("repo")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("WWW-Authenticate"),
            "error should mention missing header, got: {err}"
        );
    }

    #[test]
    fn basic_auth_debug_redacts_credentials() {
        let auth = BasicAuth::new(
            "registry.example.com",
            reqwest::Client::new(),
            test_credentials(),
        );
        let debug_output = format!("{auth:?}");

        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output should contain [REDACTED]: {debug_output}"
        );
        assert!(
            !debug_output.contains("testuser"),
            "Debug output should not contain username: {debug_output}"
        );
        assert!(
            !debug_output.contains("testpass"),
            "Debug output should not contain password: {debug_output}"
        );
    }
}
