//! A service account impersonated through IAM `generateAccessToken`, with a
//! token of source credentials or of an external account's STS exchange.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{
    AccessToken, Call, Clients, CredentialType, Credentials, DEFAULT_SCOPE, Error, Expiry,
    ScopeList, Scopes, Secret, Setup, SourceAuth, send_json,
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

/// The impersonation URL of an impersonated service account file, which
/// must name one service account, as upstream requires.
fn parse_impersonation_url(url: &str) -> Result<Url, Error> {
    if url.matches(SERVICE_ACCOUNTS).count() != 1 {
        return Err(Error::ImpersonationUrl);
    }
    super::parse_url(IMPERSONATION_URL_FIELD, url)
}

/// The field naming the impersonation URL.
pub(super) const IMPERSONATION_URL_FIELD: &str = "service_account_impersonation_url";

/// A service account impersonated with source credentials.
#[derive(Debug)]
pub(super) struct Account {
    source: Box<Credentials>,
    url: Url,
    delegates: Option<Vec<String>>,
    scopes: ScopeList,
}

/// A `generateAccessToken` call.
pub(super) struct Generate<'a> {
    pub(super) url: &'a Url,
    pub(super) auth: SourceAuth,
    pub(super) delegates: Option<&'a [String]>,
    pub(super) scopes: &'a ScopeList,
}

/// The `generateAccessToken` request, in upstream's fields.
#[derive(Serialize)]
struct GenerateRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    delegates: Option<&'a [String]>,
    scope: &'a ScopeList,
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

impl Account {
    /// The account `json` describes, asking for `scopes`. The source is
    /// built with its own defaults, as upstream, and may be any supported
    /// type but this one.
    pub(super) async fn from_json(
        json: Value,
        scopes: Scopes,
        setup: &Setup<'_>,
    ) -> Result<Self, Error> {
        let file: ImpersonatedFile = serde_json::from_value(json).map_err(Error::Parse)?;
        let url = parse_impersonation_url(&file.service_account_impersonation_url)?;
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
            scopes: scopes.resolve(file.scopes, DEFAULT_SCOPE),
        })
    }

    /// A token for the service account, with a fresh source token.
    pub(super) async fn access_token(
        &self,
        clients: &Clients,
        clock: Clock,
    ) -> Result<AccessToken, Error> {
        let auth = self.source.source_auth(clients, clock).await?;
        let call = Generate {
            url: &self.url,
            auth,
            delegates: self.delegates.as_deref(),
            scopes: &self.scopes,
        };
        call.send(clients, clock).await
    }
}

impl Generate<'_> {
    /// A token with the call's scopes, lasting [`LIFETIME`].
    pub(super) async fn send(self, clients: &Clients, clock: Clock) -> Result<AccessToken, Error> {
        let response: GenerateResponse = send_json(clients, Call::Impersonation, |client| {
            let request = client.post(self.url.clone()).json(&GenerateRequest {
                delegates: self.delegates,
                scope: self.scopes,
                lifetime: format!("{}s", LIFETIME.as_secs_f64()),
            });
            self.auth.apply(request)
        })
        .await?;
        let expiry = parse_rfc3339_with_offset(&response.expire_time)
            .ok_or_else(|| Error::ExpireTime(response.expire_time.clone()))?;
        if expiry <= clock.now() {
            return Err(Error::AlreadyExpired(response.expire_time));
        }
        Ok(AccessToken {
            token: Secret(response.access_token),
            expiry: Expiry::At(expiry),
        })
    }
}

#[cfg(test)]
mod tests;
