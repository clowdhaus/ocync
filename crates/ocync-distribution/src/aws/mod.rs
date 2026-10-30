//! Calls to the AWS APIs ocync uses: ECR and ECR Public, signed, and for
//! the credential chain STS, the IAM Identity Center portal and SSO OIDC.
//!
//! Settings, the shared config and credentials files, signing, endpoint
//! resolution, retries, clock-skew correction and error parsing are owned
//! here, with no AWS SDK crate, and the credential chain in
//! [`credentials`].

mod calendar;
mod config;
mod credentials;
mod endpoint;
mod host;
mod profile;
#[cfg(test)]
mod replay;
mod retry;
mod sigv4;
mod xml;

pub(crate) use self::calendar::parse_rfc3339_with_offset;
use self::config::{Config, ServiceId};
pub(crate) use self::credentials::imds;
use self::credentials::{Chain, Credentials};
pub(crate) use self::host::Host;
pub(crate) use self::host::{Clock, Env, Fs, Platform, Timeouts, credential_endpoint_client};
use self::host::{Network, Transport};

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::header::{CONTENT_TYPE, InvalidHeaderValue};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use self::calendar::parse_imf_fixdate;
use self::endpoint::Endpoint;
pub(crate) use self::endpoint::{InvalidRegion, Region};
use self::sigv4::encode_pairs;
use crate::credential_cache::{CredentialCache, remaining};
use crate::error::CredentialFailure;
use crate::retry::{is_transient_transport, retrying};
use crate::scheme::UnsupportedScheme;

/// The content type of the AWS JSON 1.1 protocol both APIs speak.
const AWS_JSON_1_1: &str = "application/x-amz-json-1.1";

/// The header naming the operation, as `<target prefix>.<operation>`.
const X_AMZ_TARGET: HeaderName = HeaderName::from_static("x-amz-target");

/// A connection not made within this fails, so an unreachable endpoint
/// surfaces in seconds. It is the SDK's value.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(3100);

/// A call that receives nothing for this long after it is sent, or between
/// reads, fails, so a stalled proxy or server cannot hold it forever. It is
/// the SDK's stalled-stream grace period.
const STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// A credential load that takes longer fails. This is the SDK's bound for
/// its default retry and connect settings: every backoff between the
/// default attempts, plus each attempt at twice [`CONNECT_TIMEOUT`].
const CREDENTIALS_LOAD_TIMEOUT: Duration = Duration::from_millis(
    retry::INITIAL_BACKOFF.as_millis() as u64
        * ((1 << (retry::DEFAULT_MAX_ATTEMPTS.get() - 1)) - 1)
        + retry::DEFAULT_MAX_ATTEMPTS.get() as u64 * 2 * CONNECT_TIMEOUT.as_millis() as u64,
);

/// How long a credential without an expiry is reused, as the SDK's identity
/// cache does.
const DEFAULT_EXPIRATION: Duration = Duration::from_secs(15 * 60);

/// A signature error with the clocks closer than this is a real signature
/// error, not the local clock, as in the SDK.
const SKEW_THRESHOLD: Duration = Duration::from_secs(4 * 60);

/// A round trip longer than this says nothing about the clocks.
const MAX_MEASURED_ROUND_TRIP: Duration = Duration::from_secs(15 * 60);

/// Statuses the SDK re-sends as transient whatever the error code.
const TRANSIENT_STATUSES: &[StatusCode] = &[
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

/// Error codes the SDK re-sends as throttles.
const THROTTLING_CODES: &[&str] = &[
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "LimitExceededException",
    "RequestThrottled",
    "SlowDown",
    "PriorRequestNotComplete",
    "EC2ThrottledException",
];

/// Error codes the SDK re-sends as transient faults.
const TRANSIENT_CODES: &[&str] = &["RequestTimeout", "RequestTimeoutException"];

/// The header a service names its preferred wait before a retry in, in
/// milliseconds.
const X_AMZ_RETRY_AFTER: &str = "x-amz-retry-after";

/// Error codes a request signed at the wrong time draws, which the SDK
/// re-sends once the clocks are measured apart by more than
/// [`SKEW_THRESHOLD`].
const CLOCK_SKEW_CODES: &[&str] = &[
    "InvalidSignatureException",
    "SignatureDoesNotMatch",
    "AuthFailure",
    "RequestTimeTooSkewed",
    "AccessDeniedException",
];

/// Error codes a service refuses a request's credentials with, whatever the
/// status: AWS's common errors and their query protocol forms.
const REFUSED_CODES: &[&str] = &[
    "AccessDenied",
    "AccessDeniedException",
    "AuthFailure",
    "ExpiredToken",
    "ExpiredTokenException",
    "IncompleteSignature",
    "InvalidClientTokenId",
    "InvalidSignatureException",
    "MissingAuthenticationToken",
    "NotAuthorized",
    "SignatureDoesNotMatch",
    "UnauthorizedException",
    "UnrecognizedClientException",
];

/// What sets one AWS API apart from another to its calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApiId {
    /// Names its endpoint URL settings and its retry partition.
    service_id: ServiceId,
    /// The name its requests are signed under, such as `ecr`.
    signing_name: &'static str,
    error_format: ErrorFormat,
}

/// The AWS JSON APIs ocync calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Service {
    /// Amazon ECR, for registry tokens and layer checks.
    Ecr,
    /// Amazon ECR Public, for registry tokens.
    EcrPublic,
}

impl Service {
    /// The `x-amz-target` prefix naming the service's operations.
    fn target_prefix(self) -> &'static str {
        match self {
            Self::Ecr => "AmazonEC2ContainerRegistry_V20150921",
            Self::EcrPublic => "SpencerFrontendService",
        }
    }

    /// How calls to the API are signed, partitioned and read.
    fn api_id(self) -> ApiId {
        let (service_id, signing_name) = match self {
            Self::Ecr => (ServiceId::ECR, "ecr"),
            Self::EcrPublic => (ServiceId::ECR_PUBLIC, "ecr-public"),
        };
        ApiId {
            service_id,
            signing_name,
            error_format: ErrorFormat::Json,
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ecr => "ECR",
            Self::EcrPublic => "ECR Public",
        })
    }
}

/// An operation of one of the APIs. The value is the operation's input.
pub(crate) trait Operation: Serialize {
    /// The operation's name, such as `GetAuthorizationToken`.
    const NAME: &'static str;
    /// What the operation answers with.
    type Output: DeserializeOwned;
}

/// Why a [`Client`] could not be built from the AWS configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    /// An ECR hostname that names no region.
    #[error("cannot extract AWS region from ECR hostname '{0}'")]
    NoRegionInHostname(String),
    /// The configured region is not a valid host label.
    #[error(transparent)]
    InvalidRegion(#[from] InvalidRegion),
    /// `AWS_MAX_ATTEMPTS` or `max_attempts` is not a whole number above 0.
    #[error("invalid AWS max attempts '{text}': must be a whole number of at least 1")]
    InvalidMaxAttempts {
        /// The configured value.
        text: String,
        /// Why it did not parse.
        source: std::num::ParseIntError,
    },
    /// `AWS_RETRY_MODE` or `retry_mode` is not `standard` or `adaptive`.
    #[error("invalid AWS retry mode '{0}': must be standard or adaptive")]
    InvalidRetryMode(String),
    /// An endpoint URL is configured together with FIPS endpoints.
    #[error("Invalid Configuration: FIPS and custom endpoint are not supported")]
    FipsWithEndpointUrl,
    /// An endpoint URL is configured together with dual-stack endpoints.
    #[error("Invalid Configuration: Dualstack and custom endpoint are not supported")]
    DualStackWithEndpointUrl,
    /// The endpoint is not a URL. The text is left out: it may carry a key.
    #[error("invalid AWS endpoint URL: {0}")]
    InvalidEndpoint(#[source] url::ParseError),
    /// The endpoint is a URL with no host to send to, such as `unix:`.
    #[error("invalid AWS endpoint URL: {0}")]
    EndpointNotHttp(#[source] UnsupportedScheme),
    /// The HTTP client could not be built.
    #[error("failed to build the AWS HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

/// What an AWS error code means for the request that drew it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeKind {
    /// A throttle: re-send.
    Throttling,
    /// A transient fault: re-send.
    Transient,
    /// A signature the service rejects, perhaps for the local clock.
    ClockSkew,
    /// Anything else: final.
    Other,
}

/// An AWS error code, such as `ThrottlingException`, without the namespace
/// prefix and trailing URL some services wrap it in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ErrorCode {
    text: String,
    kind: CodeKind,
    /// The operation's model of this code, when it has one.
    modeled: Option<&'static ModeledError>,
}

/// Whether the SDK re-sends an error an operation models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeledRetry {
    /// As a transient fault.
    Transient,
    /// Only as the shared code lists say.
    Shared,
}

/// An error an operation models, as its SDK generates it.
#[derive(Debug, PartialEq, Eq)]
struct ModeledError {
    /// The code on the wire, such as `InvalidIdentityToken`.
    code: &'static str,
    /// The name the SDK reports it under, such as
    /// `InvalidIdentityTokenException`.
    name: &'static str,
    retry: ModeledRetry,
}

impl ErrorCode {
    /// Read the code from `x-amzn-errortype`, `code` or `__type` text, such as
    /// `aws.protocoltests#FooError:http://internal.amazon.com/coral/`, as the
    /// SDK does, and classify it as the shared lists and the operation's
    /// `modeled` errors say. Text that names no code is `None`.
    fn parse(raw: &str, modeled: &'static [ModeledError]) -> Option<Self> {
        let code = raw.split_once(':').map_or(raw, |(code, _)| code);
        let code = code.split_once('#').map_or(code, |(_, code)| code);
        if code.is_empty() {
            return None;
        }
        let modeled = modeled.iter().find(|m| m.code == code);
        let modeled_transient = modeled.is_some_and(|m| m.retry == ModeledRetry::Transient);
        let kind = if THROTTLING_CODES.contains(&code) {
            CodeKind::Throttling
        } else if TRANSIENT_CODES.contains(&code) || modeled_transient {
            CodeKind::Transient
        } else if CLOCK_SKEW_CODES.contains(&code) {
            CodeKind::ClockSkew
        } else {
            CodeKind::Other
        };
        Some(Self {
            text: code.to_owned(),
            kind,
            modeled,
        })
    }
}

/// An error answer from the service.
#[derive(Debug)]
struct ServiceError {
    status: StatusCode,
    code: Option<ErrorCode>,
    message: Option<String>,
    /// The wait the service asked for before a retry.
    retry_after: Option<Duration>,
}

impl ServiceError {
    /// The code comes from `x-amzn-errortype`, else the body's `code`, else
    /// its `__type`. Only string values count, and a body that is not JSON
    /// still leaves the status, which is what retry classification needs.
    fn parse(
        status: StatusCode,
        header_code: Option<&str>,
        body: &[u8],
        modeled: &'static [ModeledError],
    ) -> Self {
        let body: serde_json::Map<String, serde_json::Value> =
            crate::json::parse(body).unwrap_or_default();
        let text = |key: &str| body.get(key).and_then(serde_json::Value::as_str);
        let code = header_code
            .and_then(|raw| ErrorCode::parse(raw, modeled))
            .or_else(|| {
                text("code")
                    .or_else(|| text("__type"))
                    .and_then(|raw| ErrorCode::parse(raw, modeled))
            });
        let message = ["message", "Message", "errorMessage"]
            .into_iter()
            .find_map(text)
            .map(str::to_owned);
        Self {
            status,
            code,
            message,
            retry_after: None,
        }
    }

    /// The code and message of a query protocol `ErrorResponse`. A body that
    /// is not one still leaves the status.
    fn parse_xml(status: StatusCode, body: &[u8], modeled: &'static [ModeledError]) -> Self {
        let body = std::str::from_utf8(body).unwrap_or_default();
        Self {
            status,
            code: xml::text(body, &["Error", "Code"]).and_then(|c| ErrorCode::parse(&c, modeled)),
            message: xml::text(body, &["Error", "Message"]),
            retry_after: None,
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self
            .code
            .as_ref()
            .map_or("unknown error", |c| c.text.as_str());
        let message = self.message.as_deref().unwrap_or("no message");
        write!(f, "{code} (HTTP {}): {message}", self.status)
    }
}

/// Whether signatures correct for the measured [`ClockSkew`], off when
/// `AWS_DISABLE_CLOCK_SKEW_CORRECTION` or the profile's
/// `disable_clock_skew_correction` is true. Off, no skew is measured, so no
/// signature is shifted and no skewed signature is re-sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkewCorrection {
    On,
    Off,
}

/// How far AWS's clock is ahead of this host's, in milliseconds; negative
/// when it is behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClockSkew(i64);

impl ClockSkew {
    /// Measure the skew from a response's `Date` against the midpoint of the
    /// round trip, as the SDK does. A cached response (one with `Age`) or a
    /// round trip over [`MAX_MEASURED_ROUND_TRIP`] measures nothing.
    fn measure(headers: &HeaderMap, sent: SystemTime, received: SystemTime) -> Option<Self> {
        if headers.contains_key(http::header::AGE) {
            return None;
        }
        let date = headers.get(http::header::DATE)?.to_str().ok()?;
        let server = parse_imf_fixdate(date)?;
        let round_trip = received.duration_since(sent).unwrap_or_default();
        if round_trip > MAX_MEASURED_ROUND_TRIP {
            return None;
        }
        // A skew too large for i64 milliseconds measures nothing.
        let millis = |d: Duration| i64::try_from(d.as_millis()).ok();
        Some(match server.duration_since(sent + round_trip / 2) {
            Ok(ahead) => Self(millis(ahead)?),
            Err(behind) => Self(-millis(behind.duration())?),
        })
    }

    fn magnitude(self) -> Duration {
        Duration::from_millis(self.0.unsigned_abs())
    }

    /// `time` on AWS's clock.
    fn apply(self, time: SystemTime) -> SystemTime {
        if self.0 >= 0 {
            time.checked_add(self.magnitude())
        } else {
            time.checked_sub(self.magnitude())
        }
        .unwrap_or(time)
    }
}

impl fmt::Display for ClockSkew {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let direction = if self.0 >= 0 { "ahead of" } else { "behind" };
        write!(
            f,
            "AWS's clock is {}s {direction} this host's",
            self.magnitude().as_secs()
        )
    }
}

/// Why one attempt at a call failed.
#[derive(Debug, thiserror::Error)]
enum Failure {
    /// No credentials could be loaded. Callers keep only this text, so it
    /// names every cause and is not a source itself. Shared, because every
    /// client waiting on one failed load reports it.
    #[error("failed to load AWS credentials: {}", crate::error::error_chain(&**.0))]
    Credentials(Arc<credentials::Error>),
    /// The request did not complete.
    #[error("request failed: {0}")]
    Transport(#[source] TransportError),
    /// The service answered with an error.
    #[error("{0}")]
    Service(ServiceError),
    /// The service rejected a signature made on a clock too far from its own.
    /// The next attempt signs on AWS's clock.
    #[error("{error} ({skew})")]
    ClockSkewed {
        /// The service's answer.
        error: ServiceError,
        /// The skew measured from that answer.
        skew: ClockSkew,
    },
    /// The service's answer could not be read.
    #[error("unreadable response: {0}")]
    Decode(#[source] crate::json::Unreadable),
    /// The service's answer is longer than any answer it gives.
    #[error("the answer is over {} bytes", crate::json::MAX_SMALL_ANSWER)]
    TooLarge,
    /// A header to send, such as a credential's session token, holds a byte
    /// a header cannot carry.
    #[error("request has a header that cannot be sent: {0}")]
    InvalidHeader(#[source] InvalidHeaderValue),
}

/// A request that did not complete, its URL dropped: an endpoint URL can
/// carry a key, and the error's text reaches logs and the report.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(super) struct TransportError(reqwest::Error);

impl From<reqwest::Error> for TransportError {
    fn from(error: reqwest::Error) -> Self {
        Self(error.without_url())
    }
}

impl TransportError {
    /// Whether a re-send may succeed.
    pub(super) fn is_transient(&self) -> bool {
        is_transient_transport(&self.0)
    }
}

impl From<reqwest::Error> for Failure {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.into())
    }
}

impl Failure {
    /// Whether AWS refused the identity, the call may pass if sent again,
    /// or something else failed. A failure the call re-sends may pass; a
    /// service answer refuses with a 401, a 403 or a [`REFUSED_CODES`] code.
    fn credential_failure(&self) -> CredentialFailure {
        use retry::Retryable as _;
        match self {
            Self::Credentials(error) => error.credential_failure(),
            _ if self.retry_kind().is_some() => CredentialFailure::Transient,
            Self::Service(error)
                if matches!(
                    error.status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) || error
                    .code
                    .as_ref()
                    .is_some_and(|c| REFUSED_CODES.contains(&c.text.as_str())) =>
            {
                CredentialFailure::Refused
            }
            _ => CredentialFailure::Failed,
        }
    }
}

impl retry::Retryable for Failure {
    /// A throttle code wins over the status it came with, as in the SDK.
    fn retry_kind(&self) -> Option<retry::Kind> {
        match self {
            Self::Transport(e) => e.is_transient().then_some(retry::Kind::Transient),
            Self::Service(e) => match e.code.as_ref().map(|c| c.kind) {
                Some(CodeKind::Throttling) => Some(retry::Kind::Throttling),
                Some(CodeKind::Transient) => Some(retry::Kind::Transient),
                _ if TRANSIENT_STATUSES.contains(&e.status) => Some(retry::Kind::Transient),
                // The SDK leaves a bare 429 alone. It is retried here as the
                // throttle it is: a proxy on `HTTPS_PROXY` answers with one, and
                // a batch check given up on transfers every blob it covered.
                _ if e.status == StatusCode::TOO_MANY_REQUESTS => Some(retry::Kind::Throttling),
                _ => None,
            },
            Self::ClockSkewed { .. } => Some(retry::Kind::Server),
            Self::Credentials(_) | Self::Decode(_) | Self::TooLarge | Self::InvalidHeader(_) => {
                None
            }
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Service(error) | Self::ClockSkewed { error, .. } => error.retry_after,
            Self::Credentials(_)
            | Self::Transport(_)
            | Self::Decode(_)
            | Self::TooLarge
            | Self::InvalidHeader(_) => None,
        }
    }
}

/// Why an AWS call failed, after its retries. Callers keep only its text,
/// so the text names every cause, such as the refused connection under a
/// transport failure, and it is not a source itself.
#[derive(Debug, thiserror::Error)]
#[error("{service} {operation} failed: {}", crate::error::error_chain(failure))]
pub(crate) struct CallError {
    service: Service,
    operation: &'static str,
    failure: Failure,
}

impl CallError {
    /// Whether AWS refused the identity, the call may pass if sent again,
    /// or something else failed.
    pub(crate) fn credential_failure(&self) -> CredentialFailure {
        self.failure.credential_failure()
    }
}

/// How an API writes its error answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorFormat {
    /// The JSON protocols: `x-amzn-errortype`, `code` or `__type`.
    Json,
    /// The query protocol: `<Error><Code>` in an `ErrorResponse`.
    Xml,
}

/// Whether a request carries a signature.
enum Signing {
    /// Signed with this key.
    Signed(Credentials),
    /// Sent unsigned, as an operation that proves identity some other way
    /// is.
    Unsigned,
}

/// A request to an [`Api`]'s endpoint.
struct ApiRequest<'a> {
    /// The operation's name, for logs and errors.
    operation: &'static str,
    method: http::Method,
    /// The operation's path, joined to the endpoint's.
    path: &'static str,
    /// Headers to send, signed when the request is.
    headers: &'a [(HeaderName, &'a str)],
    body: Bytes,
    /// The errors the operation models.
    modeled_errors: &'static [ModeledError],
}

/// The settings an [`Api`] is built from, beside its [`ApiId`].
struct ApiParts {
    region: Region,
    endpoint: Endpoint,
    retry: retry::Strategy,
    partition: Arc<retry::Partition>,
    skew_correction: SkewCorrection,
    stall_timeout: Duration,
}

/// One AWS API in one region: where its requests go, and the retry, clock
/// skew and error handling every call to it shares.
struct Api {
    http: Transport,
    /// The last [`ClockSkew`] measured, which every signature corrects for.
    /// Stays 0 when `skew_correction` is off.
    skew: AtomicI64,
    /// Whether answers are measured for skew at all.
    skew_correction: SkewCorrection,
    retry: retry::Strategy,
    /// The retry quota and rate limiter shared with every client for the
    /// same service and region.
    partition: Arc<retry::Partition>,
    region: Region,
    endpoint: Endpoint,
    id: ApiId,
}

impl Api {
    /// The API `id` with the settings `parts` hold, sending through
    /// `network` with the client settings of every AWS API call.
    fn new(network: &Network, id: ApiId, parts: ApiParts) -> Result<Self, reqwest::Error> {
        let ApiParts {
            region,
            endpoint,
            retry,
            partition,
            skew_correction,
            stall_timeout,
        } = parts;
        Ok(Self {
            http: network.transport(api_client(stall_timeout))?,
            skew: AtomicI64::default(),
            skew_correction,
            retry,
            partition,
            region,
            endpoint,
            id,
        })
    }

    /// Send `request`, re-sending throttles, transient faults and
    /// clock-skewed signatures as the SDK's retry policy allows (see
    /// [`retry`]), and return the successful answer's body. `signing` is
    /// asked before each attempt whether to sign, and with which key.
    async fn call<F, Fut>(&self, request: &ApiRequest<'_>, signing: F) -> Result<Bytes, Failure>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Signing, Failure>>,
    {
        self.call_with(request, &[], signing).await
    }

    /// Send `request` unsigned with `query`, as an operation that proves
    /// identity some other way is.
    async fn call_unsigned(
        &self,
        request: &ApiRequest<'_>,
        query: &[(&str, &str)],
    ) -> Result<Bytes, Failure> {
        self.call_with(request, query, || async { Ok(Signing::Unsigned) })
            .await
    }

    async fn call_with<F, Fut>(
        &self,
        request: &ApiRequest<'_>,
        query: &[(&str, &str)],
        signing: F,
    ) -> Result<Bytes, Failure>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Signing, Failure>>,
    {
        let url = self.url(request.path, query);
        let signed_headers: Vec<(&str, &str)> = request
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .chain([("host", self.endpoint.host())])
            .collect();
        let mut policy = retry::CallPolicy::new(&self.retry, &self.partition);
        let (url, signing, signed_headers) = (&url, &signing, &signed_headers);
        retrying(request.operation, &mut policy, |attempt| async move {
            self.partition.send_token(&self.retry, attempt).await;
            let signing = signing().await?;
            self.send(request, url, signed_headers, &signing).await
        })
        .await
    }

    /// The URL of the operation at `path` with `query`, each query value
    /// encoded as the SDK encodes it.
    fn url(&self, path: &str, query: &[(&str, &str)]) -> Url {
        let mut url = endpoint::join_path(self.endpoint.url(), path);
        if !query.is_empty() {
            url.set_query(Some(&encode_pairs(query.iter().copied())));
        }
        url
    }

    /// Send `request` to `url` once. `signed_headers` are its headers and
    /// `host`, as a signature covers them.
    async fn send(
        &self,
        request: &ApiRequest<'_>,
        url: &Url,
        signed_headers: &[(&str, &str)],
        signing: &Signing,
    ) -> Result<Bytes, Failure> {
        let sent = SystemTime::now();
        let applied = ClockSkew(self.skew.load(Ordering::Relaxed));
        let mut outgoing = reqwest::Request::new(request.method.clone(), url.clone());
        *outgoing.body_mut() = Some(request.body.clone().into());
        let headers = outgoing.headers_mut();
        for (name, value) in request.headers {
            let value = HeaderValue::from_str(value).map_err(Failure::InvalidHeader)?;
            headers.insert(name, value);
        }
        if let Signing::Signed(credentials) = signing {
            let signed = sigv4::sign(
                &sigv4::Request {
                    method: &request.method,
                    path: url.path(),
                    query: url.query().unwrap_or_default(),
                    headers: signed_headers,
                    body: &request.body,
                },
                sigv4::AccessKey {
                    id: credentials.access_key_id(),
                    secret: credentials.secret_access_key(),
                    session_token: credentials.session_token(),
                },
                &sigv4::CredentialScope {
                    region: self.region.as_str(),
                    service: self.id.signing_name,
                },
                applied.apply(sent),
            );
            for (name, value) in signed.headers() {
                let value = HeaderValue::try_from(value).map_err(Failure::InvalidHeader)?;
                headers.insert(HeaderName::from_static(name), value);
            }
        }
        let mut response = self.http.send(outgoing).await?;
        let measured = match self.skew_correction {
            SkewCorrection::On => ClockSkew::measure(response.headers(), sent, SystemTime::now()),
            SkewCorrection::Off => None,
        };
        if let Some(skew) = measured {
            self.skew.store(skew.0, Ordering::Relaxed);
        }
        let status = response.status();
        let header_code = response.headers().get("x-amzn-errortype").cloned();
        let retry_after = response
            .headers()
            .get(X_AMZ_RETRY_AFTER)
            .and_then(|v| v.to_str().ok()?.parse().ok())
            .map(Duration::from_millis);
        if status.is_success() {
            return crate::json::read_capped(&mut response, crate::json::MAX_SMALL_ANSWER)
                .await?
                .map(Bytes::from)
                .ok_or(Failure::TooLarge);
        }
        let mut bytes = Vec::new();
        crate::json::read_start(&mut response, &mut bytes).await?;
        let modeled = request.modeled_errors;
        let error = ServiceError {
            retry_after,
            ..match self.id.error_format {
                ErrorFormat::Json => ServiceError::parse(
                    status,
                    header_code.as_ref().and_then(|v| v.to_str().ok()),
                    &bytes,
                    modeled,
                ),
                ErrorFormat::Xml => ServiceError::parse_xml(status, &bytes, modeled),
            }
        };
        let signature_rejected = matches!(signing, Signing::Signed(_))
            && error
                .code
                .as_ref()
                .is_some_and(|c| c.kind == CodeKind::ClockSkew);
        // The SDK compares the whole skew. Once a signature is already on
        // AWS's clock, a rejection is real (a denied role or a bad key), so
        // only the skew this signature left uncorrected counts.
        Err(match measured {
            Some(skew)
                if signature_rejected
                    && ClockSkew(skew.0 - applied.0).magnitude() > SKEW_THRESHOLD =>
            {
                Failure::ClockSkewed { error, skew }
            }
            _ => Failure::Service(error),
        })
    }
}

/// Where a client's credentials come from.
enum Provider {
    /// The default credential chain.
    Chain(Box<Chain>),
    /// A test's own loader, to count or stall loads.
    #[cfg(test)]
    Test(tests::Loader),
}

impl Provider {
    async fn load(&self) -> Result<Credentials, credentials::Error> {
        match self {
            Self::Chain(chain) => chain.load().await,
            #[cfg(test)]
            Self::Test(loader) => loader().await,
        }
    }
}

/// A credential provider and the credential it last loaded, shared by every
/// client built for one profile and region in one run. Held in an `Arc`, and
/// synchronized, because the AWS client is a `Send + Sync` leaf: the auth
/// providers holding it must be `Send + Sync`.
struct SharedCredentials {
    provider: Provider,
    cache: CredentialCache<Credentials, Arc<credentials::Error>>,
}

impl SharedCredentials {
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            cache: CredentialCache::new(),
        }
    }

    /// The cached credential, loaded again shortly before it expires.
    /// Concurrent callers share one load instead of each asking IMDS or STS,
    /// and a caller that was waiting on a load that failed gets its error
    /// rather than running the chain again, so a hung source costs one bound,
    /// not one per waiting client.
    async fn load(&self) -> Result<Credentials, Failure> {
        self.cache
            .get_or_refresh_sharing_failure(async {
                let loaded = tokio::time::timeout(CREDENTIALS_LOAD_TIMEOUT, self.provider.load())
                    .await
                    .map_err(|_| credentials::Error::TimedOut(CREDENTIALS_LOAD_TIMEOUT))
                    .and_then(|loaded| loaded)
                    .map_err(Arc::new)?;
                let ttl = loaded.expiry().map_or(DEFAULT_EXPIRATION, remaining);
                Ok((loaded, ttl))
            })
            .await
            .map_err(Failure::Credentials)
    }
}

/// What [`SharedCredentials`] are keyed by. The region is part of it because
/// the chain calls STS in its client's region.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderKey {
    /// The resolved profile name, so a named profile and the same profile
    /// selected through `AWS_PROFILE` share one entry.
    profile: String,
    region: Region,
}

/// The AWS credentials one run's clients share, by profile and region,
/// rather than one load per client, reloaded shortly before they expire.
/// Create one per run, so the next watch
/// cycle reads the shared AWS files again and drops credentials AWS may have
/// revoked.
#[derive(Default)]
pub struct AwsCredentialProviders(RefCell<HashMap<ProviderKey, Arc<SharedCredentials>>>);

impl fmt::Debug for AwsCredentialProviders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentialProviders")
            .finish_non_exhaustive()
    }
}

impl AwsCredentialProviders {
    /// The credentials for `key`, built by `build` when there are none yet.
    /// A failed build is not kept, so it fails only its caller.
    fn get_or_try_insert<E>(
        &self,
        key: ProviderKey,
        build: impl FnOnce() -> Result<SharedCredentials, E>,
    ) -> Result<Arc<SharedCredentials>, E> {
        if let Some(shared) = self.0.borrow().get(&key) {
            return Ok(Arc::clone(shared));
        }
        let shared = Arc::new(build()?);
        self.0.borrow_mut().insert(key, Arc::clone(&shared));
        Ok(shared)
    }

    /// For each set of credentials held, how many clients use it, in
    /// ascending order. A test hook for the CLI's wiring.
    #[doc(hidden)]
    pub fn users(&self) -> Vec<usize> {
        let mut users: Vec<usize> = self
            .0
            .borrow()
            .values()
            .map(|shared| Arc::strong_count(shared) - 1)
            .collect();
        users.sort_unstable();
        users
    }
}

/// The signing name of STS.
const STS_SIGNING_NAME: &str = "sts";

/// Signs the STS `GetCallerIdentity` request a Google external account sends
/// as its subject token, with the default profile's credentials for one
/// region from the run's shared AWS credentials.
pub(crate) struct CallerIdentitySigner {
    credentials: Arc<SharedCredentials>,
    region: Region,
}

impl fmt::Debug for CallerIdentitySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallerIdentitySigner")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

/// Why a request was not signed: no credentials were loaded.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct SigningError(Failure);

impl SigningError {
    /// Whether AWS refused the identity, loading it may pass if tried
    /// again, or something else failed.
    pub(crate) fn credential_failure(&self) -> CredentialFailure {
        self.0.credential_failure()
    }
}

/// The AWS settings a [`CallerIdentitySigner`] is built from: the
/// environment and shared files of one host, read once.
pub(crate) struct CallerIdentityConfig<'h> {
    host: &'h Host,
    config: Config,
}

impl<'h> CallerIdentityConfig<'h> {
    /// The settings of `host`.
    pub(crate) fn load(host: &'h Host) -> Self {
        Self {
            host,
            config: Config::load(host, None),
        }
    }

    /// The region `AWS_REGION`, `AWS_DEFAULT_REGION` or the selected
    /// profile's `region` names, or `None` when none does.
    pub(crate) fn region(&self) -> Result<Option<Region>, InvalidRegion> {
        self.config.region()
    }

    /// A signer for `region`, taking the credentials from
    /// `aws_credentials`.
    pub(crate) fn signer(
        self,
        region: Region,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<CallerIdentitySigner, ConfigError> {
        let retry = self.config.retry()?;
        let credentials =
            shared_credentials(self.host, self.config, &region, &retry, aws_credentials)?;
        Ok(CallerIdentitySigner {
            credentials,
            region,
        })
    }
}

impl CallerIdentitySigner {
    /// The method of the request signed.
    pub(crate) const METHOD: http::Method = http::Method::POST;

    /// The region the request is signed for.
    pub(crate) fn region(&self) -> &Region {
        &self.region
    }

    /// Sign a [`Self::METHOD`] request to `url` with `headers` and an empty
    /// body, at `time`, the `host` header signed as [`host_header`].
    pub(crate) async fn sign(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
        time: SystemTime,
    ) -> Result<CallerIdentityHeaders, SigningError> {
        let credentials = self.credentials.load().await.map_err(SigningError)?;
        let headers: Vec<(&str, &str)> = [("host", host_header(url))]
            .into_iter()
            .chain(headers.iter().copied())
            .collect();
        let signed = sigv4::sign(
            &sigv4::Request {
                method: &Self::METHOD,
                path: url.path(),
                query: url.query().unwrap_or_default(),
                headers: &headers,
                body: &[],
            },
            sigv4::AccessKey {
                id: credentials.access_key_id(),
                secret: credentials.secret_access_key(),
                session_token: credentials.session_token(),
            },
            &sigv4::CredentialScope {
                region: self.region.as_str(),
                service: STS_SIGNING_NAME,
            },
            time,
        );
        let owned = |(name, value): (&str, &str)| (name.to_owned(), value.to_owned());
        let mut sorted: Vec<(String, String)> = headers
            .into_iter()
            .map(owned)
            .chain(signed.date_and_token().map(owned))
            .collect();
        sorted.sort_unstable();
        Ok(CallerIdentityHeaders {
            signed: sorted,
            authorization: signed.authorization().to_owned(),
        })
    }
}

/// The headers of a signed caller identity request.
pub(crate) struct CallerIdentityHeaders {
    /// Every signed header, `host` among them, sorted by name.
    signed: Vec<(String, String)>,
    /// The `authorization` header's value, which covers `signed`.
    authorization: String,
}

impl CallerIdentityHeaders {
    /// Every signed header sorted by name, then `authorization` under
    /// `authorization_name`.
    pub(crate) fn headers<'a>(
        &'a self,
        authorization_name: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.signed
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain([(authorization_name, self.authorization.as_str())])
    }
}

/// Prints header names only, as the values hold a session token and a
/// signature.
impl fmt::Debug for CallerIdentityHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.headers("authorization").map(|(name, _)| name))
            .finish()
    }
}

/// The `host` header a request to `url` carries: its host, and its port
/// when it is not the scheme's.
fn host_header(url: &Url) -> &str {
    &url[url::Position::BeforeHost..url::Position::AfterPort]
}

#[cfg(test)]
impl SigningError {
    /// The error of a credential load that timed out.
    pub(crate) fn timed_out() -> Self {
        Self(Failure::Credentials(Arc::new(
            credentials::Error::TimedOut(CREDENTIALS_LOAD_TIMEOUT),
        )))
    }

    /// The error of a web identity load that STS refused.
    pub(crate) fn refused() -> Self {
        Self(Failure::Credentials(Arc::new(
            credentials::tests::web_identity_refused(),
        )))
    }
}

#[cfg(test)]
impl CallerIdentitySigner {
    /// A signer in `region` with the example key and `session_token`.
    pub(crate) fn example(region: &str, session_token: Option<&str>) -> Self {
        Self {
            credentials: Arc::new(SharedCredentials::new(tests::static_credentials(
                session_token,
            ))),
            region: Region::parse(region).unwrap(),
        }
    }
}

/// The credentials for `config`'s profile in `region`, from
/// `aws_credentials`, with a chain built for them if there are none yet.
fn shared_credentials(
    host: &Host,
    config: Config,
    region: &Region,
    retry: &retry::Settings,
    aws_credentials: &AwsCredentialProviders,
) -> Result<Arc<SharedCredentials>, ConfigError> {
    let key = ProviderKey {
        profile: config.profile_name().to_owned(),
        region: region.clone(),
    };
    aws_credentials.get_or_try_insert(key, || {
        let chain = Chain::new(host, region, config, retry).map_err(ConfigError::HttpClient)?;
        Ok::<_, ConfigError>(SharedCredentials::new(Provider::Chain(Box::new(chain))))
    })
}

/// A client for one AWS JSON API in one region.
pub(crate) struct Client {
    api: Api,
    credentials: Arc<SharedCredentials>,
    service: Service,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("service", &self.service)
            .field("endpoint", &self.api.endpoint)
            .finish_non_exhaustive()
    }
}

/// Everything a [`Client`] is built from.
struct Parts {
    credentials: Arc<SharedCredentials>,
    service: Service,
    network: Network,
    api: ApiParts,
}

impl Client {
    /// A client for `service` in `region`, configured from the process
    /// environment and the shared AWS files, `profile` naming the profile
    /// to use in place of `AWS_PROFILE`, with credentials shared through
    /// `aws_credentials`.
    pub(crate) fn from_env(
        region: &str,
        profile: Option<&str>,
        service: Service,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Self, ConfigError> {
        Self::new(&Host::current(), region, profile, service, aws_credentials)
    }

    /// A client for `service` in `region`, configured from `host` for
    /// `profile`.
    fn new(
        host: &Host,
        region: &str,
        profile: Option<&str>,
        service: Service,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Self, ConfigError> {
        let config = Config::load(host, profile);
        let region = Region::parse(region)?;
        let endpoint = endpoint::resolve(
            service,
            &region,
            config.fips(),
            config.dual_stack(),
            config.endpoint_url(service.api_id().service_id)?,
        )?;
        let retry = config.retry()?;
        let api = ApiParts {
            partition: retry::partition(service.api_id().service_id, &region),
            endpoint,
            retry: retry::Strategy::new(&retry),
            skew_correction: config.skew_correction(),
            stall_timeout: STALL_TIMEOUT,
            region,
        };
        let credentials = shared_credentials(host, config, &api.region, &retry, aws_credentials)?;
        Self::from_parts(Parts {
            credentials,
            service,
            network: host.network.clone(),
            api,
        })
    }

    fn from_parts(parts: Parts) -> Result<Self, ConfigError> {
        let Parts {
            credentials,
            service,
            network,
            api,
        } = parts;
        Ok(Self {
            api: Api::new(&network, service.api_id(), api).map_err(ConfigError::HttpClient)?,
            credentials,
            service,
        })
    }

    /// Call `O` with `input`, retried as [`Api::call`] does, and decode its
    /// output.
    pub(crate) async fn call<O: Operation>(&self, input: &O) -> Result<O::Output, CallError> {
        let target = format!("{}.{}", self.service.target_prefix(), O::NAME);
        let request = ApiRequest {
            operation: O::NAME,
            method: http::Method::POST,
            path: "/",
            headers: &[(CONTENT_TYPE, AWS_JSON_1_1), (X_AMZ_TARGET, &target)],
            body: Bytes::from(serde_json::to_vec(input).expect("operation inputs serialize")),
            modeled_errors: &[],
        };
        self.api
            .call(&request, || async {
                self.credentials.load().await.map(Signing::Signed)
            })
            .await
            .and_then(|body| crate::json::parse(&body).map_err(Failure::Decode))
            .map_err(|failure| CallError {
                service: self.service,
                operation: O::NAME,
                failure,
            })
    }
}

/// The HTTP client settings for AWS API calls. The client never follows a
/// redirect, since a signature names the host it was made for, and it uses
/// `HTTPS_PROXY` and `NO_PROXY` as the rest of ocync does.
fn api_client(stall_timeout: Duration) -> reqwest::ClientBuilder {
    crate::client::no_redirect_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(stall_timeout)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::num::NonZeroU32;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use wiremock::matchers::{any, body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::retry::Retryable as _;
    use super::*;

    /// An operation the tests call, answered with `{"ok": true}`.
    #[derive(Serialize)]
    struct Probe {}

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct ProbeOutput {
        ok: bool,
    }

    impl Operation for Probe {
        const NAME: &'static str = "GetAuthorizationToken";
        type Output = ProbeOutput;
    }

    /// A test's own credential loader.
    pub(super) type Loader = Box<
        dyn Fn() -> Pin<Box<dyn Future<Output = Result<Credentials, credentials::Error>> + Send>>
            + Send
            + Sync,
    >;

    /// A provider answering with the example key and `session_token`.
    pub(super) fn static_credentials(session_token: Option<&str>) -> Provider {
        let credentials = credentials::tests::key(session_token, None);
        Provider::Test(Box::new(move || {
            let credentials = credentials.clone();
            Box::pin(async move { Ok(credentials) })
        }))
    }

    /// The SDK's settings with `max_attempts` and a 1 ms first backoff, so a
    /// test's retries take milliseconds.
    fn strategy(max_attempts: NonZeroU32) -> retry::Strategy {
        retry::Strategy::new(&retry::Settings {
            max_attempts,
            initial_backoff: Duration::from_millis(1),
            ..retry::Settings::default()
        })
    }

    /// The parts of an ECR client with static credentials and the
    /// [`api_parts`] for `endpoint`.
    fn parts(endpoint: &str) -> Parts {
        Parts {
            credentials: Arc::new(SharedCredentials::new(static_credentials(None))),
            service: Service::Ecr,
            network: Network::Real,
            api: api_parts(endpoint),
        }
    }

    /// The settings of an API in `us-east-1` at `endpoint`, with the SDK's
    /// default attempts and its own retry quota.
    fn api_parts(endpoint: &str) -> ApiParts {
        ApiParts {
            region: Region::parse("us-east-1").unwrap(),
            endpoint: Endpoint::parse(endpoint).unwrap(),
            retry: strategy(retry::DEFAULT_MAX_ATTEMPTS),
            partition: Arc::default(),
            skew_correction: SkewCorrection::On,
            stall_timeout: STALL_TIMEOUT,
        }
    }

    /// An ECR client for `server` that makes `max_attempts` attempts.
    pub(crate) fn client(server: &MockServer, max_attempts: NonZeroU32) -> Client {
        let mut parts = parts(&server.uri());
        parts.api.retry = strategy(max_attempts);
        Client::from_parts(parts).unwrap()
    }

    fn ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true}))
    }

    /// ECR's answer to a throttled request.
    pub(crate) fn throttle() -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "__type": "ThrottlingException",
            "message": "Rate exceeded"
        }))
    }

    /// The signed `host` keeps a port the URL names, and drops the
    /// scheme's own. Breaks if a custom STS port is signed without it.
    #[test]
    fn signs_the_host_with_its_port() {
        for (url, host) in [
            (
                "https://sts.internal.example:8443/",
                "sts.internal.example:8443",
            ),
            (
                "https://sts.us-east-1.amazonaws.com:443/",
                "sts.us-east-1.amazonaws.com",
            ),
            (
                "https://sts.us-east-1.amazonaws.com/",
                "sts.us-east-1.amazonaws.com",
            ),
        ] {
            assert_eq!(host_header(&Url::parse(url).unwrap()), host);
        }
    }

    /// An answer longer than any AWS answer is refused, not buffered, and
    /// not re-sent. Breaks if the body is read whatever its length.
    #[tokio::test]
    async fn an_oversized_answer_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b' ';
                crate::json::MAX_SMALL_ANSWER
                    + 1
            ]))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::from_parts(parts(&server.uri())).unwrap();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert!(matches!(err.failure, Failure::TooLarge), "{err}");
    }

    /// An error answer longer than the limit is still the service's error,
    /// classified by its status, so a large proxy error page is retried or
    /// refused by what it says. Breaks if the limit applies to error answers.
    #[tokio::test]
    async fn an_oversized_error_answer_is_the_service_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_bytes(vec![
                b' ';
                crate::json::MAX_SMALL_ANSWER
                    + 1
            ]))
            .mount(&server)
            .await;

        let client = Client::from_parts(parts(&server.uri())).unwrap();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert!(matches!(err.failure, Failure::Service(_)), "{err}");
    }

    /// An error answer cut off partway is a transport failure, which may
    /// pass if sent again, not the service's error read from a part. Breaks
    /// if a cut-off error body is parsed as if whole.
    #[tokio::test]
    async fn a_cut_off_error_answer_is_a_transport_failure() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.read(&mut [0; 4096]).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 100\r\n\r\n{")
                    .await;
            }
        });

        let mut parts = parts(&endpoint);
        parts.api.retry = strategy(NonZeroU32::MIN);
        let client = Client::from_parts(parts).unwrap();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert!(matches!(err.failure, Failure::Transport(_)), "{err}");
    }

    /// The request carries the AWS JSON protocol headers and a signature
    /// scoped to the region and the service's signing name.
    #[tokio::test]
    async fn sends_a_signed_json_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("content-type", AWS_JSON_1_1))
            .and(header(
                X_AMZ_TARGET,
                "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken",
            ))
            .and(header("x-amz-security-token", "session"))
            .and(header_exists("x-amz-date"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::from_parts(Parts {
            credentials: Arc::new(SharedCredentials::new(static_credentials(Some("session")))),
            ..parts(&server.uri())
        })
        .unwrap();
        assert_eq!(
            client.call(&Probe {}).await.unwrap(),
            ProbeOutput { ok: true }
        );

        let request = &server.received_requests().await.unwrap()[0];
        let authorization = request.headers["authorization"].to_str().unwrap();
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
                && authorization.contains(
                    "/us-east-1/ecr/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target, Signature="
                ),
            "{authorization}"
        );
    }

    /// ECR answers a throttle with 400 and `ThrottlingException`, which the
    /// SDK re-sends, up to the configured attempts (`AWS_MAX_ATTEMPTS`) and
    /// no further: one attempt means no retry at all.
    #[tokio::test]
    async fn retries_a_throttle_up_to_the_configured_attempts() {
        for (max_attempts, sent) in [(3, 3), (1, 1), (10, 10)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(throttle())
                .expect(sent)
                .mount(&server)
                .await;
            let err = client(&server, NonZeroU32::new(max_attempts).unwrap())
                .call(&Probe {})
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "ECR GetAuthorizationToken failed: ThrottlingException (HTTP 400 Bad Request): Rate exceeded"
            );
            assert_eq!(err.credential_failure(), CredentialFailure::Transient);
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(throttle())
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client(&server, retry::DEFAULT_MAX_ATTEMPTS)
                .call(&Probe {})
                .await
                .unwrap(),
            ProbeOutput { ok: true }
        );
    }

    /// A server or proxy that accepts the request and then sends nothing
    /// fails the call once the stall limit passes, rather than holding it.
    #[tokio::test]
    async fn fails_a_call_whose_answer_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });
        let mut parts = parts(&format!("http://{address}"));
        parts.api.retry = strategy(NonZeroU32::MIN);
        parts.api.stall_timeout = Duration::from_millis(200);
        let client = Client::from_parts(parts).unwrap();
        let err = tokio::time::timeout(Duration::from_secs(10), client.call(&Probe {}))
            .await
            .expect("the stall limit ends the call")
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("ECR GetAuthorizationToken failed: request failed"),
            "{err}"
        );
        held.abort();
    }

    /// ECR's answer to a request whose identity may not call it.
    pub(crate) fn access_denied() -> ResponseTemplate {
        ResponseTemplate::new(400)
            .insert_header(
                "x-amzn-errortype",
                "AccessDeniedException:http://internal.amazon.com/coral/com.amazon.coral.service/",
            )
            .set_body_json(serde_json::json!({"Message": "not authorized"}))
    }

    /// A refused request is final: re-sending an access denial only spends
    /// the account's request quota.
    #[tokio::test]
    async fn does_not_retry_a_permanent_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(access_denied())
            .expect(1)
            .mount(&server)
            .await;

        let err = client(&server, retry::DEFAULT_MAX_ATTEMPTS)
            .call(&Probe {})
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ECR GetAuthorizationToken failed: AccessDeniedException (HTTP 400 Bad Request): not authorized"
        );
        assert_eq!(err.credential_failure(), CredentialFailure::Refused);
    }

    /// A failure the call re-sends may pass, an answer with a 401, a 403 or
    /// a code that refuses credentials is refused, and anything else failed.
    /// Breaks if a throttle or a credential timeout exits as refused
    /// credentials, or a refusal stops counting as one.
    #[test]
    fn classifies_what_aws_refuses() {
        let service = |status: u16, code: Option<&str>| {
            Failure::Service(ServiceError::parse(
                StatusCode::from_u16(status).unwrap(),
                code,
                b"",
                &[],
            ))
        };
        let load = |error| Failure::Credentials(Arc::new(error));
        for (failure, expected) in [
            (
                service(400, Some("ThrottlingException")),
                CredentialFailure::Transient,
            ),
            (service(503, None), CredentialFailure::Transient),
            (
                service(403, Some("ThrottlingException")),
                CredentialFailure::Transient,
            ),
            (
                service(400, Some("UnrecognizedClientException")),
                CredentialFailure::Refused,
            ),
            (
                service(400, Some("ExpiredTokenException")),
                CredentialFailure::Refused,
            ),
            (service(401, None), CredentialFailure::Refused),
            (service(403, None), CredentialFailure::Refused),
            (
                service(400, Some("InvalidParameterException")),
                CredentialFailure::Failed,
            ),
            (
                load(credentials::Error::TimedOut(CREDENTIALS_LOAD_TIMEOUT)),
                CredentialFailure::Transient,
            ),
            (
                load(credentials::Error::NotLoaded("no keys".to_owned())),
                CredentialFailure::Failed,
            ),
        ] {
            assert_eq!(failure.credential_failure(), expected, "{failure}");
        }
    }

    /// A throttle code wins over the status it came with, a timeout code or
    /// a 500, 502, 503 or 504 is transient, a bare 429 is a throttle, and
    /// anything else, or an answer that does not parse, is not retried.
    #[test]
    fn classifies_answers_as_the_sdk_does() {
        let kind_modeled = |status: u16, code: Option<&str>, modeled| {
            Failure::Service(ServiceError::parse(
                StatusCode::from_u16(status).unwrap(),
                code,
                b"",
                modeled,
            ))
            .retry_kind()
        };
        let kind = |status, code| kind_modeled(status, code, &[]);
        // A code one operation models as transient is final for the rest.
        let modeled = &[ModeledError {
            code: "IDPCommunicationError",
            name: "IdpCommunicationErrorException",
            retry: ModeledRetry::Transient,
        }];
        let idp = Some("IDPCommunicationError");
        assert_eq!(
            kind_modeled(400, idp, modeled),
            Some(retry::Kind::Transient)
        );
        assert_eq!(kind(400, idp), None);
        assert_eq!(
            kind(503, Some("ThrottlingException")),
            Some(retry::Kind::Throttling)
        );
        assert_eq!(kind(429, None), Some(retry::Kind::Throttling));
        assert_eq!(
            kind(400, Some("RequestTimeoutException")),
            Some(retry::Kind::Transient)
        );
        for status in [500, 502, 503, 504] {
            assert_eq!(kind(status, None), Some(retry::Kind::Transient), "{status}");
        }
        assert_eq!(kind(501, None), None);
        assert_eq!(kind(400, Some("InvalidParameterException")), None);
        let undecodable = crate::json::parse::<ProbeOutput>(b"{").unwrap_err();
        assert_eq!(Failure::Decode(undecodable).retry_kind(), None);
    }

    /// A retry waits as long as `x-amz-retry-after` asks, and spends from
    /// the quota the client shares: with too little left, the call stops
    /// retrying whatever attempts remain.
    #[tokio::test]
    async fn waits_as_asked_and_spends_the_shared_quota() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(throttle().insert_header(X_AMZ_RETRY_AFTER, "300"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ok())
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        client(&server, retry::DEFAULT_MAX_ATTEMPTS)
            .call(&Probe {})
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(throttle())
            .expect(2)
            .mount(&server)
            .await;
        let mut parts = parts(&server.uri());
        parts.api.retry = strategy(NonZeroU32::new(10).unwrap());
        parts.api.partition = Arc::new(retry::Partition::with_capacity(5));
        let client = Client::from_parts(parts).unwrap();
        client.call(&Probe {}).await.unwrap_err();
    }

    /// A host clock 10 minutes off draws a signature error. The SDK measures
    /// the skew from the answer's `Date` and signs the retry on AWS's clock;
    /// without that, every call fails until the clock is fixed. A skew under
    /// the threshold, or a `Date` from a cache, is a real signature error
    /// and is not re-sent, nor is a rejection of a signature already made on
    /// AWS's clock. With correction off, a skewed signature is a plain
    /// signature error too.
    #[tokio::test]
    async fn signs_on_the_aws_clock_after_a_skewed_signature() {
        let rejected = |ahead: Duration, cached: bool| {
            let date = httpdate::fmt_http_date(SystemTime::now() + ahead);
            let response = ResponseTemplate::new(400)
                .insert_header("date", date.as_str())
                .set_body_json(serde_json::json!({"__type": "InvalidSignatureException"}));
            if cached {
                response.insert_header("age", "1")
            } else {
                response
            }
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(rejected(Duration::from_secs(600), false))
            .expect(1)
            .mount(&server)
            .await;
        let corrected = client(&server, retry::DEFAULT_MAX_ATTEMPTS);
        corrected.api.skew.store(600_000, Ordering::Relaxed);
        corrected.call(&Probe {}).await.unwrap_err();

        for (ahead, cached, skew_correction, sent) in [
            (Duration::from_secs(600), false, SkewCorrection::On, 2),
            (Duration::from_secs(120), false, SkewCorrection::On, 1),
            (Duration::from_secs(600), true, SkewCorrection::On, 1),
            (Duration::from_secs(600), false, SkewCorrection::Off, 1),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(rejected(ahead, cached))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .respond_with(ok())
                .expect(sent - 1)
                .mount(&server)
                .await;
            let mut parts = parts(&server.uri());
            parts.api.skew_correction = skew_correction;
            let client = Client::from_parts(parts).unwrap();
            let result = client.call(&Probe {}).await;
            assert_eq!(
                result.is_ok(),
                sent == 2,
                "{ahead:?} cached={cached} {skew_correction:?}"
            );

            let requests = server.received_requests().await.unwrap();
            if let [first, second] = requests.as_slice() {
                // Seconds into the day of an `x-amz-date`, `YYYYMMDDTHHMMSSZ`.
                let seconds = |r: &wiremock::Request| {
                    let text = r.headers["x-amz-date"].to_str().unwrap();
                    let part = |range: std::ops::Range<usize>| text[range].parse::<i64>().unwrap();
                    part(9..11) * 3600 + part(11..13) * 60 + part(13..15)
                };
                let shift = (seconds(second) - seconds(first)).rem_euclid(86_400);
                assert!((598..=602).contains(&shift), "signed {shift}s later");
            }
        }
    }

    /// An unsigned call that draws a signature error is final, however far
    /// apart the clocks are: there is no signature to make again. Breaks if
    /// a skewed answer re-sends an unsigned request.
    #[tokio::test]
    async fn does_not_resend_an_unsigned_call_for_skew() {
        let server = MockServer::start().await;
        let ahead = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(600));
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .insert_header("date", ahead.as_str())
                    .set_body_json(serde_json::json!({"__type": "InvalidSignatureException"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let api = Api::new(
            &Network::Real,
            Service::Ecr.api_id(),
            api_parts(&server.uri()),
        )
        .unwrap();
        let request = ApiRequest {
            operation: "Probe",
            method: http::Method::POST,
            path: "/",
            headers: &[],
            body: Bytes::new(),
            modeled_errors: &[],
        };
        let err = api
            .call(&request, || async { Ok(Signing::Unsigned) })
            .await
            .unwrap_err();
        assert!(matches!(err, Failure::Service(_)), "{err}");
    }

    /// The operation's path joins a configured endpoint's path with one
    /// slash, as the SDK joins them, and an unsigned call's query has every
    /// byte outside the unreserved set encoded. Breaks if the endpoint's
    /// path is dropped, a slash doubles, or a query value is sent as is.
    #[tokio::test]
    async fn joins_the_operation_path_to_the_endpoint_path() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(ok()).mount(&server).await;
        let api = Api::new(
            &Network::Real,
            Service::Ecr.api_id(),
            api_parts(&format!("{}/prefix/", server.uri())),
        )
        .unwrap();
        let request = |method, path| ApiRequest {
            operation: "Probe",
            method,
            path,
            headers: &[],
            body: Bytes::new(),
            modeled_errors: &[],
        };
        let query = [("role_name", "a b+c/d"), ("account_id", "1")];
        api.call_unsigned(
            &request(http::Method::GET, "/federation/credentials"),
            &query,
        )
        .await
        .unwrap();
        api.call(&request(http::Method::POST, "/"), || async {
            Ok(Signing::Signed(credentials::tests::key(None, None)))
        })
        .await
        .unwrap();
        let requests = server.received_requests().await.unwrap();
        let sent: Vec<_> = requests
            .iter()
            .map(|r| (r.url.path(), r.url.query()))
            .collect();
        assert_eq!(
            sent,
            [
                (
                    "/prefix/federation/credentials",
                    Some("role_name=a%20b%2Bc%2Fd&account_id=1")
                ),
                ("/prefix/", None),
            ]
        );
    }

    /// The skew is the server's `Date` against the midpoint of the round
    /// trip. A cached answer or a missing `Date` measures nothing, and so
    /// does a round trip too long to place the server's reading.
    #[test]
    fn measures_skew_from_the_date_header() {
        let sent = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let headers = |date: SystemTime, age: bool| {
            let mut h = HeaderMap::new();
            h.insert(
                http::header::DATE,
                httpdate::fmt_http_date(date).parse().unwrap(),
            );
            if age {
                h.insert(http::header::AGE, "1".parse().unwrap());
            }
            h
        };
        let two_secs = sent + Duration::from_secs(2);
        let cases: &[(&str, HeaderMap, SystemTime, Option<ClockSkew>)] = &[
            (
                "ahead",
                headers(sent + Duration::from_secs(601), false),
                two_secs,
                Some(ClockSkew(600_000)),
            ),
            (
                "behind",
                headers(sent - Duration::from_secs(299), false),
                two_secs,
                Some(ClockSkew(-300_000)),
            ),
            ("cached", headers(sent, true), two_secs, None),
            ("no date", HeaderMap::new(), two_secs, None),
            (
                "slow",
                headers(sent, false),
                sent + Duration::from_secs(16 * 60),
                None,
            ),
        ];
        for (name, headers, received, expected) in cases {
            assert_eq!(
                ClockSkew::measure(headers, sent, *received),
                *expected,
                "{name}"
            );
        }
        assert_eq!(ClockSkew(-5000).apply(sent), sent - Duration::from_secs(5));
    }

    #[test]
    fn reads_the_error_code_as_the_sdk_does() {
        let parse = |header: Option<&str>, body: &str| {
            let e = ServiceError::parse(StatusCode::BAD_REQUEST, header, body.as_bytes(), &[]);
            (e.code.map(|c| (c.text, c.kind)), e.message)
        };
        let code = |text: &str, kind| Some((text.to_owned(), kind));
        let cases = [
            (
                None,
                r#"{"__type":"FooError"}"#,
                code("FooError", CodeKind::Other),
                None,
            ),
            (
                None,
                r#"{"code":"BarError","__type":"FooError"}"#,
                code("BarError", CodeKind::Other),
                None,
            ),
            (
                Some(
                    "aws.protocoltests.restjson#ThrottlingException:http://internal.amazon.com/coral/",
                ),
                r#"{"__type":"Other"}"#,
                code("ThrottlingException", CodeKind::Throttling),
                None,
            ),
            // An empty header names no code, so the body's is used.
            (
                Some(""),
                r#"{"__type":"InvalidSignatureException"}"#,
                code("InvalidSignatureException", CodeKind::ClockSkew),
                None,
            ),
            // Two message spellings, or a field that is not a string, do not
            // lose the code.
            (
                None,
                r#"{"__type":"FooError","message":"a","Message":"b","code":7}"#,
                code("FooError", CodeKind::Other),
                Some("a".to_owned()),
            ),
            (None, "not json", None, None),
        ];
        for (header, body, expected_code, expected_message) in cases {
            assert_eq!(
                parse(header, body),
                (expected_code, expected_message),
                "{body}"
            );
        }
    }

    /// A credential is reused until shortly before it expires, so a run of
    /// layer checks does not reload it from IMDS or STS on every call.
    #[tokio::test]
    async fn reuses_a_credential_until_it_nears_expiry() {
        for (lifetime, loads) in [(Duration::from_secs(3600), 1), (Duration::from_secs(5), 2)] {
            let count = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&count);
            let client = Client::from_parts(Parts {
                credentials: Arc::new(SharedCredentials::new(Provider::Test(Box::new(
                    move || {
                        counted.fetch_add(1, Ordering::Relaxed);
                        let expiry = SystemTime::now() + lifetime;
                        Box::pin(async move { Ok(credentials::tests::key(None, Some(expiry))) })
                    },
                )))),
                ..parts("https://example.com")
            })
            .unwrap();
            client.credentials.load().await.unwrap();
            client.credentials.load().await.unwrap();
            assert_eq!(count.load(Ordering::Relaxed), loads, "{lifetime:?}");
        }
    }

    /// Clients waiting on one load that fails all get its error instead of
    /// each running the chain again in turn. A later request loads again.
    #[tokio::test(start_paused = true)]
    async fn waiting_clients_share_a_failed_load() {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        let shared = SharedCredentials::new(Provider::Test(Box::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(credentials::Error::NotLoaded("down".to_owned()))
            })
        })));
        let (a, b, c) = tokio::join!(shared.load(), shared.load(), shared.load());
        assert!(a.is_err() && b.is_err() && c.is_err());
        assert_eq!(count.load(Ordering::Relaxed), 1);
        shared.load().await.unwrap_err();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    /// A credential source that never answers, such as a hung
    /// `credential_process`, fails the call after the SDK's bound instead of
    /// holding it forever.
    #[tokio::test(start_paused = true)]
    async fn gives_up_on_a_credential_source_that_hangs() {
        let client = Client::from_parts(Parts {
            credentials: Arc::new(SharedCredentials::new(Provider::Test(Box::new(|| {
                Box::pin(std::future::pending())
            })))),
            ..parts("https://example.com")
        })
        .unwrap();
        let started = tokio::time::Instant::now();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "ECR GetAuthorizationToken failed: failed to load AWS credentials: credentials provider timed out after 21.6 seconds"
        );
        assert_eq!(started.elapsed(), CREDENTIALS_LOAD_TIMEOUT);
    }

    /// A configured region that is not a host label fails when the client is
    /// built, before it can reach an endpoint host name.
    #[test]
    fn refuses_a_region_that_is_not_a_host_label() {
        let host = config::tests::host(&[], "");
        let client = |region| {
            Client::new(
                &host,
                region,
                None,
                Service::Ecr,
                &AwsCredentialProviders::default(),
            )
        };
        client("us-west-2").unwrap();
        let err = client("evil.example/x").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRegion(_)), "{err}");
    }

    /// The settings reach the client: the skew switch, FIPS on the
    /// endpoint and the attempt count. Breaks if `new` drops one.
    #[test]
    fn new_applies_the_settings() {
        let host = config::tests::host(
            &[
                ("AWS_DISABLE_CLOCK_SKEW_CORRECTION", "true"),
                ("AWS_USE_FIPS_ENDPOINT", "true"),
                ("AWS_MAX_ATTEMPTS", "1"),
            ],
            "",
        );
        let new = |host| {
            Client::new(
                host,
                "us-west-2",
                None,
                Service::Ecr,
                &AwsCredentialProviders::default(),
            )
        };
        let client = new(&host).unwrap();
        assert_eq!(client.api.skew_correction, SkewCorrection::Off);
        assert_eq!(
            client.api.endpoint.host(),
            "api.ecr-fips.us-west-2.amazonaws.com"
        );
        assert_eq!(client.api.retry.max_attempts(), 1);
        let bad = config::tests::host(&[("AWS_RETRY_MODE", "legacy")], "");
        let err = new(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRetryMode(_)), "{err}");
    }

    /// A named profile replaces `AWS_PROFILE` for the client's settings.
    /// Breaks if `new` reads the selected profile instead of the named one.
    #[test]
    fn new_reads_the_named_profile() {
        let host = config::tests::host(
            &[("AWS_PROFILE", "default")],
            "[default]\nuse_fips_endpoint = true\n[profile other]\nuse_fips_endpoint = false\n",
        );
        let host_name = |profile| {
            Client::new(
                &host,
                "us-west-2",
                profile,
                Service::Ecr,
                &AwsCredentialProviders::default(),
            )
            .unwrap()
            .api
            .endpoint
            .host()
            .to_owned()
        };
        assert_eq!(host_name(None), "api.ecr-fips.us-west-2.amazonaws.com");
        assert_eq!(host_name(Some("other")), "api.ecr.us-west-2.amazonaws.com");
    }

    /// One set of credentials per profile and region: a second request for
    /// the same key builds nothing, another region or profile builds its
    /// own, and a failed build is not kept, so the next request builds again.
    #[test]
    fn shares_credentials_per_profile_and_region() {
        let aws_credentials = AwsCredentialProviders::default();
        let builds = AtomicUsize::new(0);
        let key = |profile: &str, region: &str| ProviderKey {
            profile: profile.to_owned(),
            region: Region::parse(region).unwrap(),
        };
        let get = |key| {
            aws_credentials.get_or_try_insert(key, || {
                builds.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(SharedCredentials::new(static_credentials(None)))
            })
        };
        let first = get(key("default", "us-west-2")).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &get(key("default", "us-west-2")).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &get(key("default", "us-east-1")).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &get(key("other", "us-west-2")).unwrap()
        ));
        assert_eq!(builds.load(Ordering::Relaxed), 3);

        let failed = aws_credentials.get_or_try_insert(key("failing", "us-west-2"), || Err(()));
        assert!(failed.is_err());
        get(key("failing", "us-west-2")).unwrap();
        assert_eq!(
            builds.load(Ordering::Relaxed),
            4,
            "a failed build must not be kept"
        );
    }

    /// A named profile and the same profile selected through `AWS_PROFILE`
    /// share credentials, across services. Breaks if the key were the raw
    /// profile argument instead of the resolved name.
    #[test]
    fn clients_share_credentials_for_the_resolved_profile() {
        let host = config::tests::host(&[("AWS_PROFILE", "other")], "[profile other]\n");
        let aws_credentials = AwsCredentialProviders::default();
        let client = |profile, service| {
            Client::new(&host, "us-east-1", profile, service, &aws_credentials).unwrap()
        };
        let auth = client(None, Service::Ecr);
        let public = client(Some("other"), Service::EcrPublic);
        assert!(Arc::ptr_eq(&auth.credentials, &public.credentials));
    }

    /// A transport failure's text names its deepest cause and does not
    /// carry the endpoint URL's path, which may hold a key. Breaks if the
    /// refused connection is dropped from the text callers keep.
    #[tokio::test]
    async fn transport_errors_name_their_cause_without_the_endpoint_url() {
        let mut parts = parts(&format!("http://{}/secret-key", crate::REFUSING));
        parts.api.retry = strategy(NonZeroU32::MIN);
        let client = Client::from_parts(parts).unwrap();
        let err = client.call(&Probe {}).await.unwrap_err();
        let text = err.to_string();
        let refused = crate::refusal();
        assert!(
            text.ends_with(&refused),
            "{text} does not end with {refused}"
        );
        assert!(!format!("{text}{err:?}").contains("secret-key"), "{err:?}");
    }

    /// A session token a header cannot carry fails the call before anything
    /// is sent, and is not retried. Breaks if the request goes out without
    /// the token or the failure is re-sent.
    #[tokio::test]
    async fn refuses_a_header_that_cannot_be_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ok())
            .expect(0)
            .mount(&server)
            .await;
        let client = Client::from_parts(Parts {
            credentials: Arc::new(SharedCredentials::new(static_credentials(Some(
                "token\nwith a newline",
            )))),
            ..parts(&server.uri())
        })
        .unwrap();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert!(matches!(err.failure, Failure::InvalidHeader(_)), "{err}");
    }
}
