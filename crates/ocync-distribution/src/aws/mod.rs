//! Signed calls to the AWS JSON APIs ocync uses: ECR and ECR Public.
//!
//! Signing, endpoint resolution, retries, clock-skew correction and error
//! parsing are owned here. Credentials and the FIPS, dual-stack, endpoint URL
//! and attempt settings still come from `aws-config`'s `SdkConfig`.

mod endpoint;
mod sigv4;

use std::fmt;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use aws_config::SdkConfig;
use aws_credential_types::Credentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_types::service_config::ServiceConfigKey;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use self::endpoint::{DualStack, Endpoint, Fips};
use crate::auth::ecr::{CredentialCache, remaining};
use crate::retry::{MAX_TRANSIENT_RETRIES, Transient, is_transient_transport, retrying};

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

/// Attempts per call when the configuration sets none, as in the SDK.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// The SDK's first backoff, which doubles for each later retry.
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

/// Statuses the SDK re-sends whatever the error code.
const TRANSIENT_STATUSES: &[StatusCode] = &[
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

/// Error codes the SDK re-sends: throttles and transient service faults.
const TRANSIENT_CODES: &[&str] = &[
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
    "RequestTimeout",
    "RequestTimeoutException",
];

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// The configuration has no credentials provider.
    #[error("no AWS credentials provider configured")]
    MissingCredentialsProvider,
    /// An endpoint URL is configured together with FIPS endpoints.
    #[error("Invalid Configuration: FIPS and custom endpoint are not supported")]
    FipsWithEndpointUrl,
    /// An endpoint URL is configured together with dual-stack endpoints.
    #[error("Invalid Configuration: Dualstack and custom endpoint are not supported")]
    DualStackWithEndpointUrl,
    /// The endpoint is not a URL.
    #[error("invalid AWS endpoint '{url}': {source}")]
    InvalidEndpoint {
        /// The text that failed to parse.
        url: String,
        /// Why it is not a URL.
        source: url::ParseError,
    },
    /// The endpoint is a URL with no host to send to, such as `unix:`.
    #[error("AWS endpoint '{url}' is not an http or https URL")]
    EndpointNotHttp {
        /// The configured text.
        url: String,
    },
    /// The HTTP client could not be built.
    #[error("failed to build the AWS HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

/// What an AWS error code means for the request that drew it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeKind {
    /// A throttle or transient fault: re-send.
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
        let kind = if TRANSIENT_CODES.contains(&code) {
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

/// Month names as an IMF-fixdate spells them.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Read an IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`, the `Date` form
/// RFC 9110 requires of senders. Fractional seconds, which the SDK also
/// accepts, are dropped. Any other text is `None`.
fn parse_imf_fixdate(text: &str) -> Option<SystemTime> {
    let (weekday, rest) = text.split_once(", ")?;
    let mut fields = rest.split(' ');
    let (day, month, year, time, zone) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    if weekday.len() != 3 || zone != "GMT" || fields.next().is_some() || day.len() != 2 {
        return None;
    }
    let number = |field: &str| -> Option<u64> {
        field
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| field.parse().ok())
            .flatten()
    };
    let month = MONTHS.iter().position(|m| *m == month)? as u64 + 1;
    let (day, year) = (number(day)?, number(year)?);
    let time = time.split_once('.').map_or(time, |(whole, _)| whole);
    let mut clock = time.split(':').map(number);
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || year > 9999
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = sigv4::days_from_civil(year, month, day)?;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Why one attempt at a call failed.
#[derive(Debug, thiserror::Error)]
enum Failure {
    /// No credentials could be loaded.
    #[error("failed to load AWS credentials: {0}")]
    Credentials(#[source] CredentialsError),
    /// The request did not complete.
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

impl Transient for Failure {
    /// What the SDK re-sends.
    fn is_transient(&self) -> bool {
        match self {
            Self::Transport(e) => is_transient_transport(e),
            Self::Service(e) => {
                TRANSIENT_STATUSES.contains(&e.status)
                    || e.code
                        .as_ref()
                        .is_some_and(|c| c.kind == CodeKind::Transient)
            }
            Self::ClockSkewed { .. } => true,
            Self::Credentials(_) | Self::Decode(_) => false,
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
    max_attempts: u32,
    service: Service,
    region: String,
    endpoint: Endpoint,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("service", &self.service)
            .field("endpoint", &self.endpoint.url().as_str())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for `service` in the region and with the credentials,
    /// endpoint settings, endpoint URL override and attempt count `config`
    /// carries.
    pub(crate) fn new(config: &SdkConfig, service: Service) -> Result<Self, ConfigError> {
        let region = config
            .region()
            .ok_or(ConfigError::MissingRegion)?
            .to_string();
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
        let max_attempts = config
            .retry_config()
            .map_or(DEFAULT_MAX_ATTEMPTS, |r| r.max_attempts());
        static WARNED: std::sync::Once = std::sync::Once::new();
        if max_attempts > MAX_TRANSIENT_RETRIES + 1 {
            WARNED.call_once(|| {
                tracing::warn!(
                %service,
                configured = max_attempts,
                used = MAX_TRANSIENT_RETRIES + 1,
                    "AWS max attempts is above what ocync makes; using the lower count"
                );
            });
        }
        Self::with_endpoint(
            provider,
            service,
            region,
            endpoint,
            max_attempts,
            STALL_TIMEOUT,
        )
    }

    fn with_endpoint(
        provider: SharedCredentialsProvider,
        service: Service,
        region: String,
        endpoint: Endpoint,
        max_attempts: u32,
        stall_timeout: Duration,
    ) -> Result<Self, ConfigError> {
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
            max_attempts,
            service,
            region,
            endpoint,
        })
    }

    /// Call `O` with `input`, re-sending throttles, transient faults and
    /// clock-skewed signatures up to the configured attempt count, and
    /// decode its output.
    ///
    /// The schedule between attempts is the crate's (see
    /// [`crate::retry`]), so at most `MAX_TRANSIENT_RETRIES + 1` attempts are
    /// made whatever the configuration asks for.
    pub(crate) async fn call<O: Operation>(&self, input: &O) -> Result<O::Output, CallError> {
        let body = Bytes::from(serde_json::to_vec(input).expect("operation inputs serialize"));
        let target = format!("{}.{}", self.service.target_prefix(), O::NAME);
        let budget = AtomicU32::new(self.max_attempts.saturating_sub(1));
        retrying(O::NAME, &budget, || self.send(&target, body.clone()))
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
        let skew = ClockSkew(self.skew.load(Ordering::Relaxed));
        let signed = sigv4::sign(
            &sigv4::Request {
                method: &http::Method::POST,
                path: self.endpoint.url().path(),
                query: self.endpoint.url().query(),
                headers: &signed_headers,
                body: &body,
            },
            sigv4::AccessKey {
                id: credentials.access_key_id(),
                secret: credentials.secret_access_key(),
                session_token: credentials.session_token(),
            },
            &sigv4::CredentialScope {
                region: &self.region,
                service: self.service.signing_name(),
            },
            skew.apply(sent),
        );
        let mut request = self.http.post(self.endpoint.url().clone()).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        for (name, value) in signed {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(Failure::Transport)?;
        let measured = ClockSkew::measure(response.headers(), sent, SystemTime::now());
        if let Some(skew) = measured {
            self.skew.store(skew.0, Ordering::Relaxed);
        }
        let status = response.status();
        let header_code = response
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .and_then(ErrorCode::parse);
        let bytes = response.bytes().await.map_err(Failure::Transport)?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(Failure::Decode);
        }
        let error = ServiceError::parse(status, header_code, &bytes);
        let signature_rejected = error
            .code
            .as_ref()
            .is_some_and(|c| c.kind == CodeKind::ClockSkew);
        Err(match measured {
            Some(skew) if signature_rejected && skew.magnitude() > SKEW_THRESHOLD => {
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

    fn client(server: &MockServer, max_attempts: u32) -> Client {
        Client::with_endpoint(
            static_credentials(None),
            Service::Ecr,
            "us-east-1".into(),
            Endpoint::parse(&server.uri()).unwrap(),
            max_attempts,
            STALL_TIMEOUT,
        )
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

        let client = Client::with_endpoint(
            static_credentials(Some("session")),
            Service::Ecr,
            "us-east-1".into(),
            Endpoint::parse(&server.uri()).unwrap(),
            DEFAULT_MAX_ATTEMPTS,
            STALL_TIMEOUT,
        )
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
        let cap = MAX_TRANSIENT_RETRIES as u64 + 1;
        for (max_attempts, sent) in [(3, 3), (1, 1), (10, cap)] {
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
        let client = Client::with_endpoint(
            static_credentials(None),
            Service::Ecr,
            "us-east-1".into(),
            Endpoint::parse(&format!("http://{address}")).unwrap(),
            1,
            Duration::from_millis(200),
        )
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

    /// A host clock 10 minutes off draws a signature error. The SDK measures
    /// the skew from the answer's `Date` and signs the retry on AWS's clock;
    /// without that, every call fails until the clock is fixed. A skew under
    /// the threshold, or a `Date` from a cache, is a real signature error
    /// and is not re-sent.
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

    /// The parser agrees with `httpdate` on what `httpdate` writes, takes
    /// the SDK's fractional seconds, and refuses the obsolete forms and
    /// anything not in GMT.
    #[test]
    fn reads_an_imf_fixdate() {
        for secs in [0, 951_782_400, 1_709_251_199, 1_735_689_599, 4_107_542_400] {
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            let text = httpdate::fmt_http_date(time);
            assert_eq!(parse_imf_fixdate(&text), Some(time), "{text}");
        }
        assert_eq!(
            parse_imf_fixdate("Mon, 16 Dec 2019 23:48:18.123 GMT"),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_576_540_098))
        );
        for bad in [
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, +6 Nov 1994 08:49:37 GMT",
            "Sat, 01 Jan 0000 00:00:00 GMT",
        ] {
            assert_eq!(parse_imf_fixdate(bad), None, "{bad}");
        }
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
                code("ThrottlingException", CodeKind::Transient),
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
            let client = Client::with_endpoint(
                SharedCredentialsProvider::new(Counting(count.clone(), lifetime)),
                Service::Ecr,
                "us-east-1".into(),
                Endpoint::parse("https://example.com").unwrap(),
                DEFAULT_MAX_ATTEMPTS,
                STALL_TIMEOUT,
            )
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
        let client = Client::with_endpoint(
            SharedCredentialsProvider::new(Hangs),
            Service::Ecr,
            "us-east-1".into(),
            Endpoint::parse("https://example.com").unwrap(),
            DEFAULT_MAX_ATTEMPTS,
            STALL_TIMEOUT,
        )
        .unwrap();
        let started = tokio::time::Instant::now();
        let err = client.call(&Probe {}).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "ECR GetAuthorizationToken failed: failed to load AWS credentials: credentials provider timed out after 21 seconds"
        );
        assert_eq!(started.elapsed(), CREDENTIALS_LOAD_TIMEOUT);
    }
}
