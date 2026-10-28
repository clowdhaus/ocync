//! An external account (Workload Identity Federation): a subject token from
//! a file, a URL or AWS, exchanged at Google's STS for an access token, then
//! for a service account's token when the file names one to impersonate.

use std::collections::HashMap;
use std::path::PathBuf;

use http::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;
use url::Url;

use super::aws_source;
use super::impersonated::{Generate, IMPERSONATION_URL_FIELD};
use super::parse_url;
use super::{
    AccessToken, Call, Clients, DEFAULT_SCOPE, Error, ScopeList, Scopes, Secret, Setup, SourceAuth,
    expiring, send_json, send_text,
};
use crate::aws::{Clock, Fs};

/// The scope of an STS token that is only exchanged again, for a service
/// account's.
const IAM_SCOPE: &str = "https://www.googleapis.com/auth/iam";

/// The STS grant, RFC 8693 token exchange.
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";

/// The token type asked of STS.
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

/// The `format.type` that reads the token from a JSON field. Any other
/// type reads the whole text, as upstream.
const JSON_FORMAT: &str = "json";

/// An external account file, as upstream reads it.
#[derive(Deserialize)]
struct AccountFile {
    audience: String,
    subject_token_type: String,
    token_url: String,
    service_account_impersonation_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<Secret>,
    scopes: Option<Vec<String>>,
    credential_source: SourceFile,
    workforce_pool_user_project: Option<String>,
}

/// Where the subject token comes from, selected in upstream's order: a
/// source with `environment_id` is AWS, then one with `executable`, `url`
/// or `file`. The selected source must then parse, so an AWS source with a
/// malformed field is an error rather than read as the URL source it also
/// names.
enum SourceFile {
    Aws(aws_source::SourceFile),
    Executable,
    Url(UrlFile),
    File(FileFile),
}

impl<'de> Deserialize<'de> for SourceFile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = Value::deserialize(deserializer)?;
        let names = |key: &str| value.get(key).is_some();
        let source = if names("environment_id") {
            serde_json::from_value(value).map(Self::Aws)
        } else if names("executable") {
            serde_json::from_value::<ExecutableSource>(value).map(|_| Self::Executable)
        } else if names("url") {
            serde_json::from_value(value).map(Self::Url)
        } else if names("file") {
            serde_json::from_value(value).map(Self::File)
        } else {
            return Err(D::Error::custom(
                "credential_source names no environment_id, executable, url or file",
            ));
        };
        source.map_err(D::Error::custom)
    }
}

/// An executable source: its `executable.command` is all that selects it.
#[derive(Deserialize)]
struct ExecutableSource {
    #[serde(rename = "executable")]
    _executable: ExecutableFile,
}

/// A URL source.
#[derive(Deserialize)]
struct UrlFile {
    url: String,
    headers: Option<HashMap<String, String>>,
    format: Option<FormatFile>,
}

/// A file source.
#[derive(Deserialize)]
struct FileFile {
    file: String,
    format: Option<FormatFile>,
}

/// An executable source's settings: the command is all that selects it.
#[derive(Deserialize)]
struct ExecutableFile {
    #[serde(rename = "command")]
    _command: IgnoredAny,
}

/// How a subject token is written.
#[derive(Deserialize)]
struct FormatFile {
    #[serde(rename = "type")]
    kind: String,
    subject_token_field_name: Option<String>,
}

/// How a subject token is written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Format {
    /// The whole text, untrimmed.
    Text,
    /// A string field of a JSON object.
    Json { field: String },
}

impl From<Option<FormatFile>> for Format {
    fn from(format: Option<FormatFile>) -> Self {
        match format {
            Some(format) if format.kind == JSON_FORMAT => Self::Json {
                field: format.subject_token_field_name.unwrap_or_default(),
            },
            _ => Self::Text,
        }
    }
}

impl Format {
    /// The subject token in `text`.
    fn read(&self, text: String) -> Result<Secret, Error> {
        match self {
            Self::Text => Ok(Secret(text)),
            Self::Json { field } => {
                let json: Value = serde_json::from_str(&text).map_err(Error::SubjectJson)?;
                match json.get(field) {
                    Some(Value::String(token)) => Ok(Secret(token.clone())),
                    _ => Err(Error::SubjectField(field.clone())),
                }
            }
        }
    }
}

/// The EC2 instance metadata service's IPv6 address, which is unique-local
/// rather than link-local.
const EC2_METADATA_IPV6: std::net::Ipv6Addr =
    std::net::Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

/// Host names that only ever name this host or its metadata service.
const LOCAL_DOMAINS: [&str; 2] = ["localhost", super::metadata::DEFAULT_HOST];

/// The client that fetches a URL source: the metadata client, with no
/// proxy, for this host or a metadata service, such as Azure's at a
/// link-local address, so its token never reaches a proxy host; else the
/// API client, through the proxy.
fn url_client<'a>(clients: &'a Clients, url: &Url) -> &'a reqwest::Client {
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_link_local() || ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_unicast_link_local() || ip.is_loopback() || ip == EC2_METADATA_IPV6
        }
        Some(url::Host::Domain(domain)) => LOCAL_DOMAINS
            .iter()
            .any(|local| domain.trim_end_matches('.').eq_ignore_ascii_case(local)),
        None => false,
    };
    if local {
        &clients.metadata
    } else {
        &clients.api
    }
}

/// Where the subject token comes from.
#[derive(Debug)]
enum Source {
    /// A file, such as a projected Kubernetes service account token, read
    /// on every exchange.
    File {
        path: PathBuf,
        format: Format,
        fs: Fs,
    },
    /// A URL, fetched with `headers` on every exchange.
    Url {
        url: Url,
        headers: HeaderMap,
        format: Format,
    },
    /// A signed AWS `GetCallerIdentity` request.
    Aws(aws_source::Source),
}

impl Source {
    /// The subject token now.
    async fn subject_token(&self, clients: &Clients, clock: Clock) -> Result<Secret, Error> {
        match self {
            Self::File { path, format, fs } => {
                let bytes = fs.read(path).map_err(|source| Error::ReadFile {
                    path: path.clone(),
                    source,
                })?;
                format.read(String::from_utf8(bytes).map_err(Error::SubjectNotUtf8)?)
            }
            Self::Url {
                url,
                headers,
                format,
            } => {
                let request = url_client(clients, url)
                    .get(url.clone())
                    .headers(headers.clone())
                    .timeout(SUBJECT_URL_TIMEOUT);
                format.read(send_text(Call::SubjectUrl, request).await?)
            }
            Self::Aws(aws) => aws.subject_token(clock.now()).await,
        }
    }
}

/// The field naming the STS endpoint.
const TOKEN_URL_FIELD: &str = "token_url";

/// The field naming a URL source.
const SUBJECT_URL_FIELD: &str = "credential_source.url";

/// A URL source's `headers`, parsed once so a bad one fails at load. The
/// values are marked sensitive, so they never print: one can carry a token,
/// as GitHub Actions' `Authorization` header does.
fn header_map(headers: HashMap<String, String>) -> Result<HeaderMap, Error> {
    headers
        .into_iter()
        .map(|(name, value)| {
            let parsed = HeaderName::try_from(name.as_str())
                .ok()
                .zip(HeaderValue::try_from(value).ok());
            let (name, mut value) = parsed.ok_or(Error::SubjectHeader(name))?;
            value.set_sensitive(true);
            Ok((name, value))
        })
        .collect()
}

/// How long a URL source may take, as upstream's client timeout.
const SUBJECT_URL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How the STS exchange is authenticated, as upstream decides it.
#[derive(Debug)]
enum StsAuth {
    /// The file sets a client ID and secret: HTTP Basic.
    Client { id: String, secret: Secret },
    /// The file sets neither but names a workforce pool user project: the
    /// `options` form field, `{"userProject": ...}`.
    UserProject { options: String },
    /// Nothing is sent: one of the client ID and secret, or neither and no
    /// project.
    Unauthenticated,
}

/// An external account.
#[derive(Debug)]
pub(crate) struct Account {
    audience: String,
    subject_token_type: String,
    token_url: Url,
    impersonation_url: Option<Url>,
    auth: StsAuth,
    /// The scopes of the token returned.
    scopes: ScopeList,
    /// The STS `scope`: the IAM scope when the STS token is exchanged
    /// again, else [`Self::scopes`].
    sts_scope: String,
    source: Source,
}

/// The STS answer.
#[derive(Deserialize)]
struct StsResponse {
    access_token: String,
    #[serde(rename = "issued_token_type")]
    _issued_token_type: IgnoredAny,
    #[serde(rename = "token_type")]
    _token_type: IgnoredAny,
    expires_in: u64,
}

impl Account {
    /// The account `json` describes, asking for `scopes`. An AWS source
    /// resolves its region and takes its AWS credentials here.
    pub(super) async fn from_json(
        json: Value,
        scopes: Scopes,
        setup: &Setup<'_>,
    ) -> Result<Self, Error> {
        let file: AccountFile = serde_json::from_value(json).map_err(Error::Parse)?;
        if file.workforce_pool_user_project.is_some() && !is_workforce_pool_audience(&file.audience)
        {
            return Err(Error::WorkforceProject);
        }
        let source = match file.credential_source {
            SourceFile::Aws(aws) => {
                Source::Aws(aws_source::Source::new(setup, &aws, &file.audience).await?)
            }
            SourceFile::Executable => return Err(Error::ExecutableSource),
            SourceFile::Url(UrlFile {
                url,
                headers,
                format,
            }) => Source::Url {
                url: parse_url(SUBJECT_URL_FIELD, &url)?,
                headers: header_map(headers.unwrap_or_default())?,
                format: format.into(),
            },
            SourceFile::File(FileFile { file, format }) => Source::File {
                path: PathBuf::from(file),
                format: format.into(),
                fs: setup.host.fs.clone(),
            },
        };
        let auth = match (file.client_id, file.client_secret) {
            (Some(id), Some(secret)) => StsAuth::Client { id, secret },
            (None, None) => match file.workforce_pool_user_project {
                Some(project) => StsAuth::UserProject {
                    options: serde_json::json!({ "userProject": project }).to_string(),
                },
                None => StsAuth::Unauthenticated,
            },
            _ => StsAuth::Unauthenticated,
        };
        let scopes = scopes.resolve(
            file.scopes.filter(|scopes| !scopes.is_empty()),
            DEFAULT_SCOPE,
        );
        let impersonation_url = file
            .service_account_impersonation_url
            .map(|url| parse_url(IMPERSONATION_URL_FIELD, &url))
            .transpose()?;
        let sts_scope = match impersonation_url {
            Some(_) => IAM_SCOPE.to_owned(),
            None => scopes.joined(" "),
        };
        Ok(Self {
            audience: file.audience,
            subject_token_type: file.subject_token_type,
            token_url: parse_url(TOKEN_URL_FIELD, &file.token_url)?,
            impersonation_url,
            auth,
            scopes,
            sts_scope,
            source,
        })
    }

    /// A token: the subject token exchanged at STS, then for the
    /// impersonated service account's when one is named.
    pub(super) async fn access_token(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<AccessToken, Error> {
        let subject_token = self.source.subject_token(clients, clock).await?;
        let mut form = vec![
            ("grant_type", GRANT_TYPE),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("subject_token", subject_token.expose()),
            ("subject_token_type", &self.subject_token_type),
            ("scope", &self.sts_scope),
            ("audience", &self.audience),
        ];
        if let StsAuth::UserProject { options } = &self.auth {
            form.push(("options", options));
        }
        let mut request = clients.api.post(self.token_url.clone()).form(&form);
        if let StsAuth::Client { id, secret } = &self.auth {
            // Standard padded base64, with the header marked sensitive.
            request = request.basic_auth(id, Some(secret.expose()));
        }
        let now = clock.now();
        let response: StsResponse = send_json(Call::StsExchange, request).await?;
        let Some(url) = &self.impersonation_url else {
            return expiring(response.access_token, now, Some(response.expires_in));
        };
        let call = Generate {
            url,
            auth: SourceAuth::new(&response.access_token, None)?,
            delegates: None,
            scopes: &self.scopes,
        };
        call.send(clients, clock).await
    }
}

/// Whether `audience` names a workforce pool provider:
/// `//iam.googleapis.com/locations/<location>/workforcePools/<pool>/providers/<provider>`.
fn is_workforce_pool_audience(audience: &str) -> bool {
    let path = audience
        .strip_prefix("//iam.googleapis.com/")
        .unwrap_or(audience);
    let mut parts = path.split('/');
    matches!(
        (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ),
        (
            Some("locations"),
            Some(_),
            Some("workforcePools"),
            Some(pool),
            Some("providers"),
            Some(provider),
            None,
        ) if !pool.is_empty() && !provider.is_empty()
    )
}

#[cfg(test)]
mod tests;
