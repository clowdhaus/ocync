//! The SSO and `credential_process` profiles, loaded through the
//! `aws-config` profile provider.

use aws_config::provider_config::ProviderConfig;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::error::CredentialsError;

use super::super::config::Config;
use super::super::endpoint::{DualStack, Fips, Region};
use super::super::host::Env;
use super::{Credentials, Error};

/// The `aws-config` provider of the selected profile.
pub(super) enum Profile {
    /// This process's profile provider, configured as the SDK's default
    /// chain configures it.
    Sdk(Box<aws_config::profile::ProfileFileCredentialsProvider>),
    /// A test environment. `aws-config` reads the real environment, files
    /// and network, so a test that reaches it is a test bug.
    #[cfg(test)]
    Unreachable,
}

impl Profile {
    /// The provider for `env`, calling STS in `region` with the profile,
    /// FIPS and dual-stack settings `config` holds.
    pub(super) fn new(env: &Env, region: &Region, config: &Config) -> Self {
        match env {
            Env::Process => {
                let conf = ProviderConfig::default()
                    .with_region(Some(aws_config::Region::new(region.as_str().to_owned())))
                    .with_use_fips(Some(config.fips() == Fips::On))
                    .with_use_dual_stack(Some(config.dual_stack() == DualStack::On));
                Self::Sdk(Box::new(
                    aws_config::profile::ProfileFileCredentialsProvider::builder()
                        .configure(&conf)
                        .profile_name(config.profile_name())
                        .build(),
                ))
            }
            #[cfg(test)]
            Env::Fixed(_) => Self::Unreachable,
        }
    }

    /// Credentials from the profile through its `aws-config` provider.
    pub(super) async fn load(&self) -> Result<Credentials, Error> {
        let provider = match self {
            Self::Sdk(provider) => provider,
            #[cfg(test)]
            Self::Unreachable => panic!("a test reached the aws-config profile provider"),
        };
        match provider.provide_credentials().await {
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
