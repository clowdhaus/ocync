//! A service account impersonated through IAM `generateAccessToken`, with a
//! token of source credentials or of an external account's STS exchange.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    AccessToken, Call, Clients, CredentialType, Credentials, DEFAULT_SCOPE, Error, REGISTRY_SCOPE,
    Scopes, Secret, Setup, send_json,
};
use crate::aws::{Clock, parse_rfc3339_with_offset};

/// How long an impersonated token is asked to last, as upstream.
const LIFETIME: Duration = Duration::from_secs(3600);

/// What precedes the service account in an impersonation URL.
const SERVICE_ACCOUNTS: &str = "/serviceAccounts/";

/// An impersonated service account file, as upstream reads it.
#[derive(Deserialize)]
struct ImpersonatedFile {
    service_account_impersonation_url: String,
    source_credentials: Value,
    delegates: Option<Vec<String>>,
    scopes: Option<Vec<String>>,
}

/// An impersonation URL that names one service account, as upstream
/// requires of an impersonated service account file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImpersonationUrl(String);

impl ImpersonationUrl {
    /// `url`, if [`SERVICE_ACCOUNTS`] occurs in it exactly once.
    fn parse(url: String) -> Result<Self, Error> {
        match url.matches(SERVICE_ACCOUNTS).count() {
            1 => Ok(Self(url)),
            _ => Err(Error::ImpersonationUrl),
        }
    }
}

/// A service account impersonated with source credentials.
#[derive(Debug)]
pub(crate) struct Impersonated {
    source: Box<Credentials>,
    url: ImpersonationUrl,
    delegates: Option<Vec<String>>,
    scopes: Vec<String>,
}

/// The `generateAccessToken` request, in upstream's fields.
#[derive(Serialize)]
struct GenerateRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    delegates: Option<&'a [String]>,
    scope: &'a [String],
    lifetime: String,
}

/// The `generateAccessToken` answer.
#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expireTime")]
    expire_time: String,
}

impl Impersonated {
    /// The account `json` describes, asking for `scopes`. The source is
    /// built with its own defaults, as upstream, and may be any supported
    /// type but this one.
    pub(super) async fn from_json(
        json: Value,
        scopes: Scopes,
        setup: &Setup<'_>,
    ) -> Result<Self, Error> {
        let file: ImpersonatedFile = serde_json::from_value(json).map_err(Error::Parse)?;
        let url = ImpersonationUrl::parse(file.service_account_impersonation_url)?;
        let kind = match CredentialType::of(&file.source_credentials)? {
            CredentialType::Impersonated => return Err(Error::NestedImpersonation),
            kind => kind,
        };
        let source =
            Credentials::from_json(kind, file.source_credentials, Scopes::Default, setup).await?;
        Ok(Self {
            source: Box::new(source),
            url,
            delegates: file.delegates,
            scopes: match scopes {
                Scopes::Registry => vec![REGISTRY_SCOPE.to_owned()],
                Scopes::Default => file
                    .scopes
                    .unwrap_or_else(|| vec![DEFAULT_SCOPE.to_owned()]),
            },
        })
    }

    /// A token for the service account, with a fresh source token.
    pub(super) async fn access_token(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<AccessToken, Error> {
        let headers = self.source.source_headers(clients, clock).await?;
        generate_access_token(
            clients,
            clock,
            &self.url.0,
            &headers,
            self.delegates.as_deref(),
            &self.scopes,
        )
        .await
    }
}

/// Call `generateAccessToken` at `url` with `headers` from the source, for
/// a token with `scopes` lasting [`LIFETIME`].
pub(super) async fn generate_access_token(
    clients: &Clients,
    clock: Clock,
    url: &str,
    headers: &[(&'static str, String)],
    delegates: Option<&[String]>,
    scopes: &[String],
) -> Result<AccessToken, Error> {
    let mut request = clients.api.post(url).json(&GenerateRequest {
        delegates,
        scope: scopes,
        lifetime: format!("{}s", LIFETIME.as_secs_f64()),
    });
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response: GenerateResponse = send_json(Call::Impersonation, request).await?;
    let expiry = parse_rfc3339_with_offset(&response.expire_time)
        .ok_or_else(|| Error::ExpireTime(response.expire_time.clone()))?;
    if expiry <= clock.now() {
        return Err(Error::AlreadyExpired(response.expire_time));
    }
    Ok(AccessToken {
        token: Secret(response.access_token),
        expiry: Some(expiry),
    })
}

#[cfg(test)]
mod tests;
