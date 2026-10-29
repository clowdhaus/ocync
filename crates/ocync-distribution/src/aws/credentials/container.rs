//! Credentials from the container endpoint, as the SDK loads them for ECS
//! task roles and EKS Pod Identity: a GET to the URL the environment names,
//! with the authorization token it names, answered with the JSON document
//! the instance metadata service also answers with.
//!
//! A full URI over plain HTTP must name an address the container agent
//! listens on, so the token never goes in the clear to any other host. The
//! endpoint and the token are read on every load, since EKS rotates the
//! token file.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use http::header::{ACCEPT, AUTHORIZATION, InvalidHeaderValue};
use http::uri::InvalidUri;
use http::{HeaderValue, Method, StatusCode, Uri};
use url::Url;

use super::super::TRANSIENT_STATUSES;
use super::super::TransportError;
use super::super::host::{
    Dns, Env, Fs, Host, Network, Timeouts, Transport, credential_endpoint_client,
};
use super::super::retry::{self, Kind, Retryable};
use super::json::{self, Code, Document};
use super::{Credentials, Error as ChainError};
use crate::retry::retrying;
use crate::scheme::{Scheme, UnsupportedScheme};

/// The path and query of the ECS agent's endpoint, which wins over
/// [`FULL_URI`].
const RELATIVE_URI: &str = "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI";

/// The whole URL of the endpoint.
const FULL_URI: &str = "AWS_CONTAINER_CREDENTIALS_FULL_URI";

/// The authorization token.
const AUTHORIZATION_TOKEN: &str = "AWS_CONTAINER_AUTHORIZATION_TOKEN";

/// A file holding the authorization token, which wins over
/// [`AUTHORIZATION_TOKEN`].
const AUTHORIZATION_TOKEN_FILE: &str = "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE";

/// The ECS agent, which a relative URI is under.
const ECS_AGENT: Ipv4Addr = Ipv4Addr::new(169, 254, 170, 2);

/// The EKS Pod Identity agent over IPv4.
const EKS_AGENT_V4: Ipv4Addr = Ipv4Addr::new(169, 254, 170, 23);

/// The EKS Pod Identity agent over IPv6, `fd00:ec2::23`.
const EKS_AGENT_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x23);

/// The media type asked for.
const JSON: &str = "application/json";

/// A connection not made within 2 seconds fails, and so does a read that
/// waits more than 5.
const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(2),
    read: Duration::from_secs(5),
};

/// Why the source is not configured, as upstream words it.
const NOT_CONFIGURED: &str = "No environment variables were set to configure ECS provider";

/// Why a full URI is refused.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum FullUriError {
    /// The text is not a URL.
    #[error("URI was invalid")]
    Invalid(#[source] url::ParseError),
    /// The URL names no host.
    #[error("URI did not specify a host")]
    MissingHost,
    /// The URL's scheme is neither `http` nor `https`.
    #[error(transparent)]
    NotHttp(UnsupportedScheme),
    /// The host name resolves to no address.
    #[error("DNS lookup returned no addresses")]
    NoAddresses,
    /// A plain HTTP URL whose host is neither loopback nor a container
    /// agent.
    #[error("URI did not refer to an allowed IP address")]
    DisallowedIp,
    /// The host name does not resolve.
    #[error("failed to perform DNS lookup while validating URI")]
    DnsLookupFailed(#[source] io::Error),
}

/// Why the endpoint settings do not resolve. Each names its cause in its
/// text and as its source, as upstream does.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum EndpointError {
    /// The relative URI does not parse.
    #[error("invalid relative URI for ECS provider ({err}): {uri}")]
    Relative {
        /// Why.
        #[source]
        err: InvalidUri,
        /// The setting.
        uri: String,
    },
    /// The full URI is refused.
    #[error("invalid full URI for ECS provider ({err}): {uri}")]
    Full {
        /// Why.
        #[source]
        err: FullUriError,
        /// The setting.
        uri: String,
    },
}

/// Why the source failed.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The endpoint settings do not resolve.
    #[error(transparent)]
    Endpoint(EndpointError),
    /// The token file does not read.
    #[error("could not read the auth token file {}", path.display())]
    TokenFile {
        /// The file.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The token is not a header value.
    #[error("the auth token could not be used as an HTTP header value. {0}")]
    InvalidToken(#[source] InvalidHeaderValue),
    /// The client pinned to a host's checked addresses could not be built.
    #[error("could not build the HTTP client for the container endpoint")]
    Client(#[source] reqwest::Error),
    /// The request did not complete.
    #[error("could not communicate with the container endpoint")]
    Transport(#[source] TransportError),
    /// A failing status.
    #[error("Non-success status from HTTP credential provider: {0:?}")]
    Status(StatusCode),
    /// The credentials document does not read.
    #[error(transparent)]
    Json(json::Error),
    /// An error document in place of credentials.
    #[error("failed to load credentials [{code}]: {message}")]
    Document {
        /// The error code.
        code: Code,
        /// The error message.
        message: String,
    },
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.into())
    }
}

/// The SDK retries a transport failure, a 500, 502, 503 or 504, and a
/// success whose body does not read.
impl Retryable for Error {
    fn retry_kind(&self) -> Option<Kind> {
        match self {
            Self::Transport(e) => e.is_transient().then_some(Kind::Transient),
            Self::Status(status) => TRANSIENT_STATUSES
                .contains(status)
                .then_some(Kind::Transient),
            Self::Json(_) => Some(Kind::Server),
            Self::Endpoint(_)
            | Self::Client(_)
            | Self::TokenFile { .. }
            | Self::InvalidToken(_)
            | Self::Document { .. } => None,
        }
    }
}

/// Where the environment points the source.
enum Endpoint {
    /// Neither URI is set.
    NotConfigured,
    /// A URI is set and refused.
    Misconfigured(EndpointError),
    /// Where requests go.
    Ready(Target),
}

/// The endpoint's URL, and how a request reaches it.
enum Target {
    /// Through the source's client, which resolves the host itself: the
    /// ECS agent, an address plain HTTP may reach, or HTTPS, whose
    /// certificate names the host.
    Direct(Url),
    /// Plain HTTP to a host name, connected only to the addresses checked.
    Pinned(Pinned),
}

impl Target {
    /// The URL requests go to.
    fn url(&self) -> &Url {
        match self {
            Self::Direct(url) | Self::Pinned(Pinned { url, .. }) => url,
        }
    }
}

/// A plain HTTP URL whose host is a name, with the addresses that name
/// resolved to. Built only by [`full`].
struct Pinned {
    url: Url,
    addresses: PlainHttpAddresses,
}

impl Pinned {
    /// The client settings that connect the URL's host to the checked
    /// addresses alone. The client takes the port from the URL, so each
    /// address is given port 0.
    fn client(&self) -> reqwest::ClientBuilder {
        let host = self.url.host_str().expect("a pinned URL names its host");
        let addresses: Vec<_> = self
            .addresses
            .0
            .iter()
            .map(|&ip| SocketAddr::new(ip, 0))
            .collect();
        credential_endpoint_client(TIMEOUTS).resolve_to_addrs(host, &addresses)
    }
}

/// The addresses a host name resolved to, when plain HTTP may carry the
/// token to them: at least one, and each loopback or a container agent.
struct PlainHttpAddresses(Vec<IpAddr>);

/// Refuses an address plain HTTP may not reach, with a warning naming it,
/// and an empty answer, which upstream lets through.
impl TryFrom<Vec<IpAddr>> for PlainHttpAddresses {
    type Error = FullUriError;

    fn try_from(addresses: Vec<IpAddr>) -> Result<Self, FullUriError> {
        if let Some(ip) = addresses.iter().find(|&&ip| !allows_plain_http(ip)) {
            tracing::warn!(
                %ip,
                "HTTP credential provider cannot be used: Address does not resolve to an allowed IP."
            );
            return Err(FullUriError::DisallowedIp);
        }
        if addresses.is_empty() {
            return Err(FullUriError::NoAddresses);
        }
        Ok(Self(addresses))
    }
}

/// The authorization token, marked sensitive so it never prints: its
/// `Debug` shows `Sensitive`.
#[derive(Debug)]
struct AuthorizationToken(HeaderValue);

impl AuthorizationToken {
    fn new(mut value: HeaderValue) -> Self {
        value.set_sensitive(true);
        Self(value)
    }
}

/// Where a token was read from, for the warning about one that is not a
/// header value.
#[derive(Debug, Clone, Copy)]
enum TokenSource {
    File,
    Environment,
}

/// The container endpoint source.
pub(super) struct Provider {
    env: Env,
    fs: Fs,
    dns: Dns,
    network: Network,
    /// The client for a [`Target::Direct`] endpoint.
    transport: Transport,
    retry: retry::Strategy,
    partition: retry::Partition,
}

impl Provider {
    /// The source `host` configures.
    pub(super) fn new(host: &Host) -> Result<Self, reqwest::Error> {
        Ok(Self {
            env: host.env.clone(),
            fs: host.fs.clone(),
            dns: host.dns.clone(),
            transport: host
                .network
                .transport(credential_endpoint_client(TIMEOUTS))?,
            network: host.network.clone(),
            retry: retry::Strategy::new(&retry::Settings::default()),
            partition: retry::Partition::default(),
        })
    }

    /// The credentials the endpoint answers with. The token is read first,
    /// so a token file that does not read fails the load even with no
    /// endpoint set, as it does upstream.
    pub(super) async fn load(&self) -> Result<Credentials, ChainError> {
        let token = authorization(&self.env, &self.fs).map_err(ChainError::Container)?;
        let target = match endpoint(&self.env, &self.dns).await {
            Endpoint::NotConfigured => {
                return Err(ChainError::NotLoaded(NOT_CONFIGURED.to_owned()));
            }
            Endpoint::Misconfigured(e) => return Err(ChainError::Container(Error::Endpoint(e))),
            Endpoint::Ready(target) => target,
        };
        let pinned_transport;
        let transport = match &target {
            Target::Direct(_) => &self.transport,
            Target::Pinned(pinned) => {
                pinned_transport = self
                    .network
                    .transport(pinned.client())
                    .map_err(|e| ChainError::Container(Error::Client(e)))?;
                &pinned_transport
            }
        };
        let url = target.url();
        tracing::debug!("loading credentials from the container endpoint");
        let mut policy = retry::CallPolicy::new(&self.retry, &self.partition);
        retrying("container credentials request", &mut policy, |_| async {
            let response = transport
                .send(request(url, token.as_ref()))
                .await
                .map_err(Error::from)?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Status(status));
            }
            let body = response.bytes().await.map_err(Error::from)?;
            match json::parse(&body).map_err(Error::Json)? {
                Document::Credentials(credentials) => Ok(credentials),
                Document::Error { code, message } => Err(Error::Document { code, message }),
            }
        })
        .await
        .map_err(ChainError::Container)
    }
}

/// The GET to `url`, asking for JSON, with `token` when there is one.
fn request(url: &Url, token: Option<&AuthorizationToken>) -> reqwest::Request {
    let mut request = reqwest::Request::new(Method::GET, url.clone());
    let headers = request.headers_mut();
    headers.insert(ACCEPT, HeaderValue::from_static(JSON));
    if let Some(AuthorizationToken(token)) = token {
        headers.insert(AUTHORIZATION, token.clone());
    }
    request
}

/// The authorization token: the file's contents when
/// `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` is set, else
/// `AWS_CONTAINER_AUTHORIZATION_TOKEN`. A token that is not a header value
/// is refused with a warning that gives its length and whether it ends in
/// whitespace, never the token.
fn authorization(env: &Env, fs: &Fs) -> Result<Option<AuthorizationToken>, Error> {
    let (token, source) = if let Some(path) = env.get(AUTHORIZATION_TOKEN_FILE) {
        let path = PathBuf::from(path);
        let token = fs
            .read(&path)
            .map_err(|source| Error::TokenFile { path, source })?;
        (token, TokenSource::File)
    } else if let Some(token) = env.get(AUTHORIZATION_TOKEN) {
        (token.into_bytes(), TokenSource::Environment)
    } else {
        return Ok(None);
    };
    let token = HeaderValue::from_bytes(&token).map_err(|error| {
        tracing::warn!(
            ?source,
            token_length = token.len(),
            ends_with_whitespace = token.last().is_some_and(u8::is_ascii_whitespace),
            "invalid container auth token"
        );
        Error::InvalidToken(error)
    })?;
    Ok(Some(AuthorizationToken::new(token)))
}

/// The endpoint the environment names: the relative URI under the ECS
/// agent, else the full URI.
async fn endpoint(env: &Env, dns: &Dns) -> Endpoint {
    let resolved = if let Some(uri) = env.get(RELATIVE_URI) {
        relative(&uri)
            .map(Target::Direct)
            .map_err(|err| EndpointError::Relative { err, uri })
    } else if let Some(uri) = env.get(FULL_URI) {
        full(&uri, dns)
            .await
            .map_err(|err| EndpointError::Full { err, uri })
    } else {
        return Endpoint::NotConfigured;
    };
    match resolved {
        Ok(target) => Endpoint::Ready(target),
        Err(e) => Endpoint::Misconfigured(e),
    }
}

/// `uri`'s path and query under the ECS agent. Any scheme or host it
/// names is dropped, as upstream drops them.
fn relative(uri: &str) -> Result<Url, InvalidUri> {
    let parsed: Uri = uri.parse().inspect_err(|error| {
        tracing::warn!(%error, "invalid container credentials URI loaded from environment");
    })?;
    let mut url = Url::parse(&format!("http://{ECS_AGENT}")).expect("an IPv4 address is a host");
    if let Some(path_and_query) = parsed.path_and_query() {
        url.set_path(path_and_query.path());
        url.set_query(path_and_query.query());
    }
    Ok(url)
}

/// `uri` when it is HTTPS, or plain HTTP to a host whose every address is
/// a container agent or loopback. The host checked is the one the request
/// goes to, as the URL parser reads it. A host name must resolve to at
/// least one address, and the request is pinned to the addresses checked,
/// so a lookup that answers differently at connect time cannot redirect
/// the token. Any other scheme is refused here, where upstream lets the
/// request fail.
async fn full(uri: &str, dns: &Dns) -> Result<Target, FullUriError> {
    let url = Url::parse(uri).map_err(|e| match e {
        url::ParseError::RelativeUrlWithoutBase | url::ParseError::EmptyHost => {
            FullUriError::MissingHost
        }
        other => FullUriError::Invalid(other),
    })?;
    match Scheme::try_from(&url).map_err(FullUriError::NotHttp)? {
        Scheme::Https => return Ok(Target::Direct(url)),
        Scheme::Http => {}
    }
    let ip = match url.host() {
        None => return Err(FullUriError::MissingHost),
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(url::Host::Domain(name)) => {
            let addresses = dns
                .resolve(name)
                .await
                .map_err(FullUriError::DnsLookupFailed)?;
            let addresses = PlainHttpAddresses::try_from(addresses)?;
            return Ok(Target::Pinned(Pinned { url, addresses }));
        }
    };
    if allows_plain_http(ip) {
        Ok(Target::Direct(url))
    } else {
        Err(FullUriError::DisallowedIp)
    }
}

/// Whether plain HTTP may carry the token to `ip`: loopback, or a container
/// agent.
fn allows_plain_http(ip: IpAddr) -> bool {
    ip.is_loopback()
        || ip == IpAddr::V4(ECS_AGENT)
        || ip == IpAddr::V4(EKS_AGENT_V4)
        || ip == IpAddr::V6(EKS_AGENT_V6)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use serde::Deserialize;
    use serde_json::json;

    use super::super::super::replay::{Recorded, Replay, record};
    use super::super::fixtures::DATA;
    use super::*;
    use crate::error::error_chain;
    use crate::test_logs::capture_logs;

    /// The relative URI the tests set.
    const RELATIVE: &str = "/v2/credentials/task";

    /// A credentials document.
    const DOCUMENT: &str = r#"{"AccessKeyId": "ASIARTEST", "SecretAccessKey": "testsecret",
        "Token": "testtoken", "Expiration": "2021-09-21T04:16:53Z"}"#;

    /// A Unix host with `vars` set and `files`, where `names` resolve, on
    /// `network`.
    fn host(
        vars: &[(&str, &str)],
        files: &[(&str, &[u8])],
        names: &[(&str, &[IpAddr])],
        network: Network,
    ) -> Host {
        let own = |(k, v): &(&str, &str)| ((*k).to_owned(), (*v).to_owned());
        Host {
            env: Env::Fixed(vars.iter().map(own).collect()),
            fs: Fs::Fixed(
                files
                    .iter()
                    .map(|(path, body)| (PathBuf::from(path), body.to_vec()))
                    .collect(),
            ),
            network,
            dns: Dns::Fixed(
                names
                    .iter()
                    .map(|(name, ips)| ((*name).to_owned(), ips.to_vec()))
                    .collect(),
            ),
            ..Host::fixed()
        }
    }

    /// A host with [`RELATIVE`] set, answering with `exchanges`.
    fn replaying(exchanges: impl IntoIterator<Item = Recorded>) -> (Host, Arc<Replay>) {
        let replay = Arc::new(Replay::of(exchanges));
        let network = Network::Replay(Arc::clone(&replay));
        (host(&[(RELATIVE_URI, RELATIVE)], &[], &[], network), replay)
    }

    /// A GET of [`RELATIVE`] answered with `status` and `body`.
    fn answer(id: u64, status: StatusCode, body: &[u8]) -> Recorded {
        let uri = format!("http://{ECS_AGENT}{RELATIVE}");
        let headers = json!({ "accept": [JSON] });
        record(id, &Method::GET, &uri, headers, status, json!({}), body)
    }

    /// The URL or error text `vars` resolve to, as upstream's cases check it.
    async fn resolved(
        vars: &[(&str, &str)],
        names: &[(&str, &[IpAddr])],
    ) -> Result<String, String> {
        let host = host(vars, &[], names, Network::Real);
        match endpoint(&host.env, &host.dns).await {
            Endpoint::NotConfigured => Err(NOT_CONFIGURED.to_owned()),
            Endpoint::Misconfigured(e) => Err(error_chain(&e)),
            Endpoint::Ready(target) => Ok(target.url().as_str().to_owned()),
        }
    }

    /// `localhost` as upstream's test resolver answers it.
    const LOCALHOST: (&str, &[IpAddr]) = (
        "localhost",
        &[
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
        ],
    );

    /// The upstream endpoint cases: the relative URI under the ECS agent
    /// and winning over the full URI, HTTPS to any host, and plain HTTP to
    /// loopback or an agent only. Breaks if a case resolves elsewhere.
    #[tokio::test(flavor = "current_thread")]
    async fn resolves_the_endpoint_as_upstream_does() {
        #[derive(Deserialize)]
        enum Expected {
            Ok(String),
            ErrorContains(String),
        }
        #[derive(Deserialize)]
        struct Case {
            docs: String,
            env: HashMap<String, String>,
            result: Expected,
        }
        #[derive(Deserialize)]
        struct Cases {
            tests: Vec<Case>,
        }
        let path = Path::new(DATA).join("ecs-tests.json");
        let cases: Cases = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(cases.tests.len(), 11);
        for case in cases.tests {
            let vars: Vec<_> = case
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let got = resolved(&vars, &[LOCALHOST]).await;
            let docs = &case.docs;
            match case.result {
                Expected::Ok(url) => assert_eq!(got, Ok(url), "{docs}"),
                Expected::ErrorContains(text) => {
                    let err = got.expect_err(docs);
                    assert!(err.contains(&text), "{docs}: {err}");
                }
            }
        }
    }

    /// Plain HTTP goes only to a host whose every address is loopback or an
    /// agent, a name that resolves to nothing or does not resolve is
    /// refused, and so is any scheme but `http` and `https`. Breaks if one
    /// allowed address admits a host, an empty answer passes, another
    /// scheme is taken as plain HTTP, or the IP check is skipped.
    #[tokio::test(flavor = "current_thread")]
    async fn sends_plain_http_only_to_an_agent() {
        let mixed: &[IpAddr] = &[IpAddr::V4(Ipv4Addr::LOCALHOST), "10.0.0.1".parse().unwrap()];
        let agent: &[IpAddr] = &[IpAddr::V4(EKS_AGENT_V4)];
        let names = [
            ("mixed.test", mixed),
            ("agent.test", agent),
            ("empty.test", &[]),
        ];
        let refused = "URI did not refer to an allowed IP address";
        for (uri, expected) in [
            ("http://10.0.0.1/creds", Err(refused)),
            ("http://mixed.test/creds", Err(refused)),
            (
                "http://empty.test/creds",
                Err("DNS lookup returned no addresses"),
            ),
            (
                "ftp://169.254.170.2/creds",
                Err("unsupported scheme 'ftp', not http or https"),
            ),
            (
                "http://unknown.test/creds",
                Err("failed to perform DNS lookup"),
            ),
            ("http://[::1/creds", Err("URI was invalid")),
            ("http://[::1]:8080/creds", Ok("http://[::1]:8080/creds")),
            ("http://agent.test/creds", Ok("http://agent.test/creds")),
            ("https://10.0.0.1/creds", Ok("https://10.0.0.1/creds")),
        ] {
            let got = resolved(&[(FULL_URI, uri)], &names).await;
            match expected {
                Ok(url) => assert_eq!(got.as_deref(), Ok(url), "{uri}"),
                Err(text) => {
                    let err = got.expect_err(uri);
                    assert!(err.contains(text), "{uri}: {err}");
                }
            }
        }
    }

    /// A relative URI keeps only its path and query, and one that does not
    /// parse is refused. Breaks if a host it names is used.
    #[tokio::test(flavor = "current_thread")]
    async fn keeps_only_the_path_of_a_relative_uri() {
        let got = resolved(&[(RELATIVE_URI, "http://other.test/creds?a=1")], &[]).await;
        assert_eq!(got.as_deref(), Ok("http://169.254.170.2/creds?a=1"));
        let err = resolved(&[(RELATIVE_URI, "")], &[]).await.unwrap_err();
        assert_eq!(
            err,
            "invalid relative URI for ECS provider (empty string): : empty string"
        );
    }

    /// The token comes from the file before the variable, and an invalid
    /// token is refused with neither the error nor the logs carrying it.
    /// Breaks if the variable wins or the token is logged.
    #[tokio::test(flavor = "current_thread")]
    async fn reads_the_token_file_before_the_variable() {
        let read = |vars: &[(&str, &str)]| {
            let host = host(vars, &[("/token", b"from-file\xe9")], &[], Network::Real);
            authorization(&host.env, &host.fs)
        };
        let file = read(&[
            (AUTHORIZATION_TOKEN_FILE, "/token"),
            (AUTHORIZATION_TOKEN, "from-env"),
        ]);
        let AuthorizationToken(file) = file.unwrap().unwrap();
        assert_eq!(file.as_bytes(), b"from-file\xe9");
        assert!(file.is_sensitive());
        let AuthorizationToken(env) = read(&[(AUTHORIZATION_TOKEN, "from-env")]).unwrap().unwrap();
        assert_eq!(env, "from-env");
        assert!(env.is_sensitive());
        assert!(read(&[]).unwrap().is_none());

        let missing = read(&[(AUTHORIZATION_TOKEN_FILE, "/missing")]).unwrap_err();
        assert!(
            matches!(&missing, Error::TokenFile { path, .. } if path == Path::new("/missing")),
            "{missing:?}"
        );
        for token in ["secret-token\n", "secret-token\u{7f}"] {
            let (err, logs) = capture_logs(async { read(&[(AUTHORIZATION_TOKEN, token)]) }).await;
            let err = err.unwrap_err();
            assert!(matches!(err, Error::InvalidToken(_)), "{err:?}");
            assert!(!error_chain(&err).contains("secret-token"), "{err}");
            assert!(logs.contains("token_length"), "{logs}");
            assert!(!logs.contains("secret-token"), "{logs}");
        }
    }

    /// The request asks for JSON and carries the token, still marked
    /// sensitive. Breaks if either header is dropped or the token prints.
    #[test]
    fn asks_for_json_with_the_token() {
        let url = Url::parse("http://169.254.170.23/v1/credentials").unwrap();
        let token = AuthorizationToken::new(HeaderValue::from_static("the-token"));
        let sent = request(&url, Some(&token));
        assert_eq!(sent.method(), Method::GET);
        assert_eq!(sent.headers()[ACCEPT], JSON);
        assert_eq!(sent.headers()[AUTHORIZATION], "the-token");
        assert!(sent.headers()[AUTHORIZATION].is_sensitive());
        assert!(!format!("{token:?}").contains("the-token"));
        assert!(!request(&url, None).headers().contains_key(AUTHORIZATION));
    }

    /// Over the network a redirect is not followed, so the token goes to
    /// the endpoint alone. Breaks if the client follows a redirect.
    #[tokio::test(flavor = "current_thread")]
    async fn does_not_follow_a_redirect() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let elsewhere = format!("{}/elsewhere", server.uri());
        Mock::given(method("GET"))
            .and(path("/creds"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", elsewhere))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DOCUMENT))
            .expect(0)
            .mount(&server)
            .await;
        let uri = format!("{}/creds", server.uri());
        let vars = [(FULL_URI, uri.as_str()), (AUTHORIZATION_TOKEN, "the-token")];
        let host = host(&vars, &[], &[], Network::Real);
        let err = Provider::new(&host).unwrap().load().await.unwrap_err();
        assert!(
            matches!(err, ChainError::Container(Error::Status(StatusCode::FOUND))),
            "{err:?}"
        );
    }

    /// Plain HTTP to a host name connects to the address checked, whatever
    /// a later lookup answers: the name here is one the system resolver
    /// does not know. Breaks if the request resolves the name again.
    #[tokio::test(flavor = "current_thread")]
    async fn connects_to_the_address_checked() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0, "no request");
            let answer = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{DOCUMENT}",
                DOCUMENT.len()
            );
            socket.write_all(answer.as_bytes()).await.unwrap();
        });
        let uri = format!("http://agent.invalid:{port}/creds");
        let checked: &[IpAddr] = &[IpAddr::V4(Ipv4Addr::LOCALHOST)];
        let host = host(
            &[(FULL_URI, &uri)],
            &[],
            &[("agent.invalid", checked)],
            Network::Real,
        );
        let loaded = Provider::new(&host).unwrap().load().await.unwrap();
        assert_eq!(loaded.access_key_id(), "ASIARTEST");
        server.await.unwrap();
    }

    /// A request that does not complete names no part of the endpoint URL,
    /// which can carry a key. Breaks if the URL reaches the error.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn keeps_the_url_out_of_a_transport_error() {
        let uri = "http://127.0.0.1:1/creds?key=secret-key";
        let host = host(&[(FULL_URI, uri)], &[], &[], Network::Real);
        let err = Provider::new(&host).unwrap().load().await.unwrap_err();
        assert!(
            matches!(err, ChainError::Container(Error::Transport(_))),
            "{err:?}"
        );
        assert!(!error_chain(&err).contains("secret-key"), "{err:?}");
    }

    /// A token file that does not read stops the chain even with no
    /// endpoint set, as upstream reads the token first. Breaks if the
    /// endpoint is checked first.
    #[tokio::test(flavor = "current_thread")]
    async fn a_missing_token_file_fails_before_the_endpoint() {
        let host = host(
            &[(AUTHORIZATION_TOKEN_FILE, "/missing")],
            &[],
            &[],
            Network::Real,
        );
        let err = Provider::new(&host).unwrap().load().await.unwrap_err();
        assert!(
            matches!(err, ChainError::Container(Error::TokenFile { .. })),
            "{err:?}"
        );
    }

    /// A 500 and a success that does not read are retried, a 404 and an
    /// error document are not, and a server error is tried three times.
    /// Breaks if a retry is added or lost.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retries_what_upstream_retries() {
        let ok = |id| answer(id, StatusCode::OK, DOCUMENT.as_bytes());
        for first in [
            answer(0, StatusCode::INTERNAL_SERVER_ERROR, b""),
            answer(0, StatusCode::OK, b"not json"),
        ] {
            let (host, replay) = replaying([first, ok(1)]);
            let loaded = Provider::new(&host).unwrap().load().await.unwrap();
            replay.assert_done();
            assert_eq!(loaded.access_key_id(), "ASIARTEST");
        }

        let document = br#"{"Code": "Throttled", "Message": "slow down"}"#;
        let unavailable = |id| answer(id, StatusCode::SERVICE_UNAVAILABLE, b"");
        for (exchanges, text) in [
            (
                vec![answer(0, StatusCode::NOT_FOUND, b"")],
                "Non-success status from HTTP credential provider: 404",
            ),
            (
                vec![answer(0, StatusCode::OK, document)],
                "failed to load credentials [Throttled]: slow down",
            ),
            (
                vec![unavailable(0), unavailable(1), unavailable(2)],
                "Non-success status from HTTP credential provider: 503",
            ),
        ] {
            let (host, replay) = replaying(exchanges);
            let err = Provider::new(&host).unwrap().load().await.unwrap_err();
            replay.assert_done();
            let ChainError::Container(error) = &err else {
                panic!("{err:?}");
            };
            assert_eq!(error.to_string(), text);
        }
    }

    /// A connection that fails is retried as transient. Breaks if a
    /// transport failure is not retried.
    #[tokio::test(flavor = "current_thread")]
    async fn retries_a_failed_connection() {
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let transport = Network::Real.transport(reqwest::Client::builder()).unwrap();
        let failure = transport.send(request(&url, None)).await.unwrap_err();
        assert_eq!(
            Error::Transport(failure.into()).retry_kind(),
            Some(Kind::Transient)
        );
    }
}
