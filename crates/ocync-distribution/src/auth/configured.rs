//! Auth provider for registries that issue their own tokens, named in the
//! `WWW-Authenticate: Bearer` challenge of a `401` from `/v2/`, signing in
//! with credentials from configuration: the ocync config, a Docker
//! `config.json`, or none.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use super::docker::{DockerConfig, resolve_from_docker_config};
use super::token_cache::TokenCache;
use super::{AuthProvider, Credentials, Scope, Token, registry_origin, scopes_cache_key};
use crate::error::Error;

/// Where a [`ConfiguredAuth`] got the credentials it sends to the token
/// endpoint.
enum SignIn {
    /// No credentials: the token request carries no `Authorization` header.
    Anonymous,
    /// Credentials from the ocync config.
    Basic(Credentials),
    /// Credentials resolved from a Docker `config.json`, or none when it
    /// holds no entry for the registry.
    DockerConfig(Option<Credentials>),
}

impl SignIn {
    fn credentials(&self) -> Option<&Credentials> {
        match self {
            Self::Anonymous | Self::DockerConfig(None) => None,
            Self::Basic(credentials) | Self::DockerConfig(Some(credentials)) => Some(credentials),
        }
    }
}

/// Auth provider that performs the Docker token-exchange flow.
///
/// When a registry responds with `401 Unauthorized` and a `WWW-Authenticate: Bearer ...`
/// header, this provider exchanges the realm and service for a token, sending
/// its credentials, if any, as HTTP Basic. Tokens are coalesced per scope:
/// concurrent fetches for the same scope produce one token exchange while
/// distinct scopes run in parallel.
pub struct ConfiguredAuth {
    /// The registry base URL (e.g. `https://registry-1.docker.io`).
    base_url: String,
    /// HTTP client for token requests.
    http: reqwest::Client,
    /// The credentials sent to the token endpoint.
    sign_in: SignIn,
    /// Per-scope coalescing token cache.
    tokens: TokenCache,
}

impl fmt::Debug for ConfiguredAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredAuth")
            .field("base_url", &self.base_url)
            .field("sign_in", &self.name())
            .field("has_credentials", &self.sign_in.credentials().is_some())
            .finish_non_exhaustive()
    }
}

impl ConfiguredAuth {
    /// Create an anonymous provider for the registry at `registry`, the base
    /// URL its client uses.
    pub fn anonymous(registry: &url::Url, http: reqwest::Client) -> Self {
        Self::new(registry, http, SignIn::Anonymous)
    }

    /// Create a provider that sends `credentials` to the token endpoint of
    /// the registry at `registry`, the base URL its client uses.
    pub fn basic(registry: &url::Url, http: reqwest::Client, credentials: Credentials) -> Self {
        Self::new(registry, http, SignIn::Basic(credentials))
    }

    /// Create a provider for the registry at `registry`, the base URL its
    /// client uses, with the credentials `config` holds under
    /// `credentials_key`, the registry as the user wrote it: Docker keys
    /// credentials by that string, explicit default port and case included,
    /// which a parsed URL normalizes away. With no entry it signs in
    /// anonymously.
    pub async fn docker_config(
        registry: &url::Url,
        http: reqwest::Client,
        credentials_key: &str,
        config: &DockerConfig,
    ) -> Result<Self, Error> {
        let credentials = resolve_from_docker_config(config, credentials_key).await?;
        if credentials.is_some() {
            tracing::debug!(registry = %credentials_key, "docker config: resolved credentials");
        } else {
            tracing::debug!(registry = %credentials_key, "docker config: no credentials, using anonymous");
        }
        Ok(Self::new(registry, http, SignIn::DockerConfig(credentials)))
    }

    fn new(registry: &url::Url, http: reqwest::Client, sign_in: SignIn) -> Self {
        Self {
            base_url: registry_origin(registry),
            http,
            sign_in,
            tokens: TokenCache::new(),
        }
    }
}

impl AuthProvider for ConfiguredAuth {
    fn name(&self) -> &'static str {
        match self.sign_in {
            SignIn::Anonymous => "anonymous",
            SignIn::Basic(_) => "basic",
            SignIn::DockerConfig(_) => "docker-config",
        }
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
                    self.tokens
                        .challenge()
                        .exchange(&self.http, &self.base_url, &scopes, self.sign_in.credentials())
                        .await
                        .inspect_err(|e| {
                            tracing::warn!(base_url = %self.base_url, scope = %key, error = %e, "token exchange failed");
                        })
                })
                .await
        })
    }

    fn refuse(&self, refused: Token) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.tokens.refuse(&self.base_url, &refused).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// A Docker config key no parsed URL produces: Docker keys credentials
    /// by the registry as the user wrote it.
    const AS_WRITTEN: &str = "Registry.Example:443";

    fn credentials() -> Credentials {
        Credentials::Basic {
            username: "testuser".into(),
            password: "testpass".into(),
        }
    }

    async fn mount_v2_challenge(server: &MockServer, expect: u64) {
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{}/token",service="test""#, server.uri()),
            ))
            .expect(expect)
            .mount(server)
            .await;
    }

    async fn mount_token(server: &MockServer, scope: &str, value: &str, expect: u64) {
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("scope", scope))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token": value, "expires_in": 3600})),
            )
            .expect(expect)
            .mount(server)
            .await;
    }

    fn url(server: &MockServer) -> url::Url {
        server.uri().parse().unwrap()
    }

    async fn docker_config_auth(server: &MockServer, entry: Option<&str>) -> ConfiguredAuth {
        let auths = entry.map_or_else(String::new, |userpass| {
            format!(
                r#""{AS_WRITTEN}": {{"auth": "{}"}}"#,
                BASE64.encode(userpass)
            )
        });
        let config: DockerConfig =
            crate::json::parse(format!(r#"{{"auths": {{{auths}}}}}"#).as_bytes()).unwrap();
        ConfiguredAuth::docker_config(&url(server), crate::test_http_client(), AS_WRITTEN, &config)
            .await
            .unwrap()
    }

    /// Each sign-in keeps its name and sends its own credentials, and only
    /// those, to the token endpoint, and Debug never prints them. Breaks if
    /// anonymous sends credentials, a Docker config entry is looked up by
    /// anything but the registry as written, a provider sends credentials it
    /// does not hold, or Debug prints a username or password.
    #[tokio::test]
    async fn each_sign_in_sends_its_credentials() {
        let basic = format!("Basic {}", BASE64.encode("testuser:testpass"));
        let docker = format!("Basic {}", BASE64.encode("user:pass"));
        let cases = [
            ("anonymous", None, "anonymous"),
            ("basic", Some(basic.as_str()), "basic"),
            (
                "docker config with an entry",
                Some(docker.as_str()),
                "docker-config",
            ),
            ("docker config without one", None, "docker-config"),
        ];
        for (case, expected, name) in cases {
            let server = MockServer::start().await;
            mount_v2_challenge(&server, 1).await;
            mount_token(&server, "repository:repo:pull", "tok", 1).await;
            let auth = match case {
                "anonymous" => ConfiguredAuth::anonymous(&url(&server), crate::test_http_client()),
                "basic" => {
                    ConfiguredAuth::basic(&url(&server), crate::test_http_client(), credentials())
                }
                "docker config with an entry" => {
                    docker_config_auth(&server, Some("user:pass")).await
                }
                "docker config without one" => docker_config_auth(&server, None).await,
                _ => unreachable!("{case}"),
            };
            assert_eq!(auth.name(), name, "{case}");
            let debug = format!("{auth:?}");
            for secret in ["testuser", "testpass", "user:pass"] {
                assert!(!debug.contains(secret), "{case}: {debug}");
            }
            assert_eq!(
                auth.get_token(&[Scope::pull("repo")])
                    .await
                    .unwrap()
                    .value(),
                "tok"
            );

            let requests = server.received_requests().await.unwrap();
            let token_request = requests.iter().find(|r| r.url.path() == "/token").unwrap();
            let sent = token_request
                .headers
                .get("authorization")
                .map(|v| v.to_str().unwrap());
            assert_eq!(sent, expected, "{case}");
        }
    }

    /// Tokens are kept per scope: a repeated scope is served from the cache,
    /// while another repository or more actions on the same one exchange
    /// again, all from one `/v2/` challenge. Breaks if the provider skips
    /// the cache or keys it by anything but the full scope.
    #[tokio::test]
    async fn tokens_are_kept_per_scope() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 1).await;
        mount_token(&server, "repository:repo:pull", "pull", 1).await;
        mount_token(&server, "repository:repo:pull,push", "push", 1).await;
        mount_token(&server, "repository:other:pull", "other", 1).await;
        let auth = ConfiguredAuth::anonymous(&url(&server), crate::test_http_client());

        for (scope, expected) in [
            (Scope::pull("repo"), "pull"),
            (Scope::pull("repo"), "pull"),
            (Scope::pull_push("repo"), "push"),
            (Scope::pull("other"), "other"),
        ] {
            assert_eq!(auth.get_token(&[scope]).await.unwrap().value(), expected);
        }
    }

    /// A refusal of another token keeps the cached token and challenge, and
    /// a refusal of the cached one drops both. Breaks if a refusal is not
    /// matched against the token refused.
    #[tokio::test]
    async fn a_refusal_drops_only_the_refused_token() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 2).await;
        mount_token(&server, "repository:repo:pull", "tok", 2).await;
        let auth = ConfiguredAuth::anonymous(&url(&server), crate::test_http_client());

        let refused = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(Token::new("other")).await;
        assert!(auth.tokens.challenge().get().await.is_some());
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(refused).await;
        assert!(auth.tokens.challenge().get().await.is_none());
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
    }

    /// A token endpoint's refusal reaches the caller with its status.
    /// Breaks if the provider swallows the failure or replaces its error.
    #[tokio::test]
    async fn a_failed_exchange_returns_its_error() {
        let server = MockServer::start().await;
        mount_v2_challenge(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let auth = ConfiguredAuth::basic(&url(&server), crate::test_http_client(), credentials());

        let err = auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert!(
            matches!(err, Error::TokenEndpoint { status, .. } if status == http::StatusCode::FORBIDDEN),
            "{err:?}"
        );
    }
}
