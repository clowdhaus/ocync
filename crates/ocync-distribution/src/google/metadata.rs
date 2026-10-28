//! The GCE and GKE metadata server: the default service account's token,
//! when no credentials file is found.

use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::{AccessToken, Call, Clients, Error, Scopes, expiring, send_json};
use crate::aws::{Clock, Env, Timeouts};

/// The variable naming the metadata server's `host[:port]`, without a
/// scheme.
const HOST_VARIABLE: &str = "GCE_METADATA_HOST";

/// The metadata server when [`HOST_VARIABLE`] is unset.
const DEFAULT_ROOT: &str = "http://metadata.google.internal";

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
pub(crate) struct Metadata {
    /// The server, `http://` and the host, with no trailing slash.
    root: String,
    /// The comma-joined scopes, or `None` for the server's default.
    scopes: Option<String>,
}

/// The token endpoint's answer.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(rename = "token_type")]
    _token_type: IgnoredAny,
    expires_in: Option<u64>,
}

impl Metadata {
    /// The server [`HOST_VARIABLE`] names, else the default, asking for
    /// `scopes`.
    pub(super) fn new(env: &Env, scopes: Scopes) -> Self {
        let root = env
            .get(HOST_VARIABLE)
            .map_or_else(|| DEFAULT_ROOT.to_owned(), |host| format!("http://{host}"));
        Self {
            root: root.trim_end_matches('/').to_owned(),
            scopes: scopes
                .registry_or_endpoint_default()
                .map(|scopes| scopes.joined(",")),
        }
    }

    /// The default service account's token, tried once, as upstream.
    pub(super) async fn access_token(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<AccessToken, Error> {
        let mut request = clients
            .metadata
            .get(format!("{}{TOKEN_PATH}", self.root))
            .header(FLAVOR.0, FLAVOR.1);
        if let Some(scopes) = &self.scopes {
            request = request.query(&[("scopes", scopes)]);
        }
        let response: TokenResponse = send_json(Call::Metadata, request).await?;
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
        let host = server.uri().trim_start_matches("http://").to_owned();
        Env::Fixed(HashMap::from([(HOST_VARIABLE.to_owned(), host)]))
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + NOW
    }

    /// Upstream `test_resolve_endpoint_default` and `_env_var`: the default
    /// server, and the variable's host with `http://` before it. Breaks if
    /// the variable is ignored or given a scheme of its own.
    #[test]
    fn resolves_the_server() {
        let default = Metadata::new(&Env::Fixed(HashMap::new()), Scopes::Default);
        assert_eq!(default.root, "http://metadata.google.internal");
        let env = Env::Fixed(HashMap::from([(
            HOST_VARIABLE.to_owned(),
            "10.0.0.1:8080/".to_owned(),
        )]));
        assert_eq!(
            Metadata::new(&env, Scopes::Default).root,
            "http://10.0.0.1:8080"
        );
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

        let token = Metadata::new(&server_env(&server), Scopes::Registry)
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap();
        assert_eq!(token.token.expose(), "ya29.mds");
        assert_eq!(token.expiry, Some(now() + Duration::from_secs(3599)));
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

        let token = Metadata::new(&server_env(&server), Scopes::Default)
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap();
        assert_eq!(token.expiry, None);
    }

    /// Upstream `credentials_headers_retryable_error`, `_nonretryable_error`
    /// and `_malformed_response_is_nonretryable`: a 503 may be sent again, a
    /// 401 and an answer that does not decode may not. Breaks if the status
    /// rules or the decode rule change.
    #[tokio::test]
    async fn classifies_failures() {
        for (status, body, transient) in [
            (503, "try again", true),
            (401, "no", false),
            (200, "not json", false),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            let error = Metadata::new(&server_env(&server), Scopes::Default)
                .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
                .await
                .unwrap_err();
            assert_eq!(error.is_transient(), transient, "{status}: {error}");
        }
    }

    /// A host with no metadata server fails at once and is not tried again,
    /// as upstream tries it once. Breaks if a refused connection becomes
    /// transient for the metadata server.
    #[tokio::test]
    async fn a_refused_connection_is_final() {
        let env = Env::Fixed(HashMap::from([(
            HOST_VARIABLE.to_owned(),
            "127.0.0.1:1".to_owned(),
        )]));
        let error = Metadata::new(&env, Scopes::Default)
            .access_token(&Clients::new().unwrap(), Clock::Fixed(now()))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Transport { .. }), "{error}");
        assert!(!error.is_transient());
    }
}
