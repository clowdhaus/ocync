use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;

use super::{AuthProvider, Scope, Token};
use crate::error::DistributionError;

/// Anonymous auth provider that performs the Docker token-exchange flow.
///
/// When a registry responds with `401 Unauthorized` and a `WWW-Authenticate: Bearer ...`
/// header, this provider extracts the realm/service and exchanges them for an anonymous
/// token. Tokens are cached until they expire or need refreshing.
pub struct AnonymousAuth {
    /// The registry host (e.g. `registry-1.docker.io`).
    registry: String,
    /// HTTP client for token requests.
    http: reqwest::Client,
    /// Cached token (shared across concurrent requests).
    cached_token: RwLock<Option<Token>>,
}

impl AnonymousAuth {
    pub fn new(registry: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            registry: registry.into(),
            http,
            cached_token: RwLock::new(None),
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
    ) -> Pin<Box<dyn Future<Output = Result<Token, DistributionError>> + Send + '_>> {
        let scopes = scopes.to_vec();
        Box::pin(async move { self.get_token_inner(&scopes).await })
    }
}

impl AnonymousAuth {
    async fn get_token_inner(&self, scopes: &[Scope]) -> Result<Token, DistributionError> {
        // Check cached token first.
        {
            let cached = self.cached_token.read().await;
            if let Some(ref token) = *cached {
                if !token.should_refresh() {
                    return Ok(token.clone());
                }
            }
        }

        // Need a fresh token — perform the exchange.
        let token = self.exchange_token(scopes).await?;

        // Cache it.
        {
            let mut cached = self.cached_token.write().await;
            *cached = Some(token.clone());
        }

        Ok(token)
    }

    /// Ping the registry's `/v2/` endpoint, parse the WWW-Authenticate header,
    /// then exchange for a token.
    async fn exchange_token(&self, scopes: &[Scope]) -> Result<Token, DistributionError> {
        let v2_url = format!("https://{}/v2/", self.registry);
        let resp = self.http.get(&v2_url).send().await?;

        if resp.status().is_success() {
            // No auth required — return a dummy token.
            return Ok(Token::new(""));
        }

        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| DistributionError::AuthFailed {
                registry: self.registry.clone(),
                reason: "401 response missing WWW-Authenticate header".into(),
            })?;

        let challenge =
            WwwAuthenticate::parse(www_auth).map_err(|reason| DistributionError::AuthFailed {
                registry: self.registry.clone(),
                reason,
            })?;

        // Build token request URL.
        let mut url =
            reqwest::Url::parse(&challenge.realm).map_err(|e| DistributionError::AuthFailed {
                registry: self.registry.clone(),
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
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;

        let token_value = token_resp
            .token
            .or(token_resp.access_token)
            .ok_or_else(|| DistributionError::AuthFailed {
                registry: self.registry.clone(),
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
pub struct WwwAuthenticate {
    /// The token endpoint URL.
    pub realm: String,
    /// The service name (optional).
    pub service: Option<String>,
}

impl WwwAuthenticate {
    /// Parse a `WWW-Authenticate` header value.
    ///
    /// Only `Bearer` challenges are supported. Returns an error string on failure.
    pub fn parse(header: &str) -> Result<Self, String> {
        let header = header.trim();

        // Must start with "Bearer " (case-insensitive).
        if !header[..6].eq_ignore_ascii_case("bearer") || header.as_bytes().get(6) != Some(&b' ') {
            return Err(format!(
                "unsupported WWW-Authenticate scheme (expected Bearer): {header}"
            ));
        }

        let params = &header[7..];
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
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    /// The token (Docker Hub uses this field).
    pub token: Option<String>,
    /// Alternative field name (some registries use this).
    pub access_token: Option<String>,
    /// Token lifetime in seconds.
    pub expires_in: Option<u64>,
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
    fn token_response_with_token_field() {
        let json = r#"{"token": "abc123", "expires_in": 300}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.token.as_deref(), Some("abc123"));
        assert!(resp.access_token.is_none());
        assert_eq!(resp.expires_in, Some(300));
    }

    #[test]
    fn token_response_with_access_token_field() {
        let json = r#"{"access_token": "xyz789"}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert!(resp.token.is_none());
        assert_eq!(resp.access_token.as_deref(), Some("xyz789"));
        assert!(resp.expires_in.is_none());
    }

    #[test]
    fn token_response_with_both_fields() {
        let json = r#"{"token": "primary", "access_token": "fallback", "expires_in": 600}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.token.as_deref(), Some("primary"));
        assert_eq!(resp.access_token.as_deref(), Some("fallback"));
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
}
