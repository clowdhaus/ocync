//! Signed calls to the AWS JSON APIs ocync uses: ECR and ECR Public.
//!
//! Signing, endpoint resolution, retries and error parsing are owned here.
//! Credentials and the FIPS, dual-stack and endpoint URL settings still come
//! from `aws-config`'s [`SdkConfig`].

pub(crate) mod endpoint;
pub(crate) mod sigv4;

use std::fmt;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, SystemTime};

use aws_config::SdkConfig;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_types::service_config::ServiceConfigKey;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use url::Url;

use self::endpoint::{DualStack, Fips};
use crate::retry::{MAX_TRANSIENT_RETRIES, is_transient_transport, retrying_when};

/// The content type of the AWS JSON 1.1 protocol both APIs speak.
const AWS_JSON_1_1: &str = "application/x-amz-json-1.1";

/// The AWS SDK's connect timeout, so an unreachable endpoint fails as fast
/// as it did through the SDK.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(3100);

/// How long a credential without an expiry is reused, as the SDK's identity
/// cache does.
const DEFAULT_EXPIRATION: Duration = Duration::from_secs(15 * 60);

/// A credential is replaced this long before it expires, as in the SDK.
const REFRESH_BUFFER: Duration = Duration::from_secs(10);

/// Error codes the SDK retries: throttles and transient service faults.
const RETRYABLE_CODES: &[&str] = &[
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
    /// The HTTP client could not be built.
    #[error("failed to build the AWS HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

/// An AWS error code, such as `ThrottlingException`, without the namespace
/// prefix and trailing URL some services wrap it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorCode(String);

impl ErrorCode {
    /// Read the code from `x-amzn-errortype`, `code` or `__type` text, such as
    /// `aws.protocoltests#FooError:http://internal.amazon.com/coral/`.
    fn parse(raw: &str) -> Self {
        let without_url = raw.split(':').next().unwrap_or_default();
        Self(
            without_url
                .rsplit('#')
                .next()
                .unwrap_or_default()
                .to_owned(),
        )
    }

    /// Whether the SDK re-sends a request that failed with this code.
    fn is_retryable(&self) -> bool {
        RETRYABLE_CODES.contains(&self.0.as_str())
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an AWS call failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallError {
    /// No credentials could be loaded.
    #[error("failed to load AWS credentials: {0}")]
    Credentials(#[source] CredentialsError),
    /// The request did not complete.
    #[error("request failed: {0}")]
    Transport(#[source] reqwest::Error),
    /// The service answered with an error.
    #[error(
        "{} (HTTP {status}): {}",
        code.as_ref().map_or("unknown error".to_owned(), ErrorCode::to_string),
        message.as_deref().unwrap_or("no message")
    )]
    Service {
        /// The HTTP status.
        status: http::StatusCode,
        /// The AWS error code.
        code: Option<ErrorCode>,
        /// The service's message.
        message: Option<String>,
    },
    /// The service's answer could not be read.
    #[error("unreadable response: {0}")]
    Decode(#[source] serde_json::Error),
}

impl CallError {
    /// Whether the SDK would re-send the request.
    fn is_transient(&self) -> bool {
        match self {
            Self::Transport(e) => is_transient_transport(e),
            Self::Service { status, code, .. } => {
                matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
                    || code.as_ref().is_some_and(ErrorCode::is_retryable)
            }
            Self::Credentials(_) | Self::Decode(_) => false,
        }
    }
}

/// A client for one AWS JSON API in one region.
pub(crate) struct Client {
    http: reqwest::Client,
    provider: SharedCredentialsProvider,
    credentials: Mutex<Option<(sigv4::Credentials, SystemTime)>>,
    service: Service,
    region: String,
    endpoint: Url,
    /// The `host` header the signature covers: the endpoint's host, and its
    /// port when it is not the scheme's default.
    host: String,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("service", &self.service)
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for `service` in the region and with the credentials,
    /// endpoint settings and endpoint URL override `config` carries.
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
            .map(|url| endpoint::parse(&url))
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
        Self::with_endpoint(provider, service, region, endpoint)
    }

    fn with_endpoint(
        provider: SharedCredentialsProvider,
        service: Service,
        region: String,
        endpoint: Url,
    ) -> Result<Self, ConfigError> {
        crate::install_crypto_provider();
        // Signed requests never follow a redirect: the signature names the
        // host it was made for.
        let http = reqwest::Client::builder()
            .user_agent(concat!("ocync/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ConfigError::HttpClient)?;
        let host = endpoint.host_str().unwrap_or_default();
        let host = match endpoint.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        Ok(Self {
            http,
            provider,
            credentials: Mutex::new(None),
            service,
            region,
            endpoint,
            host,
        })
    }

    /// Call `operation` with the JSON `input`, retrying throttles and
    /// transient faults, and decode its output.
    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        operation: &str,
        input: &serde_json::Value,
    ) -> Result<T, CallError> {
        let body = Bytes::from(input.to_string());
        let target = format!("{}.{operation}", self.service.target_prefix());
        let budget = AtomicU32::new(MAX_TRANSIENT_RETRIES);
        retrying_when(operation, &budget, CallError::is_transient, || {
            self.send(&target, body.clone())
        })
        .await
    }

    async fn send<T: DeserializeOwned>(&self, target: &str, body: Bytes) -> Result<T, CallError> {
        let credentials = self.credentials().await?;
        let signed = sigv4::sign(
            &sigv4::Request {
                method: &http::Method::POST,
                path: self.endpoint.path(),
                query: self.endpoint.query(),
                headers: &[
                    ("content-type", AWS_JSON_1_1),
                    ("host", &self.host),
                    ("x-amz-target", target),
                ],
                body: &body,
            },
            &credentials,
            &sigv4::Scope {
                region: &self.region,
                service: self.service.signing_name(),
            },
            SystemTime::now(),
        );
        let mut request = self
            .http
            .post(self.endpoint.clone())
            .header("content-type", AWS_JSON_1_1)
            .header("x-amz-target", target)
            .body(body);
        for (name, value) in signed {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(CallError::Transport)?;
        let status = response.status();
        let header_code = response
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .map(ErrorCode::parse);
        let bytes = response.bytes().await.map_err(CallError::Transport)?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(CallError::Decode);
        }
        Err(service_error(status, header_code, &bytes))
    }

    /// The cached credential, replaced shortly before it expires. The lock is
    /// held across the load on purpose, so concurrent callers share one load
    /// instead of each asking IMDS or STS.
    async fn credentials(&self) -> Result<sigv4::Credentials, CallError> {
        let mut cached = self.credentials.lock().await;
        if let Some((credentials, expires)) = cached.as_ref()
            && SystemTime::now() + REFRESH_BUFFER < *expires
        {
            return Ok(credentials.clone());
        }
        let loaded = self
            .provider
            .provide_credentials()
            .await
            .map_err(CallError::Credentials)?;
        let expires = loaded
            .expiry()
            .unwrap_or_else(|| SystemTime::now() + DEFAULT_EXPIRATION);
        let credentials = sigv4::Credentials {
            access_key_id: loaded.access_key_id().to_owned(),
            secret_access_key: loaded.secret_access_key().to_owned(),
            session_token: loaded.session_token().map(str::to_owned),
        };
        *cached = Some((credentials.clone(), expires));
        Ok(credentials)
    }
}

/// An AWS JSON error: the code from `x-amzn-errortype`, else the body's
/// `code` or `__type`.
fn service_error(
    status: http::StatusCode,
    header_code: Option<ErrorCode>,
    body: &[u8],
) -> CallError {
    #[derive(serde::Deserialize, Default)]
    struct Body {
        code: Option<String>,
        #[serde(rename = "__type")]
        kind: Option<String>,
        #[serde(alias = "Message", alias = "errorMessage")]
        message: Option<String>,
    }
    // An error body that is not JSON still carries the status, which is
    // what retry classification needs.
    let body: Body = serde_json::from_slice(body).unwrap_or_default();
    let code = header_code.or_else(|| body.code.or(body.kind).as_deref().map(ErrorCode::parse));
    CallError::Service {
        status,
        code,
        message: body.message,
    }
}

#[cfg(test)]
mod tests {
    use aws_credential_types::Credentials;
    use wiremock::matchers::{body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer, session_token: Option<&str>) -> Client {
        let provider = SharedCredentialsProvider::new(Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token.map(str::to_owned),
            None,
            "test",
        ));
        Client::with_endpoint(
            provider,
            Service::Ecr,
            "us-east-1".into(),
            server.uri().parse().unwrap(),
        )
        .unwrap()
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Output {
        ok: bool,
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
                "x-amz-target",
                "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken",
            ))
            .and(header("x-amz-security-token", "session"))
            .and(header_exists("x-amz-date"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let out: Output = client(&server, Some("session"))
            .call("GetAuthorizationToken", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out, Output { ok: true });

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
    /// SDK retries. Treating the 400 as final would fail the call on the
    /// first throttle.
    #[tokio::test(start_paused = true)]
    async fn retries_a_throttle_answered_with_400() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "ThrottlingException",
                "message": "Rate exceeded"
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let out: Output = client(&server, None)
            .call("BatchCheckLayerAvailability", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out, Output { ok: true });
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

        let err = client(&server, None)
            .call::<Output>("GetAuthorizationToken", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "AccessDeniedException (HTTP 400 Bad Request): not authorized"
        );
    }

    #[test]
    fn reads_the_error_code_as_the_sdk_does() {
        let code = |header: Option<&str>, body: &str| match service_error(
            http::StatusCode::BAD_REQUEST,
            header.map(ErrorCode::parse),
            body.as_bytes(),
        ) {
            CallError::Service { code, .. } => code.map(|c| c.to_string()),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            code(None, r#"{"__type":"FooError"}"#).as_deref(),
            Some("FooError")
        );
        assert_eq!(
            code(None, r#"{"code":"BarError","__type":"FooError"}"#).as_deref(),
            Some("BarError")
        );
        assert_eq!(
            code(
                Some("aws.protocoltests.restjson#FooError:http://internal.amazon.com/coral/com.amazon.coral.validate/"),
                r#"{"__type":"Other"}"#
            )
            .as_deref(),
            Some("FooError")
        );
        assert_eq!(code(None, "not json"), None);
    }

    /// A credential is reused until shortly before it expires, so a run of
    /// layer checks does not reload it from IMDS or STS on every call.
    #[tokio::test]
    async fn reuses_a_credential_until_it_nears_expiry() {
        #[derive(Debug)]
        struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>, Duration);
        impl ProvideCredentials for Counting {
            fn provide_credentials<'a>(
                &'a self,
            ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
            where
                Self: 'a,
            {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
                    Credentials::new("id", "secret", None, Some(SystemTime::now() + self.1), "t"),
                ))
            }
        }
        for (lifetime, loads) in [(Duration::from_secs(3600), 1), (Duration::from_secs(5), 2)] {
            let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let client = Client::with_endpoint(
                SharedCredentialsProvider::new(Counting(count.clone(), lifetime)),
                Service::Ecr,
                "us-east-1".into(),
                "https://example.com".parse().unwrap(),
            )
            .unwrap();
            client.credentials().await.unwrap();
            client.credentials().await.unwrap();
            assert_eq!(
                count.load(std::sync::atomic::Ordering::Relaxed),
                loads,
                "{lifetime:?}"
            );
        }
    }
}
