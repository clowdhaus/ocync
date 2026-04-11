pub mod anonymous;
pub mod docker;

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::DistributionError;

/// Minimum remaining lifetime before a token should be proactively refreshed.
const REFRESH_THRESHOLD: Duration = Duration::from_secs(15 * 60);

/// OAuth2-style scope for registry token requests.
///
/// Format: `repository:<name>:<actions>` where actions is a comma-separated
/// list (e.g. `pull`, `push`, `pull,push`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub repository: String,
    pub actions: Vec<Action>,
}

impl Scope {
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
    Pull,
    Push,
}

impl Action {
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

/// A bearer token with optional expiry tracking.
#[derive(Debug, Clone)]
pub struct Token {
    /// The raw bearer token string.
    value: String,
    /// When this token expires (if known).
    expires_at: Option<Instant>,
}

impl Token {
    /// Create a token that never expires.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            expires_at: None,
        }
    }

    /// Create a token with a known time-to-live.
    pub fn with_ttl(value: impl Into<String>, ttl: Duration) -> Self {
        Self {
            value: value.into(),
            expires_at: Some(Instant::now() + ttl),
        }
    }

    /// Create a token that expires at a specific instant.
    pub fn with_expiry(value: impl Into<String>, expires_at: Instant) -> Self {
        Self {
            value: value.into(),
            expires_at: Some(expires_at),
        }
    }

    /// The raw bearer token value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether this token has already expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Instant::now() >= exp)
    }

    /// Whether this token should be refreshed soon (less than 15 minutes remaining).
    pub fn should_refresh(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = Instant::now();
                now >= exp || exp.duration_since(now) < REFRESH_THRESHOLD
            }
            None => false,
        }
    }
}

/// Credentials for authenticating to a registry.
#[derive(Debug, Clone)]
pub enum Credentials {
    /// HTTP Basic authentication.
    Basic { username: String, password: String },
    /// A pre-existing bearer token.
    Bearer(String),
    /// A file containing a token (read on demand).
    TokenFile(PathBuf),
}

/// Trait for providers that can obtain authentication tokens for registries.
///
/// Implementations must be `Send + Sync` for use across async tasks.
pub trait AuthProvider: Send + Sync {
    /// Human-readable name of this provider (e.g. "anonymous", "docker-config").
    fn name(&self) -> &'static str;

    /// Obtain a bearer token valid for the given scopes.
    fn get_token(
        &self,
        scopes: &[Scope],
    ) -> impl std::future::Future<Output = Result<Token, DistributionError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn token_with_zero_ttl_is_expired() {
        let token = Token::with_expiry("abc123", Instant::now() - Duration::from_secs(1));
        assert!(token.is_expired());
        assert!(token.should_refresh());
    }

    #[test]
    fn token_within_refresh_threshold() {
        // 10 minutes remaining — less than the 15-minute threshold
        let token = Token::with_ttl("abc123", Duration::from_secs(600));
        assert!(!token.is_expired());
        assert!(token.should_refresh());
    }

    #[test]
    fn token_beyond_refresh_threshold() {
        // 20 minutes remaining — above the 15-minute threshold
        let token = Token::with_ttl("abc123", Duration::from_secs(1200));
        assert!(!token.is_expired());
        assert!(!token.should_refresh());
    }

    #[test]
    fn credentials_basic_variant() {
        let creds = Credentials::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        assert!(matches!(creds, Credentials::Basic { .. }));
    }

    #[test]
    fn credentials_bearer_variant() {
        let creds = Credentials::Bearer("token123".into());
        assert!(matches!(creds, Credentials::Bearer(_)));
    }

    #[test]
    fn credentials_token_file_variant() {
        let creds = Credentials::TokenFile(PathBuf::from("/tmp/token"));
        assert!(matches!(creds, Credentials::TokenFile(_)));
    }
}
