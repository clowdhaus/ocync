//! Shared Docker v2 token-exchange flow.
//!
//! Implements the challenge-response protocol used by Docker-compatible registries:
//! ping `/v2/` → parse `WWW-Authenticate: Bearer` challenge → request token with
//! optional credentials. Used by both [`super::anonymous::AnonymousAuth`] and
//! [`super::basic::BasicAuth`].

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::StatusCode;
use http::header::WWW_AUTHENTICATE;
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Host;

use super::{Credentials, Scope, Token};
use crate::error::Error;

/// Cached `WWW-Authenticate` challenge for a registry.
///
/// Stores the parsed realm + service from the first `/v2/` ping so
/// subsequent token exchanges skip the ping entirely.
pub(crate) struct ChallengeCache(Mutex<Option<WwwAuthenticate>>);

impl ChallengeCache {
    /// Create an empty challenge cache.
    pub(crate) fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Read the cached challenge (clones to release the lock before await).
    pub(crate) async fn get(&self) -> Option<WwwAuthenticate> {
        self.0.lock().await.clone()
    }

    /// Store a challenge if one was returned.
    pub(crate) async fn set(&self, challenge: Option<WwwAuthenticate>) {
        if let Some(ch) = challenge {
            *self.0.lock().await = Some(ch);
        }
    }

    /// Clear the cached challenge.
    pub(crate) async fn clear(&self) {
        *self.0.lock().await = None;
    }
}

/// The `Bearer` auth scheme prefix used in `WWW-Authenticate` challenges.
const BEARER_SCHEME: &str = "bearer";

/// Build a no-redirect HTTP client for token-bearing requests.
///
/// Uses `redirect::Policy::none()` to prevent open-redirect SSRF: a
/// 307 redirect from a malicious endpoint would forward credentials
/// (realm auth headers, ACR exchange form bodies) to the redirect
/// target. Also used by [`super::acr::AcrAuth`] for exchange POSTs.
pub(crate) fn no_redirect_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("ocync/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("no-redirect HTTP client builder should not fail")
}

/// Shorthand for realm validation errors.
fn realm_err(registry: &str, reason: String) -> Error {
    Error::AuthFailed {
        registry: registry.to_owned(),
        reason,
    }
}

/// Validate an IPv4 realm address against the cloud-metadata denylist.
fn validate_ipv4(
    ip: &Ipv4Addr,
    registry_is_loopback: bool,
    realm: &reqwest::Url,
    registry_str: &str,
) -> Result<(), Error> {
    // Link-local (169.254.0.0/16) -- AWS/GCP/Azure IMDS.
    if ip.is_link_local() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to link-local address {ip}"),
        ));
    }

    // Alibaba Cloud metadata (100.100.100.200).
    if *ip == Ipv4Addr::new(100, 100, 100, 200) {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to Alibaba Cloud metadata address {ip}"),
        ));
    }

    // Unspecified (0.0.0.0).
    if ip.is_unspecified() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to unspecified address {ip}"),
        ));
    }

    // Loopback -- allowed only when the registry itself is loopback.
    if ip.is_loopback() && !registry_is_loopback {
        return Err(realm_err(
            registry_str,
            format!(
                "realm URL '{realm}' points to loopback address {ip} but registry is not loopback"
            ),
        ));
    }

    Ok(())
}

/// Extract an embedded IPv4 address from IPv4-translated (RFC 6145) or
/// NAT64 well-known prefix (RFC 6052) IPv6 addresses.
///
/// `Ipv6Addr::to_ipv4_mapped()` only handles `::ffff:x.y.z.w`. This
/// function covers the two remaining encodings that embed an IPv4 address
/// in the last 32 bits.
fn extract_embedded_ipv4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    let segs = ip.segments();
    // ::ffff:0:x.y.z.w (IPv4-translated, RFC 6145)
    let is_translated = segs[..6] == [0, 0, 0, 0, 0xffff, 0];
    // 64:ff9b::x.y.z.w (NAT64 well-known prefix, RFC 6052)
    let is_nat64 = segs[..6] == [0x0064, 0xff9b, 0, 0, 0, 0];
    (is_translated || is_nat64).then(|| {
        Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            segs[6] as u8,
            (segs[7] >> 8) as u8,
            segs[7] as u8,
        )
    })
}

/// Validate an IPv6 realm address against the cloud-metadata denylist.
fn validate_ipv6(
    ip: &Ipv6Addr,
    registry_is_loopback: bool,
    realm: &reqwest::Url,
    registry_str: &str,
) -> Result<(), Error> {
    // Normalize IPv4-mapped IPv6 (::ffff:x.y.z.w) and delegate to IPv4 checks.
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return validate_ipv4(&mapped, registry_is_loopback, realm, registry_str);
    }

    // IPv4-translated (RFC 6145) and NAT64 (RFC 6052) also embed IPv4.
    if let Some(v4) = extract_embedded_ipv4(ip) {
        return validate_ipv4(&v4, registry_is_loopback, realm, registry_str);
    }

    // Unspecified (::).
    if ip.is_unspecified() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to unspecified address {ip}"),
        ));
    }

    // Link-local IPv6 (fe80::/10).
    // std does not have is_unicast_link_local() on stable; check manually.
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to link-local address {ip}"),
        ));
    }

    // AWS IPv6 IMDS (fd00:ec2::254).
    if *ip == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to AWS IPv6 IMDS address {ip}"),
        ));
    }

    // Loopback (::1) -- allowed only when the registry itself is loopback.
    if ip.is_loopback() && !registry_is_loopback {
        return Err(realm_err(
            registry_str,
            format!(
                "realm URL '{realm}' points to loopback address {ip} but registry is not loopback"
            ),
        ));
    }

    Ok(())
}

/// Validate a realm URL before sending credentials to it.
///
/// **Layer 1 -- structural:** must have a host, no userinfo, scheme must be
/// `https` (or `http` only when the registry itself is `http`).
///
/// **Layer 2 -- IP denylist:** blocks link-local (169.254.0.0/16), Alibaba
/// metadata (100.100.100.200), unspecified (0.0.0.0 / [::]), link-local
/// IPv6 (`fe80::/10`), AWS IPv6 IMDS (`fd00:ec2::254`), `localhost`, and
/// loopback unless the registry is also loopback. IPv4-mapped,
/// IPv4-translated (RFC 6145), and NAT64 (RFC 6052) addresses are
/// normalized before checking. Other domain names pass Layer 2 (resolved
/// later by DNS).
///
/// **Layer 3 -- no-redirect client:** enforced in [`exchange`] via
/// [`no_redirect_http_client`], not in this function.
///
/// **Layer 4 -- domain binding:** the realm host must be the registry host,
/// or share a parent domain with it (e.g. `auth.docker.io` and
/// `registry-1.docker.io` both have parent `docker.io`). Hosts with 2 or
/// fewer labels require an exact match. IP-based realms skip this check.
fn validate_realm_url(realm: &reqwest::Url, registry_base: &reqwest::Url) -> Result<(), Error> {
    let registry_str = registry_base.as_str().trim_end_matches('/');

    // Layer 1: structural checks.

    // Must have a host.
    if realm.host().is_none() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' has no host"),
        ));
    }

    // No userinfo (username/password in URL).
    if !realm.username().is_empty() || realm.password().is_some() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' contains userinfo"),
        ));
    }

    // Scheme check: https always allowed; http only when registry is also http.
    match realm.scheme() {
        "https" => {}
        "http" => {
            if registry_base.scheme() != "http" {
                return Err(realm_err(
                    registry_str,
                    format!(
                        "realm URL '{realm}' uses http but registry uses {scheme}",
                        scheme = registry_base.scheme()
                    ),
                ));
            }
        }
        _ => {
            return Err(realm_err(
                registry_str,
                format!(
                    "realm URL '{realm}' uses unsupported scheme '{scheme}'",
                    scheme = realm.scheme()
                ),
            ));
        }
    }

    // Layer 2: IP denylist (+ localhost hostname).
    let registry_is_loopback = match registry_base.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip
            .to_ipv4_mapped()
            .map_or(ip.is_loopback(), |m| m.is_loopback()),
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    };

    match realm.host() {
        Some(Host::Ipv4(ip)) => validate_ipv4(&ip, registry_is_loopback, realm, registry_str)?,
        Some(Host::Ipv6(ip)) => validate_ipv6(&ip, registry_is_loopback, realm, registry_str)?,
        Some(Host::Domain(d)) => {
            // `localhost` resolves to loopback -- apply the same conditional
            // block as IP-based loopback addresses.
            if d.eq_ignore_ascii_case("localhost") && !registry_is_loopback {
                return Err(realm_err(
                    registry_str,
                    format!("realm URL '{realm}' points to localhost but registry is not loopback"),
                ));
            }
        }
        None => unreachable!("already checked above"),
    }

    // Layer 3 (no-redirect client) is enforced in exchange() via
    // no_redirect_http_client() -- see redirect::Policy::none() and the explicit
    // redirect status check after send().

    // Layer 4: domain binding -- the realm host must be related to the
    // registry host. Prevents a compromised registry from exfiltrating
    // credentials to an unrelated domain.
    validate_domain_binding(realm, registry_base, registry_str)?;

    Ok(())
}

/// Verify the realm host is related to the registry host.
///
/// Rules:
/// - IP-based realms skip this check (Layer 2 already validated them).
/// - If either host has 2 or fewer labels (e.g. `ghcr.io`), require exact
///   host match.
/// - Otherwise, strip the leftmost label from each and compare the
///   remainders. `auth.docker.io` -> `docker.io`, `registry-1.docker.io`
///   -> `docker.io` -- match.
///
/// This prevents a compromised registry from directing credentials to an
/// unrelated domain (e.g. `realm="https://attacker.com/steal"`).
fn validate_domain_binding(
    realm: &reqwest::Url,
    registry_base: &reqwest::Url,
    registry_str: &str,
) -> Result<(), Error> {
    let realm_host = match realm.host_str() {
        Some(h) => h,
        // IP addresses and missing hosts are handled by earlier layers.
        None => return Ok(()),
    };
    let registry_host = match registry_base.host_str() {
        Some(h) => h,
        None => return Ok(()),
    };

    // IP addresses skip domain binding (already validated by Layer 2).
    if realm.host().is_some_and(|h| !matches!(h, Host::Domain(_)))
        || registry_base
            .host()
            .is_some_and(|h| !matches!(h, Host::Domain(_)))
    {
        return Ok(());
    }

    // Exact match is always allowed.
    if realm_host.eq_ignore_ascii_case(registry_host) {
        return Ok(());
    }

    // Split into labels.
    let realm_labels: Vec<&str> = realm_host.split('.').collect();
    let registry_labels: Vec<&str> = registry_host.split('.').collect();

    // If either host has 2 or fewer labels, require exact match (already
    // failed above). Stripping a label from `ghcr.io` gives `io` which is
    // too broad -- any `.io` domain would pass.
    if realm_labels.len() <= 2 || registry_labels.len() <= 2 {
        return Err(realm_err(
            registry_str,
            format!("realm host '{realm_host}' does not match registry host '{registry_host}'"),
        ));
    }

    // Strip the leftmost label and compare remainders (case-insensitive).
    let realm_parent = &realm_labels[1..];
    let registry_parent = &registry_labels[1..];

    if realm_parent.len() == registry_parent.len()
        && realm_parent
            .iter()
            .zip(registry_parent.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        Ok(())
    } else {
        Err(realm_err(
            registry_str,
            format!("realm host '{realm_host}' does not match registry host '{registry_host}'"),
        ))
    }
}

/// Perform the Docker v2 token-exchange flow.
///
/// Pings the registry's `/v2/` endpoint, parses the `WWW-Authenticate: Bearer`
/// challenge, then requests a token from the realm URL. When `credentials` is
/// `Some`, an `Authorization: Basic` header is included on the token request.
///
/// When `cached_challenge` is `Some`, the `/v2/` ping is skipped and the
/// cached realm/service are used directly. This eliminates redundant pings
/// when the challenge is stable per-registry.
///
/// Returns the token and the parsed challenge (if one was obtained). The
/// challenge is `None` when the registry requires no auth (200 on `/v2/`).
pub(crate) async fn exchange(
    http: &reqwest::Client,
    base_url: &str,
    scopes: &[Scope],
    credentials: Option<&Credentials>,
    cached_challenge: Option<&WwwAuthenticate>,
) -> Result<(Token, Option<WwwAuthenticate>), Error> {
    let registry_url = reqwest::Url::parse(base_url).map_err(|e| Error::AuthFailed {
        registry: base_url.to_owned(),
        reason: format!("invalid registry base URL: {e}"),
    })?;

    let challenge = if let Some(cached) = cached_challenge {
        tracing::debug!(base_url, "using cached WWW-Authenticate challenge");
        cached.clone()
    } else {
        let v2_url = format!("{base_url}/v2/");
        let resp = http.get(&v2_url).send().await?;

        if resp.status().is_success() {
            tracing::debug!(base_url, "registry requires no auth, returning empty token");
            return Ok((Token::new(""), None));
        }

        // Only 401 carries a WWW-Authenticate challenge. Any other non-success
        // status (403, 500, etc.) is a hard error -- don't fall through to header
        // parsing with a misleading "missing WWW-Authenticate" message.
        if resp.status() != StatusCode::UNAUTHORIZED {
            let status = resp.status();
            tracing::warn!(base_url, %status, "registry ping returned unexpected status");
            return Err(Error::AuthFailed {
                registry: base_url.to_owned(),
                reason: format!("registry ping returned {status} (expected 401 challenge)"),
            });
        }

        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::AuthFailed {
                registry: base_url.to_owned(),
                reason: "401 response missing WWW-Authenticate header".into(),
            })?;

        WwwAuthenticate::parse(www_auth).map_err(|reason| Error::AuthFailed {
            registry: base_url.to_owned(),
            reason,
        })?
    };

    // Build token request URL.
    let mut url = reqwest::Url::parse(&challenge.realm).map_err(|e| Error::AuthFailed {
        registry: base_url.to_owned(),
        reason: format!("invalid realm URL: {e}"),
    })?;

    validate_realm_url(&url, &registry_url)?;

    {
        let mut query = url.query_pairs_mut();
        if let Some(ref service) = challenge.service {
            query.append_pair("service", service);
        }
        for scope in scopes {
            query.append_pair("scope", &scope.to_string());
        }
    }

    let mut request = no_redirect_http_client().get(url);
    if let Some(creds) = credentials {
        request = request.header("Authorization", basic_header_value(creds));
    }

    let resp = request.send().await?;

    if resp.status().is_redirection() {
        let status = resp.status();
        tracing::warn!(base_url, %status, "realm URL returned redirect");
        return Err(Error::AuthFailed {
            registry: base_url.to_owned(),
            reason: format!(
                "realm URL returned redirect ({status}), which is not allowed for token endpoints"
            ),
        });
    }

    if resp.status().is_client_error() || resp.status().is_server_error() {
        let status = resp.status();
        tracing::warn!(base_url, %status, "token endpoint returned error");
        return Err(Error::AuthFailed {
            registry: base_url.to_owned(),
            reason: format!("token endpoint returned {status}"),
        });
    }
    let token_resp = resp.json::<TokenResponse>().await?;

    let token_value = token_resp
        .token
        .or(token_resp.access_token)
        .ok_or_else(|| {
            tracing::warn!(
                base_url,
                "token response missing both 'token' and 'access_token' fields"
            );
            Error::AuthFailed {
                registry: base_url.to_owned(),
                reason: "token response missing both 'token' and 'access_token' fields".into(),
            }
        })?;

    let token = match token_resp.expires_in {
        Some(secs) if secs > 0 => {
            tracing::debug!(base_url, expires_in_secs = secs, "token exchange succeeded");
            Token::with_ttl(token_value, Duration::from_secs(secs))
        }
        _ => {
            tracing::debug!(base_url, "token exchange succeeded (no expiry)");
            Token::new(token_value)
        }
    };

    Ok((token, Some(challenge)))
}

/// Build the `Authorization: Basic ...` header value from credentials.
fn basic_header_value(credentials: &Credentials) -> String {
    let Credentials::Basic { username, password } = credentials;
    let encoded = BASE64.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
}

/// Parsed `WWW-Authenticate: Bearer realm="...",service="..."` header.
///
/// Cached by auth providers to skip redundant `/v2/` pings -- the challenge
/// (realm + service) is stable per-registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WwwAuthenticate {
    /// The token endpoint URL.
    pub(crate) realm: String,
    /// The service name (optional).
    pub(crate) service: Option<String>,
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
///
/// Handles backslash-escaped quotes (`\"`) inside quoted values so that
/// a realm URL containing an escaped quote does not prematurely end the
/// quoted region.
fn split_params(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut prev_backslash = false;

    for (i, ch) in s.char_indices() {
        match ch {
            '"' if !prev_backslash => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        prev_backslash = ch == '\\' && !prev_backslash;
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

    /// Assert that realm validation rejects with an error containing `needle`.
    #[track_caller]
    fn assert_realm_rejected(result: Result<(), Error>, needle: &str) {
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(needle), "expected '{needle}' in error: {msg}");
    }

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
    fn split_params_with_escaped_quote() {
        // A realm URL containing a backslash-escaped quote should not
        // terminate the quoted region prematurely.
        let input = r#"realm="https://example.com/path?q=\"val\"",service="test""#;
        let parts = split_params(input);
        assert_eq!(parts.len(), 2, "got parts: {parts:?}");
        assert!(
            parts[0].contains(r#"\"val\""#),
            "escaped quotes missing: {}",
            parts[0]
        );
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
    async fn exchange_anonymous() {
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

        let http = crate::test_http_client();
        let (token, challenge) = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap();
        assert_eq!(token.value(), "anon-tok");
        assert!(challenge.is_some(), "challenge should be returned");
    }

    #[tokio::test]
    async fn exchange_with_credentials() {
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
        let http = crate::test_http_client();
        let (token, _) = exchange(
            &http,
            &mock.uri(),
            &[Scope::pull("repo")],
            Some(&creds),
            None,
        )
        .await
        .unwrap();
        assert_eq!(token.value(), "basic-tok");
    }

    #[tokio::test]
    async fn exchange_no_auth_required() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let (token, challenge) = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap();
        assert_eq!(token.value(), "");
        assert!(challenge.is_none(), "no challenge for no-auth registry");
    }

    #[tokio::test]
    async fn exchange_missing_www_authenticate() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("WWW-Authenticate"));
    }

    #[tokio::test]
    async fn exchange_endpoint_error() {
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

        let http = crate::test_http_client();
        let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::AuthFailed { .. }),
            "expected AuthFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("403"), "expected 403 in error: {msg}");
    }

    #[tokio::test]
    async fn exchange_access_token_fallback() {
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

        let http = crate::test_http_client();
        let (token, _) = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap();
        assert_eq!(token.value(), "fallback-tok");
    }

    #[tokio::test]
    async fn exchange_no_expiry_produces_permanent_token() {
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

        // No expires_in field -- token should be permanent.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "perm-tok"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let (token, _) = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap();
        assert_eq!(token.value(), "perm-tok");
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    #[tokio::test]
    async fn exchange_zero_expiry_treated_as_permanent() {
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

        let http = crate::test_http_client();
        let (token, _) = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap();
        assert_eq!(token.value(), "zero-tok");
        assert!(!token.is_expired());
    }

    #[tokio::test]
    async fn exchange_multi_scope_query_params() {
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

        let http = crate::test_http_client();
        let scopes = [Scope::pull("repo-a"), Scope::pull_push("repo-b")];
        let (token, _) = exchange(&http, &mock.uri(), &scopes, None, None)
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

    #[tokio::test]
    async fn exchange_non_401_returns_clear_error() {
        let mock = wiremock::MockServer::start().await;

        // Return 403 instead of 401 -- should NOT produce a misleading
        // "missing WWW-Authenticate" message.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .expect(1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("403") && msg.contains("expected 401 challenge"),
            "unexpected error message: {msg}"
        );
        // Must NOT mention WWW-Authenticate.
        assert!(
            !msg.contains("WWW-Authenticate"),
            "should not mention WWW-Authenticate for non-401: {msg}"
        );
    }

    #[tokio::test]
    async fn exchange_500_returns_clear_error() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("500") && msg.contains("expected 401 challenge"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn challenge_cache_reuse() {
        let mock = wiremock::MockServer::start().await;

        // /v2/ should only be hit once -- the second exchange reuses the cached challenge.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="test""#, mock.uri()),
            ))
            .expect(1)
            .mount(&mock)
            .await;

        // Token endpoint is called twice (different scopes).
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "reused", "expires_in": 300})),
            )
            .expect(2)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();

        // First call: no cached challenge, /v2/ is pinged.
        let (t1, challenge) = exchange(&http, &mock.uri(), &[Scope::pull("repo-a")], None, None)
            .await
            .unwrap();
        assert_eq!(t1.value(), "reused");
        let challenge = challenge.expect("first call should return challenge");

        // Second call: pass cached challenge, /v2/ is NOT pinged.
        let (t2, challenge2) = exchange(
            &http,
            &mock.uri(),
            &[Scope::pull("repo-b")],
            None,
            Some(&challenge),
        )
        .await
        .unwrap();
        assert_eq!(t2.value(), "reused");
        assert!(
            challenge2.is_some(),
            "cached path should still return challenge"
        );
        // wiremock expect(1) on /v2/ verifies it was not called a second time.
    }

    // --- Layer 1: structural validation ---

    #[test]
    fn validate_realm_rejects_http_when_registry_is_https() {
        let realm = reqwest::Url::parse("http://evil.com/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "http");
    }

    #[test]
    fn validate_realm_rejects_userinfo() {
        let realm = reqwest::Url::parse("https://user:pass@evil.com/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "userinfo");
    }

    #[test]
    fn validate_realm_rejects_non_web_scheme() {
        let realm = reqwest::Url::parse("ftp://evil.com/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "unsupported scheme");
    }

    #[test]
    fn validate_realm_rejects_no_host() {
        let realm = reqwest::Url::parse("data:text/plain,hello").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "no host");
    }

    #[test]
    fn validate_realm_allows_https_cross_domain() {
        let realm = reqwest::Url::parse("https://auth.docker.io/token").unwrap();
        let registry = reqwest::Url::parse("https://registry-1.docker.io").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_allows_http_when_registry_is_http() {
        let realm = reqwest::Url::parse("http://my-registry:5000/token").unwrap();
        let registry = reqwest::Url::parse("http://my-registry:5000").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_allows_same_host_different_port() {
        let realm = reqwest::Url::parse("https://host:8443/token").unwrap();
        let registry = reqwest::Url::parse("https://host:5000").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    // --- Layer 2: IP denylist ---

    #[test]
    fn validate_realm_rejects_link_local_ipv4() {
        let realm = reqwest::Url::parse("https://169.254.169.254/latest/meta-data").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "link-local");
    }

    #[test]
    fn validate_realm_rejects_ipv4_mapped_ipv6_metadata() {
        let realm = reqwest::Url::parse("https://[::ffff:169.254.169.254]/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "link-local");
    }

    #[test]
    fn validate_realm_rejects_aws_ipv6_imds() {
        let realm = reqwest::Url::parse("https://[fd00:ec2::254]/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "AWS IPv6 IMDS");
    }

    #[test]
    fn validate_realm_rejects_unspecified_ipv4() {
        let realm = reqwest::Url::parse("http://0.0.0.0/token").unwrap();
        let registry = reqwest::Url::parse("http://0.0.0.0").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "unspecified");
    }

    #[test]
    fn validate_realm_rejects_link_local_ipv6() {
        let realm = reqwest::Url::parse("http://[fe80::1]/token").unwrap();
        let registry = reqwest::Url::parse("http://[fe80::1]").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "link-local");
    }

    #[test]
    fn validate_realm_rejects_unspecified_ipv6() {
        let realm = reqwest::Url::parse("http://[::]/token").unwrap();
        let registry = reqwest::Url::parse("http://[::]").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "unspecified");
    }

    #[test]
    fn validate_realm_rejects_alibaba_metadata() {
        let realm = reqwest::Url::parse("https://100.100.100.200/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "Alibaba");
    }

    #[test]
    fn validate_realm_rejects_loopback_when_registry_is_not_loopback() {
        let realm = reqwest::Url::parse("https://127.0.0.1/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "loopback");
    }

    #[test]
    fn validate_realm_allows_loopback_when_registry_is_loopback() {
        let realm = reqwest::Url::parse("http://127.0.0.1:9999/token").unwrap();
        let registry = reqwest::Url::parse("http://127.0.0.1:5000").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_allows_private_ip() {
        let realm = reqwest::Url::parse("https://192.168.1.50/token").unwrap();
        let registry = reqwest::Url::parse("https://192.168.1.50").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_rejects_localhost_when_registry_is_not_loopback() {
        let realm = reqwest::Url::parse("https://localhost:9999/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "localhost");
    }

    #[test]
    fn validate_realm_allows_localhost_when_registry_is_localhost() {
        let realm = reqwest::Url::parse("http://localhost:9999/token").unwrap();
        let registry = reqwest::Url::parse("http://localhost:5000").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    // --- Layer 3: integration tests ---

    #[tokio::test]
    async fn exchange_rejects_realm_redirect() {
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
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", "http://169.254.169.254/latest/meta-data"),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::AuthFailed { .. }),
            "expected AuthFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("redirect"),
            "expected 'redirect' in error: {msg}"
        );
    }

    #[tokio::test]
    async fn exchange_validates_cached_challenge() {
        let mock = wiremock::MockServer::start().await;

        // Neither /v2/ nor /token should be called -- cached challenge skips
        // the ping, and validation rejects before the token request.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": "should-not-reach"})),
            )
            .expect(0)
            .mount(&mock)
            .await;

        let malicious_challenge = WwwAuthenticate {
            realm: "http://169.254.169.254/latest/meta-data".to_owned(),
            service: None,
        };

        let http = crate::test_http_client();
        let err = exchange(
            &http,
            &mock.uri(),
            &[Scope::pull("repo")],
            None,
            Some(&malicious_challenge),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::AuthFailed { .. }),
            "expected AuthFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("link-local"),
            "expected 'link-local' in error: {msg}"
        );
    }

    #[tokio::test]
    async fn exchange_rejects_cross_domain_realm() {
        // Use a domain-based registry with a cached challenge pointing to an
        // unrelated domain. Domain binding should reject before any HTTP
        // request is sent. (Wiremock runs on 127.0.0.1, which skips domain
        // binding by design, so we use a cached challenge with a domain URL.)
        let malicious_challenge = WwwAuthenticate {
            realm: "https://attacker.com/steal".to_owned(),
            service: Some("test".to_owned()),
        };

        let http = crate::test_http_client();
        let err = exchange(
            &http,
            "https://registry.example.com",
            &[Scope::pull("repo")],
            None,
            Some(&malicious_challenge),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::AuthFailed { .. }),
            "expected AuthFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("does not match"),
            "expected domain binding rejection: {msg}"
        );
    }

    // --- IPv6 edge cases (bypass prevention) ---

    #[test]
    fn validate_realm_rejects_ipv6_loopback_when_registry_is_not_loopback() {
        let realm = reqwest::Url::parse("https://[::1]/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "loopback");
    }

    #[test]
    fn validate_realm_rejects_ipv4_mapped_ipv6_loopback_when_registry_is_not_loopback() {
        let realm = reqwest::Url::parse("https://[::ffff:127.0.0.1]/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "loopback");
    }

    #[test]
    fn validate_realm_rejects_ipv4_translated_metadata() {
        // RFC 6145 IPv4-translated: ::ffff:0:a9fe:a9fe embeds 169.254.169.254.
        let realm = reqwest::Url::parse("https://[::ffff:0:a9fe:a9fe]/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "link-local");
    }

    #[test]
    fn validate_realm_rejects_nat64_metadata() {
        // RFC 6052 NAT64 well-known prefix: 64:ff9b::a9fe:a9fe embeds 169.254.169.254.
        let realm = reqwest::Url::parse("https://[64:ff9b::a9fe:a9fe]/token").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "link-local");
    }

    // --- Layer 4: domain binding ---

    #[test]
    fn validate_realm_allows_same_host() {
        let realm = reqwest::Url::parse("https://ghcr.io/token").unwrap();
        let registry = reqwest::Url::parse("https://ghcr.io").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_allows_sibling_subdomain() {
        // Docker Hub: registry-1.docker.io -> auth.docker.io (same parent docker.io).
        let realm = reqwest::Url::parse("https://auth.docker.io/token").unwrap();
        let registry = reqwest::Url::parse("https://registry-1.docker.io").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_rejects_unrelated_domain() {
        let realm = reqwest::Url::parse("https://attacker.com/steal").unwrap();
        let registry = reqwest::Url::parse("https://registry.example.com").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "does not match");
    }

    #[test]
    fn validate_realm_rejects_different_tld() {
        let realm = reqwest::Url::parse("https://auth.docker.com/token").unwrap();
        let registry = reqwest::Url::parse("https://registry-1.docker.io").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "does not match");
    }

    #[test]
    fn validate_realm_rejects_short_host_cross_domain() {
        // ghcr.io has only 2 labels -- require exact match.
        let realm = reqwest::Url::parse("https://evil.io/token").unwrap();
        let registry = reqwest::Url::parse("https://ghcr.io").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "does not match");
    }

    #[test]
    fn validate_realm_allows_ip_realm_skips_domain_binding() {
        // IP-based realms skip domain binding (Layer 2 already validated).
        let realm = reqwest::Url::parse("http://127.0.0.1:9999/token").unwrap();
        let registry = reqwest::Url::parse("http://127.0.0.1:5000").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_allows_domain_realm_for_ip_registry() {
        // When the registry is IP-based, domain binding is skipped since
        // Layer 2 already validated the IP. IP-based registries are
        // typically private/local and under the operator's control.
        let realm = reqwest::Url::parse("https://auth.example.com/token").unwrap();
        let registry = reqwest::Url::parse("https://192.168.1.50").unwrap();
        validate_realm_url(&realm, &registry).unwrap();
    }

    #[test]
    fn validate_realm_rejects_subdomain_of_short_host() {
        // Registry is quay.io (2 labels), realm is sub.quay.io --
        // since registry has <= 2 labels and hosts don't match exactly, reject.
        let realm = reqwest::Url::parse("https://auth.quay.io/token").unwrap();
        let registry = reqwest::Url::parse("https://quay.io").unwrap();
        assert_realm_rejected(validate_realm_url(&realm, &registry), "does not match");
    }
}
