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

/// The file where Linux reports the machine's product name.
pub(super) const PRODUCT_NAME_FILE: &str = "/sys/class/dmi/id/product_name";

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

/// A test host's probe address, in place of [`PROBE_URL`]. A test host
/// without it is not probed, so no test reaches the network. Read only in
/// tests.
pub(super) const PROBE_OVERRIDE: &str = "OCYNC_TEST_GOOGLE_PROBE";

/// Whether `host` is on Google Cloud, as Google's Go library
/// `cloud.google.com/go/compute/metadata` decides in `OnGCE` and
/// `testOnGCE`: `GCE_METADATA_HOST` set means the user said so, and the DMI
/// product name `Google` or `Google Compute Engine` (every Compute Engine
/// and GKE node) means it is. A name that says otherwise is not final,
/// because Cloud Run's first generation and Cloud Functions have no Google
/// DMI but do have a metadata server, so one probe asks the metadata IP
/// whether it is there (see [`probe`]). Go races that request against a DNS
/// lookup of `metadata.google.internal` and takes the first answer; this
/// asks the IP alone. Windows, where this does not read the registry, is
/// taken to be on it so the metadata server is asked, as upstream does.
pub(super) async fn is_on_google_cloud(host: &Host) -> bool {
    if host.env.get(HOST_VARIABLE).is_some() {
        return true;
    }
    match host.platform {
        Platform::Windows => true,
        Platform::Unix => {
            let named = host
                .fs
                .read(std::path::Path::new(PRODUCT_NAME_FILE))
                .is_ok_and(|name| {
                    matches!(name.trim_ascii(), b"Google" | b"Google Compute Engine")
                });
            named || probe(probe_url(host), PROBE_TIMEOUT).await
        }
    }
}

/// The address to probe, or `None` for no probe.
fn probe_url(host: &Host) -> Option<Url> {
    let default = Url::parse(PROBE_URL).expect("PROBE_URL is a URL");
    if cfg!(test) {
        host.env
            .get(PROBE_OVERRIDE)
            .map(|url| Url::parse(&url).expect("a probe URL"))
    } else {
        Some(default)
    }
}

/// Whether a metadata server answers `url` within `timeout`: one request,
/// not sent again, whose answer carries `Metadata-Flavor: Google`, as Go's
/// `metadataRequestStrategy` requires. The EC2 instance metadata service
/// answers on the same IP without that header, so on EC2 this is `false`, as
/// it is for a timeout, a refusal or any other answer. No proxy and no
/// redirect, so nothing leaves for another host.
async fn probe(url: Option<Url>, timeout: Duration) -> bool {
    let Some(url) = url else {
        return false;
    };
    crate::install_crypto_provider();
    let Ok(client) = crate::aws::credential_endpoint_client(Timeouts {
        connect: timeout,
        read: timeout,
    })
    .timeout(timeout)
    .build() else {
        return false;
    };
    client
        .get(url)
        .header(FLAVOR.0, FLAVOR.1)
        .send()
        .await
        .is_ok_and(|response| {
            response
                .headers()
                .get(FLAVOR.0)
                .is_some_and(|flavor| flavor == FLAVOR.1)
        })
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
        let host = env.get(HOST_VARIABLE);
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

    /// A host is on Google Cloud when `GCE_METADATA_HOST` is set or its DMI
    /// product name is Google's, and off it otherwise, whatever else the
    /// name says, when nothing answers a probe. Breaks if a rule is dropped,
    /// a near name matches, or the name is read from the real file system.
    #[tokio::test]
    async fn residency_follows_googles_rule() {
        let on = |vars: &[(&str, &str)], name: Option<&[u8]>| {
            let files: Vec<(&str, Vec<u8>)> = name
                .map(|name| (PRODUCT_NAME_FILE, name.to_vec()))
                .into_iter()
                .collect();
            let host = crate::google::tests::host(Platform::Unix, vars, &files);
            async move { is_on_google_cloud(&host).await }
        };
        assert!(on(&[(HOST_VARIABLE, "10.0.0.1")], None).await);
        assert!(on(&[], Some(b"Google Compute Engine\n")).await);
        assert!(on(&[], Some(b"Google")).await);
        assert!(!on(&[], None).await);
        assert!(!on(&[], Some(b"Dell Inc.")).await);
        assert!(!on(&[], Some(b"Google Compute Engine Plus")).await);
        assert!(!on(&[], Some(b"")).await);
        assert!(is_on_google_cloud(&crate::google::tests::host(Platform::Windows, &[], &[])).await);
    }

    /// A machine whose DMI is not Google's is on Google Cloud when the
    /// metadata IP answers with `Metadata-Flavor: Google`, as Cloud Run and
    /// Cloud Functions do, and not when it answers without it, as the EC2
    /// instance metadata service does, or answers nothing in time or at all.
    /// Breaks if any answer counts, the header is not required, the
    /// request is sent again, or the wait outlasts the timeout.
    #[tokio::test]
    async fn a_probe_needs_the_google_flavor_header() {
        let probed = |server: &MockServer| {
            let url = server.uri();
            let host = crate::google::tests::host(Platform::Unix, &[(PROBE_OVERRIDE, &url)], &[]);
            async move { is_on_google_cloud(&host).await }
        };

        let google = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("metadata-flavor", "Google"))
            .respond_with(ResponseTemplate::new(200).insert_header("Metadata-Flavor", "Google"))
            .expect(1)
            .mount(&google)
            .await;
        assert!(probed(&google).await);

        let ec2 = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("Server", "EC2ws"))
            .expect(1)
            .mount(&ec2)
            .await;
        assert!(!probed(&ec2).await);

        let wrong = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("Metadata-Flavor", "Other"))
            .expect(1)
            .mount(&wrong)
            .await;
        assert!(!probed(&wrong).await);

        let slow = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Metadata-Flavor", "Google")
                    .set_delay(Duration::from_secs(2)),
            )
            .expect(1)
            .mount(&slow)
            .await;
        let started = std::time::Instant::now();
        assert!(!probe(Url::parse(&slow.uri()).ok(), Duration::from_millis(200)).await);
        assert!(started.elapsed() < Duration::from_secs(1));

        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/", listener.local_addr().unwrap())
        };
        assert!(!probe(Url::parse(&closed).ok(), PROBE_TIMEOUT).await);
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
