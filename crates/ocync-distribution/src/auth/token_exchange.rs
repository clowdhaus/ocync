//! Shared Docker v2 token-exchange flow.
//!
//! Implements the challenge-response protocol used by Docker-compatible registries:
//! ping `/v2/` → parse `WWW-Authenticate: Bearer` challenge → request token with
//! optional credentials. Used by both [`super::anonymous::AnonymousAuth`] and
//! [`super::basic::BasicAuth`].

use std::fmt;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::header::WWW_AUTHENTICATE;
use serde::Deserialize;

use super::{Credentials, Scope, Token};
use crate::error::Error;

/// The `Bearer` auth scheme prefix used in `WWW-Authenticate` challenges.
const BEARER_SCHEME: &str = "bearer";

/// Perform the Docker v2 token-exchange flow.
///
/// Pings the registry's `/v2/` endpoint, parses the `WWW-Authenticate: Bearer`
/// challenge, then requests a token from the realm URL. When `credentials` is
/// `Some`, an `Authorization: Basic` header is included on the token request.
///
/// If `/v2/` returns 200 (no auth required), returns an empty token.
pub(crate) async fn exchange_token(
    http: &reqwest::Client,
    base_url: &str,
    scopes: &[Scope],
    credentials: Option<&Credentials>,
) -> Result<Token, Error> {
    let v2_url = format!("{base_url}/v2/");
    let resp = http.get(&v2_url).send().await?;

    if resp.status().is_success() {
        // No auth required — return a dummy token.
        return Ok(Token::new(""));
    }

    let www_auth = resp
        .headers()
        .get(WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::AuthFailed {
            registry: base_url.to_owned(),
            reason: "401 response missing WWW-Authenticate header".into(),
        })?;

    let challenge = WwwAuthenticate::parse(www_auth).map_err(|reason| Error::AuthFailed {
        registry: base_url.to_owned(),
        reason,
    })?;

    // Build token request URL.
    let mut url = reqwest::Url::parse(&challenge.realm).map_err(|e| Error::AuthFailed {
        registry: base_url.to_owned(),
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

    let mut request = http.get(url);
    if let Some(creds) = credentials {
        request = request.header("Authorization", basic_header_value(creds));
    }

    let token_resp = request
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;

    let token_value = token_resp
        .token
        .or(token_resp.access_token)
        .ok_or_else(|| Error::AuthFailed {
            registry: base_url.to_owned(),
            reason: "token response missing both 'token' and 'access_token' fields".into(),
        })?;

    let token = match token_resp.expires_in {
        Some(secs) if secs > 0 => Token::with_ttl(token_value, Duration::from_secs(secs)),
        _ => Token::new(token_value),
    };

    Ok(token)
}

/// Build the `Authorization: Basic ...` header value from credentials.
fn basic_header_value(credentials: &Credentials) -> String {
    let Credentials::Basic { username, password } = credentials;
    let encoded = BASE64.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
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

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("token", &"[REDACTED]")
            .field("access_token", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_www_authenticate_bearer() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
        let parsed = WwwAuthenticate::parse(header).unwrap();
        assert_eq!(parsed.realm, "https://auth.docker.io/token");
        assert_eq!(parsed.service.as_deref(), Some("registry.docker.io"));
    }

    #[test]
    fn parse_www_authenticate_no_service() {
        let header = r#"Bearer realm="https://ghcr.io/token""#;
        let parsed = WwwAuthenticate::parse(header).unwrap();
        assert_eq!(parsed.realm, "https://ghcr.io/token");
        assert!(parsed.service.is_none());
    }

    #[test]
    fn parse_www_authenticate_case_insensitive() {
        let header = r#"BEARER realm="https://auth.example.com/token",service="example""#;
        let parsed = WwwAuthenticate::parse(header).unwrap();
        assert_eq!(parsed.realm, "https://auth.example.com/token");
        assert_eq!(parsed.service.as_deref(), Some("example"));
    }

    #[test]
    fn parse_www_authenticate_basic_is_error() {
        let header = r#"Basic realm="Registry""#;
        let result = WwwAuthenticate::parse(header);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported"));
    }

    #[test]
    fn parse_www_authenticate_missing_realm() {
        let header = r#"Bearer service="registry.docker.io""#;
        let result = WwwAuthenticate::parse(header);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("realm"));
    }

    #[test]
    fn split_params_basic() {
        let parts = split_params(r#"realm="https://example.com",service="test""#);
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn split_params_with_comma_in_quotes() {
        let parts = split_params(r#"realm="https://example.com/a,b",service="test""#);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("a,b"));
    }

    #[test]
    fn parse_www_authenticate_empty_string() {
        let result = WwwAuthenticate::parse("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_www_authenticate_short_strings() {
        for input in ["B", "Be", "Bea", "Bear", "Beare", "Bearer"] {
            let result = WwwAuthenticate::parse(input);
            assert!(result.is_err(), "expected Err for input: {input:?}");
        }
    }

    #[test]
    fn basic_header_value_encodes_correctly() {
        let creds = Credentials::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        let header = basic_header_value(&creds);
        assert!(header.starts_with("Basic "));
        // "user:pass" base64 = "dXNlcjpwYXNz"
        assert_eq!(header, "Basic dXNlcjpwYXNz");
    }

    #[tokio::test]
    async fn exchange_token_anonymous() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="test""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "anon-tok", "expires_in": 300})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap();
        assert_eq!(token.value(), "anon-tok");
    }

    #[tokio::test]
    async fn exchange_token_with_credentials() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Basic dXNlcjpwYXNz",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "basic-tok"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let creds = Credentials::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        let http = reqwest::Client::new();
        let token = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], Some(&creds))
            .await
            .unwrap();
        assert_eq!(token.value(), "basic-tok");
    }

    #[tokio::test]
    async fn exchange_token_no_auth_required() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap();
        assert_eq!(token.value(), "");
    }

    #[tokio::test]
    async fn exchange_token_missing_www_authenticate() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let err = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("WWW-Authenticate"));
    }

    #[tokio::test]
    async fn exchange_token_endpoint_error() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let err = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Http(_)));
    }

    #[tokio::test]
    async fn exchange_token_access_token_fallback() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        // Response uses access_token field instead of token.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "fallback-tok"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap();
        assert_eq!(token.value(), "fallback-tok");
    }

    #[tokio::test]
    async fn exchange_token_no_expiry_produces_permanent_token() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        // No expires_in field — token should be permanent.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "perm-tok"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap();
        assert_eq!(token.value(), "perm-tok");
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    #[tokio::test]
    async fn exchange_token_zero_expiry_treated_as_permanent() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "zero-tok", "expires_in": 0})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_token(&http, &mock.uri(), &[Scope::pull("repo")], None)
            .await
            .unwrap();
        assert_eq!(token.value(), "zero-tok");
        assert!(!token.is_expired());
    }

    #[tokio::test]
    async fn exchange_token_multi_scope_query_params() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="svc""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "multi-tok"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let scopes = [Scope::pull("repo-a"), Scope::pull_push("repo-b")];
        let token = exchange_token(&http, &mock.uri(), &scopes, None)
            .await
            .unwrap();
        assert_eq!(token.value(), "multi-tok");

        // Verify both scope params are present in the token request URL.
        let requests = mock.received_requests().await.unwrap();
        let token_req = requests
            .iter()
            .find(|r| r.url.path() == "/token")
            .expect("token request not found");
        let scope_params: Vec<String> = token_req
            .url
            .query_pairs()
            .filter(|(k, _)| k == "scope")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(scope_params.len(), 2, "expected 2 scope params");
        assert!(scope_params.contains(&"repository:repo-a:pull".to_string()));
        assert!(scope_params.contains(&"repository:repo-b:pull,push".to_string()));
    }
}
