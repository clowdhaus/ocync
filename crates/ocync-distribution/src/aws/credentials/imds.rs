//! Credentials from the EC2 instance metadata service, `IMDSv2` only, as the
//! SDK loads them: a session token from `PUT /latest/api/token`, the role
//! from the security credentials listing, then the role's credentials.
//!
//! A host off EC2 must not stall the chain, so a token request that cannot
//! reach the service, and a role listing answered 404, mean the source is
//! not configured. Once credentials load, a failed refresh answers with the
//! last ones, their expiry pushed out if it has passed, as the SDK's static
//! stability does.

use std::future::Future;
use std::num::{NonZeroU32, ParseIntError};
use std::str::Utf8Error;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::InvalidHeaderValue;
use http::{HeaderValue, Method, StatusCode};
use url::Url;

use super::super::config::{Config, ImdsEndpointError};
use super::super::endpoint::join_path;
use super::super::host::{Clock, Env, Host, Timeouts, Transport, credential_endpoint_client};
use super::super::retry::{self, Kind, Retryable};
use super::json::{self, Code, Document};
use super::{Credentials, Error as ChainError, error_chain};
use crate::credential_cache::CredentialCache;
use crate::retry::retrying;

/// The variable that turns the source off when `true`, any case.
const DISABLED: &str = "AWS_EC2_METADATA_DISABLED";

/// Where a session token is requested.
const TOKEN_PATH: &str = "/latest/api/token";

/// Where the instance's role is listed, and under which its credentials are.
const CREDENTIALS_PATH: &str = "/latest/meta-data/iam/security-credentials/";

/// The header naming the token lifetime asked for, and granted.
const TOKEN_TTL_HEADER: &str = "x-aws-ec2-metadata-token-ttl-seconds";

/// The header carrying the session token.
pub(in crate::aws) const TOKEN_HEADER: &str = "x-aws-ec2-metadata-token";

/// The token lifetime asked for: six hours, the SDK's. A longer lifetime
/// granted is cached as this one.
const TOKEN_TTL: Duration = Duration::from_secs(21_600);

/// A token this close to expiry is replaced.
const TOKEN_REFRESH_BUFFER: Duration = Duration::from_secs(120);

/// The attempts each request makes, the first included.
const MAX_ATTEMPTS: NonZeroU32 = NonZeroU32::new(4).expect("4 is not 0");

/// A connection not made within a second fails, and so does a read that
/// waits longer. Off EC2 the address answers nothing, so this bounds what
/// the chain spends on this source.
const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(1),
    read: Duration::from_secs(1),
};

/// An attempt that takes longer fails.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// A request, its retries included, that takes longer fails.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Expired credentials served after a failed refresh are valid this long,
/// plus up to [`EXTENSION_JITTER_SECS`].
const EXTENSION: Duration = Duration::from_secs(10 * 60);

/// The most random seconds added to [`EXTENSION`].
const EXTENSION_JITTER_SECS: u64 = 300;

/// Why the service's answer to a request failed it.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum RequestError {
    /// A failing status.
    #[error("error response from IMDS (code: {})", .0.as_u16())]
    Status(StatusCode),
    /// An attempt or the whole request took too long.
    #[error("IMDS did not answer within {0:?}")]
    TimedOut(Duration),
}

impl Retryable for RequestError {
    fn retry_kind(&self) -> Option<Kind> {
        match self {
            Self::Status(status) => status_retry_kind(*status),
            Self::TimedOut(_) => None,
        }
    }
}

/// Why a token's lifetime does not read.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum TtlError {
    /// The header is not visible ASCII.
    #[error("the header is not text")]
    NotText(#[source] http::header::ToStrError),
    /// The text is not a whole number of seconds.
    #[error("not a whole number of seconds")]
    NotSeconds(#[source] ParseIntError),
}

/// Why the token request failed.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum TokenError {
    /// HTTP 403: the service is off, or the hop limit is too low.
    #[error("request forbidden: IMDS is disabled or the caller has insufficient permissions.")]
    Forbidden,
    /// HTTP 400: the request was malformed.
    #[error("invalid request parameters. This indicates a bug in ocync.")]
    InvalidParameters,
    /// The token is not a header value.
    #[error("invalid token")]
    InvalidToken(#[source] InvalidHeaderValue),
    /// The answer carries no lifetime.
    #[error("token response did not contain a TTL header")]
    NoTtl,
    /// The lifetime does not read.
    #[error("the returned TTL was invalid")]
    InvalidTtl(#[source] TtlError),
    /// The request did not complete, such as off EC2. Built only through
    /// `From`, which drops the URL.
    #[error("could not communicate with IMDS")]
    Transport(#[source] reqwest::Error),
    /// The answer failed the request.
    #[error(transparent)]
    Request(#[from] RequestError),
}

impl Retryable for TokenError {
    fn retry_kind(&self) -> Option<Kind> {
        match self {
            Self::Request(error) => error.retry_kind(),
            _ => None,
        }
    }
}

/// Why the source failed.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The endpoint settings do not resolve.
    #[error("invalid IMDS endpoint configuration")]
    Endpoint(#[source] ImdsEndpointError),
    /// No session token.
    #[error("failed to load IMDS session token")]
    Token(#[source] TokenError),
    /// The request did not complete. Built only through `From`, which drops
    /// the URL.
    #[error("an IO error occurred communicating with IMDS")]
    Transport(#[source] reqwest::Error),
    /// The answer failed the request.
    #[error(transparent)]
    Request(#[from] RequestError),
    /// The role name is not text.
    #[error("IMDS did not return valid UTF-8")]
    NotUtf8(#[source] Utf8Error),
    /// An error document in place of credentials.
    #[error("{}", DocumentText { code, message })]
    Document {
        /// The error code.
        code: Code,
        /// The error message.
        message: String,
    },
    /// The credentials document does not read.
    #[error(transparent)]
    Json(json::Error),
}

impl Retryable for Error {
    fn retry_kind(&self) -> Option<Kind> {
        match self {
            Self::Request(error) => error.retry_kind(),
            _ => None,
        }
    }
}

/// The SDK retries a server error, and a 401 for an expired token.
fn status_retry_kind(status: StatusCode) -> Option<Kind> {
    (status.is_server_error() || status == StatusCode::UNAUTHORIZED).then_some(Kind::Server)
}

/// The text of an error document, with the SDK's hint for a role that
/// does not trust EC2.
struct DocumentText<'a> {
    code: &'a Code,
    message: &'a str,
}

impl std::fmt::Display for DocumentText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { code, message } = self;
        match code {
            Code::AssumeRoleUnauthorizedAccess => write!(
                f,
                "Incorrect IMDS/IAM configuration: [{code}] {message}. \
                 Hint: Does this role have a trust relationship with EC2?"
            ),
            Code::Other(_) => write!(
                f,
                "Error retrieving credentials from IMDS: {code} {message}"
            ),
        }
    }
}

/// `work`, or [`RequestError::TimedOut`] once `limit` passes.
async fn within<T, E: From<RequestError>>(
    limit: Duration,
    work: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    tokio::time::timeout(limit, work)
        .await
        .unwrap_or_else(|_| Err(RequestError::TimedOut(limit).into()))
}

/// Whether and where the source may be asked.
enum Access {
    /// `AWS_EC2_METADATA_DISABLED` is true.
    Disabled,
    /// The endpoint settings do not resolve.
    Misconfigured(ImdsEndpointError),
    /// The service at `endpoint`, reached through `transport`.
    Ready { endpoint: Url, transport: Transport },
}

/// The endpoint URL is configurable, so a transport error drops it.
impl From<reqwest::Error> for TokenError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

/// The endpoint URL is configurable, so a transport error drops it.
impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

/// The instance metadata source.
pub(super) struct Provider {
    access: Access,
    clock: Clock,
    retry: retry::Strategy,
    partition: retry::Partition,
    token: CredentialCache<HeaderValue>,
    /// The last credentials loaded, served when a refresh fails.
    /// Synchronized, not `Rc<RefCell<_>>`, because the AWS client holding
    /// this source is a `Send + Sync` leaf. Never held across an `.await`.
    last: Mutex<Option<Credentials>>,
}

impl Provider {
    /// The source `host` and `config` configure.
    pub(super) fn new(host: &Host, config: &Config) -> Result<Self, reqwest::Error> {
        let access = if disabled(&host.env) {
            Access::Disabled
        } else {
            match config.imds_endpoint() {
                Err(e) => Access::Misconfigured(e),
                Ok(endpoint) => Access::Ready {
                    endpoint,
                    transport: host
                        .network
                        .transport(credential_endpoint_client(TIMEOUTS))?,
                },
            }
        };
        Ok(Self {
            access,
            clock: host.clock,
            retry: retry::Strategy::new(&retry::Settings {
                max_attempts: MAX_ATTEMPTS,
                ..retry::Settings::default()
            }),
            partition: retry::Partition::default(),
            token: CredentialCache::with_refresh_window(TOKEN_REFRESH_BUFFER),
            last: Mutex::default(),
        })
    }

    /// The instance role's credentials, or the last ones loaded when this
    /// load fails.
    pub(super) async fn load(&self) -> Result<Credentials, ChainError> {
        let (endpoint, transport) = match &self.access {
            Access::Disabled => {
                return Err(ChainError::NotLoaded(format!(
                    "IMDS disabled by {DISABLED} env var set to `true`"
                )));
            }
            Access::Misconfigured(e) => return Err(ChainError::Imds(Error::Endpoint(e.clone()))),
            Access::Ready {
                endpoint,
                transport,
            } => (endpoint, transport),
        };
        let loaded = self.fetch(endpoint, transport).await;
        let mut last = self.last.lock().expect("no panic while held");
        match (loaded, last.as_mut()) {
            (Err(error), Some(last)) => {
                tracing::warn!(error = %error_chain(&error), "IMDS refresh failed; using the last credentials loaded");
                // Kept extended, so the next failure reuses this expiry
                // instead of drawing new jitter.
                last.expiry = last.expiry.map(|expiry| self.extend(expiry));
                Ok(last.clone())
            }
            (loaded, _) => loaded,
        }
    }

    async fn fetch(
        &self,
        endpoint: &Url,
        transport: &Transport,
    ) -> Result<Credentials, ChainError> {
        tracing::debug!("loading credentials from IMDS");
        let listing = match self.get(endpoint, transport, CREDENTIALS_PATH).await {
            Ok(listing) => listing,
            Err(Error::Request(RequestError::Status(StatusCode::NOT_FOUND))) => {
                tracing::warn!(
                    "received 404 from IMDS when loading profile information. \
                     Hint: This instance may not have an IAM role associated."
                );
                return Err(ChainError::NotLoaded("received 404 from IMDS".to_owned()));
            }
            Err(Error::Token(e @ TokenError::Transport(_))) => {
                return Err(ChainError::NotLoaded(error_chain(&e)));
            }
            Err(e) => return Err(ChainError::Imds(e)),
        };
        let role =
            std::str::from_utf8(&listing).map_err(|e| ChainError::Imds(Error::NotUtf8(e)))?;
        tracing::debug!(%role, "loaded the IMDS role");
        let body = self
            .get(endpoint, transport, &format!("{CREDENTIALS_PATH}{role}"))
            .await
            .map_err(ChainError::Imds)?;
        match json::parse(&body).map_err(|e| ChainError::Imds(Error::Json(e)))? {
            Document::Credentials(mut credentials) => {
                credentials.expiry = credentials.expiry.map(|expiry| self.extend(expiry));
                *self.last.lock().expect("no panic while held") = Some(credentials.clone());
                Ok(credentials)
            }
            Document::Error { code, message } => {
                Err(ChainError::Imds(Error::Document { code, message }))
            }
        }
    }

    /// `expiry`, or if it has passed, a time 10 to 15 minutes from now, so
    /// expired credentials are refreshed again soon rather than reloaded on
    /// every call. The jitter is seeded by the time, as the SDK's is.
    fn extend(&self, expiry: SystemTime) -> SystemTime {
        let now = self.clock.now();
        if now < expiry {
            return expiry;
        }
        let seed = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let jitter = fastrand::Rng::with_seed(seed).u64(0..=EXTENSION_JITTER_SECS);
        let extension = EXTENSION + Duration::from_secs(jitter);
        tracing::warn!(
            minutes = extension.as_secs_f64() / 60.0,
            "IMDS credentials are expired; extending them while the service is unavailable"
        );
        now + extension
    }

    /// The body at `path`, with a session token, retried as the SDK does.
    async fn get(&self, endpoint: &Url, transport: &Transport, path: &str) -> Result<Bytes, Error> {
        let url = join_path(endpoint, path);
        let mut policy = retry::CallPolicy::new(&self.retry, &self.partition);
        let attempts = retrying("IMDS request", &mut policy, |_| async {
            let token = self
                .token(endpoint, transport)
                .await
                .map_err(Error::Token)?;
            let mut request = reqwest::Request::new(Method::GET, url.clone());
            request.headers_mut().insert(TOKEN_HEADER, token);
            within(ATTEMPT_TIMEOUT, async {
                let response = transport.send(request).await.map_err(Error::from)?;
                let status = response.status();
                if !status.is_success() {
                    return Err(RequestError::Status(status).into());
                }
                response.bytes().await.map_err(Error::from)
            })
            .await
        });
        within(OPERATION_TIMEOUT, attempts).await
    }

    /// The cached session token, or a new one once it is within
    /// [`TOKEN_REFRESH_BUFFER`] of expiry. The token request runs inside a
    /// read's attempts, so the read's [`OPERATION_TIMEOUT`] bounds it too.
    async fn token(
        &self,
        endpoint: &Url,
        transport: &Transport,
    ) -> Result<HeaderValue, TokenError> {
        self.token
            .get_or_refresh(async {
                tracing::debug!("requesting an IMDS session token");
                let url = join_path(endpoint, TOKEN_PATH);
                let mut policy = retry::CallPolicy::new(&self.retry, &self.partition);
                let (token, ttl) = retrying("IMDS token request", &mut policy, |_| async {
                    let mut request = reqwest::Request::new(Method::PUT, url.clone());
                    request
                        .headers_mut()
                        .insert(TOKEN_TTL_HEADER, HeaderValue::from(TOKEN_TTL.as_secs()));
                    within(ATTEMPT_TIMEOUT, request_token(transport, request)).await
                })
                .await?;
                Ok((token, ttl.min(TOKEN_TTL)))
            })
            .await
    }
}

/// Send the token `request` and read the token and its lifetime.
async fn request_token(
    transport: &Transport,
    request: reqwest::Request,
) -> Result<(HeaderValue, Duration), TokenError> {
    let response = transport.send(request).await.map_err(TokenError::from)?;
    match response.status() {
        StatusCode::BAD_REQUEST => return Err(TokenError::InvalidParameters),
        StatusCode::FORBIDDEN => return Err(TokenError::Forbidden),
        status if !status.is_success() => return Err(RequestError::Status(status).into()),
        _ => {}
    }
    let ttl = response
        .headers()
        .get(TOKEN_TTL_HEADER)
        .ok_or(TokenError::NoTtl)?;
    let ttl = ttl
        .to_str()
        .map_err(TtlError::NotText)
        .and_then(|ttl| ttl.parse().map_err(TtlError::NotSeconds))
        .map(Duration::from_secs)
        .map_err(TokenError::InvalidTtl)?;
    let body = response.bytes().await.map_err(TokenError::from)?;
    let mut token = HeaderValue::from_bytes(&body).map_err(TokenError::InvalidToken)?;
    token.set_sensitive(true);
    Ok((token, ttl))
}

/// Whether `AWS_EC2_METADATA_DISABLED` is `true`, any case.
fn disabled(env: &Env) -> bool {
    env.get(DISABLED)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Arc;

    use serde::Deserialize;
    use serde_json::json;

    use super::super::super::host::{Fs, Network};
    use super::super::super::replay::{Recorded, Replay, record};
    use super::super::fixtures::DATA;
    use super::*;

    /// The base URL the default IPv4 endpoint resolves to.
    const BASE: &str = "http://169.254.169.254";

    /// The session token the recorded service grants.
    const TOKEN: &str = "token-a";

    /// A credentials document expiring at 2021-09-21T04:16:53Z.
    const DOCUMENT: &str = r#"{"Code": "Success", "AccessKeyId": "ASIARTEST",
        "SecretAccessKey": "testsecret", "Token": "testtoken",
        "Expiration": "2021-09-21T04:16:53Z"}"#;

    /// 2021-09-21T04:16:53Z, when [`DOCUMENT`] expires.
    const EXPIRY: Duration = Duration::from_secs(1_632_197_813);

    /// A Unix host with `vars` set and `files`, at `clock`, on `network`.
    fn host(
        vars: HashMap<String, String>,
        files: HashMap<PathBuf, Vec<u8>>,
        clock: Clock,
        network: Network,
    ) -> Host {
        Host {
            env: Env::Fixed(vars),
            fs: Fs::Fixed(files),
            clock,
            network,
            ..Host::fixed()
        }
    }

    /// A host with nothing set, a minute before [`EXPIRY`], answering with
    /// `replay`.
    fn replaying(replay: &Arc<Replay>) -> Host {
        let now = UNIX_EPOCH + EXPIRY - Duration::from_secs(60);
        host(
            HashMap::new(),
            HashMap::new(),
            Clock::Fixed(now),
            Network::Replay(Arc::clone(replay)),
        )
    }

    /// The source `host` configures.
    fn imds(host: &Host) -> Provider {
        Provider::new(host, &Config::load(host, None)).unwrap()
    }

    /// A token request answered with `status`, a lifetime of `ttl` when
    /// given, and `token`.
    fn put_token(id: u64, status: StatusCode, ttl: Option<&str>, token: &[u8]) -> Recorded {
        let headers = ttl.map_or_else(|| json!({}), |ttl| json!({ TOKEN_TTL_HEADER: [ttl] }));
        let uri = format!("{BASE}{TOKEN_PATH}");
        record(id, &Method::PUT, &uri, json!({}), status, headers, token)
    }

    /// A six-hour token request answered with [`TOKEN`].
    fn granted(id: u64) -> Recorded {
        put_token(id, StatusCode::OK, Some("21600"), TOKEN.as_bytes())
    }

    /// A read of `path` under the credentials path with [`TOKEN`], answered
    /// with `status` and `body`.
    fn get(id: u64, path: &str, status: StatusCode, body: &[u8]) -> Recorded {
        let uri = format!("{BASE}{CREDENTIALS_PATH}{path}");
        let token = json!({ TOKEN_HEADER: [TOKEN] });
        record(id, &Method::GET, &uri, token, status, json!({}), body)
    }

    /// `exchanges`, answered in order.
    fn traffic(exchanges: impl IntoIterator<Item = Recorded>) -> Arc<Replay> {
        Arc::new(Replay::of(exchanges))
    }

    /// The role listing and credentials read of a load with a token held.
    fn reads(first: u64) -> [Recorded; 2] {
        [
            get(first, "", StatusCode::OK, b"role"),
            get(first + 1, "role", StatusCode::OK, DOCUMENT.as_bytes()),
        ]
    }

    /// An upstream endpoint case.
    #[derive(Deserialize)]
    struct EndpointCase {
        docs: String,
        env: HashMap<String, String>,
        fs: HashMap<String, String>,
        endpoint_override: Option<String>,
        mode_override: Option<String>,
        result: Result<String, String>,
    }

    /// Cases whose expected error text names the `http` crate's URI parser,
    /// by `docs`, with the text the `url` crate gives instead.
    const URI_TEXT: &[(&str, &str)] = &[("invalid uri", "relative URL without a base")];

    /// The upstream endpoint cases: the setting, then the mode, from the
    /// environment before the profile, and an invalid mode or URI an error
    /// that sends nothing. Cases that set the endpoint or mode through the
    /// SDK's builder are skipped: there is no such override here. Breaks if
    /// a token request goes anywhere but the expected URL.
    #[tokio::test(flavor = "current_thread")]
    async fn resolves_the_endpoint_as_upstream_does() {
        #[derive(Deserialize)]
        struct Cases {
            tests: Vec<EndpointCase>,
        }
        let path = Path::new(DATA).join("imds-config/imds-endpoint-tests.json");
        let cases: Cases = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let (mut run, mut skipped) = (0, 0);
        for case in cases.tests {
            if case.endpoint_override.is_some() || case.mode_override.is_some() {
                skipped += 1;
                continue;
            }
            let refused = case.result.as_ref().ok().map(|uri| {
                record(
                    0,
                    &Method::PUT,
                    uri,
                    json!({}),
                    StatusCode::FORBIDDEN,
                    json!({}),
                    b"",
                )
            });
            let replay = traffic(refused);
            let files = case
                .fs
                .into_iter()
                .map(|(path, text)| (PathBuf::from(path), text.into_bytes()))
                .collect();
            let host = host(
                case.env,
                files,
                Clock::System,
                Network::Replay(Arc::clone(&replay)),
            );
            let err = imds(&host).load().await.unwrap_err();
            replay.assert_done();
            let docs = &case.docs;
            let text = error_chain(&err);
            match case.result {
                Ok(_) => assert!(
                    matches!(err, ChainError::Imds(Error::Token(TokenError::Forbidden))),
                    "{docs}: {text}"
                ),
                Err(expected) => {
                    let expected = URI_TEXT
                        .iter()
                        .find(|(name, _)| name == docs)
                        .map_or(expected.as_str(), |(_, ours)| ours);
                    assert!(
                        matches!(err, ChainError::Imds(Error::Endpoint(_))),
                        "{docs}: {text}"
                    );
                    assert!(text.contains(expected), "{docs}: {text}");
                }
            }
            run += 1;
        }
        assert_eq!((run, skipped), (11, 3));
    }

    /// Off EC2 the endpoint never answers. The token request times out on
    /// its read, is not retried, and the chain moves on with the cause in
    /// its reason. Breaks if a transport failure is retried or stops the
    /// chain.
    #[tokio::test(flavor = "current_thread")]
    async fn a_silent_endpoint_is_not_configured() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let connections = Rc::new(Cell::new(0));
        let counted = Rc::clone(&connections);
        let local = tokio::task::LocalSet::new();
        local.spawn_local(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                counted.set(counted.get() + 1);
                held.push(socket);
            }
        });
        let vars = HashMap::from([("AWS_EC2_METADATA_SERVICE_ENDPOINT".to_owned(), endpoint)]);
        let host = host(vars, HashMap::new(), Clock::System, Network::Real);
        let started = tokio::time::Instant::now();
        let err = local.run_until(imds(&host).load()).await.unwrap_err();
        let elapsed = started.elapsed();
        let ChainError::NotLoaded(reason) = &err else {
            panic!("{err}");
        };
        assert!(
            reason.starts_with("could not communicate with IMDS: "),
            "{reason}"
        );
        assert_eq!(connections.get(), 1);
        assert!(
            (TIMEOUTS.read..ATTEMPT_TIMEOUT).contains(&elapsed),
            "{elapsed:?}"
        );
    }

    /// Credentials that have expired by the clock are served valid for 10
    /// to 15 more minutes, and unexpired ones as they are. Breaks if an
    /// expired expiry is passed on, or a valid one moved.
    #[tokio::test(flavor = "current_thread")]
    async fn extends_an_expired_expiry() {
        for (now, extended) in [
            (EXPIRY - Duration::from_secs(3), false),
            (EXPIRY + Duration::from_secs(3600), true),
        ] {
            let now = UNIX_EPOCH + now;
            let replay = traffic([granted(0)].into_iter().chain(reads(1)));
            let host = host(
                HashMap::new(),
                HashMap::new(),
                Clock::Fixed(now),
                Network::Replay(replay),
            );
            let expiry = imds(&host).load().await.unwrap().expiry().unwrap();
            if extended {
                let after = expiry.duration_since(now).unwrap();
                assert!(
                    (EXTENSION..=EXTENSION + Duration::from_secs(EXTENSION_JITTER_SECS))
                        .contains(&after),
                    "{after:?}"
                );
            } else {
                assert_eq!(expiry, UNIX_EPOCH + EXPIRY);
            }
        }
    }

    /// A refresh that fails answers with the last credentials loaded, and
    /// reuses the cached session token. Breaks if the failure is returned,
    /// or a new token is requested.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_refresh_serves_the_last_credentials() {
        let failure = get(3, "", StatusCode::BAD_REQUEST, b"");
        let replay = traffic([granted(0)].into_iter().chain(reads(1)).chain([failure]));
        let source = imds(&replaying(&replay));
        let first = source.load().await.unwrap();
        let second = source.load().await.unwrap();
        replay.assert_done();
        assert_eq!(second.access_key_id(), first.access_key_id());
        assert_eq!(second.expiry(), first.expiry());
    }

    /// A refresh that fails after the last credentials expired serves them
    /// valid for 10 to 15 more minutes. Breaks if the passed expiry is
    /// served, which sends every call back to IMDS while it is down.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_refresh_extends_expired_last_credentials() {
        let now = UNIX_EPOCH + EXPIRY + Duration::from_secs(3600);
        let failure = get(3, "", StatusCode::BAD_REQUEST, b"");
        let replay = traffic([granted(0)].into_iter().chain(reads(1)).chain([failure]));
        let host = host(
            HashMap::new(),
            HashMap::new(),
            Clock::Fixed(now),
            Network::Replay(Arc::clone(&replay)),
        );
        let source = imds(&host);
        source.load().await.unwrap();
        if let Some(last) = source.last.lock().unwrap().as_mut() {
            last.expiry = Some(UNIX_EPOCH + EXPIRY);
        }
        let expiry = source.load().await.unwrap().expiry().unwrap();
        replay.assert_done();
        let after = expiry.duration_since(now).unwrap();
        assert!(
            (EXTENSION..=EXTENSION + Duration::from_secs(EXTENSION_JITTER_SECS)).contains(&after),
            "{after:?}"
        );
    }

    /// A token is reused until [`TOKEN_REFRESH_BUFFER`] before its expiry
    /// and replaced after, and one granted for longer than asked is cached
    /// as asked, past what the clock can hold included. Breaks if the
    /// buffer is not 120 seconds, or a long lifetime is added to the clock
    /// unchecked.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn replaces_a_token_near_expiry() {
        let replay = traffic(
            [granted(0)]
                .into_iter()
                .chain(reads(1))
                .chain(reads(3))
                .chain([granted(5)])
                .chain(reads(6)),
        );
        let source = imds(&replaying(&replay));
        source.load().await.unwrap();
        let reusable = TOKEN_TTL - TOKEN_REFRESH_BUFFER;
        tokio::time::advance(reusable - Duration::from_secs(1)).await;
        source.load().await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        source.load().await.unwrap();
        replay.assert_done();

        let huge = u64::MAX.to_string();
        let long = put_token(0, StatusCode::OK, Some(&huge), TOKEN.as_bytes());
        let replay = traffic([long].into_iter().chain(reads(1)));
        imds(&replaying(&replay)).load().await.unwrap();
        replay.assert_done();
    }

    /// A token answer with no lifetime, a lifetime that is not text or not
    /// a number, or a token that is not a header value fails the load.
    /// Breaks if any is taken as a token.
    #[tokio::test(flavor = "current_thread")]
    async fn refuses_a_malformed_token() {
        type Check = fn(&TokenError) -> bool;
        let cases: [(Recorded, Check); 4] = [
            (put_token(0, StatusCode::OK, None, b"t"), |e| {
                matches!(e, TokenError::NoTtl)
            }),
            (put_token(0, StatusCode::OK, Some("6h"), b"t"), |e| {
                matches!(e, TokenError::InvalidTtl(TtlError::NotSeconds(_)))
            }),
            (put_token(0, StatusCode::OK, Some("6\u{e9}"), b"t"), |e| {
                matches!(e, TokenError::InvalidTtl(TtlError::NotText(_)))
            }),
            (put_token(0, StatusCode::OK, Some("21600"), b"a\nb"), |e| {
                matches!(e, TokenError::InvalidToken(_))
            }),
        ];
        for (answer, check) in cases {
            let replay = traffic([answer]);
            let err = imds(&replaying(&replay)).load().await.unwrap_err();
            replay.assert_done();
            assert!(
                matches!(&err, ChainError::Imds(Error::Token(e)) if check(e)),
                "{err:?}"
            );
        }
    }

    /// A role listing that is not UTF-8 fails the load. Breaks if it is
    /// read lossily into a role name.
    #[tokio::test(flavor = "current_thread")]
    async fn refuses_a_role_that_is_not_text() {
        let replay = traffic([granted(0), get(1, "", StatusCode::OK, &[0xff, 0xfe])]);
        let err = imds(&replaying(&replay)).load().await.unwrap_err();
        replay.assert_done();
        assert!(
            matches!(err, ChainError::Imds(Error::NotUtf8(_))),
            "{err:?}"
        );
    }

    /// An error document fails the load with its code and message, and a
    /// role that does not trust EC2 with the SDK's hint. Breaks if the hint
    /// is lost or given for another code.
    #[tokio::test(flavor = "current_thread")]
    async fn reports_an_error_document() {
        for (code, text) in [
            (
                "AssumeRoleUnauthorizedAccess",
                "Incorrect IMDS/IAM configuration: [AssumeRoleUnauthorizedAccess] denied. \
                 Hint: Does this role have a trust relationship with EC2?",
            ),
            (
                "Throttled",
                "Error retrieving credentials from IMDS: Throttled denied",
            ),
        ] {
            let document = json!({ "Code": code, "Message": "denied" }).to_string();
            let replay = traffic([
                granted(0),
                get(1, "", StatusCode::OK, b"role"),
                get(2, "role", StatusCode::OK, document.as_bytes()),
            ]);
            let err = imds(&replaying(&replay)).load().await.unwrap_err();
            replay.assert_done();
            let ChainError::Imds(error @ Error::Document { .. }) = &err else {
                panic!("{err:?}");
            };
            assert_eq!(error.to_string(), text);
        }
    }

    /// A 401 is retried with the same token, as an expired token is upstream,
    /// and a 403 on the token request stops the chain with no retry. Breaks
    /// if a 401 fails the load or a 403 is retried or skipped.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retries_a_401_and_not_a_forbidden_token() {
        let replay = traffic(
            [granted(0), get(1, "", StatusCode::UNAUTHORIZED, b"")]
                .into_iter()
                .chain(reads(2)),
        );
        imds(&replaying(&replay)).load().await.unwrap();
        replay.assert_done();

        let replay = traffic([put_token(0, StatusCode::FORBIDDEN, None, b"")]);
        let err = imds(&replaying(&replay)).load().await.unwrap_err();
        replay.assert_done();
        assert!(
            matches!(err, ChainError::Imds(Error::Token(TokenError::Forbidden))),
            "{err:?}"
        );
    }

    /// A token request or a role listing answered after the attempt limit
    /// fails as timed out, and a retried listing whose attempts together
    /// pass the operation limit fails as timed out. Breaks if any limit is
    /// not applied where it belongs.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bounds_each_request_by_its_limits() {
        let late = ATTEMPT_TIMEOUT + Duration::from_secs(1);
        let timed_out =
            |limit| move |e: &RequestError| matches!(e, RequestError::TimedOut(l) if *l == limit);

        let replay = Arc::new(Replay::of([granted(0)]).delayed(0, late));
        let err = imds(&replaying(&replay)).load().await.unwrap_err();
        replay.assert_done();
        assert!(
            matches!(&err, ChainError::Imds(Error::Token(TokenError::Request(e))) if timed_out(ATTEMPT_TIMEOUT)(e)),
            "{err:?}"
        );

        let listing = get(1, "", StatusCode::OK, b"role");
        let replay = Arc::new(Replay::of([granted(0), listing]).delayed(1, late));
        let err = imds(&replaying(&replay)).load().await.unwrap_err();
        replay.assert_done();
        assert!(
            matches!(&err, ChainError::Imds(Error::Request(e)) if timed_out(ATTEMPT_TIMEOUT)(e)),
            "{err:?}"
        );

        // The three backoffs before the fourth attempt total at most 7
        // seconds, so with 7.6-second answers the fourth attempt always
        // starts before the 30-second limit and is always cut off by it.
        let slow = Duration::from_millis(7600);
        let failures = (1..=4).map(|id| get(id, "", StatusCode::INTERNAL_SERVER_ERROR, b""));
        let replay = (1..=4).fold(
            Replay::of([granted(0)].into_iter().chain(failures)),
            |replay, index| replay.delayed(index, slow),
        );
        let replay = Arc::new(replay);
        let err = imds(&replaying(&replay)).load().await.unwrap_err();
        replay.assert_done();
        assert!(
            matches!(&err, ChainError::Imds(Error::Request(e)) if timed_out(OPERATION_TIMEOUT)(e)),
            "{err:?}"
        );
    }

    /// A role listing whose connection drops fails the load with upstream's
    /// text for a failed read. Breaks if a dropped read is reported as the
    /// service being unreachable, which would skip the source.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_read_fails_the_load() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let local = tokio::task::LocalSet::new();
        let server = local.spawn_local(async move {
            let mut request = [0; 4096];
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(
                socket.read(&mut request).await.unwrap() > 0,
                "no token request"
            );
            let answer = format!(
                "HTTP/1.1 200 OK\r\n{TOKEN_TTL_HEADER}: 21600\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{TOKEN}",
                TOKEN.len()
            );
            socket.write_all(answer.as_bytes()).await.unwrap();
            drop(socket);
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(
                socket.read(&mut request).await.unwrap() > 0,
                "no role request"
            );
        });
        let vars = HashMap::from([("AWS_EC2_METADATA_SERVICE_ENDPOINT".to_owned(), endpoint)]);
        let host = host(vars, HashMap::new(), Clock::System, Network::Real);
        let err = local.run_until(imds(&host).load()).await.unwrap_err();
        let ChainError::Imds(error @ Error::Transport(_)) = &err else {
            panic!("{err:?}");
        };
        assert_eq!(
            error.to_string(),
            "an IO error occurred communicating with IMDS"
        );
        assert!(!error_chain(&err).contains("127.0.0.1"), "{err:?}");
        tokio::time::timeout(Duration::from_secs(5), local.run_until(server))
            .await
            .expect("the server saw both requests")
            .unwrap();
    }
}
