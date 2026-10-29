//! AWS STS, as the credential sources that assume a role call it: the query
//! protocol, a form body and an XML answer.

use std::borrow::Cow;
use std::fmt;
use std::str::Utf8Error;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use http::header::CONTENT_TYPE;

use super::super::config::Config;
use super::super::endpoint::{CredentialApi, Region};
use super::super::host::{Clock, Host};
use super::super::sigv4::encode_pairs;
use super::super::{
    Api, ApiRequest, ConfigError, Failure, ModeledError, ModeledRetry, ServiceError, Signing,
    calendar, retry, xml,
};
use super::web_identity::Token;
use super::{Credentials, credential_api};
use crate::error::CredentialFailure;

/// The STS API version every request names.
const VERSION: &str = "2011-06-15";

/// The content type of a query protocol request.
const FORM: &str = "application/x-www-form-urlencoded";

/// An STS operation a credential source calls.
struct Operation {
    /// The form's `Action`. The answer is in `<action>Result`.
    action: &'static str,
    /// The errors the operation models, as `aws-sdk-sts` generates them.
    modeled_errors: &'static [ModeledError],
}

/// Assume a role, signed with the credentials of the caller.
const ASSUME_ROLE: Operation = Operation {
    action: "AssumeRole",
    modeled_errors: &[
        EXPIRED_TOKEN,
        MALFORMED_POLICY,
        PACKED_POLICY_TOO_LARGE,
        REGION_DISABLED,
    ],
};

/// Exchange a web identity token for a role's credentials. The SDK
/// re-sends `IDPCommunicationError`, an identity provider STS could not
/// reach, for this operation alone.
const ASSUME_ROLE_WITH_WEB_IDENTITY: Operation = Operation {
    action: "AssumeRoleWithWebIdentity",
    modeled_errors: &[
        EXPIRED_TOKEN,
        IDP_COMMUNICATION_ERROR,
        IDP_REJECTED_CLAIM,
        INVALID_IDENTITY_TOKEN,
        MALFORMED_POLICY,
        PACKED_POLICY_TOO_LARGE,
        REGION_DISABLED,
    ],
};

const EXPIRED_TOKEN: ModeledError = modeled("ExpiredTokenException", "ExpiredTokenException");
const IDP_COMMUNICATION_ERROR: ModeledError = ModeledError {
    code: "IDPCommunicationError",
    name: "IdpCommunicationErrorException",
    retry: ModeledRetry::Transient,
};
const IDP_REJECTED_CLAIM: ModeledError = modeled("IDPRejectedClaim", "IdpRejectedClaimException");
const INVALID_IDENTITY_TOKEN: ModeledError =
    modeled("InvalidIdentityToken", "InvalidIdentityTokenException");
const MALFORMED_POLICY: ModeledError = modeled(
    "MalformedPolicyDocument",
    "MalformedPolicyDocumentException",
);
const PACKED_POLICY_TOO_LARGE: ModeledError =
    modeled("PackedPolicyTooLarge", "PackedPolicyTooLargeException");
const REGION_DISABLED: ModeledError = modeled("RegionDisabledException", "RegionDisabledException");

/// An error the SDK retries only as the shared code lists say.
const fn modeled(code: &'static str, name: &'static str) -> ModeledError {
    ModeledError {
        code,
        name,
        retry: ModeledRetry::Shared,
    }
}

/// The `configured` session name of a role, else the one the SDK makes:
/// `prefix`, then the milliseconds since the UNIX epoch on `clock`.
pub(super) fn session_name<'a>(
    configured: Option<&'a str>,
    prefix: &str,
    clock: Clock,
) -> Cow<'a, str> {
    configured.map_or_else(
        || {
            let since_epoch = clock.now().duration_since(UNIX_EPOCH).unwrap_or_default();
            Cow::Owned(format!("{prefix}-{}", since_epoch.as_millis()))
        },
        Cow::Borrowed,
    )
}

/// An error STS answered with that the operation models, as the SDK
/// prints it: its name, and the message when there is one.
#[derive(Debug)]
pub(in crate::aws) struct Exception {
    name: &'static str,
    message: Option<String>,
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)?;
        match &self.message {
            Some(message) => write!(f, ": {message}"),
            None => Ok(()),
        }
    }
}

/// Why STS gave no credentials.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The STS client could not be set up.
    #[error("the STS client could not be set up")]
    Setup(#[source] Arc<ConfigError>),
    /// STS answered with an error the operation models.
    #[error("{0}")]
    Exception(Exception),
    /// The call failed, after its retries. Boxed, as a failure can hold a
    /// credential error that holds this one.
    #[error("STS call failed")]
    Call(#[source] Box<Failure>),
    /// The answer is not UTF-8.
    #[error("STS answer is not UTF-8")]
    NotUtf8(#[source] Utf8Error),
    /// The answer holds no credentials.
    #[error("STS credentials must be defined")]
    NoCredentials,
    /// The credentials lack a field.
    #[error("STS credentials have no {0}")]
    Missing(&'static str),
    /// The expiry is not an RFC 3339 time.
    #[error("STS credential expiration '{0}' is not an RFC 3339 time")]
    Expiration(String),
}

impl Error {
    /// Whether STS refused the token or the role, the call may pass if
    /// sent again, or something else failed. An expired, rejected or
    /// invalid identity token is refused; an identity provider STS could
    /// not reach may pass.
    pub(super) fn credential_failure(&self) -> CredentialFailure {
        match self {
            Self::Exception(Exception { name, .. }) => {
                if *name == IDP_COMMUNICATION_ERROR.name {
                    CredentialFailure::Transient
                } else if [EXPIRED_TOKEN, IDP_REJECTED_CLAIM, INVALID_IDENTITY_TOKEN]
                    .iter()
                    .any(|refused| refused.name == *name)
                {
                    CredentialFailure::Refused
                } else {
                    CredentialFailure::Failed
                }
            }
            Self::Call(failure) => failure.credential_failure(),
            _ => CredentialFailure::Failed,
        }
    }
}

impl From<Failure> for Error {
    /// The exception the operation models, when STS answered with one of
    /// its codes, else the failure.
    fn from(failure: Failure) -> Self {
        let name = match &failure {
            Failure::Service(error) => error.code.as_ref().and_then(|c| c.modeled).map(|m| m.name),
            _ => None,
        };
        match (name, failure) {
            (Some(name), Failure::Service(ServiceError { message, .. })) => {
                Self::Exception(Exception { name, message })
            }
            (_, failure) => Self::Call(Box::new(failure)),
        }
    }
}

/// An `AssumeRoleWithWebIdentity` call.
pub(super) struct WebIdentity<'a> {
    pub(super) role_arn: &'a str,
    pub(super) session_name: &'a str,
    pub(super) token: &'a Token,
}

/// An `AssumeRole` call.
pub(super) struct AssumeRole<'a> {
    pub(super) role_arn: &'a str,
    pub(super) session_name: &'a str,
    /// The value the role's trust policy may require, as `external_id`
    /// sets it.
    pub(super) external_id: Option<&'a str>,
}

/// STS in one region.
pub(super) struct Client {
    /// The API, or why it could not be set up. The error is reported only
    /// by a source that calls STS, so a bad STS setting does not fail a
    /// chain whose credentials come from elsewhere.
    api: Result<Api, Arc<ConfigError>>,
}

impl Client {
    /// STS in `region`, with the endpoint and clock-skew settings `config`
    /// holds and the `retry` settings of the calls it serves.
    pub(super) fn new(
        host: &Host,
        region: &Region,
        config: &Config,
        retry: &retry::Settings,
    ) -> Self {
        let api = credential_api(host, config, retry, CredentialApi::Sts, region);
        Self {
            api: api.map_err(Arc::new),
        }
    }

    /// Exchange a web identity token for a role's credentials. The call is
    /// unsigned: the token is the proof of identity.
    pub(super) async fn assume_role_with_web_identity(
        &self,
        call: &WebIdentity<'_>,
    ) -> Result<Credentials, Error> {
        let body = web_identity_form(call);
        self.call(&ASSUME_ROLE_WITH_WEB_IDENTITY, body, || async {
            Ok(Signing::Unsigned)
        })
        .await
    }

    /// Assume a role with the credentials of `caller`, which sign the call.
    pub(super) async fn assume_role(
        &self,
        caller: &Credentials,
        call: &AssumeRole<'_>,
    ) -> Result<Credentials, Error> {
        let body = assume_role_form(call);
        self.call(&ASSUME_ROLE, body, || async {
            Ok(Signing::Signed(caller.clone()))
        })
        .await
    }

    /// Send `operation` with the form `body`, signed as `signing` says,
    /// and read the credentials it answers with.
    async fn call<F, Fut>(
        &self,
        operation: &Operation,
        body: String,
        signing: F,
    ) -> Result<Credentials, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Signing, Failure>>,
    {
        let api = self.api.as_ref().map_err(|e| Error::Setup(Arc::clone(e)))?;
        let request = ApiRequest {
            operation: operation.action,
            method: http::Method::POST,
            path: "/",
            headers: &[(CONTENT_TYPE, FORM)],
            body: Bytes::from(body),
            modeled_errors: operation.modeled_errors,
        };
        let answer = api.call(&request, signing).await?;
        credentials(operation, &answer)
    }
}

/// The form body of `call`, fields in the SDK's order.
fn web_identity_form(call: &WebIdentity<'_>) -> String {
    form(
        &ASSUME_ROLE_WITH_WEB_IDENTITY,
        [
            ("RoleArn", call.role_arn),
            ("RoleSessionName", call.session_name),
            ("WebIdentityToken", call.token.as_str()),
        ],
    )
}

/// The form body of `call`, fields in the SDK's order.
fn assume_role_form(call: &AssumeRole<'_>) -> String {
    let fields = [
        ("RoleArn", call.role_arn),
        ("RoleSessionName", call.session_name),
    ];
    form(
        &ASSUME_ROLE,
        fields
            .into_iter()
            .chain(call.external_id.map(|id| ("ExternalId", id))),
    )
}

/// The form body of `operation` with `fields` after its action and
/// version, every byte outside the unreserved set encoded, as the SDK
/// encodes a query protocol request.
fn form<'a>(operation: &Operation, fields: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let head = [("Action", operation.action), ("Version", VERSION)];
    encode_pairs(head.into_iter().chain(fields))
}

/// The credentials in the answer's `<action>Result` element.
fn credentials(operation: &Operation, body: &[u8]) -> Result<Credentials, Error> {
    let document = std::str::from_utf8(body).map_err(Error::NotUtf8)?;
    let result = format!("{}Result", operation.action);
    let inner = xml::find(document, &[&result, "Credentials"]).ok_or(Error::NoCredentials)?;
    let field = |name: &'static str| xml::text(inner, &[name]).ok_or(Error::Missing(name));
    let expiration = field("Expiration")?;
    let expiry = calendar::parse_rfc3339(&expiration).ok_or(Error::Expiration(expiration))?;
    Ok(Credentials {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: Some(field("SessionToken")?),
        expiry: Some(expiry),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    use http::StatusCode;

    use super::super::super::ErrorCode;
    use super::super::super::host::Fs;
    use super::super::fixtures::DATA;
    use super::*;

    /// The recorded default chain case `name`.
    fn recorded_case(name: &str) -> std::path::PathBuf {
        Path::new(DATA)
            .join("default-credential-provider-chain")
            .join(name)
    }

    /// The bodies the SDK sent in a recorded case, in order.
    fn recorded_bodies(case: &Path) -> Vec<String> {
        let traffic: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(case.join("http-traffic.json")).unwrap())
                .unwrap();
        traffic["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| {
                let data = &event["action"]["Data"];
                (data["direction"] == "Request").then(|| data["data"]["Utf8"].as_str())?
            })
            .map(str::to_owned)
            .collect()
    }

    /// The form body is the one the SDK sent: fields in its order, `:`
    /// and `/` encoded, and the token file sent whole, trailing newline
    /// included. Breaks if a field moves, a reserved byte is left as is, or
    /// the token is trimmed.
    #[test]
    fn encodes_the_form_as_the_sdk_does() {
        let case = recorded_case("web_identity_token_invalid_jwt");
        let token = Token::read(&Fs::Real, case.join("fs/token.jwt")).unwrap();
        let body = web_identity_form(&WebIdentity {
            role_arn: "arn:aws:iam::123456789:role/RoleB",
            session_name: "web-identity-token-profile-1629229438294",
            token: &token,
        });
        assert_eq!(recorded_bodies(&case), [body]);
    }

    /// The `AssumeRole` form is the one the SDK sent, with the session name
    /// it makes, and `ExternalId` follows when set. Breaks if a field moves
    /// or the external ID is dropped.
    #[test]
    fn encodes_the_assume_role_form_as_the_sdk_does() {
        let case = recorded_case("web_identity_token_source_profile");
        let mut call = AssumeRole {
            role_arn: "arn:aws:iam::123456789:role/RoleA",
            session_name: "assume-role-from-profile-1629230104880",
            external_id: None,
        };
        assert_eq!(recorded_bodies(&case)[1], assume_role_form(&call));
        call.external_id = Some("id/1");
        assert_eq!(
            assume_role_form(&call),
            "Action=AssumeRole&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A123456789%3Arole%2FRoleA\
             &RoleSessionName=assume-role-from-profile-1629230104880&ExternalId=id%2F1"
        );
    }

    /// The credentials are read from the result element, and an answer
    /// without them, without a field or with an expiry that is not a time is
    /// refused with the reason. Breaks if a missing field is read as empty.
    #[test]
    fn reads_the_credentials_or_says_what_is_missing() {
        let read = |inner: &str| {
            let answer = format!(
                "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult>\
                 {inner}</AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>"
            );
            credentials(&ASSUME_ROLE_WITH_WEB_IDENTITY, answer.as_bytes())
        };
        let loaded = read(
            "<Credentials><AccessKeyId>AKID</AccessKeyId><SecretAccessKey>secret</SecretAccessKey>\
             <SessionToken>token</SessionToken><Expiration>2021-08-16T20:52:53Z</Expiration>\
             </Credentials>",
        )
        .unwrap();
        assert_eq!(loaded.access_key_id(), "AKID");
        assert_eq!(loaded.secret_access_key(), "secret");
        assert_eq!(loaded.session_token(), Some("token"));
        assert_eq!(
            loaded.expiry(),
            Some(UNIX_EPOCH + Duration::from_secs(1_629_147_173))
        );
        for (inner, error) in [
            ("", "STS credentials must be defined"),
            (
                "<Credentials><AccessKeyId>AKID</AccessKeyId><SecretAccessKey>s</SecretAccessKey>\
                 <Expiration>2021-08-16T20:52:53Z</Expiration></Credentials>",
                "STS credentials have no SessionToken",
            ),
            (
                "<Credentials><Expiration>tomorrow</Expiration></Credentials>",
                "STS credential expiration 'tomorrow' is not an RFC 3339 time",
            ),
        ] {
            assert_eq!(read(inner).unwrap_err().to_string(), error);
        }
        let not_utf8 = credentials(&ASSUME_ROLE, b"\xff").unwrap_err();
        assert!(matches!(not_utf8, Error::NotUtf8(_)), "{not_utf8}");
    }

    /// An expired, rejected or invalid identity token and an access denial
    /// are refused, an identity provider STS could not reach or a server
    /// error may pass, and any other answer failed. Breaks if a transient
    /// STS failure exits as refused credentials, or a refusal stops counting
    /// as one.
    #[test]
    fn classifies_what_sts_refuses() {
        for (status, code, expected) in [
            (400, "IDPRejectedClaim", CredentialFailure::Refused),
            (400, "InvalidIdentityToken", CredentialFailure::Refused),
            (400, "ExpiredTokenException", CredentialFailure::Refused),
            (403, "AccessDenied", CredentialFailure::Refused),
            (400, "IDPCommunicationError", CredentialFailure::Transient),
            (503, "InternalFailure", CredentialFailure::Transient),
            (400, "MalformedPolicyDocument", CredentialFailure::Failed),
        ] {
            let error = Error::from(Failure::Service(ServiceError {
                status: StatusCode::from_u16(status).unwrap(),
                code: ErrorCode::parse(code, ASSUME_ROLE_WITH_WEB_IDENTITY.modeled_errors),
                message: None,
                retry_after: None,
            }));
            assert_eq!(error.credential_failure(), expected, "{code}");
        }
    }

    /// A code the operation models is reported by the SDK's name for it,
    /// with the message; any other answer stays the call's failure. Breaks
    /// if a modeled error loses its name or an unmodeled one is renamed.
    #[test]
    fn names_a_modeled_error_as_the_sdk_does() {
        let error = |code: &str, message: Option<&str>| {
            Error::from(Failure::Service(ServiceError {
                status: StatusCode::BAD_REQUEST,
                code: ErrorCode::parse(code, ASSUME_ROLE_WITH_WEB_IDENTITY.modeled_errors),
                message: message.map(str::to_owned),
                retry_after: None,
            }))
            .to_string()
        };
        assert_eq!(
            error("InvalidIdentityToken", Some("No OpenIDConnect provider")),
            "InvalidIdentityTokenException: No OpenIDConnect provider"
        );
        assert_eq!(error("IDPRejectedClaim", None), "IdpRejectedClaimException");
        assert_eq!(error("AccessDenied", Some("no")), "STS call failed");
    }
}
