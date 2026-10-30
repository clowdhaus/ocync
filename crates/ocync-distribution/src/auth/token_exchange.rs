//! Shared Docker v2 token-exchange flow.
//!
//! Implements the challenge-response protocol used by Docker-compatible registries:
//! ping `/v2/` → parse `WWW-Authenticate: Bearer` challenge → request token with
//! optional credentials.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::StatusCode;
use http::header::WWW_AUTHENTICATE;
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Host;

use super::{Credentials, Scope, Token};
use crate::client::no_redirect_http_client;
use crate::error::Error;
use crate::retry::send_retrying;
use crate::scheme::Scheme;

/// Cached `WWW-Authenticate` challenge for a registry.
///
/// Stores the parsed realm + service from the first `/v2/` ping so
/// subsequent token exchanges skip the ping entirely.
///
/// A challenge learned by an exchange that started from the cached one, and
/// began before the cache was last cleared, is not stored: the clearing
/// judged the challenge that exchange started from. An exchange that pinged
/// `/v2/` for its own keeps what it learned.
#[derive(Debug, Default)]
pub(crate) struct ChallengeCache(Mutex<Cached>);

/// What a [`ChallengeCache`] holds.
#[derive(Debug, Default)]
struct Cached {
    challenge: Option<WwwAuthenticate>,
    /// How many times the cache has been cleared.
    clears: u64,
}

impl ChallengeCache {
    /// Exchange `credentials` for a token for `scopes`, skipping the ping
    /// when a challenge is cached, and keep the challenge the exchange
    /// learned unless it started from the cached one and the cache was
    /// cleared while it ran.
    pub(crate) async fn exchange(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        scopes: &[Scope],
        credentials: Option<&Credentials>,
    ) -> Result<Token, Error> {
        let (started_from_cache, clears) = {
            let cached = self.0.lock().await;
            (cached.challenge.clone(), cached.clears)
        };
        let (token, learned) = exchange(
            http,
            base_url,
            scopes,
            credentials,
            started_from_cache.as_ref(),
        )
        .await?;
        if let Some(challenge) = learned {
            let mut cached = self.0.lock().await;
            if started_from_cache.is_none() || cached.clears == clears {
                cached.challenge = Some(challenge);
            }
        }
        Ok(token)
    }

    /// The cached challenge (test helper).
    #[cfg(test)]
    pub(crate) async fn get(&self) -> Option<WwwAuthenticate> {
        self.0.lock().await.challenge.clone()
    }

    /// Cache `challenge` (test helper).
    #[cfg(test)]
    pub(crate) async fn set(&self, challenge: WwwAuthenticate) {
        self.0.lock().await.challenge = Some(challenge);
    }

    /// Clear the cached challenge, and so discard what exchanges that began
    /// earlier learn.
    pub(crate) async fn clear(&self) {
        let mut cached = self.0.lock().await;
        cached.challenge = None;
        cached.clears += 1;
    }
}

/// The `Bearer` auth scheme prefix used in `WWW-Authenticate` challenges.
const BEARER_SCHEME: &str = "bearer";

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

/// Validate an IPv6 realm address against the cloud-metadata denylist.
fn validate_ipv6(
    ip: &Ipv6Addr,
    registry_is_loopback: bool,
    realm: &reqwest::Url,
    registry_str: &str,
) -> Result<(), Error> {
    // An IPv6 form of an IPv4 address gets the IPv4 checks.
    if let Some(v4) = crate::net::embedded_ipv4(ip) {
        return validate_ipv4(&v4, registry_is_loopback, realm, registry_str);
    }

    // Unspecified (::).
    if ip.is_unspecified() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to unspecified address {ip}"),
        ));
    }

    if ip.is_unicast_link_local() {
        return Err(realm_err(
            registry_str,
            format!("realm URL '{realm}' points to link-local address {ip}"),
        ));
    }

    // AWS IPv6 IMDS (fd00:ec2::254).
    if *ip == crate::net::EC2_METADATA_IPV6 {
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
/// metadata (100.100.100.200), unspecified (`0.0.0.0` / `[::]`), link-local
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
/// fewer labels require an exact match. An IP realm must be the registry's
/// own address, and an IP and a name never bind.
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
    match Scheme::try_from(realm) {
        Ok(Scheme::Https) => {}
        Ok(Scheme::Http) => {
            if !matches!(Scheme::try_from(registry_base), Ok(Scheme::Http)) {
                return Err(realm_err(
                    registry_str,
                    format!(
                        "realm URL '{realm}' uses http but registry uses {scheme}",
                        scheme = registry_base.scheme()
                    ),
                ));
            }
        }
        Err(unsupported) => {
            return Err(realm_err(
                registry_str,
                format!("realm URL '{realm}' uses {unsupported}"),
            ));
        }
    }

    // Layer 2: IP denylist (+ localhost hostname).
    let registry_is_loopback = is_loopback(registry_base);

    match realm.host() {
        Some(Host::Ipv4(ip)) => validate_ipv4(&ip, registry_is_loopback, realm, registry_str)?,
        Some(Host::Ipv6(ip)) => validate_ipv6(&ip, registry_is_loopback, realm, registry_str)?,
        Some(Host::Domain(_)) => {
            if is_loopback(realm) && !registry_is_loopback {
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

/// Whether `url` names this host: a loopback address, IPv4-mapped or not,
/// or `localhost`, with or without its trailing dot.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip_of(&ip).is_loopback(),
        None => false,
    }
}

/// An IPv6 host as the address it names: an IPv4-mapped one is its IPv4
/// address. Only the mapped form is read, since this grants: a translated
/// or NAT64 address goes through a translator off this host.
fn ip_of(ip: &Ipv6Addr) -> IpAddr {
    ip.to_ipv4_mapped().map_or(IpAddr::V6(*ip), IpAddr::V4)
}

/// The address an IP host names, or none for a name.
fn address(host: &Host<&str>) -> Option<IpAddr> {
    match host {
        Host::Domain(_) => None,
        Host::Ipv4(ip) => Some(IpAddr::V4(*ip)),
        Host::Ipv6(ip) => Some(ip_of(ip)),
    }
}

/// Verify the realm host is related to the registry host.
///
/// Rules:
/// - An IP host has no domain to bind to, so an IP realm must be the
///   registry's own address, and a realm and registry where one is an IP
///   and the other a name are refused. A loopback realm, IPv4-mapped or
///   not, or `localhost`, is allowed for a loopback registry.
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
    let (realm_host, registry_host) = match (realm.host(), registry_base.host()) {
        (Some(Host::Domain(realm_host)), Some(Host::Domain(registry_host))) => {
            (realm_host, registry_host)
        }
        _ if is_loopback(realm) && is_loopback(registry_base) => return Ok(()),
        (Some(realm_ip), Some(registry_ip)) if address(&realm_ip) == address(&registry_ip) => {
            return Ok(());
        }
        _ => {
            return Err(realm_err(
                registry_str,
                format!(
                    "realm URL '{realm}' is not on the registry's address '{}'; \
                     configure the registry by the realm's IP address, or by name for a named realm",
                    registry_base.host_str().unwrap_or_default()
                ),
            ));
        }
    };

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
        let resp = send_retrying(http.get(&v2_url), "registry ping").await?;

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
            return Err(Error::RegistryPing {
                registry: base_url.to_owned(),
                status,
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

    let resp = send_retrying(request, "token endpoint").await?;

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

    let status = resp.status();
    if status.is_client_error() || status.is_server_error() {
        tracing::warn!(base_url, %status, "token endpoint returned error");
        return Err(Error::TokenEndpoint {
            registry: base_url.to_owned(),
            status,
        });
    }
    let token_resp: TokenResponse =
        crate::json::answer(resp, crate::json::MAX_SMALL_ANSWER).await?;

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
            .field("token", &crate::REDACTED)
            .field("access_token", &crate::REDACTED)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::testing;

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

        testing::challenge(&mock, "test")
            .expect(1)
            .mount(&mock)
            .await;

        testing::token("anon-tok").expect(1).mount(&mock).await;

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

    /// A throttled or failing token endpoint is re-sent, not surfaced.
    ///
    /// 95 mappings against one registry need 95 scoped tokens, so this
    /// endpoint is high-volume exactly when a run is widest. Surfacing a
    /// 429 or 503 loses a mapping the registry never refused. Breaks if
    /// either status fails the exchange on its first answer.
    #[tokio::test]
    async fn exchange_retries_a_transient_token_endpoint_status() {
        for status in [429, 503] {
            let mock = wiremock::MockServer::start().await;

            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/v2/"))
                .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                    "WWW-Authenticate",
                    format!(r#"Bearer realm="{}/token""#, mock.uri()),
                ))
                .mount(&mock)
                .await;

            // Failing once, then answered.
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/token"))
                .respond_with(wiremock::ResponseTemplate::new(status))
                .up_to_n_times(1)
                .mount(&mock)
                .await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/token"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"token": "issued"})),
                )
                .mount(&mock)
                .await;

            let http = crate::test_http_client();
            let (token, _) = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
                .await
                .unwrap_or_else(|e| panic!("{status} must be retried, not surfaced: {e}"));
            assert_eq!(token.value(), "issued", "{status}");
        }
    }

    /// Retries are bounded: an endpoint that only throttles still returns.
    #[tokio::test]
    async fn exchange_gives_up_on_a_persistently_throttled_endpoint() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token""#, mock.uri()),
            ))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(429))
            .expect(u64::from(crate::retry::MAX_TRANSIENT_RETRIES) + 1)
            .mount(&mock)
            .await;

        let http = crate::test_http_client();
        let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("429"),
            "the surfaced error must still say it was throttled: {err}"
        );
    }

    /// A token endpoint's refusal is sent once and surfaced. Breaks if a
    /// 401 or 403 is retried like an outage.
    #[tokio::test]
    async fn exchange_endpoint_error() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
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
                .respond_with(wiremock::ResponseTemplate::new(status))
                .expect(1)
                .mount(&mock)
                .await;

            let http = crate::test_http_client();
            let err = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None)
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::TokenEndpoint { status: s, .. } if s == status),
                "expected a {status} from the token endpoint, got: {err:?}"
            );
            assert_eq!(err.status_code(), None, "not a registry answer");
        }
    }

    /// How long an exchanged token is kept.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Lifetime {
        /// Never expires.
        Permanent,
        /// Expires within the early-refresh window.
        Short,
        /// Expires well past the early-refresh window.
        Long,
    }

    /// The token is read from `token`, else `access_token`; a positive
    /// `expires_in` sets its lifetime, and one with no or a zero
    /// `expires_in` never expires; an answer with neither field is refused.
    /// Breaks if either field is ignored, a positive lifetime is dropped or
    /// shortened, a missing one is read as already expired, or an empty answer yields
    /// an empty token.
    #[tokio::test]
    async fn the_token_answer_is_read() {
        let cases = [
            (
                serde_json::json!({"token": "tok", "access_token": "other"}),
                Some(("tok", Lifetime::Permanent)),
            ),
            (
                serde_json::json!({"access_token": "fallback-tok"}),
                Some(("fallback-tok", Lifetime::Permanent)),
            ),
            (
                serde_json::json!({"token": "short-tok", "expires_in": 10}),
                Some(("short-tok", Lifetime::Short)),
            ),
            (
                serde_json::json!({"token": "long-tok", "expires_in": 3600}),
                Some(("long-tok", Lifetime::Long)),
            ),
            (
                serde_json::json!({"token": "zero-tok", "expires_in": 0}),
                Some(("zero-tok", Lifetime::Permanent)),
            ),
            (serde_json::json!({}), None),
        ];
        for (body, expected) in cases {
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
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&body))
                .expect(1)
                .mount(&mock)
                .await;

            let http = crate::test_http_client();
            let exchanged = exchange(&http, &mock.uri(), &[Scope::pull("repo")], None, None).await;
            match expected {
                Some((value, lifetime)) => {
                    let (token, _) = exchanged.unwrap();
                    assert_eq!(token.value(), value, "{body}");
                    assert!(!token.is_expired(), "{body}");
                    assert_eq!(
                        token.expires_at.is_none(),
                        lifetime == Lifetime::Permanent,
                        "{body}"
                    );
                    assert_eq!(
                        token.should_refresh(),
                        lifetime == Lifetime::Short,
                        "{body}"
                    );
                }
                None => assert!(
                    matches!(exchanged, Err(Error::AuthFailed { .. })),
                    "{body}: {exchanged:?}"
                ),
            }
        }
    }

    #[tokio::test]
    async fn exchange_multi_scope_query_params() {
        let mock = wiremock::MockServer::start().await;

        testing::challenge(&mock, "svc")
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
        assert!(err.is_auth_error(), "{msg}");
        // Must NOT mention WWW-Authenticate.
        assert!(
            !msg.contains("WWW-Authenticate"),
            "should not mention WWW-Authenticate for non-401: {msg}"
        );
    }

    /// A registry ping that fails with an outage is retried, then not
    /// reported as refused credentials. Breaks if a 5xx ping is sent once or
    /// exits with the auth code.
    #[tokio::test]
    async fn exchange_500_returns_clear_error() {
        let mock = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(u64::from(crate::retry::MAX_TRANSIENT_RETRIES) + 1)
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
        assert!(!err.is_auth_error(), "{msg}");
        assert_eq!(
            err.status_code(),
            None,
            "the engine must not retry it again"
        );
    }

    #[tokio::test]
    async fn challenge_cache_reuse() {
        let mock = wiremock::MockServer::start().await;

        // /v2/ should only be hit once -- the second exchange reuses the cached challenge.
        testing::challenge(&mock, "test")
            .expect(1)
            .mount(&mock)
            .await;

        // Token endpoint is called twice (different scopes).
        testing::token("reused").expect(2).mount(&mock).await;

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

    // --- Realm validation ---

    /// What a realm check decides.
    #[derive(Debug, Clone, Copy)]
    enum Verdict {
        Allowed,
        /// Refused, with this text in the error.
        Rejected(&'static str),
    }

    /// A realm is checked against its registry before any credential is
    /// sent to it: scheme, userinfo and host, the cloud metadata and
    /// loopback denylist, and the domain binding. Breaks if a row's realm
    /// is let through when it should be refused, refused when it should
    /// pass, or refused for another reason.
    #[test]
    fn realms_are_checked_before_credentials_are_sent() {
        let cases = [
            // rejects http when registry is https
            (
                "http://evil.com/token",
                "https://registry.example.com",
                Verdict::Rejected("uses http but registry uses https"),
            ),
            // rejects userinfo
            (
                "https://user:pass@evil.com/token",
                "https://registry.example.com",
                Verdict::Rejected("userinfo"),
            ),
            // rejects a username without a password
            (
                "https://user@evil.com/token",
                "https://registry.example.com",
                Verdict::Rejected("userinfo"),
            ),
            // allows a public IPv6 realm, which no denylist entry matches
            (
                "https://[2001:db8::1]/token",
                "https://[2001:db8::1]:5000",
                Verdict::Allowed,
            ),
            // a registry known by name may not send credentials to an IP,
            // nor an IP registry to a name or another address
            (
                "https://203.0.113.9/steal",
                "https://registry.example.com",
                Verdict::Rejected("is not on the registry's address"),
            ),
            (
                "https://[2001:db8::1]/steal",
                "https://registry.example.com",
                Verdict::Rejected("is not on the registry's address"),
            ),
            (
                "https://auth.example.com/token",
                "https://192.168.1.50",
                Verdict::Rejected("is not on the registry's address"),
            ),
            (
                "https://203.0.113.9/steal",
                "https://192.168.1.50",
                Verdict::Rejected("is not on the registry's address"),
            ),
            (
                "http://10.0.0.1:9999/token",
                "http://localhost:5000",
                Verdict::Rejected("is not on the registry's address"),
            ),
            // allows the registry's own address written IPv4-mapped
            (
                "https://[::ffff:192.168.1.50]/token",
                "https://192.168.1.50",
                Verdict::Allowed,
            ),
            // allows a loopback realm for `localhost.`
            (
                "http://127.0.0.1:9999/token",
                "http://localhost.:5000",
                Verdict::Allowed,
            ),
            // allows another port on the registry's own address
            (
                "https://192.168.1.50:5001/token",
                "https://192.168.1.50:5000",
                Verdict::Allowed,
            ),
            // allows a loopback realm for a localhost registry
            (
                "http://127.0.0.1:9999/token",
                "http://localhost:5000",
                Verdict::Allowed,
            ),
            // rejects non web scheme
            (
                "ftp://evil.com/token",
                "https://registry.example.com",
                Verdict::Rejected("unsupported scheme"),
            ),
            // rejects no host
            (
                "data:text/plain,hello",
                "https://registry.example.com",
                Verdict::Rejected("no host"),
            ),
            // Docker Hub: registry-1.docker.io hands out tokens from its
            // sibling auth.docker.io, under the same parent docker.io.
            (
                "https://auth.docker.io/token",
                "https://registry-1.docker.io",
                Verdict::Allowed,
            ),
            // allows http when registry is http
            (
                "http://my-registry:5000/token",
                "http://my-registry:5000",
                Verdict::Allowed,
            ),
            // allows same host different port
            (
                "https://host:8443/token",
                "https://host:5000",
                Verdict::Allowed,
            ),
            // rejects link local ipv4
            (
                "https://169.254.169.254/latest/meta-data",
                "https://registry.example.com",
                Verdict::Rejected("link-local"),
            ),
            // rejects ipv4 mapped ipv6 metadata
            (
                "https://[::ffff:169.254.169.254]/token",
                "https://registry.example.com",
                Verdict::Rejected("link-local"),
            ),
            // rejects aws ipv6 imds
            (
                "https://[fd00:ec2::254]/token",
                "https://registry.example.com",
                Verdict::Rejected("AWS IPv6 IMDS"),
            ),
            // rejects unspecified ipv4
            (
                "http://0.0.0.0/token",
                "http://0.0.0.0",
                Verdict::Rejected("unspecified"),
            ),
            // rejects link local ipv6
            (
                "http://[fe80::1]/token",
                "http://[fe80::1]",
                Verdict::Rejected("link-local"),
            ),
            // rejects unspecified ipv6
            (
                "http://[::]/token",
                "http://[::]",
                Verdict::Rejected("unspecified"),
            ),
            // rejects alibaba metadata
            (
                "https://100.100.100.200/token",
                "https://registry.example.com",
                Verdict::Rejected("Alibaba"),
            ),
            // rejects loopback when registry is not loopback
            (
                "https://127.0.0.1/token",
                "https://registry.example.com",
                Verdict::Rejected("loopback"),
            ),
            // allows loopback when registry is loopback
            (
                "http://127.0.0.1:9999/token",
                "http://127.0.0.1:5000",
                Verdict::Allowed,
            ),
            // allows an IPv4-mapped loopback realm for a registry on this host
            (
                "http://[::ffff:127.0.0.1]:9999/token",
                "http://[::ffff:127.0.0.1]:5000",
                Verdict::Allowed,
            ),
            // allows private ip
            (
                "https://192.168.1.50/token",
                "https://192.168.1.50",
                Verdict::Allowed,
            ),
            // rejects localhost when registry is not loopback
            (
                "https://localhost:9999/token",
                "https://registry.example.com",
                Verdict::Rejected("points to localhost but registry is not loopback"),
            ),
            // allows localhost when registry is localhost
            (
                "http://localhost:9999/token",
                "http://localhost:5000",
                Verdict::Allowed,
            ),
            // rejects ipv6 loopback when registry is not loopback
            (
                "https://[::1]/token",
                "https://registry.example.com",
                Verdict::Rejected("loopback"),
            ),
            // rejects ipv4 mapped ipv6 loopback when registry is not loopback
            (
                "https://[::ffff:127.0.0.1]/token",
                "https://registry.example.com",
                Verdict::Rejected("loopback"),
            ),
            // RFC 6145 IPv4-translated: ::ffff:0:a9fe:a9fe embeds 169.254.169.254
            (
                "https://[::ffff:0:a9fe:a9fe]/token",
                "https://registry.example.com",
                Verdict::Rejected("link-local"),
            ),
            // RFC 6052 NAT64 well-known prefix: 64:ff9b::a9fe:a9fe embeds
            // 169.254.169.254
            (
                "https://[64:ff9b::a9fe:a9fe]/token",
                "https://registry.example.com",
                Verdict::Rejected("link-local"),
            ),
            // allows same host
            ("https://ghcr.io/token", "https://ghcr.io", Verdict::Allowed),
            // rejects unrelated domain
            (
                "https://attacker.com/steal",
                "https://registry.example.com",
                Verdict::Rejected("does not match"),
            ),
            // rejects different tld
            (
                "https://auth.docker.com/token",
                "https://registry-1.docker.io",
                Verdict::Rejected("does not match"),
            ),
            // ghcr.io has only two labels, so only an exact match passes:
            // dropping one would leave every .io domain
            (
                "https://evil.io/token",
                "https://ghcr.io",
                Verdict::Rejected("does not match"),
            ),
            // quay.io has only two labels, so even its own subdomain needs an
            // exact match
            (
                "https://auth.quay.io/token",
                "https://quay.io",
                Verdict::Rejected("does not match"),
            ),
        ];
        for (realm, registry, expected) in cases {
            let result = validate_realm_url(
                &reqwest::Url::parse(realm).unwrap(),
                &reqwest::Url::parse(registry).unwrap(),
            );
            match expected {
                Verdict::Allowed => {
                    result.unwrap_or_else(|e| panic!("{realm} for {registry}: {e}"))
                }
                Verdict::Rejected(needle) => {
                    let err = result.expect_err(realm).to_string();
                    assert!(err.contains(needle), "{realm} for {registry}: {err}");
                }
            }
        }
    }

    // --- Realm validation in an exchange ---

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

    /// An exchange that started from the cached challenge, and was cleared
    /// while it ran, does not store the challenge it learned, and one that
    /// pinged `/v2/` itself does. Breaks if a landing puts back the
    /// challenge a clearing judged, or an exchange that never used it
    /// discards what it learned.
    #[tokio::test]
    async fn only_an_exchange_from_a_cleared_challenge_discards_what_it_learned() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for from_cache in [true, false] {
            let server = MockServer::start().await;
            testing::challenge(&server, "test").mount(&server).await;
            Mock::given(method("GET"))
                .and(path("/token"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"token": "tok", "expires_in": 300}))
                        .set_delay(Duration::from_millis(300)),
                )
                .mount(&server)
                .await;
            let http = crate::test_http_client();
            let uri = server.uri();
            let cache = ChallengeCache::default();
            if from_cache {
                cache
                    .set(WwwAuthenticate {
                        realm: format!("{uri}/token"),
                        service: Some("test".to_owned()),
                    })
                    .await;
            }
            let scopes = [Scope::pull("repo")];

            // The clearing lands once the exchange has sent its first
            // request and waits on the delayed token endpoint.
            let (token, ()) = tokio::join!(cache.exchange(&http, &uri, &scopes, None), async {
                while server
                    .received_requests()
                    .await
                    .unwrap_or_default()
                    .is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                cache.clear().await;
            },);
            token.unwrap();
            assert_eq!(cache.get().await.is_some(), !from_cache, "{from_cache}");
        }
    }
}
