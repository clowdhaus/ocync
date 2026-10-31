//! Google Cloud Application Default Credentials, as `google-cloud-auth`
//! 1.18.0 resolves them, with every hash and signature made by `aws-lc-rs`.
//!
//! The credentials come from the file `GOOGLE_APPLICATION_CREDENTIALS`
//! names, else from gcloud's well-known file, else from the metadata server
//! of the GCE or GKE host. Loading finds only a file; the metadata server is
//! [`Credentials::metadata`], which a caller that reads anonymously without
//! one first asks [`is_on_google_cloud`] whether to use. A file holds a service account key, an authorized
//! user's refresh token, an external account (Workload Identity Federation)
//! or a service account impersonated with any of those.

mod aws_source;
mod external_account;
mod impersonated;
mod metadata;
mod service_account;
mod user_account;

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use http::HeaderValue;
use serde_json::Value;
use url::Url;

use crate::json::MAX_SMALL_ANSWER;

use crate::scheme::{Scheme, UnsupportedScheme};

use crate::REDACTED;
use crate::aws::{AwsCredentialProviders, Clock, Env, Host, Platform};
use crate::error::{CredentialFailure, OAuthCode, truncate_quoted};
use crate::retry::{Bounded, retrying};

/// The scope every registry token is asked for: push and pull on Artifact
/// Registry and Container Registry.
const REGISTRY_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

/// The scope a credential asks for when none is set, as upstream's
/// `DEFAULT_SCOPE`.
const DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The variable naming the credentials file.
const CREDENTIALS_VARIABLE: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// gcloud's well-known credentials file, under `HOME` on Unix and `APPDATA`
/// on Windows (AIP-4113).
const WELL_KNOWN_FILE: [&str; 2] = ["gcloud", "application_default_credentials.json"];

/// The directory under `HOME` that holds [`WELL_KNOWN_FILE`] on Unix.
const UNIX_CONFIG_DIR: &str = ".config";

/// A connection to a Google API not made within this fails.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A Google API call that receives nothing for this long, after it is sent
/// or between reads, fails.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// How a request reaches its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Straight to this host or a metadata service, never through a proxy,
    /// so a token never reaches a proxy host.
    Direct,
    /// Through `HTTPS_PROXY` and `NO_PROXY`, like registry traffic.
    Proxied,
}

/// The HTTP clients the credentials are fetched with, one per [`Route`].
#[derive(Debug, Clone)]
pub(crate) struct Clients {
    direct: reqwest::Client,
    proxied: reqwest::Client,
}

impl Clients {
    /// Clients with ocync's TLS provider and no redirects, so a body
    /// carrying a secret is never sent on to another host.
    pub(crate) fn new() -> Result<Self, Error> {
        crate::install_crypto_provider();
        let proxied = crate::client::no_redirect_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(Error::Client)?;
        let direct = crate::aws::credential_endpoint_client(metadata::TIMEOUTS)
            .build()
            .map_err(Error::Client)?;
        Ok(Self { direct, proxied })
    }

    /// The client for `call`, on its route. Only [`send`] picks one, so a
    /// request and the call its retries are judged by always agree.
    fn client(&self, call: Call) -> &reqwest::Client {
        match call.route() {
            Route::Direct => &self.direct,
            Route::Proxied => &self.proxied,
        }
    }
}

/// A secret: a key, token or client secret. It never prints.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(String);

impl Secret {
    /// The value, to send.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

/// When a token stops working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// At this time.
    At(SystemTime),
    /// Not as far as its source says: an answer with no lifetime. Such a
    /// token is used for the run, as upstream never refreshes one.
    Never,
}

/// An `OAuth2` access token and when it expires.
#[derive(Debug, Clone)]
pub(crate) struct AccessToken {
    pub(crate) token: Secret,
    pub(crate) expiry: Expiry,
}

/// The token in an answer that says it lasts `expires_in` seconds from
/// `now`, or that does not say. A lifetime the clock cannot hold is refused.
fn expiring(token: String, now: SystemTime, expires_in: Option<u64>) -> Result<AccessToken, Error> {
    let expiry = match expires_in {
        Some(secs) => now
            .checked_add(Duration::from_secs(secs))
            .map(Expiry::At)
            .ok_or(Error::ExpiresIn(secs))?,
        None => Expiry::Never,
    };
    Ok(AccessToken {
        token: Secret(token),
        expiry,
    })
}

/// The Google endpoint a request went to, named as upstream names it in
/// its errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Call {
    /// An authorized user's refresh at the `OAuth2` token endpoint.
    UserToken,
    /// The STS token exchange of an external account.
    StsExchange,
    /// IAM `generateAccessToken` for an impersonated service account.
    Impersonation,
    /// The GCE metadata server.
    Metadata,
    /// An external account's URL subject token source, on the route its
    /// host takes.
    SubjectUrl(Route),
    /// The EC2 instance metadata service, for an AWS source's region.
    AwsRegion,
}

impl Call {
    /// What failed, as the error says it.
    fn as_str(self) -> &'static str {
        match self {
            Self::UserToken => "failed to refresh user access token",
            Self::StsExchange => "failed to exchange token",
            Self::Impersonation => "failed to generate access token",
            Self::Metadata => "failed to fetch access token",
            Self::SubjectUrl(_) => "failed to request subject token",
            Self::AwsRegion => "could not resolve AWS region",
        }
    }

    /// How the call reaches its host: the metadata services directly,
    /// Google's endpoints through the proxy.
    fn route(self) -> Route {
        match self {
            Self::Metadata | Self::AwsRegion => Route::Direct,
            Self::SubjectUrl(route) => route,
            Self::UserToken | Self::StsExchange | Self::Impersonation => Route::Proxied,
        }
    }
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why no access token was obtained.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// `GOOGLE_APPLICATION_CREDENTIALS` names a file that does not exist.
    /// Upstream does not fall back to the metadata server here.
    #[error(
        "{}. This file name was found in the `GOOGLE_APPLICATION_CREDENTIALS` environment variable. Verify this environment variable points to a valid file.",
        .0.display()
    )]
    MissingFile(PathBuf),
    /// A file did not read.
    #[error("failed to read {}", path.display())]
    ReadFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The credentials are not valid JSON, or lack a field their type needs.
    #[error("failed to parse the credentials")]
    Parse(#[source] crate::json::Unreadable),
    /// The credentials have no `type` field.
    #[error("no `type` field found.")]
    NoType,
    /// The credentials' `type` field is not a string.
    #[error("`type` field is not a string.")]
    TypeNotString,
    /// The credentials are of a type ocync does not support.
    #[error("unsupported credential type `{0}`")]
    UnsupportedType(String),
    /// An impersonated service account's source is itself impersonated.
    #[error(
        "source credential of type `impersonated_service_account` is not supported. Use the `delegates` field to specify a delegation chain."
    )]
    NestedImpersonation,
    /// An impersonation URL does not name one service account.
    #[error("invalid service account impersonation URL")]
    ImpersonationUrl,
    /// An authorized user belongs to a universe other than `googleapis.com`.
    #[error("User Account Credentials are not supported in universes other than googleapis.com")]
    UserUniverse,
    /// A workforce pool project is set on an audience that is not a
    /// workforce pool.
    #[error("workforce_pool_user_project should not be set for non-workforce pool credentials")]
    WorkforceProject,
    /// A service account's private key is not a PKCS#8 RSA key.
    #[error("failed to parse service account private key")]
    PrivateKey(#[source] service_account::KeyError),
    /// A service account's token was not signed.
    #[error("failed to sign the service account token")]
    Sign(#[source] aws_lc_rs::error::Unspecified),
    /// A token's `expires_in` is past what the clock can hold.
    #[error("invalid expires_in {0}: past what the clock can hold")]
    ExpiresIn(u64),
    /// The clock reads before the Unix epoch, which a JWT cannot carry.
    #[error("the system clock is before 1970")]
    ClockBeforeEpoch,
    /// An HTTP client did not build.
    #[error("failed to build the HTTP client")]
    Client(#[source] reqwest::Error),
    /// A request, or the read of its answer, did not complete. The error
    /// holds no URL, as a URL's query can hold a token.
    #[error("{call}")]
    Transport {
        call: Call,
        #[source]
        source: reqwest::Error,
    },
    /// An endpoint answered with an error. The body is cut short, as it can
    /// hold a token; `code` is the OAuth error code the whole body names.
    #[error("{call}, status {status}, body=<{body}>")]
    Status {
        call: Call,
        status: http::StatusCode,
        body: String,
        code: Option<OAuthCode>,
    },
    /// An endpoint's answer is not the JSON expected.
    #[error("{call}: unreadable response")]
    Decode {
        call: Call,
        #[source]
        source: crate::json::Unreadable,
    },
    /// An endpoint's answer is not UTF-8.
    #[error("{call}: the answer is not UTF-8")]
    NotUtf8 {
        call: Call,
        #[source]
        source: std::string::FromUtf8Error,
    },
    /// A subject token file is not UTF-8.
    #[error("the subject token is not UTF-8")]
    SubjectNotUtf8(#[source] std::string::FromUtf8Error),
    /// A JSON subject token document does not parse.
    #[error("the subject token document is not JSON")]
    SubjectJson(#[source] crate::json::Unreadable),
    /// A `json` subject token format names no field to read.
    #[error("credential_source.format of type json names no subject_token_field_name")]
    NoSubjectField,
    /// A subject token was not found where the credentials say.
    #[error("failed to read subject token field `{0}` as string")]
    SubjectField(String),
    /// An impersonated token's `expireTime` is not RFC 3339.
    #[error("invalid expireTime `{0}` in the generateAccessToken response")]
    ExpireTime(String),
    /// An impersonated token expired before it arrived. Upstream panics.
    #[error("the impersonated access token expired at {0} before it was received")]
    AlreadyExpired(String),
    /// An executable subject token source, which ocync does not run.
    #[error("unsupported subject token source `executable`")]
    ExecutableSource,
    /// The metadata server's host variable does not parse as a host.
    #[error("{} is not a valid host[:port]", metadata::HOST_VARIABLE)]
    MetadataHostInvalid(#[source] url::ParseError),
    /// The metadata server's host variable has more than `host[:port]`.
    #[error(
        "{} must be host[:port], with no path, query or user info",
        metadata::HOST_VARIABLE
    )]
    MetadataHostNotBare,
    /// The token an impersonation is made with cannot be sent as a header.
    #[error("the source credential's access token cannot be sent as a header")]
    SourceToken(#[source] http::header::InvalidHeaderValue),
    /// An authorized user's `quota_project_id` cannot be sent as a header.
    #[error("invalid `quota_project_id`")]
    QuotaProject(#[source] http::header::InvalidHeaderValue),
    /// A URL subject token source names a header that is not valid.
    #[error("invalid subject token URL header `{name}`")]
    SubjectHeader {
        name: String,
        #[source]
        source: http::Error,
    },
    /// A URL in the credentials file does not parse.
    #[error("invalid `{field}`")]
    Url {
        field: &'static str,
        #[source]
        source: url::ParseError,
    },
    /// No AWS region is configured, and the file names no `region_url` to
    /// ask.
    #[error("no AWS region configured and no `region_url` to look it up")]
    NoAwsRegion,
    /// No AWS region is configured, and `AWS_EC2_METADATA_DISABLED` keeps
    /// it from being asked of the instance metadata service.
    #[error(
        "could not resolve AWS region: none is configured and AWS_EC2_METADATA_DISABLED is set"
    )]
    AwsMetadataDisabled,
    /// The AWS region configured or answered is not a valid region.
    #[error("invalid AWS region")]
    AwsRegion(#[source] crate::aws::InvalidRegion),
    /// The instance metadata service answered with a session token a
    /// header cannot carry.
    #[error("invalid AWS instance metadata session token")]
    AwsMetadataToken(#[source] http::header::InvalidHeaderValue),
    /// A URL in the credentials file is not an http or https URL, so it
    /// names no host to send to.
    #[error("invalid `{field}`")]
    UrlScheme {
        field: &'static str,
        #[source]
        source: UnsupportedScheme,
    },
    /// The AWS settings for an AWS subject token are invalid.
    #[error("invalid AWS configuration for the AWS subject token")]
    AwsConfig(#[source] crate::aws::ConfigError),
    /// The AWS subject token was not signed.
    #[error("failed to sign the AWS subject token")]
    AwsSigning(#[source] crate::aws::SigningError),
    /// An answer was longer than any token response.
    #[error("{call}, answer over {MAX_SMALL_ANSWER} bytes")]
    BodyTooLarge { call: Call },
}

impl Error {
    /// Whether the call may succeed if sent again, as upstream classifies
    /// it: a 5xx, 408 or 429 answer, or a transport failure the crate
    /// retries. An answer that is not the JSON expected is final. A GCE or
    /// GKE metadata server that does not connect or answer is sent again, as
    /// GKE's takes a few seconds to start serving a new pod. The EC2
    /// instance metadata service a region lookup asks is up whenever its
    /// host is, so one that does not connect or answer in time never will.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => crate::retry::is_transient_status(*status),
            Self::Transport { call, source } => {
                let final_failure = match call {
                    Call::AwsRegion => source.is_connect() || source.is_timeout(),
                    Call::Metadata
                    | Call::UserToken
                    | Call::StsExchange
                    | Call::Impersonation
                    | Call::SubjectUrl(_) => false,
                };
                !final_failure && crate::retry::is_transient_transport(source)
            }
            _ => false,
        }
    }

    /// Whether Google refused the identity, the call may pass if sent
    /// again, or something else failed. Google's OAuth token endpoints
    /// refuse by their error code; impersonation and the metadata server
    /// refuse only with 401 or 403, so a metadata 404 is a host with no
    /// service account. A subject token source or the AWS region lookup
    /// never refuses, as neither is Google judging the identity; an AWS
    /// subject token that was not signed is classified as the AWS failure
    /// under it.
    pub(crate) fn credential_failure(&self) -> CredentialFailure {
        match self {
            Self::AwsSigning(error) => error.credential_failure(),
            Self::Status {
                status, call, code, ..
            } => match call {
                Call::UserToken | Call::StsExchange => CredentialFailure::of_oauth(*status, *code),
                Call::Impersonation | Call::Metadata => CredentialFailure::of_service(*status),
                Call::SubjectUrl(_) | Call::AwsRegion => CredentialFailure::of_unjudged(*status),
            },
            _ if self.is_transient() => CredentialFailure::Transient,
            _ => CredentialFailure::Failed,
        }
    }

    /// A request of `call` that did not complete, its URL dropped.
    fn transport(call: Call, source: reqwest::Error) -> Self {
        Self::Transport {
            call,
            source: source.without_url(),
        }
    }

    /// An error for an answer whose status is not a success, its body read
    /// up to [`MAX_SMALL_ANSWER`] for its OAuth error code and quoted cut short.
    async fn status(call: Call, response: reqwest::Response) -> Self {
        let status = response.status();
        let body = crate::json::error_body(response).await;
        Self::Status {
            call,
            status,
            body: truncate_quoted(&body).to_owned(),
            code: OAuthCode::read(body.as_bytes()),
        }
    }
}

/// Send the request `build` makes for `call` on its route's client, and
/// read its successful answer. A transient failure resends this request
/// alone, so a failure late in a chain repeats none of the calls before
/// it. These calls retry themselves because the prepare phase reaches them
/// before the engine's retry exists.
async fn send(
    clients: &Clients,
    call: Call,
    build: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<Vec<u8>, Error> {
    let (client, request) = build(clients.client(call)).build_split();
    let request = request.map_err(|source| Error::transport(call, source))?;
    retrying(
        "Google credential request",
        &mut Bounded(Error::is_transient),
        |_| {
            let request = request
                .try_clone()
                .expect("every request here has a buffered body");
            send_once(call, &client, request)
        },
    )
    .await
}

/// One try of [`send`].
async fn send_once(
    call: Call,
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<Vec<u8>, Error> {
    let transport = |source| Error::transport(call, source);
    let mut response = client.execute(request).await.map_err(transport)?;
    if !response.status().is_success() {
        return Err(Error::status(call, response).await);
    }
    crate::json::read_capped(&mut response, MAX_SMALL_ANSWER)
        .await
        .map_err(transport)?
        .ok_or(Error::BodyTooLarge { call })
}

/// [`send`] the request `build` makes for `call`, and decode its JSON
/// answer.
async fn send_json<T: serde::de::DeserializeOwned>(
    clients: &Clients,
    call: Call,
    build: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<T, Error> {
    let body = send(clients, call, build).await?;
    crate::json::parse(&body).map_err(|source| Error::Decode { call, source })
}

/// [`send`] the request `build` makes for `call`, and read its answer as
/// text, which must be UTF-8.
async fn send_text(
    clients: &Clients,
    call: Call,
    build: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<String, Error> {
    let body = send(clients, call, build).await?;
    String::from_utf8(body).map_err(|source| Error::NotUtf8 { call, source })
}

/// The scopes a credential asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scopes {
    /// [`REGISTRY_SCOPE`], in place of any the file sets.
    Registry,
    /// The scopes the file sets, else the type's default. The source of an
    /// impersonated service account asks for these, as upstream.
    Default,
}

impl Scopes {
    /// The registry scope, else the file's `listed` scopes, else `default`.
    fn resolve(self, listed: Option<Vec<String>>, default: &'static str) -> ScopeList {
        self.registry_or_endpoint_default().unwrap_or_else(|| {
            ScopeList(match listed {
                Some(listed) => listed.into_iter().map(Cow::Owned).collect(),
                None => vec![Cow::Borrowed(default)],
            })
        })
    }

    /// The registry scope, or `None` to leave the scopes to the endpoint.
    fn registry_or_endpoint_default(self) -> Option<ScopeList> {
        match self {
            Self::Registry => Some(ScopeList(vec![Cow::Borrowed(REGISTRY_SCOPE)])),
            Self::Default => None,
        }
    }
}

/// The scopes a token is asked for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
struct ScopeList(Vec<Cow<'static, str>>);

impl ScopeList {
    /// The scopes, separated by `separator`.
    fn joined(&self, separator: &str) -> String {
        self.0.join(separator)
    }
}

/// What an IAM `generateAccessToken` call carries from the token it is
/// made with: the token as an `authorization` header that never prints,
/// and the project billed when the source names one.
#[derive(Debug)]
struct SourceAuth {
    authorization: HeaderValue,
    quota_project: Option<HeaderValue>,
}

impl SourceAuth {
    /// The authorization of `token`, billing `quota_project` when set.
    fn new(token: &str, quota_project: Option<HeaderValue>) -> Result<Self, Error> {
        let mut authorization =
            HeaderValue::try_from(format!("Bearer {token}")).map_err(Error::SourceToken)?;
        authorization.set_sensitive(true);
        Ok(Self {
            authorization,
            quota_project,
        })
    }

    /// `request` carrying this authorization.
    fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header(http::header::AUTHORIZATION, &self.authorization);
        match &self.quota_project {
            Some(project) => request.header(user_account::QUOTA_PROJECT_HEADER, project),
            None => request,
        }
    }
}

pub(crate) use self::metadata::is_on_google_cloud;
#[cfg(test)]
pub(crate) use self::metadata::{probe_answer, probing};

/// Credentials that obtain access tokens.
#[derive(Debug)]
pub(crate) struct Credentials(Kind);

/// The kinds of [`Credentials`].
#[derive(Debug)]
enum Kind {
    ServiceAccount(service_account::Account),
    AuthorizedUser(user_account::AuthorizedUser),
    ExternalAccount(Box<external_account::Account>),
    Impersonated(impersonated::Account),
    Metadata(metadata::Server),
}

impl Credentials {
    /// The credentials the file of `host` holds, asking for the registry
    /// scope, or `None` when it has no file (see [`find_file`]). An external
    /// account with an AWS subject token takes its AWS credentials from
    /// `aws_credentials`.
    pub(crate) async fn load(
        host: &Host,
        clients: &Clients,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Option<Self>, Error> {
        let setup = Setup {
            host,
            clients,
            aws_credentials,
        };
        let Some(contents) = find_file(host)? else {
            return Ok(None);
        };
        let json: Value = crate::json::parse(&contents).map_err(Error::Parse)?;
        let kind = CredentialType::of(&json)?;
        Self::from_json(kind, json, Scopes::Registry, &setup)
            .await
            .map(Some)
    }

    /// The metadata server of `host`, at `GCE_METADATA_HOST` when it is set.
    pub(crate) fn metadata(host: &Host) -> Result<Self, Error> {
        metadata::Server::new(&host.env, Scopes::Registry)
            .map(|server| Self(Kind::Metadata(server)))
    }

    /// The credentials of type `kind` that `json` describes.
    fn from_json<'a>(
        kind: CredentialType,
        json: Value,
        scopes: Scopes,
        setup: &'a Setup<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, Error>> + 'a>> {
        Box::pin(async move {
            Ok(Self(match kind {
                CredentialType::ServiceAccount => {
                    Kind::ServiceAccount(service_account::Account::from_json(json, scopes)?)
                }
                CredentialType::AuthorizedUser => {
                    Kind::AuthorizedUser(user_account::AuthorizedUser::from_json(json, scopes)?)
                }
                CredentialType::ExternalAccount => Kind::ExternalAccount(Box::new(
                    external_account::Account::from_json(json, scopes, setup).await?,
                )),
                CredentialType::Impersonated => {
                    Kind::Impersonated(impersonated::Account::from_json(json, scopes, setup).await?)
                }
            }))
        })
    }

    /// A new access token.
    pub(crate) fn access_token<'a>(
        &'a self,
        clients: &'a Clients,
        clock: Clock,
    ) -> Pin<Box<dyn Future<Output = Result<AccessToken, Error>> + Send + 'a>> {
        Box::pin(async move {
            match &self.0 {
                Kind::ServiceAccount(account) => account.access_token(clock.now()),
                Kind::AuthorizedUser(user) => user.access_token(clients, clock).await,
                Kind::ExternalAccount(account) => account.access_token(clients, clock).await,
                Kind::Impersonated(account) => account.access_token(clients, clock).await,
                Kind::Metadata(server) => server.access_token(clients, clock).await,
            }
        })
    }

    /// What the IAM call of an impersonated service account carries from
    /// these source credentials: a new token, and the quota project of an
    /// authorized user, as upstream forwards its source's headers.
    async fn source_auth(&self, clients: &Clients, clock: Clock) -> Result<SourceAuth, Error> {
        let token = self.access_token(clients, clock).await?;
        let quota_project = match &self.0 {
            Kind::AuthorizedUser(user) => user.quota_project().cloned(),
            _ => None,
        };
        SourceAuth::new(token.token.expose(), quota_project)
    }
}

/// A credentials file's `type`, for the types ocync supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialType {
    ServiceAccount,
    AuthorizedUser,
    ExternalAccount,
    Impersonated,
}

impl CredentialType {
    /// The type `json` names. Any other is unsupported and named.
    fn of(json: &Value) -> Result<Self, Error> {
        match json.get("type") {
            None => Err(Error::NoType),
            Some(Value::String(kind)) => match kind.as_str() {
                "service_account" => Ok(Self::ServiceAccount),
                "authorized_user" => Ok(Self::AuthorizedUser),
                "external_account" => Ok(Self::ExternalAccount),
                "impersonated_service_account" => Ok(Self::Impersonated),
                _ => Err(Error::UnsupportedType(kind.clone())),
            },
            Some(_) => Err(Error::TypeNotString),
        }
    }
}

/// `url`, the value of `field`: an http or https URL, which always names
/// a host.
fn parse_url(field: &'static str, url: &str) -> Result<Url, Error> {
    let url = Url::parse(url).map_err(|source| Error::Url { field, source })?;
    Scheme::try_from(&url).map_err(|source| Error::UrlScheme { field, source })?;
    Ok(url)
}

/// What credentials are built with.
struct Setup<'a> {
    host: &'a Host,
    clients: &'a Clients,
    aws_credentials: &'a AwsCredentialProviders,
}

/// The contents of the credentials file, or `None` when there is none. The
/// file `GOOGLE_APPLICATION_CREDENTIALS` names must exist. Without that
/// variable, gcloud's well-known file is used if it exists.
fn find_file(host: &Host) -> Result<Option<Vec<u8>>, Error> {
    if let Some(path) = host.env.get_os(CREDENTIALS_VARIABLE) {
        let path = PathBuf::from(path);
        return match host.fs.read(&path) {
            Ok(contents) => Ok(Some(contents)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::MissingFile(path)),
            Err(source) => Err(Error::ReadFile { path, source }),
        };
    }
    let Some(path) = well_known_path(&host.env, host.platform) else {
        return Ok(None);
    };
    match host.fs.read(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::ReadFile { path, source }),
    }
}

/// gcloud's well-known credentials file on `platform`, or `None` when its
/// root variable is unset.
fn well_known_path(env: &Env, platform: Platform) -> Option<PathBuf> {
    let (root, dirs): (_, &[&str]) = match platform {
        Platform::Unix => (env.get_os("HOME")?, &[UNIX_CONFIG_DIR]),
        Platform::Windows => (env.get_os("APPDATA")?, &[]),
    };
    let mut path = PathBuf::from(root);
    path.extend(dirs.iter().chain(&WELL_KNOWN_FILE));
    Some(path)
}

#[cfg(test)]
pub(crate) mod tests;
