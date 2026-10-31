//! Auth provider for registries that issue their own tokens, named in the
//! `WWW-Authenticate: Bearer` challenge of a `401` from `/v2/`, signing in
//! with credentials from configuration: the ocync config, a Docker
//! `config.json`, or none.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use super::docker::{DockerConfig, resolve_from_docker_config};
use super::token_cache::TokenCache;
use super::{AuthProvider, Credentials, Presents, Scope, Token, registry_origin, scopes_cache_key};
use crate::error::Error;

/// Where an [`Auth`] got the credentials it sends to the token
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
pub struct Auth {
    /// The registry base URL (e.g. `https://registry-1.docker.io`).
    base_url: String,
    http: reqwest::Client,
    sign_in: SignIn,
    tokens: TokenCache,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("base_url", &self.base_url)
            .field("sign_in", &self.name())
            .field("has_credentials", &self.sign_in.credentials().is_some())
            .finish_non_exhaustive()
    }
}

impl Auth {
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
        tracing::debug!(
            registry = %credentials_key,
            has_credentials = credentials.is_some(),
            "docker config: credentials resolved"
        );
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

impl AuthProvider for Auth {
    fn name(&self) -> &'static str {
        match self.sign_in {
            SignIn::Anonymous => "anonymous",
            SignIn::Basic(_) => "basic",
            SignIn::DockerConfig(_) => "docker-config",
        }
    }

    fn presents(&self) -> Presents {
        match self.sign_in.credentials() {
            Some(_) => Presents::Credentials,
            None => Presents::Nothing,
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
    use crate::auth::testing;

    /// A Docker config key no parsed URL produces: Docker keys credentials
    /// by the registry as the user wrote it.
    const AS_WRITTEN: &str = "Registry.Example:443";

    fn credentials() -> Credentials {
        Credentials::Basic {
            username: "testuser".into(),
            password: "testpass".into(),
        }
    }

    /// A provider presents credentials only when its sign-in holds some, so
    /// a docker config with no entry for the registry presents nothing
    /// although it is not named anonymous. Breaks if a name stands in for
    /// the answer or a sign-in is misread.
    #[test]
    fn presents_credentials_only_when_the_sign_in_holds_some() {
        let registry: url::Url = "https://registry.example".parse().unwrap();
        let http = crate::test_http_client();
        for (sign_in, expected) in [
            (SignIn::Anonymous, Presents::Nothing),
            (SignIn::DockerConfig(None), Presents::Nothing),
            (
                SignIn::DockerConfig(Some(credentials())),
                Presents::Credentials,
            ),
            (SignIn::Basic(credentials()), Presents::Credentials),
        ] {
            let auth = Auth::new(&registry, http.clone(), sign_in);
            assert_eq!(auth.presents(), expected, "{}", auth.name());
        }
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

    async fn docker_config_auth(server: &MockServer, entry: Option<&str>) -> Auth {
        let auths = entry.map_or_else(String::new, |userpass| {
            format!(
                r#""{AS_WRITTEN}": {{"auth": "{}"}}"#,
                BASE64.encode(userpass)
            )
        });
        let config: DockerConfig =
            crate::json::parse(format!(r#"{{"auths": {{{auths}}}}}"#).as_bytes()).unwrap();
        Auth::docker_config(&url(server), crate::test_http_client(), AS_WRITTEN, &config)
            .await
            .unwrap()
    }

    /// A sign-in under test, with what it must send and be called.
    #[derive(Debug, Clone, Copy)]
    enum Case {
        Anonymous,
        Basic,
        DockerConfigEntry,
        DockerConfigNoEntry,
    }

    impl Case {
        async fn auth(self, server: &MockServer) -> Auth {
            match self {
                Self::Anonymous => Auth::anonymous(&url(server), crate::test_http_client()),
                Self::Basic => Auth::basic(&url(server), crate::test_http_client(), credentials()),
                Self::DockerConfigEntry => {
                    docker_config_auth(server, Some("dockeruser:dockersecret")).await
                }
                Self::DockerConfigNoEntry => docker_config_auth(server, None).await,
            }
        }

        fn sent(self) -> Option<String> {
            let userpass = match self {
                Self::Anonymous | Self::DockerConfigNoEntry => return None,
                Self::Basic => "testuser:testpass",
                Self::DockerConfigEntry => "dockeruser:dockersecret",
            };
            Some(format!("Basic {}", BASE64.encode(userpass)))
        }

        fn name(self) -> &'static str {
            match self {
                Self::Anonymous => "anonymous",
                Self::Basic => "basic",
                Self::DockerConfigEntry | Self::DockerConfigNoEntry => "docker-config",
            }
        }
    }

    /// Each sign-in keeps its name and sends its own credentials, and only
    /// those, to the token endpoint, and Debug never prints them. Breaks if
    /// anonymous sends credentials, a Docker config entry is looked up by
    /// anything but the registry as written, a provider sends credentials it
    /// does not hold, or Debug prints a username or password.
    #[tokio::test]
    async fn each_sign_in_sends_its_credentials() {
        for case in [
            Case::Anonymous,
            Case::Basic,
            Case::DockerConfigEntry,
            Case::DockerConfigNoEntry,
        ] {
            let server = MockServer::start().await;
            testing::challenge(&server, "test")
                .expect(1)
                .mount(&server)
                .await;
            mount_token(&server, "repository:repo:pull", "tok", 1).await;
            let auth = case.auth(&server).await;
            let expected = case.sent();

            assert_eq!(auth.name(), case.name(), "{case:?}");
            let debug = format!("{auth:?}");
            let shown = format!("has_credentials: {}", expected.is_some());
            assert!(
                debug.contains(case.name()) && debug.contains(&shown),
                "{case:?}: {debug}"
            );
            for secret in ["testuser", "testpass", "dockeruser", "dockersecret"] {
                assert!(!debug.contains(secret), "{case:?}: {debug}");
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
            assert_eq!(sent, expected.as_deref(), "{case:?}");
        }
    }

    /// Tokens are kept per scope: a repeated scope is served from the cache,
    /// while another repository or more actions on the same one exchange
    /// again, all from one `/v2/` challenge. Breaks if the provider skips
    /// the cache or keys it by anything but the full scope.
    #[tokio::test]
    async fn tokens_are_kept_per_scope() {
        let server = MockServer::start().await;
        testing::challenge(&server, "test")
            .expect(1)
            .mount(&server)
            .await;
        mount_token(&server, "repository:repo:pull", "pull", 1).await;
        mount_token(&server, "repository:repo:pull,push", "push", 1).await;
        mount_token(&server, "repository:other:pull", "other", 1).await;
        let auth = Auth::anonymous(&url(&server), crate::test_http_client());

        for (scope, expected) in [
            (Scope::pull("repo"), "pull"),
            (Scope::pull("repo"), "pull"),
            (Scope::pull_push("repo"), "push"),
            (Scope::pull("other"), "other"),
        ] {
            assert_eq!(auth.get_token(&[scope]).await.unwrap().value(), expected);
        }
    }

    /// The provider hands the token cache the token refused: a refusal of
    /// another token costs no request, and one of the cached token makes the
    /// next call sign in again. Breaks if the provider drops the refused
    /// token or never passes the refusal on.
    #[tokio::test]
    async fn a_refusal_drops_only_the_refused_token() {
        let server = MockServer::start().await;
        testing::challenge(&server, "test")
            .expect(2)
            .mount(&server)
            .await;
        mount_token(&server, "repository:repo:pull", "tok", 2).await;
        let auth = Auth::anonymous(&url(&server), crate::test_http_client());

        let refused = auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(Token::new("other")).await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        auth.refuse(refused).await;
        auth.get_token(&[Scope::pull("repo")]).await.unwrap();
        // expect(2) on `/v2/` and `/token`: the first call and the one after
        // the matching refusal, none after the other.
    }

    /// A token endpoint's refusal reaches the caller with its status.
    /// Breaks if the provider swallows the failure or replaces its error.
    #[tokio::test]
    async fn a_failed_exchange_returns_its_error() {
        let server = MockServer::start().await;
        testing::challenge(&server, "test")
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let auth = Auth::basic(&url(&server), crate::test_http_client(), credentials());

        let err = auth.get_token(&[Scope::pull("repo")]).await.unwrap_err();
        assert!(
            matches!(err, Error::TokenEndpoint { status, .. } if status == http::StatusCode::FORBIDDEN),
            "{err:?}"
        );
    }
}
