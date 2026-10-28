//! Google Cloud Application Default Credentials, as `google-cloud-auth`
//! 1.18.0 resolves them, with every hash and signature made by `aws-lc-rs`.
//!
//! The credentials come from the file `GOOGLE_APPLICATION_CREDENTIALS`
//! names, else from gcloud's well-known file, else from the metadata server
//! of the GCE or GKE host. A file holds a service account key, an authorized
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

use serde_json::Value;
use url::Url;

use crate::REDACTED;
use crate::aws::{AwsCredentialProviders, Clock, Env, Host, Platform};
use crate::error::truncate_quoted;

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

/// The HTTP clients the credentials are fetched with.
#[derive(Debug, Clone)]
pub(crate) struct Clients {
    /// Google's token, STS and IAM endpoints and a remote federation URL
    /// source, through `HTTPS_PROXY` like registry traffic.
    api: reqwest::Client,
    /// The GCE metadata server, the EC2 instance metadata service and a
    /// local federation URL source: no proxy, so a token never reaches a
    /// proxy host.
    metadata: reqwest::Client,
}

impl Clients {
    /// Clients with ocync's TLS provider and no redirects, so a body
    /// carrying a secret is never sent on to another host.
    pub(crate) fn new() -> Result<Self, Error> {
        crate::install_crypto_provider();
        let api = crate::client::no_redirect_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(Error::Client)?;
        let metadata = crate::aws::credential_endpoint_client(metadata::TIMEOUTS)
            .build()
            .map_err(Error::Client)?;
        Ok(Self { api, metadata })
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

/// An `OAuth2` access token and when it expires. A token with no expiry is
/// used for the run, as upstream never refreshes one.
#[derive(Debug, Clone)]
pub(crate) struct AccessToken {
    pub(crate) token: Secret,
    pub(crate) expiry: Option<SystemTime>,
}

/// The token in an answer that says it lasts `expires_in` seconds from
/// `now`, or that does not say. A lifetime the clock cannot hold is refused.
fn expiring(token: String, now: SystemTime, expires_in: Option<u64>) -> Result<AccessToken, Error> {
    let expiry = expires_in
        .map(|secs| {
            now.checked_add(Duration::from_secs(secs))
                .ok_or(Error::ExpiresIn(secs))
        })
        .transpose()?;
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
    /// An external account's URL subject token source.
    SubjectUrl,
    /// The EC2 instance metadata service, for an AWS source's region.
    AwsRegion,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UserToken => "failed to refresh user access token",
            Self::StsExchange => "failed to exchange token",
            Self::Impersonation => "failed to generate access token",
            Self::Metadata => "failed to fetch access token",
            Self::SubjectUrl => "failed to request subject token",
            Self::AwsRegion => "could not resolve AWS region",
        })
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
    Parse(#[source] serde_json::Error),
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
    /// hold a token.
    #[error("{call}, status {status}, body=<{body}>")]
    Status {
        call: Call,
        status: http::StatusCode,
        body: String,
    },
    /// An endpoint's answer is not the JSON expected.
    #[error("{call}: unreadable response")]
    Decode {
        call: Call,
        #[source]
        source: serde_json::Error,
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
    SubjectJson(#[source] serde_json::Error),
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
    /// A URL subject token source names a header that is not valid.
    #[error("invalid subject token URL header `{0}`")]
    SubjectHeader(String),
    /// A URL in the credentials file does not parse.
    #[error("invalid `{field}`")]
    Url {
        field: &'static str,
        #[source]
        source: url::ParseError,
    },
    /// The AWS region for an AWS subject token was not found.
    #[error("could not resolve AWS region")]
    NoAwsRegion,
    /// The AWS STS URL does not parse.
    #[error("invalid AWS STS URL")]
    StsUrl(#[source] url::ParseError),
    /// The AWS STS URL names no host.
    #[error("invalid AWS STS URL: missing host")]
    StsUrlHost,
    /// The AWS settings for an AWS subject token are invalid.
    #[error("invalid AWS configuration for the AWS subject token")]
    AwsConfig(#[source] crate::aws::ConfigError),
    /// The AWS subject token was not signed.
    #[error("failed to sign the AWS subject token")]
    AwsSigning(#[source] crate::aws::SigningError),
}

impl Error {
    /// Whether the call may succeed if sent again, as upstream classifies
    /// it: a 5xx, 408 or 429 answer, or a transport failure the crate
    /// retries. An answer that is not the JSON expected is final. A
    /// metadata server that does not connect is final too: a host with no
    /// metadata server never gets one. One that connects and then times
    /// out or resets, as GKE's can while it mints a token, may be sent again.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => {
                status.is_server_error()
                    || *status == http::StatusCode::REQUEST_TIMEOUT
                    || *status == http::StatusCode::TOO_MANY_REQUESTS
            }
            Self::Transport { call, source } => {
                !(*call == Call::Metadata && source.is_connect())
                    && crate::retry::is_transient_transport(source)
            }
            _ => false,
        }
    }

    /// An error for an answer whose status is not a success.
    async fn status(call: Call, response: reqwest::Response) -> Self {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Self::Status {
            call,
            status,
            body: truncate_quoted(&body).to_owned(),
        }
    }
}

/// Send `request` for `call` and read its successful answer.
async fn send(call: Call, request: reqwest::RequestBuilder) -> Result<bytes::Bytes, Error> {
    let transport = |source: reqwest::Error| Error::Transport {
        call,
        source: source.without_url(),
    };
    let response = request.send().await.map_err(transport)?;
    if !response.status().is_success() {
        return Err(Error::status(call, response).await);
    }
    response.bytes().await.map_err(transport)
}

/// Send `request` for `call` and decode its JSON answer.
async fn send_json<T: serde::de::DeserializeOwned>(
    call: Call,
    request: reqwest::RequestBuilder,
) -> Result<T, Error> {
    let body = send(call, request).await?;
    serde_json::from_slice(&body).map_err(|source| Error::Decode { call, source })
}

/// Send `request` for `call` and read its answer as text, which must be
/// UTF-8.
async fn send_text(call: Call, request: reqwest::RequestBuilder) -> Result<String, Error> {
    let body = send(call, request).await?;
    String::from_utf8(body.into()).map_err(|source| Error::NotUtf8 { call, source })
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
        ScopeList(match (self, listed) {
            (Self::Registry, _) => vec![Cow::Borrowed(REGISTRY_SCOPE)],
            (Self::Default, Some(listed)) => listed.into_iter().map(Cow::Owned).collect(),
            (Self::Default, None) => vec![Cow::Borrowed(default)],
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
/// made with: the token, and the project billed when the source names one.
struct SourceAuth {
    bearer: Secret,
    quota_project: Option<String>,
}

impl SourceAuth {
    /// `request` carrying this authorization.
    fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.bearer_auth(self.bearer.expose());
        match &self.quota_project {
            Some(project) => request.header(user_account::QUOTA_PROJECT_HEADER, project),
            None => request,
        }
    }
}

/// Credentials that obtain access tokens.
#[derive(Debug)]
pub(crate) enum Credentials {
    ServiceAccount(service_account::Account),
    AuthorizedUser(user_account::AuthorizedUser),
    ExternalAccount(Box<external_account::Account>),
    Impersonated(impersonated::Account),
    Metadata(metadata::Server),
}

impl Credentials {
    /// The Application Default Credentials of `host`, asking for the
    /// registry scope. An external account with an AWS subject token
    /// takes its AWS credentials from `aws_credentials`.
    pub(crate) async fn load(
        host: &Host,
        clients: &Clients,
        aws_credentials: &AwsCredentialProviders,
    ) -> Result<Self, Error> {
        let setup = Setup {
            host,
            clients,
            aws_credentials,
        };
        match find_file(host)? {
            None => Ok(Self::Metadata(metadata::Server::new(
                &host.env,
                Scopes::Registry,
            ))),
            Some(contents) => {
                let json: Value = serde_json::from_slice(&contents).map_err(Error::Parse)?;
                let kind = CredentialType::of(&json)?;
                Self::from_json(kind, json, Scopes::Registry, &setup).await
            }
        }
    }

    /// The credentials of type `kind` that `json` describes.
    fn from_json<'a>(
        kind: CredentialType,
        json: Value,
        scopes: Scopes,
        setup: &'a Setup<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, Error>> + 'a>> {
        Box::pin(async move {
            match kind {
                CredentialType::ServiceAccount => {
                    service_account::Account::from_json(json, scopes).map(Self::ServiceAccount)
                }
                CredentialType::AuthorizedUser => {
                    user_account::AuthorizedUser::from_json(json, scopes).map(Self::AuthorizedUser)
                }
                CredentialType::ExternalAccount => {
                    external_account::Account::from_json(json, scopes, setup)
                        .await
                        .map(|account| Self::ExternalAccount(Box::new(account)))
                }
                CredentialType::Impersonated => {
                    impersonated::Account::from_json(json, scopes, setup)
                        .await
                        .map(Self::Impersonated)
                }
            }
        })
    }

    /// A new access token.
    pub(crate) fn access_token<'a>(
        &'a self,
        clients: &'a Clients,
        clock: Clock,
    ) -> Pin<Box<dyn Future<Output = Result<AccessToken, Error>> + Send + 'a>> {
        Box::pin(async move {
            match self {
                Self::ServiceAccount(account) => account.access_token(clock.now()),
                Self::AuthorizedUser(user) => user.access_token(clients, clock).await,
                Self::ExternalAccount(account) => account.access_token(clients, clock).await,
                Self::Impersonated(account) => account.access_token(clients, clock).await,
                Self::Metadata(server) => server.access_token(clients, clock).await,
            }
        })
    }

    /// What the IAM call of an impersonated service account carries from
    /// these source credentials: a new token, and the quota project of an
    /// authorized user, as upstream forwards its source's headers.
    async fn source_auth(&self, clients: &Clients, clock: Clock) -> Result<SourceAuth, Error> {
        let token = self.access_token(clients, clock).await?;
        let quota_project = match self {
            Self::AuthorizedUser(user) => user.quota_project_id().map(str::to_owned),
            _ => None,
        };
        Ok(SourceAuth {
            bearer: token.token,
            quota_project,
        })
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

/// `url`, the value of `field`.
fn parse_url(field: &'static str, url: &str) -> Result<Url, Error> {
    Url::parse(url).map_err(|source| Error::Url { field, source })
}

/// What credentials are built with.
struct Setup<'a> {
    host: &'a Host,
    clients: &'a Clients,
    aws_credentials: &'a AwsCredentialProviders,
}

/// The contents of the credentials file, or `None` to use the metadata
/// server: the file `GOOGLE_APPLICATION_CREDENTIALS` names, which must
/// exist, else gcloud's well-known file if it does.
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
