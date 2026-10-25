//! The profile, web identity, container and instance metadata sources,
//! each loaded through its `aws-config` provider.

use aws_config::provider_config::ProviderConfig;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::error::CredentialsError;

use super::super::config::Config;
use super::super::endpoint::{DualStack, Fips, Region};
use super::super::host::Env;
use super::{Credentials, Error};

/// A source loaded through `aws-config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    /// The selected profile.
    Profile,
    /// Web identity from the environment.
    WebIdentity,
    /// The container endpoint.
    Container,
    /// The instance metadata service.
    Imds,
}

/// The `aws-config` providers of the slots.
pub(super) enum Sources {
    /// This process's sources, configured as the SDK's default chain
    /// configures them.
    Sdk(Box<Providers>),
    /// A test environment. `aws-config` reads the real environment, files
    /// and network, so a test that reaches a slot is a test bug.
    #[cfg(test)]
    Unreachable,
    /// A test's answer for each slot.
    #[cfg(test)]
    Scripted(fn(Slot) -> Result<Credentials, Error>),
}

/// One provider per slot.
pub(super) struct Providers {
    profile: aws_config::profile::ProfileFileCredentialsProvider,
    web_identity: aws_config::web_identity_token::WebIdentityTokenCredentialsProvider,
    container: aws_config::ecs::EcsCredentialsProvider,
    imds: aws_config::imds::credentials::ImdsCredentialsProvider,
}

impl Sources {
    /// The providers for `env`, calling STS in `region` with the profile,
    /// FIPS and dual-stack settings `config` holds.
    pub(super) fn new(env: &Env, region: &Region, config: &Config) -> Self {
        match env {
            Env::Process => {
                let conf = ProviderConfig::default()
                    .with_region(Some(aws_config::Region::new(region.as_str().to_owned())))
                    .with_use_fips(Some(config.fips() == Fips::On))
                    .with_use_dual_stack(Some(config.dual_stack() == DualStack::On));
                Self::Sdk(Box::new(Providers {
                    profile: aws_config::profile::ProfileFileCredentialsProvider::builder()
                        .configure(&conf)
                        .profile_name(config.profile_name())
                        .build(),
                    web_identity:
                        aws_config::web_identity_token::WebIdentityTokenCredentialsProvider::builder()
                            .configure(&conf)
                            .build(),
                    container: aws_config::ecs::EcsCredentialsProvider::builder()
                        .configure(&conf)
                        .build(),
                    imds: aws_config::imds::credentials::ImdsCredentialsProvider::builder()
                        .configure(&conf)
                        .build(),
                }))
            }
            #[cfg(test)]
            Env::Fixed(_) => Self::Unreachable,
        }
    }

    /// Credentials from `slot` through its `aws-config` provider.
    pub(super) async fn load(&self, slot: Slot) -> Result<Credentials, Error> {
        let providers = match self {
            Self::Sdk(providers) => providers,
            #[cfg(test)]
            Self::Unreachable => panic!("a test reached the {slot:?} source"),
            #[cfg(test)]
            Self::Scripted(answer) => return answer(slot),
        };
        let loaded = match slot {
            Slot::Profile => providers.profile.provide_credentials().await,
            Slot::WebIdentity => providers.web_identity.provide_credentials().await,
            Slot::Container => providers.container.provide_credentials().await,
            Slot::Imds => providers.imds.provide_credentials().await,
        };
        match loaded {
            Ok(credentials) => Ok(Credentials {
                access_key_id: credentials.access_key_id().to_owned(),
                secret_access_key: credentials.secret_access_key().to_owned(),
                session_token: credentials.session_token().map(str::to_owned),
                expiry: credentials.expiry(),
            }),
            Err(e @ CredentialsError::CredentialsNotLoaded(_)) => Err(Error::NotLoaded(
                aws_smithy_types::error::display::DisplayErrorContext(&e).to_string(),
            )),
            Err(e) => Err(Error::Sdk(e)),
        }
    }
}
