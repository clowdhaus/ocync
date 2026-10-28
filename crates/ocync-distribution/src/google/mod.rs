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

use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::REDACTED;
use crate::aws::{AwsCredentialProviders, Clock, Env, Fs, Platform};
use crate::error::truncate_quoted;

/// The scope every registry token is asked for: push and pull on Artifact
/// Registry and Container Registry.
const REGISTRY_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

/// The scope a credential asks for when none is set, as upstream's
/// `DEFAULT_SCOPE`.
const DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The variable naming the credentials file.
const CREDENTIALS_FILE: &str = "GOOGLE_APPLICATION_CREDENTIALS";

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

/// The process environment and files the credentials come from.
#[derive(Debug, Clone)]
pub(crate) struct Host {
    pub(crate) env: Env,
    pub(crate) fs: Fs,
    pub(crate) platform: Platform,
}

impl Host {
    /// This process on the platform it runs on.
    pub(crate) fn current() -> Self {
        Self {
            env: Env::Process,
            fs: Fs::Real,
            platform: Platform::current(),
        }
    }
}

/// The HTTP clients the credentials are fetched with.
#[derive(Debug, Clone)]
pub(crate) struct Clients {
    /// Google's token, STS and IAM endpoints and a federation URL source,
    /// through `HTTPS_PROXY` like registry traffic.
    api: reqwest::Client,
    /// The GCE metadata server and the EC2 instance metadata service: no
    /// proxy, so a token never reaches a proxy host.
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
/// used until a registry refuses it, as upstream never refreshes one.
#[derive(Debug, Clone)]
pub(crate) struct AccessToken {
    pub(crate) token: Secret,
    pub(crate) expiry: Option<SystemTime>,
}

/// The token in an answer that says it lasts `expires_in` seconds from
/// `now`, or that does not say.
fn expiring(token: String, now: SystemTime, expires_in: Option<u64>) -> AccessToken {
    AccessToken {
        token: Secret(token),
        expiry: expires_in.and_then(|secs| now.checked_add(Duration::from_secs(secs))),
    }
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
    #[error("failed to parse the credentials: {0}")]
    Parse(String),
    /// The credentials are of a type ocync does not support.
    #[error("unsupported credential type `{0}`")]
    UnsupportedType(String),
    /// A service account's private key is not a PKCS#8 RSA key.
    #[error("failed to parse service account private key: {0}")]
    PrivateKey(String),
    /// An HTTP client did not build.
    #[error("failed to build the HTTP client")]
    Client(#[source] reqwest::Error),
    /// A request did not complete.
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
    /// An endpoint's answer did not decode.
    #[error("{call}: unreadable response")]
    Decode {
        call: Call,
        #[source]
        source: reqwest::Error,
    },
    /// A subject token was not found where the credentials say.
    #[error("failed to read subject token field `{0}` as string")]
    SubjectField(String),
    /// An impersonated token's `expireTime` is not RFC 3339.
    #[error("invalid expireTime `{0}` in the generateAccessToken response")]
    ExpireTime(String),
    /// An impersonated token expired before it arrived. Upstream panics.
    #[error("the impersonated access token expired at {0} before it was received")]
    AlreadyExpired(String),
    /// A subject token source ocync does not support.
    #[error("unsupported subject token source `{0}`")]
    UnsupportedSource(&'static str),
    /// The AWS region for an AWS subject token was not found.
    #[error("could not resolve AWS region")]
    NoAwsRegion,
    /// The AWS subject token was not signed.
    #[error("failed to sign the AWS subject token: {0}")]
    AwsSigning(String),
}

impl Error {
    /// Whether the call may succeed if sent again, as upstream classifies
    /// it: a 5xx, 408 or 429 answer, or a request that did not connect or
    /// complete. The metadata server is the exception: a host with no
    /// metadata server never gets one, and upstream tries it once.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => {
                status.is_server_error()
                    || *status == http::StatusCode::REQUEST_TIMEOUT
                    || *status == http::StatusCode::TOO_MANY_REQUESTS
            }
            Self::Transport { call, source } => {
                *call != Call::Metadata && (source.is_connect() || source.is_request())
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

    fn parse(error: impl fmt::Display) -> Self {
        Self::Parse(error.to_string())
    }
}

/// Send `request` for `call` and decode its JSON answer.
async fn send_json<T: serde::de::DeserializeOwned>(
    call: Call,
    request: reqwest::RequestBuilder,
) -> Result<T, Error> {
    let response = request
        .send()
        .await
        .map_err(|source| Error::Transport { call, source })?;
    if !response.status().is_success() {
        return Err(Error::status(call, response).await);
    }
    response
        .json()
        .await
        .map_err(|source| Error::Decode { call, source })
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

/// Credentials that obtain access tokens.
#[derive(Debug)]
pub(crate) enum Credentials {
    ServiceAccount(service_account::ServiceAccount),
    AuthorizedUser(user_account::AuthorizedUser),
    ExternalAccount(external_account::ExternalAccount),
    Impersonated(impersonated::Impersonated),
    Metadata(metadata::Metadata),
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
            None => Ok(Self::Metadata(metadata::Metadata::new(
                &host.env,
                Scopes::Registry,
            ))),
            Some(contents) => {
                let json: Value = serde_json::from_slice(&contents).map_err(Error::parse)?;
                Self::from_json(json, Scopes::Registry, &setup).await
            }
        }
    }

    /// The credentials `json` describes, by its `type`.
    fn from_json<'a>(
        json: Value,
        scopes: Scopes,
        setup: &'a Setup<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, Error>> + 'a>> {
        Box::pin(async move {
            let kind = match json.get("type") {
                None => return Err(Error::parse("no `type` field found.")),
                Some(Value::String(kind)) => kind.clone(),
                Some(_) => return Err(Error::parse("`type` field is not a string.")),
            };
            match kind.as_str() {
                "service_account" => service_account::ServiceAccount::from_json(json, scopes)
                    .map(Self::ServiceAccount),
                "authorized_user" => {
                    user_account::AuthorizedUser::from_json(json, scopes).map(Self::AuthorizedUser)
                }
                "external_account" => {
                    external_account::ExternalAccount::from_json(json, scopes, setup)
                        .await
                        .map(Self::ExternalAccount)
                }
                "impersonated_service_account" => {
                    impersonated::Impersonated::from_json(json, scopes, setup)
                        .await
                        .map(Self::Impersonated)
                }
                _ => Err(Error::UnsupportedType(kind)),
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

    /// The headers the IAM call of an impersonated service account carries
    /// from these source credentials: the token, and the quota project of
    /// an authorized user, as upstream forwards its source's headers.
    async fn source_headers(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<Vec<(&'static str, String)>, Error> {
        let token = self.access_token(clients, clock).await?;
        let mut headers = vec![("authorization", format!("Bearer {}", token.token.expose()))];
        if let Self::AuthorizedUser(user) = self
            && let Some(project) = user.quota_project_id()
        {
            headers.push((user_account::QUOTA_PROJECT_HEADER, project.to_owned()));
        }
        Ok(headers)
    }
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
    if let Some(path) = host.env.get(CREDENTIALS_FILE) {
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
        Platform::Unix => (env.get("HOME")?, &[UNIX_CONFIG_DIR]),
        Platform::Windows => (env.get("APPDATA")?, &[]),
    };
    let mut path = PathBuf::from(root);
    path.extend(dirs.iter().chain(&WELL_KNOWN_FILE));
    Some(path)
}

#[cfg(test)]
mod tests;
