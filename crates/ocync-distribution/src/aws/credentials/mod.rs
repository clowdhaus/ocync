//! The AWS credential chain: where the key that signs each call comes from,
//! tried in the SDK's order.
//!
//! A source not configured on this host answers [`Error::NotLoaded`] and
//! the chain tries the next one. Any other error stops the chain: a source
//! that is configured and fails is reported, never skipped.

mod container;
mod environment;
#[cfg(test)]
mod fixtures;
mod imds;
mod json;
mod process;
mod profile;
mod sso;
mod sso_token;
mod sts;
mod web_identity;

#[cfg(test)]
pub(super) use imds::TOKEN_HEADER;
pub(super) use sso_token::Sessions;

use std::fmt;
use std::time::{Duration, SystemTime};

use super::config::Config;
use super::endpoint::{self, CredentialApi, Region};
use super::host::Host;
use super::{Api, ApiParts, ConfigError, STALL_TIMEOUT, retry};

/// What a redacted value prints as, as elsewhere in this crate.
const REDACTED: &str = "[REDACTED]";

/// An access key, with the session token and expiry a temporary one
/// carries.
#[derive(Clone)]
pub(super) struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expiry: Option<SystemTime>,
}

impl Credentials {
    /// The access key ID.
    pub(super) fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// The secret access key.
    pub(super) fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }

    /// The session token of a temporary key.
    pub(super) fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }

    /// When a temporary key expires.
    pub(super) fn expiry(&self) -> Option<SystemTime> {
        self.expiry
    }
}

/// The secret key and session token print redacted, so a logged or
/// panicking value never carries them.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &REDACTED)
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| REDACTED),
            )
            .field("expiry", &self.expiry)
            .finish()
    }
}

/// Why no credentials were loaded.
#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    /// The source is not configured on this host, so the chain tries the
    /// next one. The text says what was missing.
    #[error("the credential provider was not enabled: {0}")]
    NotLoaded(String),
    /// No source is configured on this host. Final: nothing is left to try.
    #[error(
        "the credential provider was not enabled: no source in the AWS credential chain is configured ({})",
        skipped(.0)
    )]
    NoSource(Vec<(Source, String)>),
    /// The chain did not answer in time.
    #[error("credentials provider timed out after {} seconds", .0.as_secs())]
    TimedOut(Duration),
    /// The profile is configured and gave no credentials.
    #[error("failed to load credentials from the profile")]
    Profile(#[source] profile::Error),
    /// The web identity token was not exchanged for credentials.
    #[error("failed to load credentials from web identity")]
    WebIdentity(#[source] web_identity::Error),
    /// The container endpoint failed.
    #[error("failed to load credentials from the container endpoint")]
    Container(#[source] container::Error),
    /// The instance metadata service failed.
    #[error("failed to load credentials from IMDS")]
    Imds(#[source] imds::Error),
}

impl From<profile::Error> for Error {
    /// A profile that is missing, holds no credential settings, lacks an
    /// SSO setting or names only a login token is not configured, as
    /// upstream; any other failure stops the chain.
    fn from(error: profile::Error) -> Self {
        match error {
            profile::Error::NoProfiles
            | profile::Error::NoCredentials(_)
            | profile::Error::MissingProfile { .. }
            | profile::Error::MissingField { .. }
            | profile::Error::TokenProfile => Self::NotLoaded(error.to_string()),
            error => Self::Profile(error),
        }
    }
}

/// The credential API `api` in `region`, with the endpoint and clock-skew
/// settings `config` holds and the `retry` settings of the calls the chain
/// makes.
fn credential_api(
    host: &Host,
    config: &Config,
    retry: &retry::Settings,
    api: CredentialApi,
    region: &Region,
) -> Result<Api, ConfigError> {
    let id = api.api_id();
    let url = config.endpoint_url(id.service_id)?;
    let endpoint =
        endpoint::resolve_credential_api(api, region, config.fips(), config.dual_stack(), url)?;
    let parts = ApiParts {
        region: region.clone(),
        endpoint,
        retry: retry::Strategy::new(retry),
        partition: retry::partition(id.service_id, region),
        skew_correction: config.skew_correction(),
        stall_timeout: STALL_TIMEOUT,
    };
    Api::new(&host.network, id, parts).map_err(ConfigError::HttpClient)
}

/// `error` and each of its sources, joined as upstream prints them, so a
/// reason kept as text still names its cause.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        text.push_str(": ");
        text.push_str(&e.to_string());
        source = e.source();
    }
    text
}

/// Each source and why it was not configured, as `source: reason; ...`.
fn skipped(reasons: &[(Source, String)]) -> String {
    reasons
        .iter()
        .map(|(source, reason)| format!("{source}: {reason}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The sources of the chain, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    /// `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
    Environment,
    /// The selected profile of the shared config and credentials files.
    Profile,
    /// `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, as IRSA sets them.
    WebIdentity,
    /// The ECS or EKS Pod Identity container endpoint.
    Container,
    /// The EC2 instance metadata service.
    Imds,
}

impl Source {
    /// The order the SDK tries the sources in.
    const ORDER: [Self; 5] = [
        Self::Environment,
        Self::Profile,
        Self::WebIdentity,
        Self::Container,
        Self::Imds,
    ];
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Environment => "environment",
            Self::Profile => "profile",
            Self::WebIdentity => "web identity",
            Self::Container => "container",
            Self::Imds => "instance metadata",
        })
    }
}

/// The default credential chain for one environment, region and profile.
pub(super) struct Chain {
    host: Host,
    /// The shared files and the selected profile.
    config: Config,
    /// The retry settings of the calls the chain makes.
    retry: retry::Settings,
    /// STS in the region of the calls the chain signs.
    sts: sts::Client,
    container: container::Provider,
    imds: imds::Provider,
}

impl Chain {
    /// The chain `host` and `config` configure, for calls in `region` with
    /// the `retry` settings the chain's calls share.
    pub(super) fn new(
        host: &Host,
        region: &Region,
        config: Config,
        retry: &retry::Settings,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            retry: *retry,
            sts: sts::Client::new(host, region, &config, retry),
            container: container::Provider::new(host)?,
            imds: imds::Provider::new(host, &config)?,
            host: host.clone(),
            config,
        })
    }

    /// Credentials from the first configured source.
    pub(super) async fn load(&self) -> Result<Credentials, Error> {
        self.load_in(&Source::ORDER).await
    }

    /// Credentials from the first configured source of `order`.
    async fn load_in(&self, order: &[Source]) -> Result<Credentials, Error> {
        let mut skipped = Vec::new();
        for &source in order {
            match self.load_from(source).await {
                Err(Error::NotLoaded(reason)) => {
                    tracing::debug!(%source, reason, "AWS credential source not configured");
                    skipped.push((source, reason));
                }
                Ok(credentials) => {
                    tracing::debug!(%source, "AWS credentials loaded");
                    return Ok(credentials);
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::NoSource(skipped))
    }

    async fn load_from(&self, source: Source) -> Result<Credentials, Error> {
        match source {
            Source::Environment => environment::load(&self.host.env),
            // Boxed, as its base may be another source of the chain.
            Source::Profile => Box::pin(self.load_profile()).await,
            Source::WebIdentity => web_identity::load(&self.host, &self.sts).await,
            Source::Container => self.container.load().await,
            // Its nested retries and timeouts make a future too large to
            // inline in every API call's frame.
            Source::Imds => Box::pin(self.imds.load()).await,
        }
    }

    /// Credentials from the selected profile: its base's, then each role's
    /// assumed with the credentials before it.
    async fn load_profile(&self) -> Result<Credentials, Error> {
        let sections = self
            .config
            .sections()
            .map_err(|e| profile::Error::Parse(e.clone()))?;
        let profile::Plan { base, roles } = profile::resolve(sections, self.config.profile_name())?;
        let failed = |error| profile::Error::Base(Box::new(error));
        let mut credentials = match base {
            profile::Base::Named(named) => self.load_from(named.source()).await.map_err(failed)?,
            profile::Base::Keys(keys) => keys,
            profile::Base::WebIdentity(role) => web_identity::assume(&self.host, &self.sts, &role)
                .await
                .map_err(|e| failed(Error::WebIdentity(e)))?,
            profile::Base::Process(command) => process::load(command)
                .await
                .map_err(profile::Error::Process)?,
            profile::Base::Sso(role) => sso::load(&self.host, &self.config, &self.retry, &role)
                .await
                .map_err(profile::Error::Sso)?,
        };
        for role in &roles {
            credentials = role
                .assume(&self.sts, &credentials, self.host.clock)
                .await?;
        }
        Ok(credentials)
    }
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain").finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The SDK documentation's example key, with `session_token` and
    /// `expiry`.
    pub(in crate::aws) fn key(
        session_token: Option<&str>,
        expiry: Option<SystemTime>,
    ) -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: session_token.map(str::to_owned),
            expiry,
        }
    }

    /// The chain of `host` with `profile` selected, for calls in
    /// `us-east-1`.
    fn chain_of(host: &Host, profile: Option<&str>) -> Chain {
        let config = Config::load(host, profile);
        let region = Region::parse("us-east-1").unwrap();
        Chain::new(host, &region, config, &retry::Settings::default()).unwrap()
    }

    /// A chain with `vars` set and a config file holding `config_file`,
    /// whose sources panic if they send a request.
    fn chain(vars: &[(&str, &str)], config_file: &str) -> Chain {
        chain_of(&super::super::config::tests::host(vars, config_file), None)
    }

    /// A source that is not configured is skipped for the next one. Breaks
    /// if a skip stops the chain.
    #[tokio::test]
    async fn skips_sources_that_are_not_configured() {
        let config_file = "[default]\naws_access_key_id = AKIDEXAMPLE\n\
            aws_secret_access_key = secret\naws_session_token = profile\n";
        let loaded = chain(&[], config_file).load().await.unwrap();
        assert_eq!(loaded.session_token(), Some("profile"));
    }

    /// The profile the client names beats `AWS_PROFILE`. Breaks if the
    /// profile source reads `AWS_PROFILE` itself.
    #[tokio::test]
    async fn loads_the_profile_the_client_names() {
        let config_file = "[profile env]\naws_access_key_id = ENV\naws_secret_access_key = s\n\
            [profile named]\naws_access_key_id = NAMED\naws_secret_access_key = s\n";
        let host = super::super::config::tests::host(&[("AWS_PROFILE", "env")], config_file);
        let loaded = chain_of(&host, Some("named")).load().await.unwrap();
        assert_eq!(loaded.access_key_id(), "NAMED");
    }

    /// Each role a profile chains to is assumed with the credentials of the
    /// role before it, not the base's keys. Breaks if a later role signs
    /// with an earlier key.
    #[tokio::test]
    async fn signs_each_role_with_the_one_before() {
        use wiremock::matchers::{body_string_contains, header, header_regex, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let answer = |id: &str| {
            ResponseTemplate::new(200).set_body_string(format!(
                "<AssumeRoleResponse><AssumeRoleResult><Credentials>\
                 <AccessKeyId>{id}</AccessKeyId><SecretAccessKey>secret</SecretAccessKey>\
                 <SessionToken>{id}-token</SessionToken>\
                 <Expiration>2100-01-01T00:00:00Z</Expiration>\
                 </Credentials></AssumeRoleResult></AssumeRoleResponse>"
            ))
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("role%2FFirst"))
            .and(body_string_contains(
                "&RoleSessionName=assume-role-from-profile-",
            ))
            .and(header_regex("authorization", "Credential=BASE/"))
            .respond_with(answer("FIRST"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("role%2FSecond"))
            .and(header_regex("authorization", "Credential=FIRST/"))
            .and(header("x-amz-security-token", "FIRST-token"))
            .respond_with(answer("SECOND"))
            .expect(1)
            .mount(&server)
            .await;
        let config_file = "[profile second]\nrole_arn = arn:aws:iam::1:role/Second\n\
            source_profile = first\n\
            [profile first]\nrole_arn = arn:aws:iam::1:role/First\nsource_profile = base\n\
            [profile base]\naws_access_key_id = BASE\naws_secret_access_key = secret\n";
        let mut host = super::super::config::tests::host(
            &[("AWS_ENDPOINT_URL_STS", &server.uri())],
            config_file,
        );
        host.network = super::super::host::Network::Real;
        let loaded = chain_of(&host, Some("second")).load().await.unwrap();
        assert_eq!(loaded.access_key_id(), "SECOND");
    }

    /// A configured source that fails stops the chain: web identity with a
    /// token file that does not read fails, and the container source after
    /// it sends no request, though its endpoint is set. Breaks if a failure
    /// is skipped like a source that is not configured.
    #[tokio::test]
    async fn stops_at_a_source_that_fails() {
        let vars = [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "/missing.jwt"),
            ("AWS_ROLE_ARN", "arn:aws:iam::123456789012:role/test-role"),
            ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/creds"),
        ];
        let err = chain(&vars, "").load().await.unwrap_err();
        assert!(matches!(err, Error::WebIdentity(_)), "{err}");
        assert_eq!(
            error_chain(&err),
            "failed to load credentials from web identity: \
             failed to read the web identity token file /missing.jwt: \
             No such file or directory (os error 2)"
        );
    }

    /// STS endpoint settings that do not resolve fail only a source that
    /// calls STS: keys in the environment still load. Breaks if the STS
    /// endpoint is resolved where the chain is built.
    #[tokio::test]
    async fn an_sts_endpoint_that_does_not_resolve_fails_only_web_identity() {
        let unresolved = [
            ("AWS_USE_FIPS_ENDPOINT", "true"),
            ("AWS_ENDPOINT_URL_STS", "https://sts.example"),
        ];
        let keys = [
            ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ];
        let loaded = chain(&[unresolved.as_slice(), &keys].concat(), "")
            .load()
            .await
            .unwrap();
        assert_eq!(loaded.access_key_id(), "AKIDEXAMPLE");

        let web_identity = [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "/home/.aws/config"),
            ("AWS_ROLE_ARN", "arn:aws:iam::123456789012:role/test-role"),
        ];
        let err = chain(&[unresolved.as_slice(), &web_identity].concat(), "")
            .load()
            .await
            .unwrap_err();
        assert_eq!(
            error_chain(&err),
            "failed to load credentials from web identity: \
             failed to assume the role with the web identity token: \
             the STS client could not be set up: \
             Invalid Configuration: FIPS and custom endpoint are not supported"
        );
    }

    /// With no source configured, the error is final and names each source
    /// with its reason, in the text upstream reports.
    #[tokio::test]
    async fn names_every_source_when_none_is_configured() {
        let err = chain(&[("AWS_EC2_METADATA_DISABLED", "TRUE")], "")
            .load()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSource(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "the credential provider was not enabled: no source in the AWS credential chain \
             is configured (environment: environment variable not set; \
             profile: No profiles were defined; web identity: $AWS_WEB_IDENTITY_TOKEN_FILE was not set; \
             container: No environment variables were set to configure ECS provider; \
             instance metadata: IMDS disabled by AWS_EC2_METADATA_DISABLED env var set to `true`)"
        );
    }

    /// The secret key and session token never reach `Debug` output, which
    /// logs and panics print. Breaks if either field prints as is.
    #[test]
    fn debug_redacts_the_secret_and_session_token() {
        let credentials = Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "the-secret".to_owned(),
            session_token: Some("the-token".to_owned()),
            expiry: None,
        };
        let printed = format!("{credentials:?}");
        assert!(printed.contains("AKIDEXAMPLE"), "{printed}");
        assert!(!printed.contains("the-secret"), "{printed}");
        assert!(!printed.contains("the-token"), "{printed}");
    }
}
