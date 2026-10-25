//! The settings the AWS SDKs read from environment variables and the shared
//! config file, resolved as `aws-config` 1.13.0 resolves them: an
//! environment variable beats the selected profile, and a service-specific
//! value beats a global one.

use super::endpoint::{DualStack, Fips};
use super::env::{Env, Fs, Platform};
use super::profile::{self, Location, OtherKey, ParseError, Properties, Sections};
use super::{ConfigError, SkewCorrection, retry};

/// The profile property naming the `[services name]` section that holds
/// service-specific settings.
const SERVICES: &str = "services";

/// The environment and shared files the AWS settings and credentials are
/// read from, with the selected profile.
#[derive(Debug)]
pub(super) struct Config {
    env: Env,
    fs: Fs,
    platform: Platform,
    sections: Sections,
    /// Why the shared files did not parse. Settings then come from the
    /// environment alone, and the profile credential source fails with it.
    parse_error: Option<ParseError>,
    profile: String,
}

impl Config {
    /// Reads the shared files. `profile` names the profile to use, else
    /// `AWS_PROFILE` does, else `default`.
    pub(super) fn load(env: Env, fs: Fs, platform: Platform, profile: Option<&str>) -> Self {
        let location = Location::find(&env, platform);
        let (sections, parse_error) = match profile::load(&env, &fs, &location) {
            Ok(sections) => (sections, None),
            Err(e) => {
                tracing::warn!(error = %e, "AWS config files ignored for settings");
                (Sections::default(), Some(e))
            }
        };
        Self {
            profile: profile.map_or(location.profile, str::to_owned),
            env,
            fs,
            platform,
            sections,
            parse_error,
        }
    }

    /// The environment.
    pub(super) fn env(&self) -> &Env {
        &self.env
    }

    /// The file system.
    pub(super) fn fs(&self) -> &Fs {
        &self.fs
    }

    /// The platform rules for the home directory.
    pub(super) fn platform(&self) -> Platform {
        self.platform
    }

    /// The parsed shared files, or why they did not parse.
    pub(super) fn sections(&self) -> Result<&Sections, &ParseError> {
        self.parse_error.as_ref().map_or(Ok(&self.sections), Err)
    }

    /// The selected profile's name.
    pub(super) fn profile_name(&self) -> &str {
        &self.profile
    }

    /// The selected profile's properties, if it exists.
    pub(super) fn profile(&self) -> Option<&Properties> {
        self.sections.profiles.get(&self.profile)
    }

    /// A setting: `<env_key>_<SERVICE_ID>`, then `env_key`, then
    /// `profile_key` under the service in the profile's services section,
    /// then `profile_key` in the profile.
    fn value(&self, env_key: &str, profile_key: &str, service_id: Option<&str>) -> Option<String> {
        let from_env = service_id
            .and_then(|id| {
                let id = id.to_uppercase().replace(' ', "_");
                self.env.get(&format!("{env_key}_{id}"))
            })
            .or_else(|| self.env.get(env_key));
        from_env.or_else(|| {
            let profile = self.profile()?;
            service_id
                .and_then(|id| {
                    let key = OtherKey {
                        section_key: SERVICES.to_owned(),
                        section_name: profile.get(SERVICES)?.clone(),
                        property: id.to_lowercase().replace(' ', "-"),
                        sub_property: profile_key.to_owned(),
                    };
                    self.sections.other.get(&key).cloned()
                })
                .or_else(|| profile.get(profile_key).cloned())
        })
    }

    /// A `true` or `false` setting, any case. Anything else is ignored with
    /// a warning, as the SDK does.
    fn flag(&self, env_key: &str, profile_key: &str) -> Option<bool> {
        let value = self.value(env_key, profile_key, None)?;
        if value.eq_ignore_ascii_case("true") {
            Some(true)
        } else if value.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            tracing::warn!(
                setting = env_key,
                value,
                "AWS setting ignored, not true or false"
            );
            None
        }
    }

    /// `AWS_USE_FIPS_ENDPOINT` or `use_fips_endpoint`.
    pub(super) fn fips(&self) -> Fips {
        match self.flag("AWS_USE_FIPS_ENDPOINT", "use_fips_endpoint") {
            Some(true) => Fips::On,
            _ => Fips::Off,
        }
    }

    /// `AWS_USE_DUALSTACK_ENDPOINT` or `use_dualstack_endpoint`.
    pub(super) fn dual_stack(&self) -> DualStack {
        match self.flag("AWS_USE_DUALSTACK_ENDPOINT", "use_dualstack_endpoint") {
            Some(true) => DualStack::On,
            _ => DualStack::Off,
        }
    }

    /// `AWS_DISABLE_CLOCK_SKEW_CORRECTION` or
    /// `disable_clock_skew_correction`.
    pub(super) fn skew_correction(&self) -> SkewCorrection {
        match self.flag(
            "AWS_DISABLE_CLOCK_SKEW_CORRECTION",
            "disable_clock_skew_correction",
        ) {
            Some(true) => SkewCorrection::Off,
            _ => SkewCorrection::On,
        }
    }

    /// The endpoint URL configured for the service `service_id`, unless
    /// `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` or
    /// `ignore_configured_endpoint_urls` is true.
    pub(super) fn endpoint_url(&self, service_id: &str) -> Option<String> {
        let ignore = self.flag(
            "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS",
            "ignore_configured_endpoint_urls",
        );
        if ignore == Some(true) {
            return None;
        }
        self.value("AWS_ENDPOINT_URL", "endpoint_url", Some(service_id))
    }

    /// `AWS_MAX_ATTEMPTS` or `max_attempts`, `AWS_RETRY_MODE` or
    /// `retry_mode`, and `AWS_NEW_RETRIES_2026`. An invalid attempt count
    /// or mode is an error.
    pub(super) fn retry(&self) -> Result<retry::Config, ConfigError> {
        let mut config = retry::Config::default();
        if let Some(text) = self.value("AWS_MAX_ATTEMPTS", "max_attempts", None) {
            config.max_attempts = match text.parse::<u32>() {
                Ok(n) if n > 0 => n,
                _ => return Err(ConfigError::InvalidMaxAttempts(text)),
            };
        }
        if let Some(text) = self.value("AWS_RETRY_MODE", "retry_mode", None) {
            config.mode = match text.trim() {
                m if m.eq_ignore_ascii_case("standard") => retry::Mode::Standard,
                m if m.eq_ignore_ascii_case("adaptive") => retry::Mode::Adaptive,
                _ => return Err(ConfigError::InvalidRetryMode(text)),
            };
        }
        if self
            .env
            .get("AWS_NEW_RETRIES_2026")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            config.version = retry::Version::V2_1;
        }
        Ok(config)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;

    /// A config read from `vars` and a config file holding `config_file`.
    pub(in crate::aws) fn config(vars: &[(&str, &str)], config_file: &str) -> Config {
        let mut env: HashMap<_, _> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        env.insert("HOME".to_owned(), "/home".to_owned());
        let fs = Fs::Fixed(HashMap::from([(
            PathBuf::from("/home/.aws/config"),
            config_file.as_bytes().to_vec(),
        )]));
        Config::load(Env::Fixed(env), fs, Platform::Unix, None)
    }

    const SERVICES_FILE: &str = "[default]\nendpoint_url = http://profile-global\nservices = local\n\
        [services local]\necr =\n  endpoint_url = http://profile-ecr\n\
        ecr_public =\n  endpoint_url = http://wrong-spelling\n\
        ecr-public =\n  endpoint_url = http://profile-ecr-public\n";

    /// The endpoint URL precedence `aws-config` applies: service env, global
    /// env, service profile, global profile; ECR Public spelled
    /// `ECR_PUBLIC` in the environment and `ecr-public` in the file; the
    /// ignore switch drops all of them. Breaks if any step is reordered or
    /// a spelling differs.
    #[test]
    fn endpoint_url_precedence() {
        type Case<'a> = (&'a [(&'a str, &'a str)], &'a str, Option<&'a str>);
        let cases: &[Case<'_>] = &[
            (
                &[
                    ("AWS_ENDPOINT_URL_ECR", "http://env-ecr"),
                    ("AWS_ENDPOINT_URL", "http://env-global"),
                ],
                "ECR",
                Some("http://env-ecr"),
            ),
            (
                &[("AWS_ENDPOINT_URL", "http://env-global")],
                "ECR",
                Some("http://env-global"),
            ),
            (&[], "ECR", Some("http://profile-ecr")),
            (&[], "ECR PUBLIC", Some("http://profile-ecr-public")),
            (
                &[("AWS_ENDPOINT_URL_ECR_PUBLIC", "http://env-ecr-public")],
                "ECR PUBLIC",
                Some("http://env-ecr-public"),
            ),
            (&[], "STS", Some("http://profile-global")),
            (
                &[
                    ("AWS_ENDPOINT_URL_ECR", "http://env-ecr"),
                    ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "TRUE"),
                ],
                "ECR",
                None,
            ),
        ];
        for (vars, service, expected) in cases {
            let config = config(vars, SERVICES_FILE);
            assert_eq!(
                config.endpoint_url(service).as_deref(),
                *expected,
                "{service} {vars:?}"
            );
        }
        let ignored_in_file = config(
            &[],
            &SERVICES_FILE.replace(
                "[services local]",
                "ignore_configured_endpoint_urls = true\n[services local]",
            ),
        );
        assert_eq!(ignored_in_file.endpoint_url("ECR"), None);
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
        assert_eq!(default.max_attempts, 3);
        assert_eq!(default.mode, retry::Mode::Standard);
        assert_eq!(default.version, retry::Version::V2_0);
        let file = "[default]\nmax_attempts = 7\nretry_mode = Adaptive\n";
        let from_file = config(&[], file).retry().unwrap();
        assert_eq!(from_file.max_attempts, 7);
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
        assert_eq!(from_env.max_attempts, 2);
        assert_eq!(from_env.mode, retry::Mode::Standard);
        assert_eq!(from_env.version, retry::Version::V2_1);
        let profile_2026 = config(&[], "[default]\naws_new_retries_2026 = true\n");
        assert_eq!(profile_2026.retry().unwrap().version, retry::Version::V2_0);

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

    /// A named profile replaces `AWS_PROFILE` for every setting. Breaks if
    /// the override is read only for credentials.
    #[test]
    fn named_profile_selects_settings() {
        let file =
            "[default]\nuse_fips_endpoint = true\n[profile other]\nuse_fips_endpoint = false\n";
        let mut env = HashMap::from([("HOME".to_owned(), "/home".to_owned())]);
        env.insert("AWS_PROFILE".to_owned(), "default".to_owned());
        let fs = Fs::Fixed(HashMap::from([(
            PathBuf::from("/home/.aws/config"),
            file.as_bytes().to_vec(),
        )]));
        let named = Config::load(Env::Fixed(env), fs, Platform::Unix, Some("other"));
        assert_eq!(named.profile_name(), "other");
        assert_eq!(named.fips(), Fips::Off);
    }

    /// A config file that does not parse leaves the environment's settings
    /// in force and keeps the error for the profile credential source.
    #[test]
    fn unparsable_file_keeps_env_settings() {
        let config = config(&[("AWS_USE_FIPS_ENDPOINT", "true")], "[profile broken");
        assert_eq!(config.fips(), Fips::On);
        assert!(config.sections().is_err());
    }
}
