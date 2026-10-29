//! Credentials for an IAM Identity Center role: the portal's
//! `GetRoleCredentials`, called with the cached login token, as
//! `aws-config` 1.13.0 calls it.

use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use http::HeaderName;
use serde::Deserialize;

use super::super::config::Config;
use super::super::endpoint::{CredentialApi, Region};
use super::super::host::Host;
use super::super::{ApiRequest, ConfigError, Failure, retry};
use super::sso_token::{self, Login};
use super::{Credentials, credential_api};
use crate::error::CredentialFailure;

/// The header that carries the login token: the call is not signed.
const BEARER_TOKEN: HeaderName = HeaderName::from_static("x-amz-sso_bearer_token");

/// The path of `GetRoleCredentials`.
const ROLE_CREDENTIALS_PATH: &str = "/federation/credentials";

/// The query names of `GetRoleCredentials`.
const ROLE_NAME: &str = "role_name";
const ACCOUNT_ID: &str = "account_id";

/// An IAM Identity Center role a profile names.
#[derive(Debug)]
pub(super) struct Role<'a> {
    pub(super) login: Login<'a>,
    /// `sso_region`, where the portal and SSO OIDC are called.
    pub(super) region: Region,
    pub(super) account_id: &'a str,
    pub(super) role_name: &'a str,
}

/// Why the role gave no credentials. The texts of the answer's faults are
/// upstream's.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// No login token was loaded.
    #[error(transparent)]
    Token(sso_token::Error),
    /// The portal client could not be set up.
    #[error("the SSO client could not be set up")]
    Setup(#[source] ConfigError),
    /// The call failed, after its retries. Boxed, as a failure can hold a
    /// credential error that holds this one.
    #[error("SSO GetRoleCredentials failed")]
    Call(#[source] Box<Failure>),
    /// The answer is not the JSON the call answers with.
    #[error("SSO GetRoleCredentials answer could not be read")]
    Answer(#[source] crate::json::Unreadable),
    /// The answer holds no credentials.
    #[error("SSO did not return credentials")]
    NoCredentials,
    /// The credentials have no key ID.
    #[error("no access key id in response")]
    NoAccessKeyId,
    /// The credentials have no secret key.
    #[error("no secret key in response")]
    NoSecretKey,
    /// The expiry is out of the clock's range.
    #[error("expiration could not be converted into a system time")]
    Expiration,
}

impl Error {
    /// Whether the login or the role was refused, the call may pass if sent
    /// again, or something else failed. An expired login is refused: it
    /// needs a new sign-in.
    pub(super) fn credential_failure(&self) -> CredentialFailure {
        match self {
            Self::Token(sso_token::Error::Expired) => CredentialFailure::Refused,
            Self::Token(sso_token::Error::RefreshFailed) => CredentialFailure::Transient,
            Self::Call(failure) => failure.credential_failure(),
            _ => CredentialFailure::Failed,
        }
    }
}

/// The credentials of `role`, from the portal in its region, with the
/// settings `config` holds and the `retry` settings of the calls the chain
/// makes.
pub(super) async fn load(
    host: &Host,
    config: &Config,
    retry: &retry::Settings,
    role: &Role<'_>,
) -> Result<Credentials, Error> {
    let token = sso_token::load(host, config, retry, role.login, &role.region)
        .await
        .map_err(Error::Token)?;
    let api = credential_api(host, config, retry, CredentialApi::Sso, &role.region)
        .map_err(Error::Setup)?;
    let request = ApiRequest {
        operation: "GetRoleCredentials",
        method: http::Method::GET,
        path: ROLE_CREDENTIALS_PATH,
        headers: &[(BEARER_TOKEN, token.as_str())],
        body: Bytes::new(),
        modeled_errors: &[],
    };
    let query = [(ROLE_NAME, role.role_name), (ACCOUNT_ID, role.account_id)];
    let answer = api
        .call_unsigned(&request, &query)
        .await
        .map_err(|e| Error::Call(Box::new(e)))?;
    credentials(&answer)
}

/// A `GetRoleCredentials` answer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    role_credentials: Option<RoleCredentials>,
}

/// The credentials in an answer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleCredentials {
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    /// Milliseconds since the UNIX epoch. The SDK reads a missing one as 0.
    #[serde(default)]
    expiration: i64,
}

/// The credentials in the answer `body`.
fn credentials(body: &[u8]) -> Result<Credentials, Error> {
    let answer: Answer = crate::json::parse(body).map_err(Error::Answer)?;
    let role = answer.role_credentials.ok_or(Error::NoCredentials)?;
    let access_key_id = role.access_key_id.ok_or(Error::NoAccessKeyId)?;
    let secret_access_key = role.secret_access_key.ok_or(Error::NoSecretKey)?;
    let since_epoch = Duration::from_millis(role.expiration.unsigned_abs());
    let expiry = if role.expiration >= 0 {
        UNIX_EPOCH.checked_add(since_epoch)
    } else {
        UNIX_EPOCH.checked_sub(since_epoch)
    }
    .ok_or(Error::Expiration)?;
    Ok(Credentials {
        access_key_id,
        secret_access_key,
        session_token: role.session_token,
        expiry: Some(expiry),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::aws::host::{Env, Fs, Network};

    /// The call is a GET of the role's path and query, carrying the cached
    /// token in its own header and no signature, in the profile's region.
    /// Breaks if the token, account or role is sent elsewhere, or the call
    /// is signed.
    #[tokio::test]
    async fn calls_the_portal_as_the_sdk_does() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/federation/credentials"))
            .and(query_param("role_name", "MySsoRole"))
            .and(query_param("account_id", "123456789"))
            .and(header("x-amz-sso_bearer_token", "a-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "roleCredentials": {
                    "accessKeyId": "ASIARCORRECT",
                    "secretAccessKey": "secretkeycorrect",
                    "sessionToken": "tokencorrect",
                    "expiration": 1_234_567_890_000_i64,
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let host = Host {
            env: Env::Fixed(HashMap::from([
                ("HOME".to_owned(), "/home".to_owned()),
                ("AWS_ENDPOINT_URL_SSO".to_owned(), server.uri()),
            ])),
            fs: Fs::Fixed(HashMap::from([(
                PathBuf::from("/home/.aws/sso/cache/dace00cba5f8355ec9d274ceb2bcebdfbeed0e12.json"),
                br#"{"accessToken": "a-token", "expiresAt": "2080-10-16T03:56:45Z"}"#.to_vec(),
            )])),
            network: Network::Real,
            ..Host::fixed()
        };
        let role = Role {
            login: Login::Legacy {
                start_url: "https://ssotest.awsapps.com/start",
            },
            region: Region::parse("us-east-2").unwrap(),
            account_id: "123456789",
            role_name: "MySsoRole",
        };
        let config = Config::load(&host, None);
        let loaded = load(&host, &config, &retry::Settings::default(), &role)
            .await
            .unwrap();
        assert_eq!(loaded.access_key_id(), "ASIARCORRECT");
        assert_eq!(loaded.secret_access_key(), "secretkeycorrect");
        assert_eq!(loaded.session_token(), Some("tokencorrect"));
        assert_eq!(
            loaded.expiry(),
            Some(UNIX_EPOCH + Duration::from_secs(1_234_567_890))
        );
        let requests = server.received_requests().await.unwrap();
        assert!(!requests[0].headers.contains_key("authorization"));
    }

    /// The answer is read as the SDK reads it: no session token needed, a
    /// missing expiry read as the epoch, and each missing part named.
    /// Breaks if a missing key reads as empty or the expiry's unit differs.
    #[test]
    fn reads_the_answer_as_the_sdk_does() {
        let lasting =
            credentials(br#"{"roleCredentials": {"accessKeyId": "a", "secretAccessKey": "s"}}"#)
                .unwrap();
        assert_eq!(
            (lasting.session_token(), lasting.expiry()),
            (None, Some(UNIX_EPOCH))
        );
        let before = credentials(
            br#"{"roleCredentials": {"accessKeyId": "a", "secretAccessKey": "s", "expiration": -1000}}"#,
        )
        .unwrap();
        assert_eq!(
            before.expiry(),
            UNIX_EPOCH.checked_sub(Duration::from_secs(1))
        );
        for (body, error) in [
            (&br#"{}"#[..], "SSO did not return credentials"),
            (
                br#"{"roleCredentials": {"secretAccessKey": "s"}}"#,
                "no access key id in response",
            ),
            (
                br#"{"roleCredentials": {"accessKeyId": "a"}}"#,
                "no secret key in response",
            ),
            (
                br#"{"roleCredentials": {"accessKeyId": 7, "secretAccessKey": "s"}}"#,
                "SSO GetRoleCredentials answer could not be read",
            ),
        ] {
            let message = credentials(body).err().unwrap().to_string();
            assert_eq!(message, error);
        }
    }
}
