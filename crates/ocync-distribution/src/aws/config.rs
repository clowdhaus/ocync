//! The settings the AWS SDKs read from environment variables and the shared
//! config file, resolved as `aws-config` 1.13.0 resolves them: an
//! environment variable beats the selected profile, and a service-specific
//! value beats a global one.

use std::num::NonZeroU32;

use super::endpoint::{DualStack, Endpoint, Fips};
use super::host::{Env, Host};
use super::profile::{self, Location, ParseError, Properties, Sections};
use super::{ConfigError, SkewCorrection, retry};

/// The profile property naming the `[services name]` section that holds
/// service-specific settings.
const SERVICES: &str = "services";

/// A setting's environment variable and profile property.
#[derive(Debug, Clone, Copy)]
struct Setting {
    env: &'static str,
    profile: &'static str,
}

/// The variables naming the region, in the SDK's order.
const REGION_VARIABLES: [&str; 2] = ["AWS_REGION", "AWS_DEFAULT_REGION"];

/// The profile property naming the region.
const REGION_PROPERTY: &str = "region";

const USE_FIPS: Setting = Setting {
    env: "AWS_USE_FIPS_ENDPOINT",
    profile: "use_fips_endpoint",
};

const USE_DUAL_STACK: Setting = Setting {
    env: "AWS_USE_DUALSTACK_ENDPOINT",
    profile: "use_dualstack_endpoint",
};

const DISABLE_SKEW_CORRECTION: Setting = Setting {
    env: "AWS_DISABLE_CLOCK_SKEW_CORRECTION",
    profile: "disable_clock_skew_correction",
};

const IGNORE_ENDPOINT_URLS: Setting = Setting {
    env: "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS",
    profile: "ignore_configured_endpoint_urls",
};

const ENDPOINT_URL: Setting = Setting {
    env: "AWS_ENDPOINT_URL",
    profile: "endpoint_url",
};

const MAX_ATTEMPTS: Setting = Setting {
    env: "AWS_MAX_ATTEMPTS",
    profile: "max_attempts",
};

const RETRY_MODE: Setting = Setting {
    env: "AWS_RETRY_MODE",
    profile: "retry_mode",
};

/// Selects retry behavior 2.1. Read only from the environment.
const NEW_RETRIES_2026: &str = "AWS_NEW_RETRIES_2026";

const IMDS_ENDPOINT: Setting = Setting {
    env: "AWS_EC2_METADATA_SERVICE_ENDPOINT",
    profile: "ec2_metadata_service_endpoint",
};

const IMDS_ENDPOINT_MODE: Setting = Setting {
    env: "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE",
    profile: "ec2_metadata_service_endpoint_mode",
};

/// The instance metadata service over IPv4.
const IMDS_IPV4: &str = "http://169.254.169.254";

/// The instance metadata service over IPv6.
const IMDS_IPV6: &str = "http://[fd00:ec2::254]";

/// Which address the instance metadata service is reached at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImdsMode {
    IpV4,
    IpV6,
}

impl std::str::FromStr for ImdsMode {
    type Err = ImdsEndpointError;

    /// `IPv4` or `IPv6`, any case.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.eq_ignore_ascii_case("ipv4") {
            Ok(Self::IpV4)
        } else if text.eq_ignore_ascii_case("ipv6") {
            Ok(Self::IpV6)
        } else {
            Err(ImdsEndpointError::Mode(text.to_owned()))
        }
    }
}

/// Why the instance metadata endpoint settings do not resolve.
#[derive(Debug, Clone, thiserror::Error)]
pub(super) enum ImdsEndpointError {
    /// The endpoint mode is not `IPv4` or `IPv6`.
    #[error("`{0}` is not a valid endpoint mode")]
    Mode(String),
    /// The endpoint is not a URL.
    #[error("invalid IMDS endpoint URI: {0}")]
    Uri(#[source] url::ParseError),
}

/// A service's name in its endpoint URL settings, as each source spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ServiceId {
    /// The suffix of `AWS_ENDPOINT_URL_<SERVICE>`, such as `ECR_PUBLIC`.
    env: &'static str,
    /// The property in a services section, such as `ecr-public`.
    profile: &'static str,
}

impl ServiceId {
    /// Amazon ECR.
    pub(super) const ECR: Self = Self {
        env: "ECR",
        profile: "ecr",
    };
    /// Amazon ECR Public.
    pub(super) const ECR_PUBLIC: Self = Self {
        env: "ECR_PUBLIC",
        profile: "ecr-public",
    };
    /// AWS STS.
    pub(super) const STS: Self = Self {
        env: "STS",
        profile: "sts",
    };
    /// The IAM Identity Center portal.
    pub(super) const SSO: Self = Self {
        env: "SSO",
        profile: "sso",
    };
    /// IAM Identity Center OIDC.
    pub(super) const SSO_OIDC: Self = Self {
        env: "SSO_OIDC",
        profile: "sso-oidc",
    };
}

/// The environment and shared files the AWS settings are read from, with
/// the selected profile.
#[derive(Debug)]
pub(super) struct Config {
    env: Env,
    sections: Result<Sections, ParseError>,
    profile: String,
}

impl Config {
    /// Reads the shared files. `profile` names the profile to use, else
    /// `AWS_PROFILE` does, else `default`. Files that do not parse leave
    /// the environment's settings in force, with a warning.
    pub(super) fn load(host: &Host, profile: Option<&str>) -> Self {
        let location = Location::find(&host.env, host.platform);
        let sections = profile::load(&host.fs, &location);
        if let Err(e) = &sections {
            tracing::warn!(error = %e, "AWS config files ignored for settings");
        }
        Self {
            profile: profile.map_or(location.profile, str::to_owned),
            env: host.env.clone(),
            sections,
        }
    }

    /// The region `AWS_REGION`, `AWS_DEFAULT_REGION` or the selected
    /// profile's `region` names, in the SDK's order.
    pub(super) fn region(&self) -> Option<String> {
        REGION_VARIABLES
            .iter()
            .find_map(|name| self.env.get(name))
            .or_else(|| self.profile()?.get(REGION_PROPERTY).cloned())
    }

    /// The selected profile's name.
    pub(super) fn profile_name(&self) -> &str {
        &self.profile
    }

    /// What the shared files hold, or why they do not parse.
    pub(super) fn sections(&self) -> Result<&Sections, &ParseError> {
        self.sections.as_ref()
    }

    /// The selected profile's properties, if it exists.
    fn profile(&self) -> Option<&Properties> {
        self.sections.as_ref().ok()?.profiles.get(&self.profile)
    }

    /// `setting`: for `service`, `<env>_<SERVICE>` first and the profile's
    /// services section after the environment; then the global values.
    fn value(&self, setting: Setting, service: Option<ServiceId>) -> Option<String> {
        let from_env = service
            .and_then(|id| self.env.get(&format!("{}_{}", setting.env, id.env)))
            .or_else(|| self.env.get(setting.env));
        from_env.or_else(|| {
            let profile = self.profile()?;
            service
                .and_then(|id| {
                    let sections = self.sections.as_ref().ok()?;
                    let section = sections.services.get(profile.get(SERVICES)?)?;
                    section.get(id.profile)?.get(setting.profile).cloned()
                })
                .or_else(|| profile.get(setting.profile).cloned())
        })
    }

    /// A `true` or `false` setting.
    fn flag(&self, setting: Setting) -> Option<bool> {
        parse_flag(setting.env, &self.value(setting, None)?)
    }

    /// `AWS_USE_FIPS_ENDPOINT` or `use_fips_endpoint`.
    pub(super) fn fips(&self) -> Fips {
        match self.flag(USE_FIPS) {
            Some(true) => Fips::On,
            _ => Fips::Off,
        }
    }

    /// `AWS_USE_DUALSTACK_ENDPOINT` or `use_dualstack_endpoint`.
    pub(super) fn dual_stack(&self) -> DualStack {
        match self.flag(USE_DUAL_STACK) {
            Some(true) => DualStack::On,
            _ => DualStack::Off,
        }
    }

    /// `AWS_DISABLE_CLOCK_SKEW_CORRECTION` or
    /// `disable_clock_skew_correction`.
    pub(super) fn skew_correction(&self) -> SkewCorrection {
        match self.flag(DISABLE_SKEW_CORRECTION) {
            Some(true) => SkewCorrection::Off,
            _ => SkewCorrection::On,
        }
    }

    /// The endpoint URL configured for `service`, unless
    /// `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` or
    /// `ignore_configured_endpoint_urls` is true.
    pub(super) fn endpoint_url(&self, service: ServiceId) -> Result<Option<Endpoint>, ConfigError> {
        if self.flag(IGNORE_ENDPOINT_URLS) == Some(true) {
            return Ok(None);
        }
        self.value(ENDPOINT_URL, Some(service))
            .map(|url| Endpoint::parse(&url))
            .transpose()
    }

    /// The instance metadata service's base URL: `AWS_EC2_METADATA_SERVICE_ENDPOINT`
    /// or `ec2_metadata_service_endpoint`, else the address the endpoint
    /// mode (`AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE` or
    /// `ec2_metadata_service_endpoint_mode`) names, IPv4 by default.
    pub(super) fn imds_endpoint(&self) -> Result<url::Url, ImdsEndpointError> {
        let url = match self.value(IMDS_ENDPOINT, None) {
            Some(url) => url::Url::parse(&url),
            None => {
                let mode = self.value(IMDS_ENDPOINT_MODE, None);
                match mode.map(|m| m.parse()).transpose()? {
                    Some(ImdsMode::IpV6) => url::Url::parse(IMDS_IPV6),
                    Some(ImdsMode::IpV4) | None => url::Url::parse(IMDS_IPV4),
                }
            }
        };
        url.map_err(ImdsEndpointError::Uri)
    }

    /// `AWS_MAX_ATTEMPTS` or `max_attempts`, `AWS_RETRY_MODE` or
    /// `retry_mode`, and `AWS_NEW_RETRIES_2026`. An invalid attempt count
    /// or mode is an error.
    pub(super) fn retry(&self) -> Result<retry::Settings, ConfigError> {
        let mut config = retry::Settings::default();
        if let Some(text) = self.value(MAX_ATTEMPTS, None) {
            config.max_attempts = text
                .parse::<NonZeroU32>()
                .map_err(|source| ConfigError::InvalidMaxAttempts { text, source })?;
        }
        if let Some(text) = self.value(RETRY_MODE, None) {
            config.mode = text.parse()?;
        }
        let new_retries = self.env.get(NEW_RETRIES_2026);
        if new_retries.and_then(|v| parse_flag(NEW_RETRIES_2026, &v)) == Some(true) {
            config.version = retry::Version::V2_1;
        }
        Ok(config)
    }
}

/// `true` or `false`, any case, for the setting named `name`. Anything else
/// is ignored with a warning, as the SDK does.
fn parse_flag(name: &'static str, value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        tracing::warn!(
            setting = name,
            value,
            "AWS setting ignored, not true or false"
        );
        None
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::super::host::Fs;
    use super::*;

    /// A Unix host with `vars` set and a config file holding `config_file`,
    /// on the system clock, that sends no request: any request panics.
    pub(in crate::aws) fn host(vars: &[(&str, &str)], config_file: &str) -> Host {
        let mut env: HashMap<_, _> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        env.insert("HOME".to_owned(), "/home".to_owned());
        Host {
            env: Env::Fixed(env),
            fs: Fs::Fixed(HashMap::from([(
                PathBuf::from("/home/.aws/config"),
                config_file.as_bytes().to_vec(),
            )])),
            ..Host::fixed()
        }
    }

    /// The config of [`host`] with no named profile.
    fn config(vars: &[(&str, &str)], config_file: &str) -> Config {
        Config::load(&host(vars, config_file), None)
    }

    const SERVICES_FILE: &str = "[default]\nendpoint_url = http://profile-global\nservices = local\n\
        [services local]\necr =\n  endpoint_url = http://profile-ecr\n\
        ecr_public =\n  endpoint_url = http://wrong-spelling\n\
        ecr-public =\n  endpoint_url = http://profile-ecr-public\n\
        sts =\n  endpoint_url = http://profile-sts\n";

    /// A service with no setting of its own, so only global values apply.
    const OTHER: ServiceId = ServiceId {
        env: "OTHER",
        profile: "other",
    };

    /// The endpoint URL precedence `aws-config` applies: service env, global
    /// env, service profile, global profile; ECR Public spelled
    /// `ECR_PUBLIC` in the environment and `ecr-public` in the file, STS
    /// `STS` and `sts`; the ignore switch drops all of them. Breaks if any step is reordered or
    /// a spelling differs.
    #[test]
    fn endpoint_url_precedence() {
        type Case<'a> = (&'a [(&'a str, &'a str)], ServiceId, Option<&'a str>);
        let cases: &[Case<'_>] = &[
            (
                &[
                    ("AWS_ENDPOINT_URL_ECR", "http://env-ecr"),
                    ("AWS_ENDPOINT_URL", "http://env-global"),
                ],
                ServiceId::ECR,
                Some("http://env-ecr"),
            ),
            (
                &[("AWS_ENDPOINT_URL", "http://env-global")],
                ServiceId::ECR,
                Some("http://env-global"),
            ),
            (&[], ServiceId::ECR, Some("http://profile-ecr")),
            (
                &[],
                ServiceId::ECR_PUBLIC,
                Some("http://profile-ecr-public"),
            ),
            (
                &[("AWS_ENDPOINT_URL_ECR_PUBLIC", "http://env-ecr-public")],
                ServiceId::ECR_PUBLIC,
                Some("http://env-ecr-public"),
            ),
            (&[], ServiceId::STS, Some("http://profile-sts")),
            (
                &[("AWS_ENDPOINT_URL_STS", "http://env-sts")],
                ServiceId::STS,
                Some("http://env-sts"),
            ),
            (&[], OTHER, Some("http://profile-global")),
            (
                &[
                    ("AWS_ENDPOINT_URL_ECR", "http://env-ecr"),
                    ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "TRUE"),
                ],
                ServiceId::ECR,
                None,
            ),
        ];
        let url = |config: &Config, service| {
            let endpoint = config.endpoint_url(service).unwrap()?;
            Some(endpoint.url().as_str().trim_end_matches('/').to_owned())
        };
        for (vars, service, expected) in cases {
            let config = config(vars, SERVICES_FILE);
            assert_eq!(
                url(&config, *service).as_deref(),
                *expected,
                "{service:?} {vars:?}"
            );
        }
        let ignored_in_file = config(
            &[],
            &SERVICES_FILE.replace(
                "[services local]",
                "ignore_configured_endpoint_urls = true\n[services local]",
            ),
        );
        assert_eq!(url(&ignored_in_file, ServiceId::ECR), None);
    }

    /// The switches read `true` and `false` in any case from either source,
    /// the environment first, and ignore anything else. Breaks if an
    /// invalid value turns a switch on or the profile beats the environment.
    #[test]
    fn switches() {
        let file = "[default]\nuse_fips_endpoint = true\nuse_dualstack_endpoint = yes\n";
        let from_file = config(&[], file);
        assert_eq!(from_file.fips(), Fips::On);
        assert_eq!(from_file.dual_stack(), DualStack::Off);
        assert_eq!(from_file.skew_correction(), SkewCorrection::On);
        let from_env = config(
            &[
                ("AWS_USE_FIPS_ENDPOINT", "False"),
                ("AWS_USE_DUALSTACK_ENDPOINT", "TRUE"),
                ("AWS_DISABLE_CLOCK_SKEW_CORRECTION", "true"),
            ],
            file,
        );
        assert_eq!(from_env.fips(), Fips::Off);
        assert_eq!(from_env.dual_stack(), DualStack::On);
        assert_eq!(from_env.skew_correction(), SkewCorrection::Off);
    }

    /// Attempts, mode and the 2026 retry behavior come from the environment
    /// or the profile; the 2026 switch only from the environment. An
    /// invalid attempt count or mode is an error, where the SDK panics.
    #[test]
    fn retry_settings() {
        let default = config(&[], "").retry().unwrap();
        assert_eq!(default.max_attempts.get(), 3);
        assert_eq!(default.mode, retry::Mode::Standard);
        assert_eq!(default.version, retry::Version::V2_0);
        let file = "[default]\nmax_attempts = 7\nretry_mode = Adaptive\n";
        let from_file = config(&[], file).retry().unwrap();
        assert_eq!(from_file.max_attempts.get(), 7);
        assert_eq!(from_file.mode, retry::Mode::Adaptive);
        assert_eq!(from_file.version, retry::Version::V2_0);
        let from_env = config(
            &[
                ("AWS_MAX_ATTEMPTS", "2"),
                ("AWS_RETRY_MODE", " standard "),
                ("AWS_NEW_RETRIES_2026", "True"),
            ],
            file,
        )
        .retry()
        .unwrap();
        assert_eq!(from_env.max_attempts.get(), 2);
        assert_eq!(from_env.mode, retry::Mode::Standard);
        assert_eq!(from_env.version, retry::Version::V2_1);
        let profile_2026 = config(&[], "[default]\naws_new_retries_2026 = true\n");
        assert_eq!(profile_2026.retry().unwrap().version, retry::Version::V2_0);
        let not_a_flag = config(&[("AWS_NEW_RETRIES_2026", "yes")], "");
        assert_eq!(not_a_flag.retry().unwrap().version, retry::Version::V2_0);

        for (vars, error) in [
            (("AWS_MAX_ATTEMPTS", "0"), "invalid AWS max attempts '0'"),
            (
                ("AWS_MAX_ATTEMPTS", "many"),
                "invalid AWS max attempts 'many'",
            ),
            (
                ("AWS_RETRY_MODE", "legacy"),
                "invalid AWS retry mode 'legacy'",
            ),
        ] {
            let err = config(&[vars], "").retry().unwrap_err().to_string();
            assert!(err.starts_with(error), "{err}");
        }
    }

    /// A config file that does not parse leaves the environment's settings
    /// in force. Breaks if a parse error discards them too.
    #[test]
    fn unparsable_file_keeps_env_settings() {
        let config = config(&[("AWS_USE_FIPS_ENDPOINT", "true")], "[profile broken");
        assert_eq!(config.fips(), Fips::On);
    }
}
