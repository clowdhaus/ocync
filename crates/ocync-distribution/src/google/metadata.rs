//! The GCE and GKE metadata server: the default service account's token,
//! when no credentials file is found.

use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;

use url::Url;

use super::{AccessToken, Call, Clients, Error, Scopes, expiring, send_json};
use crate::aws::{Clock, Env, Host, Platform, Timeouts};

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

/// The server `env` names with [`HOST_VARIABLE`], where an empty value is
/// unset, as in Go.
fn named_host(env: &Env) -> Option<String> {
    env.get(HOST_VARIABLE).filter(|host| !host.is_empty())
}

/// The file where Linux reports the machine's product name.
const PRODUCT_NAME_FILE: &str = "/sys/class/dmi/id/product_name";

/// The address Google's Go library `cloud.google.com/go/compute/metadata`
/// probes to tell whether a machine is on Google Cloud: the metadata server
/// by IP, which needs no DNS.
const PROBE_URL: &str = "http://169.254.169.254/";

/// The most a probe of [`PROBE_URL`] may take, connecting included. It is
/// the 2 second dial timeout of Go's default metadata client, which Go's
/// `testOnGCE` names as the bound on a machine off Google Cloud ("worst case
/// this should return when the ... Dial.Timeout fires (in two seconds)").
/// Go's client allows 5 seconds in all, but a metadata server answers
/// within the connection, so a longer wait only delays machines off Google
/// Cloud.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether `host` is on Google Cloud. Google's Go library
/// `cloud.google.com/go/compute/metadata` (`OnGCE`, `testOnGCE`) always
/// probes, by an HTTP request to the metadata IP raced against a DNS lookup
/// of `metadata.google.internal`, and reads the DMI product name only to
/// decide whether to wait for both answers. This differs in two ways. A
/// `GCE_METADATA_HOST` that is not empty, or the DMI product name `Google` or
/// `Google Compute Engine` (every Compute Engine and GKE node), is taken as
/// final with no probe, so those hosts never wait. Otherwise one probe asks
/// the metadata IP alone, which finds Cloud Run's first generation and Cloud
/// Functions, whose DMI is not Google's: see [`probe`]. A host whose name is
/// not read, as on Windows here, is probed like any other.
pub(crate) async fn is_on_google_cloud(host: &Host) -> bool {
    if named_host(&host.env).is_some() {
        return true;
    }
    let named = match host.platform {
        Platform::Windows => false,
        Platform::Unix => host
            .fs
            .read(std::path::Path::new(PRODUCT_NAME_FILE))
            .is_ok_and(|name| matches!(name.trim_ascii(), b"Google" | b"Google Compute Engine")),
    };
    named || probe(host).await
}

/// Whether a metadata server answers `host`'s network within
/// [`PROBE_TIMEOUT`]: one request, not sent again, whose answer carries `Metadata-Flavor:
/// Google`, as Go's `metadataRequestStrategy` requires. The EC2 instance
/// metadata service answers on the same IP without that header, so on EC2
/// this is `false`, as it is for a timeout, a refusal or any other answer.
/// No proxy and no redirect, so nothing leaves for another host.
async fn probe(host: &Host) -> bool {
    let client = crate::aws::credential_endpoint_client(Timeouts {
        connect: PROBE_TIMEOUT,
        read: PROBE_TIMEOUT,
    });
    let Ok(transport) = host.network.transport(client) else {
        return false;
    };
    let mut request = reqwest::Request::new(
        reqwest::Method::GET,
        Url::parse(PROBE_URL).expect("PROBE_URL is a URL"),
    );
    request
        .headers_mut()
        .insert(FLAVOR.0, http::HeaderValue::from_static(FLAVOR.1));
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, transport.send(request)).await,
        Ok(Ok(response)) if response
            .headers()
            .get(FLAVOR.0)
            .is_some_and(|flavor| flavor == FLAVOR.1)
    )
}

/// A replay answering the probe once with `headers`, as a host whose network
/// is it sees: the probe must carry `Metadata-Flavor: Google`.
#[cfg(test)]
pub(crate) fn probe_answer(headers: serde_json::Value) -> std::sync::Arc<crate::aws::Replay> {
    std::sync::Arc::new(crate::aws::Replay::of([crate::aws::record(
        0,
        &http::Method::GET,
        PROBE_URL,
        serde_json::json!({ FLAVOR.0: [FLAVOR.1] }),
        http::StatusCode::OK,
        headers,
        b"",
    )]))
}

/// `platform` with `vars` and the DMI `name`, whose network is `replay`.
#[cfg(test)]
pub(crate) fn probing(
    platform: Platform,
    vars: &[(&str, &str)],
    name: Option<&[u8]>,
    replay: &std::sync::Arc<crate::aws::Replay>,
) -> Host {
    let files: Vec<(&str, Vec<u8>)> = name
        .map(|name| (PRODUCT_NAME_FILE, name.to_vec()))
        .into_iter()
        .collect();
    let mut host = crate::google::tests::host(platform, vars, &files);
    host.network = crate::aws::Network::Replay(std::sync::Arc::clone(replay));
    host
}

/// The metadata server's token endpoint.
#[derive(Debug)]
pub(super) struct Server {
    /// The token URL, with the comma-joined scopes as its query unless the
    /// server's default is wanted.
    token_url: Url,
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
        let host = named_host(env);
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
        Ok(Self { token_url })
    }

    /// The default service account's token.
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
        .await?;
        expiring(response.access_token, clock.now(), response.expires_in)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use std::sync::Arc;

    use super::*;
    use crate::aws::{Replay, record};
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

    /// `GCE_METADATA_HOST` that is not empty, or a Google DMI product name,
    /// is final with no probe, and the host's network answers nothing, so a
    /// probe would fail the test. Breaks if either probes.
    #[tokio::test]
    async fn a_named_host_or_a_google_name_is_not_probed() {
        let none = Arc::new(Replay::of([]));
        for (vars, name) in [
            (&[(HOST_VARIABLE, "10.0.0.1")][..], None),
            (&[], Some(&b"Google Compute Engine\n"[..])),
            (&[], Some(&b"Google"[..])),
        ] {
            let host = probing(Platform::Unix, vars, name, &none);
            assert!(is_on_google_cloud(&host).await);
        }
    }

    /// Any other host is probed once, whatever its DMI name or platform, and
    /// is on Google Cloud only if the answer carries `Metadata-Flavor:
    /// Google`: not an answer without it, as the EC2 instance metadata
    /// service sends, nor one with another value. An empty
    /// `GCE_METADATA_HOST` is unset, as in Go. Breaks if a host is not
    /// probed, any answer counts, the header is not required, or Windows is
    /// taken to be on Google Cloud.
    #[tokio::test]
    async fn any_other_host_is_probed_for_the_google_flavor() {
        let google = serde_json::json!({"Metadata-Flavor": ["Google"]});
        let ec2 = serde_json::json!({"Server": ["EC2ws"]});
        let other = serde_json::json!({"Metadata-Flavor": ["Other"]});
        for (platform, vars, name, answer, expected) in [
            (Platform::Unix, &[][..], None, &google, true),
            (Platform::Unix, &[], Some(&b"Dell Inc."[..]), &google, true),
            (Platform::Unix, &[], Some(&b"Dell Inc."[..]), &ec2, false),
            (Platform::Unix, &[], Some(&b""[..]), &other, false),
            (
                Platform::Unix,
                &[],
                Some(&b"Google Compute Engine Plus"[..]),
                &ec2,
                false,
            ),
            (Platform::Unix, &[(HOST_VARIABLE, "")], None, &ec2, false),
            (Platform::Windows, &[], None, &google, true),
            (Platform::Windows, &[], None, &ec2, false),
        ] {
            let replay = probe_answer(answer.clone());
            let host = probing(platform, vars, name, &replay);
            assert_eq!(is_on_google_cloud(&host).await, expected, "{answer}");
            replay.assert_done();
        }
    }

    /// A probe nothing answers within the timeout says no, after waiting no
    /// longer than the timeout. Breaks if the wait is unbounded or a late
    /// answer counts.
    #[tokio::test(start_paused = true)]
    async fn a_probe_with_no_answer_in_time_says_no() {
        let replay = Arc::new(
            Replay::of([record(
                0,
                &http::Method::GET,
                PROBE_URL,
                serde_json::json!({ FLAVOR.0: [FLAVOR.1] }),
                http::StatusCode::OK,
                serde_json::json!({"Metadata-Flavor": ["Google"]}),
                b"",
            )])
            .delayed(0, PROBE_TIMEOUT * 5),
        );
        let host = probing(Platform::Unix, &[], None, &replay);
        let started = tokio::time::Instant::now();

        assert!(!is_on_google_cloud(&host).await);
        assert_eq!(started.elapsed(), PROBE_TIMEOUT);
    }

    /// An empty `GCE_METADATA_HOST` names no server, as in Go. Breaks if it
    /// fails to parse as a host.
    #[test]
    fn an_empty_host_variable_is_the_default_server() {
        let server = Server::new(&host_env(""), Scopes::Default).unwrap();
        assert_eq!(
            server.token_url.as_str(),
            format!("http://metadata.google.internal{TOKEN_PATH}")
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
