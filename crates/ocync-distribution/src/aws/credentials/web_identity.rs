//! Credentials for a role assumed with a web identity token: the role
//! `AWS_ROLE_ARN` names with the token in `AWS_WEB_IDENTITY_TOKEN_FILE`, as
//! EKS IAM roles for service accounts set them, or one a profile names.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::str::Utf8Error;

use super::super::host::{Fs, Host};
use super::{Credentials, Error as ChainError, REDACTED, sts};

/// The variable naming the token file.
const TOKEN_FILE: &str = "AWS_WEB_IDENTITY_TOKEN_FILE";

/// The variable naming the role.
const ROLE_ARN: &str = "AWS_ROLE_ARN";

/// The variable naming the role session.
const SESSION_NAME: &str = "AWS_ROLE_SESSION_NAME";

/// A web identity token, as read from its file: the proof of identity STS
/// takes in place of a signature. It never prints.
pub(super) struct Token(String);

impl Token {
    /// The token in the file at `path`, whole: no trimming, as upstream
    /// sends it.
    pub(super) fn read(fs: &Fs, path: PathBuf) -> Result<Self, Error> {
        let bytes = match fs.read(&path) {
            Ok(bytes) => bytes,
            Err(source) => return Err(Error::TokenFile { path, source }),
        };
        String::from_utf8(bytes)
            .map(Self)
            .map_err(|e| Error::NotUtf8(e.utf8_error()))
    }

    /// The token, to send.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Why the source is configured and gave no credentials.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The token file did not read.
    #[error("failed to read the web identity token file {}", path.display())]
    TokenFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The token file is not UTF-8.
    #[error("WebIdentityToken was not valid UTF-8")]
    NotUtf8(#[source] Utf8Error),
    /// STS refused the token or failed.
    #[error("failed to assume the role with the web identity token")]
    Sts(#[source] sts::Error),
}

/// Who configured a role, which names the session the SDK makes for it.
#[derive(Debug, Clone, Copy)]
pub(super) enum Origin {
    /// The variables IRSA sets.
    Environment,
    /// A profile's `role_arn` and `web_identity_token_file`.
    Profile,
}

impl Origin {
    /// The session name's prefix.
    fn session_prefix(self) -> &'static str {
        match self {
            Self::Environment => "web-identity-token",
            Self::Profile => "web-identity-token-profile",
        }
    }
}

/// A role to assume with the token in a file.
#[derive(Debug)]
pub(super) struct Role<'a> {
    pub(super) arn: &'a str,
    pub(super) token_file: &'a str,
    pub(super) session_name: Option<&'a str>,
    pub(super) origin: Origin,
}

/// Credentials for the role the environment names, from STS in the region
/// `sts` calls. Only an unset token file or role variable leaves the source
/// not configured, as upstream.
pub(super) async fn load(host: &Host, sts: &sts::Client) -> Result<Credentials, ChainError> {
    let not_loaded = |reason: &str| ChainError::NotLoaded(reason.to_owned());
    let token_file = host
        .env
        .get(TOKEN_FILE)
        .ok_or_else(|| not_loaded("$AWS_WEB_IDENTITY_TOKEN_FILE was not set"))?;
    let arn = host
        .env
        .get(ROLE_ARN)
        .ok_or_else(|| not_loaded("AWS_ROLE_ARN environment variable must be set"))?;
    let session_name = host.env.get(SESSION_NAME);
    let role = Role {
        arn: &arn,
        token_file: &token_file,
        session_name: session_name.as_deref(),
        origin: Origin::Environment,
    };
    assume(host, sts, &role)
        .await
        .map_err(ChainError::WebIdentity)
}

/// Credentials for `role`, from STS in the region `sts` calls. With no
/// session name set, the SDK's is made from the role's origin and the time.
pub(super) async fn assume(
    host: &Host,
    sts: &sts::Client,
    role: &Role<'_>,
) -> Result<Credentials, Error> {
    let session_name =
        sts::session_name(role.session_name, role.origin.session_prefix(), host.clock);
    let token = Token::read(&host.fs, PathBuf::from(role.token_file))?;
    sts.assume_role_with_web_identity(&sts::WebIdentity {
        role_arn: role.arn,
        session_name: &session_name,
        token: &token,
    })
    .await
    .map_err(Error::Sts)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::super::config::Config;
    use super::super::super::endpoint::Region;
    use super::super::super::host::{Clock, Env, Network};
    use super::super::super::retry;
    use super::super::error_chain;
    use super::super::fixtures::capture_logs;
    use super::*;

    const TOKEN: &str = "header.payload.signature\n";
    const ROLE: &str = "arn:aws:iam::123456789012:role/test-role";

    /// The answer STS gave in the recorded `web_identity_token_env` case.
    const ANSWER: &str = "<AssumeRoleWithWebIdentityResponse \
        xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><AssumeRoleWithWebIdentityResult>\
        <Credentials><AccessKeyId>AKIDTEST</AccessKeyId>\
        <SecretAccessKey>SECRETKEYTEST</SecretAccessKey><SessionToken>SESSIONTOKEN_TEST</SessionToken>\
        <Expiration>2021-08-16T20:52:53Z</Expiration></Credentials>\
        </AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>";

    fn sts_error(code: &str) -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_string(format!(
            "<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code>\
             <Message>No OpenIDConnect provider found in your account</Message></Error>\
             </ErrorResponse>"
        ))
    }

    /// A host with `vars`, the token in `/token.jwt`, STS at `server`, and
    /// the clock one second past the epoch, with STS for it.
    fn source(server: &MockServer, vars: &[(&str, &str)]) -> (Host, sts::Client) {
        let mut env: HashMap<_, _> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        env.insert("AWS_ENDPOINT_URL_STS".to_owned(), server.uri());
        let host = Host {
            env: Env::Fixed(env),
            fs: Fs::Fixed(HashMap::from([(
                PathBuf::from("/token.jwt"),
                TOKEN.as_bytes().to_vec(),
            )])),
            clock: Clock::Fixed(UNIX_EPOCH + Duration::from_secs(1)),
            network: Network::Real,
            ..Host::fixed()
        };
        let retry = retry::Settings {
            initial_backoff: Duration::from_millis(1),
            ..retry::Settings::default()
        };
        let region = Region::parse("us-east-1").unwrap();
        let sts = sts::Client::new(&host, &region, &Config::load(&host, None), &retry).unwrap();
        (host, sts)
    }

    const CONFIGURED: &[(&str, &str)] = &[(TOKEN_FILE, "/token.jwt"), (ROLE_ARN, ROLE)];

    /// The token is sent unsigned, as a form, with a session name made from
    /// the clock when none is set, and the role's credentials come back.
    /// Breaks if the call is signed or the default session name differs.
    #[tokio::test]
    async fn exchanges_the_token_unsigned_for_the_role() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains(
                "&RoleSessionName=web-identity-token-1000&WebIdentityToken=header.payload.signature%0A",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(ANSWER))
            .expect(1)
            .mount(&server)
            .await;
        let (host, sts) = source(&server, CONFIGURED);
        let loaded = load(&host, &sts).await.unwrap();
        assert_eq!(loaded.access_key_id(), "AKIDTEST");
        assert_eq!(loaded.session_token(), Some("SESSIONTOKEN_TEST"));
        let request = &server.received_requests().await.unwrap()[0];
        assert!(!request.headers.contains_key("authorization"));
        assert!(!request.headers.contains_key("x-amz-date"));
    }

    /// An identity provider STS could not reach is retried, as the SDK
    /// retries that code for this operation alone. Breaks if the code is
    /// final.
    #[tokio::test]
    async fn retries_an_identity_provider_that_did_not_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sts_error("IDPCommunicationError"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ANSWER))
            .expect(1)
            .mount(&server)
            .await;
        let (host, sts) = source(&server, CONFIGURED);
        assert_eq!(load(&host, &sts).await.unwrap().access_key_id(), "AKIDTEST");
    }

    /// A refused token stops the chain after one request, with the error
    /// upstream reports, and the token reaches neither the logs nor the
    /// error. Breaks if a refusal is retried or the token prints.
    #[tokio::test]
    async fn reports_a_refused_token_without_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sts_error("InvalidIdentityToken"))
            .expect(1)
            .mount(&server)
            .await;
        let (host, sts) = source(&server, CONFIGURED);
        let (result, logs) = capture_logs(load(&host, &sts)).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, ChainError::WebIdentity(Error::Sts(_))),
            "{err}"
        );
        let message = error_chain(&err);
        assert!(
            message.contains(
                "InvalidIdentityTokenException: No OpenIDConnect provider found in your account"
            ),
            "{message}"
        );
        assert!(!message.contains("header.payload"), "{message}");
        assert!(!logs.contains("header.payload"), "{logs}");
    }

    /// Only an unset token file or role variable leaves the source not
    /// configured, with upstream's reason; a token that is not UTF-8 stops
    /// the chain. Breaks if a configured source with a bad token is skipped.
    #[tokio::test]
    async fn reads_only_unset_variables_as_not_configured() {
        let server = MockServer::start().await;
        let reason = |vars: &[(&str, &str)]| {
            let (host, sts) = source(&server, vars);
            async move {
                match load(&host, &sts).await {
                    Err(ChainError::NotLoaded(reason)) => reason,
                    other => panic!("{other:?}"),
                }
            }
        };
        assert_eq!(
            reason(&[(ROLE_ARN, ROLE)]).await,
            "$AWS_WEB_IDENTITY_TOKEN_FILE was not set"
        );
        assert_eq!(
            reason(&[(TOKEN_FILE, "/token.jwt")]).await,
            "AWS_ROLE_ARN environment variable must be set"
        );

        let (mut host, sts) = source(&server, CONFIGURED);
        host.fs = Fs::Fixed(HashMap::from([(PathBuf::from("/token.jwt"), vec![0xff])]));
        let err = load(&host, &sts).await.unwrap_err();
        assert!(
            matches!(err, ChainError::WebIdentity(Error::NotUtf8(_))),
            "{err}"
        );
    }

    /// The token never prints through `Debug`.
    #[test]
    fn debug_redacts_the_token() {
        assert_eq!(format!("{:?}", Token(TOKEN.to_owned())), REDACTED);
    }
}
