//! The GCE and GKE metadata server: the default service account's token,
//! when no credentials file is found.

use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;

use url::Url;

use super::{AccessToken, Call, Clients, Error, Scopes, expiring, send_json};
use crate::aws::{Clock, Env, Timeouts};

/// The variable naming the metadata server's `host[:port]`, without a
/// scheme.
pub(super) const HOST_VARIABLE: &str = "GCE_METADATA_HOST";

/// The metadata server's host when [`HOST_VARIABLE`] is unset.
pub(super) const DEFAULT_HOST: &str = "metadata.google.internal";

/// The path of the default service account's token.
const TOKEN_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/token";

/// The header every metadata request carries, and its value.
const FLAVOR: (&str, &str) = ("metadata-flavor", "Google");

/// Off Google Cloud nothing answers, so a connection is given a second, as
/// for the EC2 instance metadata service. GKE's metadata server can take
/// seconds to mint a Workload Identity token, so a read is given longer.
pub(super) const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(1),
    read: Duration::from_secs(10),
};

/// The metadata server's token endpoint.
#[derive(Debug)]
pub(super) struct Server {
    /// The token URL, with the comma-joined scopes as its query unless the
    /// server's default is wanted.
    token_url: Url,
    /// Whether the address is the default one rather than one
    /// [`HOST_VARIABLE`] names. Only a default address that never connects
    /// means there is no metadata server; a named one that does not is a
    /// source that failed.
    ambient: bool,
}

/// The token endpoint's answer.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(rename = "token_type")]
    _token_type: IgnoredAny,
    expires_in: Option<u64>,
}

impl Server {
    /// The server at the `host[:port]` [`HOST_VARIABLE`] names, else the
    /// default, asking for `scopes`. A value that is not a bare
    /// `host[:port]`, such as one with a path or user info, is an error.
    pub(super) fn new(env: &Env, scopes: Scopes) -> Result<Self, Error> {
        let host = env.get(HOST_VARIABLE);
        let ambient = host.is_none();
        let mut token_url = Url::parse(&format!(
            "http://{}",
            host.as_deref().unwrap_or(DEFAULT_HOST)
        ))
        .map_err(Error::MetadataHostInvalid)?;
        if token_url.path() != "/"
            || token_url.query().is_some()
            || token_url.fragment().is_some()
            || !token_url.username().is_empty()
            || token_url.password().is_some()
        {
            return Err(Error::MetadataHostNotBare);
        }
        token_url.set_path(TOKEN_PATH);
        if let Some(scopes) = scopes.registry_or_endpoint_default() {
            token_url
                .query_pairs_mut()
                .append_pair("scopes", &scopes.joined(","));
        }
        Ok(Self { token_url, ambient })
    }

    /// The default service account's token. A default address that never
    /// connects, after the retries every metadata call gets, is
    /// [`Error::NoSource`]: nothing here is a source to fail.
    pub(super) async fn access_token(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<AccessToken, Error> {
        let response: TokenResponse = send_json(clients, Call::Metadata, |client| {
            client
                .get(self.token_url.clone())
                .header(FLAVOR.0, FLAVOR.1)
        })
        .await
        .map_err(|e| {
            if self.ambient && e.is_connect_failure() {
                Error::NoSource(Box::new(e))
            } else {
                e
            }
        })?;
        expiring(response.access_token, clock.now(), response.expires_in)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::google::REGISTRY_SCOPE;

    const NOW: Duration = Duration::from_secs(1_700_000_000);

    fn server_env(server: &MockServer) -> Env {
        host_env(server.uri().trim_start_matches("http://"))
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + NOW
    }

    fn host_env(host: &str) -> Env {
        Env::Fixed(HashMap::from([(HOST_VARIABLE.to_owned(), host.to_owned())]))
    }

    /// Upstream `test_resolve_endpoint_default` and `_env_var`: the default
    /// server, and the variable's host with `http://` before it. A host that
    /// forms no URL, or a value with a path, query, fragment or user info,
    /// is an error, not a request elsewhere. Upstream accepts the latter.
    /// Breaks if the variable is ignored, given a scheme of its own, or not
    /// parsed.
    #[test]
    fn resolves_the_server() {
        let default = Server::new(&Env::Fixed(HashMap::new()), Scopes::Default).unwrap();
        assert_eq!(
            default.token_url.as_str(),
            format!("http://metadata.google.internal{TOKEN_PATH}")
        );
        let custom = Server::new(&host_env("10.0.0.1:8080/"), Scopes::Default).unwrap();
        assert_eq!(
            custom.token_url.as_str(),
            format!("http://10.0.0.1:8080{TOKEN_PATH}")
        );
        let error = Server::new(&host_env("bad host"), Scopes::Default).unwrap_err();
        assert!(matches!(error, Error::MetadataHostInvalid(_)), "{error}");
        for not_a_host in [
            "10.0.0.1:8080/x",
            "user:pw@10.0.0.1",
            "10.0.0.1?q",
            "10.0.0.1#f",
        ] {
            let error = Server::new(&host_env(not_a_host), Scopes::Default).unwrap_err();
            assert!(
                matches!(error, Error::MetadataHostNotBare),
                "{not_a_host}: {error}"
            );
        }
    }

    /// A default address that never connects is no credential source, but a
    /// named one that does not connect is a source that failed. Breaks if a
    /// broken source silently becomes anonymous, or an absent one stays an
    /// error.
    #[tokio::test(start_paused = true)]
    async fn only_an_unreachable_default_address_is_no_source() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let mut server = Server::new(&host_env(&closed.to_string()), Scopes::Registry).unwrap();
        let clients = Clients::new().unwrap();
        async fn fetch(server: &Server, clients: &Clients) -> Error {
            server
                .access_token(clients, Clock::Fixed(now()))
                .await
                .unwrap_err()
        }

        let named = fetch(&server, &clients).await;
        assert!(matches!(named, Error::Transport { .. }), "{named}");

        server.ambient = true;
        let default = fetch(&server, &clients).await;
        assert!(default.is_no_source(), "{default}");
    }

    /// A server that answers with an error is a source that failed, even at
    /// the default address. Breaks if any answer counts as no source.
    #[tokio::test]
    async fn an_answering_server_is_a_source() {
        let answering = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&answering)
            .await;
        let mut server = Server::new(&server_env(&answering), Scopes::Registry).unwrap();
        server.ambient = true;

        let error = server
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap_err();

        assert!(matches!(error, Error::Status { .. }), "{error:?}");
    }

    /// Upstream `token_provider_full` and `headers_success_with_quota_project`:
    /// the flavor header, the scopes as a query, and the expiry from
    /// `expires_in`. Breaks if the header or scope is not sent, or the expiry
    /// is not read.
    #[tokio::test]
    async fn fetches_the_token_with_scopes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(TOKEN_PATH))
            .and(header("metadata-flavor", "Google"))
            .and(query_param("scopes", REGISTRY_SCOPE))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.mds", "token_type": "Bearer", "expires_in": 3599
            })))
            .expect(1)
            .mount(&server)
            .await;

        let token = Server::new(&server_env(&server), Scopes::Registry)
            .unwrap()
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap();
        assert_eq!(token.token.expose(), "ya29.mds");
        assert_eq!(
            token.expiry,
            crate::google::Expiry::At(now() + Duration::from_secs(3599))
        );
    }

    /// Upstream `token_provider_full_no_scopes`: no scopes query when none is
    /// asked for, and a token with no `expires_in` has no expiry. Breaks if
    /// an empty scope is sent or an expiry made up.
    #[tokio::test]
    async fn sends_no_scopes_for_the_default() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(TOKEN_PATH))
            .and(query_param_is_missing("scopes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.mds", "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let token = Server::new(&server_env(&server), Scopes::Default)
            .unwrap()
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap();
        assert_eq!(token.expiry, crate::google::Expiry::Never);
    }
}
