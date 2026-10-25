//! The AWS credential chain: where the key that signs each call comes from,
//! tried in the SDK's order.
//!
//! A source not configured on this host answers [`Error::NotLoaded`] and
//! the chain tries the next one. Any other error stops the chain: a source
//! that is configured and fails is reported, never skipped.

mod environment;
#[cfg(test)]
mod fixtures;
mod sdk;

use std::fmt;
use std::time::{Duration, SystemTime};

use super::config::Config;
use super::endpoint::Region;
use super::host::Env;

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
    /// A source loaded through `aws-config` failed.
    #[error(transparent)]
    Sdk(aws_credential_types::provider::error::CredentialsError),
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
    env: Env,
    /// The profile, web identity, container and instance metadata sources.
    sdk: sdk::Sources,
}

impl Chain {
    /// The chain `env` and `config` configure, for calls in `region`.
    pub(super) fn new(env: &Env, region: &Region, config: &Config) -> Self {
        Self {
            sdk: sdk::Sources::new(env, region, config),
            env: env.clone(),
        }
    }

    /// Credentials from the first configured source.
    pub(super) async fn load(&self) -> Result<Credentials, Error> {
        let mut skipped = Vec::new();
        for source in Source::ORDER {
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
            Source::Environment => environment::load(&self.env),
            Source::Profile => self.sdk.load(sdk::Slot::Profile).await,
            Source::WebIdentity => self.sdk.load(sdk::Slot::WebIdentity).await,
            Source::Container => self.sdk.load(sdk::Slot::Container).await,
            Source::Imds => self.sdk.load(sdk::Slot::Imds).await,
        }
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

    /// A chain with no environment key and `answer` for the other sources.
    fn chain(answer: fn(sdk::Slot) -> Result<Credentials, Error>) -> Chain {
        Chain {
            env: Env::Fixed(std::collections::HashMap::new()),
            sdk: sdk::Sources::Scripted(answer),
        }
    }

    fn not_loaded(slot: sdk::Slot) -> Error {
        Error::NotLoaded(format!("no {slot:?}"))
    }

    /// Sources that are not configured are skipped, in order, until one
    /// answers. Breaks if a skip stops the chain.
    #[tokio::test]
    async fn skips_sources_that_are_not_configured() {
        let loaded = chain(|slot| match slot {
            sdk::Slot::Imds => Ok(key(Some("imds"), None)),
            other => Err(not_loaded(other)),
        })
        .load()
        .await
        .unwrap();
        assert_eq!(loaded.session_token(), Some("imds"));
    }

    /// A configured source that fails stops the chain: the sources after it
    /// are never asked. Breaks if a failure is skipped like a source that is
    /// not configured.
    #[tokio::test]
    async fn stops_at_a_source_that_fails() {
        let err = chain(|slot| match slot {
            sdk::Slot::Profile | sdk::Slot::WebIdentity => Err(not_loaded(slot)),
            sdk::Slot::Container => Err(Error::TimedOut(Duration::from_secs(1))),
            sdk::Slot::Imds => panic!("the chain went past a failed source"),
        })
        .load()
        .await
        .unwrap_err();
        assert!(matches!(err, Error::TimedOut(_)), "{err}");
    }

    /// With no source configured, the error is final and names each source
    /// with its reason, in the text upstream reports.
    #[tokio::test]
    async fn names_every_source_when_none_is_configured() {
        let err = chain(|slot| Err(not_loaded(slot)))
            .load()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSource(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "the credential provider was not enabled: no source in the AWS credential chain \
             is configured (environment: environment variable not set; \
             profile: no Profile; web identity: no WebIdentity; \
             container: no Container; instance metadata: no Imds)"
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
