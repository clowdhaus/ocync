//! An external account (Workload Identity Federation): a subject token from
//! a file, a URL or AWS, exchanged at Google's STS for an access token, then
//! for a service account's token when the file names one to impersonate.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::Value;

use super::aws_source::AwsSource;
use super::{
    AccessToken, Call, Clients, DEFAULT_SCOPE, Error, REGISTRY_SCOPE, Scopes, Secret, Setup,
    expiring, impersonated, send_json,
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

/// Where the subject token comes from, tried in upstream's order: a source
/// with `environment_id` is AWS, then `executable`, `url` and `file`.
#[derive(Deserialize)]
#[serde(untagged)]
enum SourceFile {
    Aws {
        #[allow(dead_code, reason = "selects the AWS source, as upstream")]
        environment_id: String,
        region_url: Option<String>,
        regional_cred_verification_url: Option<String>,
        imdsv2_session_token_url: Option<String>,
    },
    Executable {
        #[allow(dead_code, reason = "selects the executable source, as upstream")]
        executable: ExecutableFile,
    },
    Url {
        url: String,
        headers: Option<HashMap<String, String>>,
        format: Option<FormatFile>,
    },
    File {
        file: String,
        format: Option<FormatFile>,
    },
}

/// An executable source's settings: the command is all that selects it.
#[derive(Deserialize)]
struct ExecutableFile {
    #[allow(dead_code, reason = "required, as upstream requires it")]
    command: String,
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
                let json: Value = serde_json::from_str(&text).map_err(Error::parse)?;
                match json.get(field) {
                    Some(Value::String(token)) => Ok(Secret(token.clone())),
                    _ => Err(Error::SubjectField(field.clone())),
                }
            }
        }
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
        url: String,
        headers: Vec<(String, String)>,
        format: Format,
    },
    /// A signed AWS `GetCallerIdentity` request.
    Aws(AwsSource),
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
                format.read(String::from_utf8(bytes).map_err(Error::parse)?)
            }
            Self::Url {
                url,
                headers,
                format,
            } => {
                let mut request = clients.api.get(url).timeout(SUBJECT_URL_TIMEOUT);
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let call = Call::SubjectUrl;
                let response = request
                    .send()
                    .await
                    .map_err(|source| Error::Transport { call, source })?;
                if !response.status().is_success() {
                    return Err(Error::status(call, response).await);
                }
                let text = response
                    .text()
                    .await
                    .map_err(|source| Error::Decode { call, source })?;
                format.read(text)
            }
            Self::Aws(aws) => aws.subject_token(clock.now()).await,
        }
    }
}

/// How long a URL source may take, as upstream's client timeout.
const SUBJECT_URL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// An external account.
#[derive(Debug)]
pub(crate) struct ExternalAccount {
    audience: String,
    subject_token_type: String,
    token_url: String,
    impersonation_url: Option<String>,
    /// The client ID and secret, sent only when both are set.
    client: Option<(String, Secret)>,
    /// The STS `options`, `{"userProject": ...}`, sent only with no client
    /// ID and no secret, as upstream.
    options: Option<String>,
    scopes: Vec<String>,
    source: Source,
}

/// The STS answer.
#[derive(Deserialize)]
struct StsResponse {
    access_token: String,
    #[allow(dead_code, reason = "required, as upstream requires it")]
    issued_token_type: String,
    #[allow(dead_code, reason = "required, as upstream requires it")]
    token_type: String,
    expires_in: u64,
}

impl ExternalAccount {
    /// The account `json` describes, asking for `scopes`. An AWS source
    /// resolves its region and takes its AWS credentials here.
    pub(super) async fn from_json(
        json: Value,
        scopes: Scopes,
        setup: &Setup<'_>,
    ) -> Result<Self, Error> {
        let file: AccountFile = serde_json::from_value(json).map_err(Error::parse)?;
        if file.workforce_pool_user_project.is_some() && !is_workforce_pool_audience(&file.audience)
        {
            return Err(Error::Parse(
                "workforce_pool_user_project should not be set for non-workforce pool credentials"
                    .into(),
            ));
        }
        let source = match file.credential_source {
            SourceFile::Aws {
                region_url,
                regional_cred_verification_url,
                imdsv2_session_token_url,
                ..
            } => Source::Aws(
                AwsSource::new(
                    setup,
                    region_url.as_deref(),
                    regional_cred_verification_url.as_deref(),
                    imdsv2_session_token_url.as_deref(),
                    &file.audience,
                )
                .await?,
            ),
            SourceFile::Executable { .. } => return Err(Error::UnsupportedSource("executable")),
            SourceFile::Url {
                url,
                headers,
                format,
            } => {
                let mut headers: Vec<_> = headers.unwrap_or_default().into_iter().collect();
                headers.sort();
                Source::Url {
                    url,
                    headers,
                    format: format.into(),
                }
            }
            SourceFile::File { file, format } => Source::File {
                path: PathBuf::from(file),
                format: format.into(),
                fs: setup.host.fs.clone(),
            },
        };
        let options = match (&file.client_id, &file.client_secret) {
            (None, None) => file
                .workforce_pool_user_project
                .map(|project| serde_json::json!({ "userProject": project }).to_string()),
            _ => None,
        };
        let client = match (file.client_id, file.client_secret) {
            (Some(id), Some(secret)) => Some((id, secret)),
            _ => None,
        };
        let scopes = match scopes {
            Scopes::Registry => vec![REGISTRY_SCOPE.to_owned()],
            Scopes::Default => file
                .scopes
                .filter(|scopes| !scopes.is_empty())
                .unwrap_or_else(|| vec![DEFAULT_SCOPE.to_owned()]),
        };
        Ok(Self {
            audience: file.audience,
            subject_token_type: file.subject_token_type,
            token_url: file.token_url,
            impersonation_url: file.service_account_impersonation_url,
            client,
            options,
            scopes,
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
        let scope = match self.impersonation_url {
            Some(_) => IAM_SCOPE.to_owned(),
            None => self.scopes.join(" "),
        };
        let mut form = vec![
            ("grant_type", GRANT_TYPE),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("subject_token", subject_token.expose()),
            ("subject_token_type", &self.subject_token_type),
            ("scope", &scope),
            ("audience", &self.audience),
        ];
        if let Some(options) = &self.options {
            form.push(("options", options));
        }
        let mut request = clients.api.post(&self.token_url).form(&form);
        if let Some((id, secret)) = &self.client {
            let basic = URL_SAFE_NO_PAD.encode(format!("{id}:{}", secret.expose()));
            request = request.header("authorization", format!("Basic {basic}"));
        }
        let now = clock.now();
        let response: StsResponse = send_json(Call::StsExchange, request).await?;
        let Some(url) = &self.impersonation_url else {
            return Ok(expiring(
                response.access_token,
                now,
                Some(response.expires_in),
            ));
        };
        let headers = [("authorization", format!("Bearer {}", response.access_token))];
        impersonated::generate_access_token(clients, clock, url, &headers, None, &self.scopes).await
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
