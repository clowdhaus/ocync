//! Signed calls to the AWS JSON APIs ocync uses: ECR and ECR Public.
//!
//! Signing, endpoint resolution, retries, clock-skew correction and error
//! parsing are owned here. Credentials and the FIPS, dual-stack, endpoint URL
//! and attempt settings still come from `aws-config`'s `SdkConfig`.

mod calendar;
mod endpoint;
mod retry;
mod sigv4;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime};

use aws_config::SdkConfig;
use aws_config::retry::RetryConfig;
use aws_credential_types::Credentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_types::service_config::ServiceConfigKey;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use self::calendar::parse_imf_fixdate;
use self::endpoint::{DualStack, Endpoint, Fips, Region};
use crate::credential_cache::{CredentialCache, remaining};
use crate::retry::{is_transient_transport, retrying};

/// The content type of the AWS JSON 1.1 protocol both APIs speak.
const AWS_JSON_1_1: &str = "application/x-amz-json-1.1";

/// The header naming the operation, as `<target prefix>.<operation>`.
const X_AMZ_TARGET: &str = "x-amz-target";

/// A connection not made within this fails, so an unreachable endpoint
/// surfaces in seconds. It is the SDK's value.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(3100);

/// A call that receives nothing for this long after it is sent, or between
/// reads, fails, so a stalled proxy or server cannot hold it forever. It is
/// the SDK's stalled-stream grace period.
const STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// The SDK's default attempts per call, which size
/// [`CREDENTIALS_LOAD_TIMEOUT`]. A call's own attempts come from its
/// `RetryConfig`.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// The SDK's default first backoff, which sizes
/// [`CREDENTIALS_LOAD_TIMEOUT`].
const SDK_INITIAL_BACKOFF_MS: u64 = 1_000;

/// A credential load that takes longer fails. This is the SDK's bound for
/// its default retry and connect settings: every backoff between
/// [`DEFAULT_MAX_ATTEMPTS`] attempts, plus each attempt at twice
/// [`CONNECT_TIMEOUT`].
const CREDENTIALS_LOAD_TIMEOUT: Duration = Duration::from_millis(
    SDK_INITIAL_BACKOFF_MS * ((1 << (DEFAULT_MAX_ATTEMPTS - 1)) - 1)
        + DEFAULT_MAX_ATTEMPTS as u64 * 2 * CONNECT_TIMEOUT.as_millis() as u64,
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

/// The AWS APIs ocync calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Service {
    /// Amazon ECR, for registry tokens and layer checks.
    Ecr,
    /// Amazon ECR Public, for registry tokens.
    EcrPublic,
}

impl Service {
    /// The name the service signs requests under.
    fn signing_name(self) -> &'static str {
        match self {
            Self::Ecr => "ecr",
            Self::EcrPublic => "ecr-public",
        }
    }

    /// The `x-amz-target` prefix naming the service's operations.
    fn target_prefix(self) -> &'static str {
        match self {
            Self::Ecr => "AmazonEC2ContainerRegistry_V20150921",
            Self::EcrPublic => "SpencerFrontendService",
        }
    }

    /// The service ID that names its endpoint URL setting, such as
    /// `AWS_ENDPOINT_URL_ECR_PUBLIC`.
    fn service_id(self) -> &'static str {
        match self {
            Self::Ecr => "ECR",
            Self::EcrPublic => "ECR PUBLIC",
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
    /// The configuration names no region.
    #[error("no AWS region configured")]
    MissingRegion,
    /// The configured region is not a valid host label.
    #[error("invalid AWS region '{0}': must be a valid host label")]
    InvalidRegion(String),
    /// The configuration has no credentials provider.
    #[error("no AWS credentials provider configured")]
    MissingCredentialsProvider,
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
    #[error("AWS endpoint URL scheme '{scheme}' is not http or https")]
    EndpointNotHttp {
        /// The configured scheme.
        scheme: String,
    },
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
}

impl ErrorCode {
    /// Read the code from `x-amzn-errortype`, `code` or `__type` text, such as
    /// `aws.protocoltests#FooError:http://internal.amazon.com/coral/`, as the
    /// SDK does. Text that names no code is `None`.
    fn parse(raw: &str) -> Option<Self> {
        let code = raw.split_once(':').map_or(raw, |(code, _)| code);
        let code = code.split_once('#').map_or(code, |(_, code)| code);
        if code.is_empty() {
            return None;
        }
        let kind = if THROTTLING_CODES.contains(&code) {
            CodeKind::Throttling
        } else if TRANSIENT_CODES.contains(&code) {
            CodeKind::Transient
        } else if CLOCK_SKEW_CODES.contains(&code) {
            CodeKind::ClockSkew
        } else {
            CodeKind::Other
        };
        Some(Self {
            text: code.to_owned(),
            kind,
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
    fn parse(status: StatusCode, header_code: Option<ErrorCode>, body: &[u8]) -> Self {
        let body: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(body).unwrap_or_default();
        let text = |key: &str| body.get(key).and_then(serde_json::Value::as_str);
        let code = header_code.or_else(|| {
            text("code")
                .or_else(|| text("__type"))
                .and_then(ErrorCode::parse)
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
    /// No credentials could be loaded.
    #[error("failed to load AWS credentials: {0}")]
    Credentials(#[source] CredentialsError),
    /// The request did not complete. Built only through `From`, which drops
    /// the URL.
    #[error("request failed: {0}")]
    Transport(#[source] reqwest::Error),
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
    Decode(#[source] serde_json::Error),
}

/// The endpoint URL may carry a key, so a transport error drops it.
impl From<reqwest::Error> for Failure {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

impl Failure {
    /// How the SDK retries this failure, or `None` when it does not. A throttle
    /// code wins over the status it came with, as in the SDK.
    fn retry_kind(&self) -> Option<retry::Kind> {
        match self {
            Self::Transport(e) => is_transient_transport(e).then_some(retry::Kind::Transient),
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
            Self::Credentials(_) | Self::Decode(_) => None,
        }
    }

    /// The wait the service asked for before a retry.
    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Service(error) | Self::ClockSkewed { error, .. } => error.retry_after,
            Self::Credentials(_) | Self::Transport(_) | Self::Decode(_) => None,
        }
    }
}

/// Why an AWS call failed, after its retries.
#[derive(Debug, thiserror::Error)]
#[error("{service} {operation} failed: {source}")]
pub(crate) struct CallError {
    service: Service,
    operation: &'static str,
    #[source]
    source: Failure,
}

/// A client for one AWS JSON API in one region.
pub(crate) struct Client {
    http: reqwest::Client,
    provider: SharedCredentialsProvider,
    credentials: CredentialCache<Credentials>,
    /// The last [`ClockSkew`] measured, which every signature corrects for.
    skew: AtomicI64,
    skew_correction: SkewCorrection,
    retry: retry::Strategy,
    /// The retry quota and rate limiter shared with every client for the
    /// same service and region.
    partition: Arc<retry::Partition>,
    service: Service,
    region: Region,
    endpoint: Endpoint,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("service", &self.service)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Everything a [`Client`] is built from.
struct Parts {
    provider: SharedCredentialsProvider,
    service: Service,
    region: Region,
    endpoint: Endpoint,
    retry: retry::Strategy,
    partition: Arc<retry::Partition>,
    skew_correction: SkewCorrection,
    stall_timeout: Duration,
}

impl Client {
    /// A client for `service` in the region and with the credentials,
    /// endpoint settings, endpoint URL override and attempt count `config`
    /// carries.
    pub(crate) fn new(config: &SdkConfig, service: Service) -> Result<Self, ConfigError> {
        let region = Region::parse(config.region().ok_or(ConfigError::MissingRegion)?.as_ref())?;
        let provider = config
            .credentials_provider()
            .ok_or(ConfigError::MissingCredentialsProvider)?;
        let endpoint_url = config
            .service_config()
            .and_then(|c| {
                let key = ServiceConfigKey::builder()
                    .service_id(service.service_id())
                    .env("AWS_ENDPOINT_URL")
                    .profile("endpoint_url")
                    .build()
                    .expect("every key part is set");
                c.load_config(key)
            })
            .or_else(|| config.endpoint_url().map(str::to_owned))
            .map(|url| Endpoint::parse(&url))
            .transpose()?;
        let fips = match config.use_fips() {
            Some(true) => Fips::On,
            _ => Fips::Off,
        };
        let dual_stack = match config.use_dual_stack() {
            Some(true) => DualStack::On,
            _ => DualStack::Off,
        };
        let endpoint = endpoint::resolve(service, &region, fips, dual_stack, endpoint_url)?;
        let skew_correction = match config.disable_clock_skew_correction() {
            Some(true) => SkewCorrection::Off,
            _ => SkewCorrection::On,
        };
        let retry = config.retry_config().map_or_else(
            || retry::Strategy::new(&RetryConfig::standard()),
            retry::Strategy::new,
        );
        Self::from_parts(Parts {
            provider,
            service,
            partition: retry::partition(service, &region),
            region,
            endpoint,
            retry,
            skew_correction,
            stall_timeout: STALL_TIMEOUT,
        })
    }

    fn from_parts(parts: Parts) -> Result<Self, ConfigError> {
        let Parts {
            provider,
            service,
            region,
            endpoint,
            retry,
            partition,
            skew_correction,
            stall_timeout,
        } = parts;
        crate::install_crypto_provider();
        // Signed requests never follow a redirect: the signature names the
        // host it was made for.
        let http = crate::client::no_redirect_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(stall_timeout)
            .build()
            .map_err(ConfigError::HttpClient)?;
        Ok(Self {
            http,
            provider,
            credentials: CredentialCache::new(),
            skew: AtomicI64::new(0),
            skew_correction,
            retry,
            partition,
            service,
            region,
            endpoint,
        })
    }

    /// Call `O` with `input`, re-sending throttles, transient faults and
    /// clock-skewed signatures as the SDK's retry policy allows (see
    /// [`retry`]), and decode its output.
    pub(crate) async fn call<O: Operation>(&self, input: &O) -> Result<O::Output, CallError> {
        let body = Bytes::from(serde_json::to_vec(input).expect("operation inputs serialize"));
        let target = format!("{}.{}", self.service.target_prefix(), O::NAME);
        let (target, body) = (&target, &body);
        let mut policy = retry::CallPolicy::new(&self.retry, &self.partition);
        retrying(O::NAME, &mut policy, |attempt| async move {
            self.partition.send_token(&self.retry, attempt).await;
            self.send(target, body.clone()).await
        })
        .await
        .map_err(|source| CallError {
            service: self.service,
            operation: O::NAME,
            source,
        })
    }

    async fn send<T: DeserializeOwned>(&self, target: &str, body: Bytes) -> Result<T, Failure> {
        let credentials = self.credentials().await?;
        let headers = [("content-type", AWS_JSON_1_1), (X_AMZ_TARGET, target)];
        let signed_headers = [headers[0], headers[1], ("host", self.endpoint.host())];
        let sent = SystemTime::now();
        let applied = ClockSkew(self.skew.load(Ordering::Relaxed));
        let signed = sigv4::sign(
            &sigv4::Request {
                method: &http::Method::POST,
                path: self.endpoint.url().path(),
                headers: &signed_headers,
                body: &body,
            },
            sigv4::AccessKey {
                id: credentials.access_key_id(),
                secret: credentials.secret_access_key(),
                session_token: credentials.session_token(),
            },
            &sigv4::CredentialScope {
                region: self.region.as_str(),
                service: self.service.signing_name(),
            },
            applied.apply(sent),
        );
        let mut request = self.http.post(self.endpoint.url().clone()).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        for (name, value) in signed {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let measured = match self.skew_correction {
            SkewCorrection::On => ClockSkew::measure(response.headers(), sent, SystemTime::now()),
            SkewCorrection::Off => None,
        };
        if let Some(skew) = measured {
            self.skew.store(skew.0, Ordering::Relaxed);
        }
        let status = response.status();
        let header_code = response
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .and_then(ErrorCode::parse);
        let retry_after = response
            .headers()
            .get(X_AMZ_RETRY_AFTER)
            .and_then(|v| v.to_str().ok()?.parse().ok())
            .map(Duration::from_millis);
        let bytes = response.bytes().await?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(Failure::Decode);
        }
        let error = ServiceError {
            retry_after,
            ..ServiceError::parse(status, header_code, &bytes)
        };
        let signature_rejected = error
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

    /// The cached credential, loaded again shortly before it expires.
    /// Concurrent callers share one load instead of each asking IMDS or STS.
    async fn credentials(&self) -> Result<Credentials, Failure> {
        self.credentials
            .get_or_refresh("aws-credentials", async {
                let loaded = tokio::time::timeout(
                    CREDENTIALS_LOAD_TIMEOUT,
                    self.provider.provide_credentials(),
                )
                .await
                .map_err(|_| CredentialsError::provider_timed_out(CREDENTIALS_LOAD_TIMEOUT))??;
                let ttl = loaded.expiry().map_or(DEFAULT_EXPIRATION, remaining);
                Ok((loaded, ttl))
            })
            .await
            .map_err(Failure::Credentials)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use wiremock::matchers::{body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    fn static_credentials(session_token: Option<&str>) -> SharedCredentialsProvider {
        SharedCredentialsProvider::new(Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token.map(str::to_owned),
            None,
            "test",
        ))
    }

    /// The SDK's settings with `max_attempts` and a 1 ms first backoff, so a
    /// test's retries take milliseconds.
    fn strategy(max_attempts: u32) -> retry::Strategy {
        retry::Strategy::new(
            &RetryConfig::standard()
                .with_max_attempts(max_attempts)
                .with_initial_backoff(Duration::from_millis(1)),
        )
    }

    /// The parts of an ECR client in `us-east-1` at `endpoint`, with static
    /// credentials, the SDK's default attempts and its own retry quota.
    fn parts(endpoint: &str) -> Parts {
        Parts {
            provider: static_credentials(None),
            service: Service::Ecr,
            region: Region::parse("us-east-1").unwrap(),
            endpoint: Endpoint::parse(endpoint).unwrap(),
            retry: strategy(DEFAULT_MAX_ATTEMPTS),
            partition: Arc::default(),
            skew_correction: SkewCorrection::On,
            stall_timeout: STALL_TIMEOUT,
        }
    }

    fn client(server: &MockServer, max_attempts: u32) -> Client {
        Client::from_parts(Parts {
            retry: strategy(max_attempts),
            ..parts(&server.uri())
        })
        .unwrap()
    }

    fn ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true}))
    }

    fn throttle() -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "__type": "ThrottlingException",
            "message": "Rate exceeded"
        }))
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
            provider: static_credentials(Some("session")),
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
            let err = client(&server, max_attempts)
                .call(&Probe {})
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "ECR GetAuthorizationToken failed: ThrottlingException (HTTP 400 Bad Request): Rate exceeded"
            );
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
            client(&server, DEFAULT_MAX_ATTEMPTS)
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
        let client = Client::from_parts(Parts {
            retry: strategy(1),
            stall_timeout: Duration::from_millis(200),
            ..parts(&format!("http://{address}"))
        })
        .unwrap();
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

    /// A refused request is final: re-sending an access denial only spends
    /// the account's request quota.
    #[tokio::test]
    async fn does_not_retry_a_permanent_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .insert_header(
                        "x-amzn-errortype",
                        "AccessDeniedException:http://internal.amazon.com/coral/com.amazon.coral.service/",
                    )
                    .set_body_json(serde_json::json!({"Message": "not authorized"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let err = client(&server, DEFAULT_MAX_ATTEMPTS)
            .call(&Probe {})
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ECR GetAuthorizationToken failed: AccessDeniedException (HTTP 400 Bad Request): not authorized"
        );
    }

    /// A throttle code wins over the status it came with, a timeout code or
    /// a 500, 502, 503 or 504 is transient, a bare 429 is a throttle, and
    /// anything else, or an answer that does not parse, is not retried.
    #[test]
    fn classifies_answers_as_the_sdk_does() {
        let kind = |status: u16, code: Option<&str>| {
            Failure::Service(ServiceError::parse(
                StatusCode::from_u16(status).unwrap(),
                code.and_then(ErrorCode::parse),
                b"",
            ))
            .retry_kind()
        };
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
        let undecodable = serde_json::from_slice::<ProbeOutput>(b"{").unwrap_err();
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
        client(&server, DEFAULT_MAX_ATTEMPTS)
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
        let client = Client::from_parts(Parts {
            retry: strategy(10),
            partition: Arc::new(retry::Partition::with_capacity(5)),
            ..parts(&server.uri())
        })
        .unwrap();
        client.call(&Probe {}).await.unwrap_err();
    }

    /// A host clock 10 minutes off draws a signature error. The SDK measures
    /// the skew from the answer's `Date` and signs the retry on AWS's clock;
    /// without that, every call fails until the clock is fixed. A skew under
    /// the threshold, or a `Date` from a cache, is a real signature error
    /// and is not re-sent, nor is a rejection of a signature already made on
    /// AWS's clock.
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
        let corrected = client(&server, DEFAULT_MAX_ATTEMPTS);
        corrected.skew.store(600_000, Ordering::Relaxed);
        corrected.call(&Probe {}).await.unwrap_err();

        for (ahead, cached, sent) in [
            (Duration::from_secs(600), false, 2),
            (Duration::from_secs(120), false, 1),
            (Duration::from_secs(600), true, 1),
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
            let result = client(&server, DEFAULT_MAX_ATTEMPTS).call(&Probe {}).await;
            assert_eq!(result.is_ok(), sent == 2, "{ahead:?} cached={cached}");

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

    /// With correction disabled, a skewed signature is a plain signature
    /// error: not re-sent, and no later signature shifts.
    #[tokio::test]
    async fn leaves_the_clock_alone_when_skew_correction_is_off() {
        let server = MockServer::start().await;
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(600));
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .insert_header("date", date.as_str())
                    .set_body_json(serde_json::json!({"__type": "InvalidSignatureException"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::from_parts(Parts {
            skew_correction: SkewCorrection::Off,
            ..parts(&server.uri())
        })
        .unwrap();
        client.call(&Probe {}).await.unwrap_err();
        assert_eq!(client.skew.load(Ordering::Relaxed), 0);
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
            let e = ServiceError::parse(
                StatusCode::BAD_REQUEST,
                header.and_then(ErrorCode::parse),
                body.as_bytes(),
            );
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
        #[derive(Debug)]
        struct Counting(Arc<AtomicUsize>, Duration);
        impl ProvideCredentials for Counting {
            fn provide_credentials<'a>(
                &'a self,
            ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
            where
                Self: 'a,
            {
                self.0.fetch_add(1, Ordering::Relaxed);
                aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
                    Credentials::new("id", "secret", None, Some(SystemTime::now() + self.1), "t"),
                ))
            }
        }
        for (lifetime, loads) in [(Duration::from_secs(3600), 1), (Duration::from_secs(5), 2)] {
            let count = Arc::new(AtomicUsize::new(0));
            let client = Client::from_parts(Parts {
                provider: SharedCredentialsProvider::new(Counting(count.clone(), lifetime)),
                ..parts("https://example.com")
            })
            .unwrap();
            client.credentials().await.unwrap();
            client.credentials().await.unwrap();
            assert_eq!(count.load(Ordering::Relaxed), loads, "{lifetime:?}");
        }
    }

    /// A credential source that never answers, such as a hung
    /// `credential_process`, fails the call after the SDK's bound instead of
    /// holding it forever.
    #[tokio::test(start_paused = true)]
    async fn gives_up_on_a_credential_source_that_hangs() {
        #[derive(Debug)]
        struct Hangs;
        impl ProvideCredentials for Hangs {
            fn provide_credentials<'a>(
                &'a self,
            ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
            where
                Self: 'a,
            {
                aws_credential_types::provider::future::ProvideCredentials::new(
                    std::future::pending(),
                )
            }
        }
        let client = Client::from_parts(Parts {
            provider: SharedCredentialsProvider::new(Hangs),
            ..parts("https://example.com")
        })
        .unwrap();
        let started = tokio::time::Instant::now();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "ECR GetAuthorizationToken failed: failed to load AWS credentials: credentials provider timed out after 21 seconds"
        );
        assert_eq!(started.elapsed(), CREDENTIALS_LOAD_TIMEOUT);
    }
    /// A configured region that is not a host label fails when the client is
    /// built, before it can reach an endpoint host name.
    #[test]
    fn refuses_a_region_that_is_not_a_host_label() {
        let config = |region: &'static str| {
            SdkConfig::builder()
                .region(aws_config::Region::new(region))
                .credentials_provider(static_credentials(None))
                .build()
        };
        Client::new(&config("us-west-2"), Service::Ecr).unwrap();
        let err = Client::new(&config("evil.example/x"), Service::Ecr).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRegion(_)), "{err}");
    }

    /// The defaults that size the credential load timeout are the SDK's
    /// standard retry defaults.
    #[test]
    fn sizes_the_credential_timeout_from_the_sdk_defaults() {
        let standard = RetryConfig::standard();
        assert_eq!(DEFAULT_MAX_ATTEMPTS, standard.max_attempts());
        assert_eq!(
            Duration::from_millis(SDK_INITIAL_BACKOFF_MS),
            standard.initial_backoff()
        );
    }

    /// A transport failure does not carry the endpoint URL, whose path may
    /// hold a key, into the error.
    #[tokio::test]
    async fn keeps_the_endpoint_url_out_of_transport_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = Client::from_parts(Parts {
            retry: strategy(1),
            ..parts(&format!("http://{address}/secret-key"))
        })
        .unwrap();
        let err = client.call(&Probe {}).await.unwrap_err();
        let mut chain = format!("{err:?}");
        let mut source: Option<&dyn std::error::Error> = Some(&err);
        while let Some(e) = source {
            chain.push_str(&e.to_string());
            source = e.source();
        }
        assert!(chain.contains("error sending request"), "{chain}");
        assert!(!chain.contains("secret-key"), "{chain}");
    }
}
