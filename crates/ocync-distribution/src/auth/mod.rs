//! Authentication providers and token management for OCI registries.

/// Azure Container Registry authentication provider.
pub mod acr;
/// Token-exchange authentication: anonymous, HTTP Basic or Docker config.
pub mod configured;
/// Hostname-based registry provider detection.
pub mod detect;
/// Docker config.json credential resolution.
pub mod docker;
/// AWS ECR Private authentication provider.
pub mod ecr;
/// AWS ECR Public authentication provider.
pub mod ecr_public;
/// Google Artifact Registry / Container Registry authentication provider.
pub mod gcp;
/// Recovery from a token endpoint refusing a cloud credential.
mod recovery;
/// Static bearer token authentication provider.
pub mod static_token;
/// Coalescing token cache used by every Bearer-token auth provider.
mod token_cache;
/// Shared Docker v2 token-exchange protocol.
pub(crate) mod token_exchange;

pub use detect::{ProviderKind, detect_provider_kind};

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::Instant;

use crate::error::Error;

/// Origin (scheme, host and port) of a registry's base URL, where a
/// token-exchange provider pings `/v2/` for its challenge. Taken from the
/// same URL the client uses, so both speak one scheme.
pub(crate) fn registry_origin(registry: &url::Url) -> String {
    registry.origin().ascii_serialization()
}

/// Tokens are proactively refreshed when their remaining lifetime drops
/// below this window. Must be shorter than the shortest token TTL we
/// encounter (Docker Hub returns 300s): a window at least that long marks
/// every such token stale on arrival, bypassing the cache entirely.
pub(crate) const EARLY_REFRESH_WINDOW: Duration = Duration::from_secs(30);

/// OAuth2-style scope for registry token requests.
///
/// Format: `repository:<name>:<actions>` where actions is a comma-separated
/// list (e.g. `pull`, `push`, `pull,push`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The repository this scope applies to.
    pub repository: String,
    /// The actions requested (e.g. pull, push).
    pub actions: Vec<Action>,
}

impl Scope {
    /// Create a scope for the given repository and actions.
    pub fn new(repository: impl Into<String>, actions: Vec<Action>) -> Self {
        Self {
            repository: repository.into(),
            actions,
        }
    }

    /// Convenience constructor for a pull-only scope.
    pub fn pull(repository: impl Into<String>) -> Self {
        Self::new(repository, vec![Action::Pull])
    }

    /// Convenience constructor for pull+push scope.
    pub fn pull_push(repository: impl Into<String>) -> Self {
        Self::new(repository, vec![Action::Pull, Action::Push])
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let actions: Vec<&str> = self.actions.iter().map(Action::as_str).collect();
        write!(f, "repository:{}:{}", self.repository, actions.join(","))
    }
}

/// An action that can be performed on a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Read-only access to a repository.
    Pull,
    /// Write access to a repository.
    Push,
}

impl Action {
    /// Return the string representation of this action.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// HTTP authentication scheme for a token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AuthScheme {
    /// `Authorization: Bearer <token>` -- used by Docker v2 token exchange.
    #[default]
    Bearer,
    /// `Authorization: Basic <base64>` -- used by ECR (value is pre-encoded).
    Basic,
}

/// An authentication token with optional expiry tracking.
#[derive(Clone)]
pub struct Token {
    /// The raw token string.
    value: String,
    /// When this token expires (if known).
    expires_at: Option<Instant>,
    /// How to format the `Authorization` header.
    scheme: AuthScheme,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("value", &crate::REDACTED)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Token {
    /// Create a Bearer token that never expires.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            expires_at: None,
            scheme: AuthScheme::Bearer,
        }
    }

    /// Create a Bearer token with a known time-to-live. A lifetime past
    /// what the clock can hold never expires.
    pub fn with_ttl(value: impl Into<String>, ttl: Duration) -> Self {
        Self {
            value: value.into(),
            expires_at: Instant::now().checked_add(ttl),
            scheme: AuthScheme::Bearer,
        }
    }

    /// Set the auth scheme (default is Bearer).
    pub fn with_scheme(mut self, scheme: AuthScheme) -> Self {
        self.scheme = scheme;
        self
    }

    /// The raw token value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether the two tokens send the same `Authorization` header,
    /// whatever their expiry.
    pub(crate) fn sends_same_header(&self, other: &Token) -> bool {
        self.value == other.value && self.scheme == other.scheme
    }

    /// The auth scheme for this token.
    pub fn scheme(&self) -> &AuthScheme {
        &self.scheme
    }

    /// Whether this token has already expired.
    pub fn is_expired(&self) -> bool {
        !is_fresh(self.expires_at, Duration::ZERO)
    }

    /// Returns `true` if the token is still usable (not expired and not within
    /// the early-refresh window).
    pub fn is_valid(&self) -> bool {
        !self.should_refresh()
    }

    /// Whether this token should be proactively refreshed (remaining lifetime
    /// is at most [`EARLY_REFRESH_WINDOW`]).
    pub fn should_refresh(&self) -> bool {
        !is_fresh(self.expires_at, EARLY_REFRESH_WINDOW)
    }
}

/// Whether a value expiring at `expires_at`, or never when `None`, has
/// more than `window` of its lifetime left, as the SDK's expiring cache
/// judges it. Time is tokio's, so a test with paused time moves it.
pub(crate) fn is_fresh(expires_at: Option<Instant>, window: Duration) -> bool {
    expires_at.is_none_or(|expires_at| {
        expires_at
            .checked_duration_since(Instant::now())
            .is_some_and(|left| left > window)
    })
}

/// Build a sorted, space-joined cache key from a set of scopes.
///
/// Used by auth providers to key their token caches. The sort ensures
/// that the same set of scopes in any order produces the same key.
pub(crate) fn scopes_cache_key(scopes: &[Scope]) -> String {
    let mut parts: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    parts.sort();
    parts.join(" ")
}

/// Credentials for authenticating to a registry.
#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    /// HTTP Basic authentication.
    Basic {
        /// The username.
        username: String,
        /// The password or token.
        password: String,
    },
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Credentials::Basic")
                .field("username", username)
                .field("password", &crate::REDACTED)
                .finish(),
        }
    }
}

/// What a refusal of a credential found stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The refused credential was the one stored, and is dropped.
    Current,
    /// Another credential, or none, was stored, and it is kept.
    Stale,
}

/// Trait for providers that can obtain authentication tokens for registries.
///
/// Uses `Pin<Box<dyn Future>>` for object safety, allowing `Box<dyn AuthProvider>`.
/// Implementations must be `Send + Sync` for use across async tasks.
pub trait AuthProvider: Send + Sync {
    /// Human-readable name of this provider (e.g. "anonymous", "docker-config").
    fn name(&self) -> &'static str;

    /// Obtain a bearer token valid for the given scopes.
    ///
    /// The `scopes` slice is converted to owned data before the future is created,
    /// so callers don't need to worry about borrow lifetimes.
    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> Pin<Box<dyn Future<Output = Result<Token, Error>> + Send + '_>>;

    /// The registry rejected `refused` with 401. Drop the stored token only
    /// if it is `refused`, so the next `get_token` call does not reuse it.
    /// A stored token that has since replaced `refused` is kept, as other
    /// requests may be using it. A fetch already in flight may still answer
    /// the next call. The cloud credential a token was exchanged with is
    /// judged only by the exchange that uses it, not by this call.
    ///
    /// Called by the client when a request returns 401.
    fn refuse(&self, refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Mocks shared by the provider tests.
#[cfg(test)]
pub(crate) mod testing {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A `/v2/` challenge naming `service`, with the realm on `server`.
    pub(crate) fn challenge(server: &MockServer, service: &str) -> Mock {
        Mock::given(method("GET")).and(path("/v2/")).respond_with(
            ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="{service}""#,
                    server.uri()
                ),
            ),
        )
    }

    /// A token endpoint answering `value` to every request.
    pub(crate) fn token(value: &str) -> Mock {
        Mock::given(method("GET")).and(path("/token")).respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"token": value, "expires_in": 300})),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credentials and token services know a registry by its host, with the
    /// port only when one is written.
    #[test]
    fn scope_display_single_action() {
        let scope = Scope::pull("library/nginx");
        assert_eq!(scope.to_string(), "repository:library/nginx:pull");
    }

    #[test]
    fn scope_display_multiple_actions() {
        let scope = Scope::pull_push("myuser/myrepo");
        assert_eq!(scope.to_string(), "repository:myuser/myrepo:pull,push");
    }

    #[test]
    fn scope_equality() {
        let a = Scope::pull("repo");
        let b = Scope::pull("repo");
        assert_eq!(a, b);
    }

    #[test]
    fn action_display() {
        assert_eq!(Action::Pull.to_string(), "pull");
        assert_eq!(Action::Push.to_string(), "push");
    }

    #[test]
    fn token_no_expiry_never_expired() {
        let token = Token::new("abc123");
        assert_eq!(token.value(), "abc123");
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    #[test]
    fn token_with_long_ttl_not_expired() {
        let token = Token::with_ttl("abc123", Duration::from_secs(3600));
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    /// A lifetime past what the clock can hold never expires. Breaks if the
    /// lifetime is added to the clock unchecked, which panics.
    #[test]
    fn token_with_a_lifetime_past_the_clock_never_expires() {
        let token = Token::with_ttl("abc123", Duration::MAX);
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    #[test]
    fn token_with_zero_ttl_is_expired() {
        let token = Token::with_ttl("abc123", Duration::ZERO);
        assert!(token.is_expired());
        assert!(token.should_refresh());
    }

    #[test]
    fn token_within_refresh_threshold() {
        // 20 seconds remaining -- less than the 30-second threshold
        let token = Token::with_ttl("abc123", Duration::from_secs(20));
        assert!(!token.is_expired());
        assert!(token.should_refresh());
    }

    #[test]
    fn token_beyond_refresh_threshold() {
        // 60 seconds remaining -- above the 30-second threshold
        let token = Token::with_ttl("abc123", Duration::from_secs(60));
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    #[test]
    fn scopes_cache_key_sorted() {
        let scopes = vec![Scope::pull("z-repo"), Scope::pull("a-repo")];
        let key = scopes_cache_key(&scopes);
        assert!(key.starts_with("repository:a-repo"));
    }

    #[test]
    fn scopes_cache_key_deterministic() {
        let k1 = scopes_cache_key(&[Scope::pull("a"), Scope::pull("b")]);
        let k2 = scopes_cache_key(&[Scope::pull("b"), Scope::pull("a")]);
        assert_eq!(k1, k2);
    }

    #[test]
    fn scopes_cache_key_single() {
        let key = scopes_cache_key(&[Scope::pull("repo")]);
        assert_eq!(key, "repository:repo:pull");
    }

    #[test]
    fn credentials_basic_variant() {
        let creds = Credentials::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        assert!(matches!(creds, Credentials::Basic { .. }));
    }
}
