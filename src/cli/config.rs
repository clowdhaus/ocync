// Config types define the YAML schema; dead_code is expected until CLI integration.
#![allow(dead_code)]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("config parse error: {0}")]
    Parse(String),

    #[error("required environment variable not set: {0}")]
    EnvVarRequired(String),

    #[error("blocked environment variable in non-auth field: {0}")]
    BlockedEnvVar(String),

    #[error("config validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub registries: HashMap<String, RegistryConfig>,

    #[serde(default)]
    pub target_groups: HashMap<String, Vec<String>>,

    #[serde(default)]
    pub defaults: Option<DefaultsConfig>,

    pub mappings: Vec<MappingConfig>,

    #[serde(default)]
    pub global: Option<GlobalConfig>,

    #[serde(default)]
    pub log_format: Option<LogFormat>,

    #[serde(default)]
    pub log_level: Option<LogLevel>,

    #[serde(default)]
    pub env_var_policy: Option<EnvVarPolicy>,
}

// ---------------------------------------------------------------------------
// Global
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GlobalConfig {
    #[serde(default = "default_max_concurrent_transfers")]
    pub max_concurrent_transfers: u32,
}

fn default_max_concurrent_transfers() -> u32 {
    50
}

// ---------------------------------------------------------------------------
// Enums for strongly-typed config fields
// ---------------------------------------------------------------------------

/// Authentication method for a registry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthType {
    /// AWS ECR token exchange.
    Ecr,
    /// Google Cloud artifact/container registry.
    Gcr,
    /// Azure Container Registry.
    Acr,
    /// GitHub Container Registry (`GITHUB_TOKEN`).
    Ghcr,
    /// Anonymous (token exchange only).
    Anonymous,
    /// HTTP basic auth.
    Basic,
    /// Bearer token.
    Token,
    /// Docker config.json credential store.
    DockerConfig,
}

/// OCI manifest format preference.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ManifestFormat {
    /// OCI image manifest.
    Oci,
    /// Docker manifest v2 schema 2.
    Docker,
    /// Auto-detect from source.
    Auto,
}

/// Structured log output format.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    /// Human-readable text.
    Text,
    /// Machine-readable JSON.
    Json,
}

/// Log verbosity level.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    /// Errors only.
    Error,
    /// Errors and warnings.
    Warn,
    /// Default verbosity.
    Info,
    /// Verbose debugging.
    Debug,
    /// Maximum verbosity.
    Trace,
}

/// ECR image tag mutability setting.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TagMutability {
    /// Tags can be overwritten.
    Mutable,
    /// Tags are write-once.
    Immutable,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RegistryConfig {
    pub url: String,

    #[serde(default)]
    pub auth_type: Option<AuthType>,

    #[serde(default)]
    pub aws_role_arn: Option<String>,

    #[serde(default)]
    pub aws_external_id: Option<String>,

    #[serde(default)]
    pub credentials: Option<CredentialsConfig>,

    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,

    #[serde(default)]
    pub max_concurrent: Option<u32>,

    #[serde(default)]
    pub chunk_size: Option<String>,

    #[serde(default)]
    pub manifest_format: Option<ManifestFormat>,

    #[serde(default)]
    pub ecr: Option<EcrConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RateLimitConfig {
    #[serde(default)]
    pub pull: Option<u32>,

    #[serde(default)]
    pub push: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CredentialsConfig {
    #[serde(default)]
    pub token: Option<String>,

    #[serde(default)]
    pub username: Option<String>,

    #[serde(default)]
    pub password: Option<String>,

    #[serde(default)]
    pub token_file: Option<String>,
}

// ---------------------------------------------------------------------------
// ECR
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EcrConfig {
    #[serde(default)]
    pub auto_create: bool,

    #[serde(default)]
    pub defaults: Option<EcrDefaults>,

    #[serde(default)]
    pub overrides: Vec<EcrOverride>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EcrDefaults {
    #[serde(default)]
    pub image_tag_mutability: Option<TagMutability>,

    #[serde(default)]
    pub image_scanning_on_push: Option<bool>,

    #[serde(default)]
    pub repository_policy_file: Option<String>,

    #[serde(default)]
    pub lifecycle_policy_file: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EcrOverride {
    /// Named `match_pattern` to avoid the `match` keyword.
    #[serde(rename = "match")]
    pub match_pattern: String,

    #[serde(default)]
    pub image_tag_mutability: Option<TagMutability>,

    #[serde(default)]
    pub lifecycle_policy_file: Option<String>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DefaultsConfig {
    #[serde(default)]
    pub source: Option<String>,

    #[serde(default)]
    pub targets: Option<TargetsValue>,

    #[serde(default)]
    pub tags: Option<TagsConfig>,

    #[serde(default)]
    pub artifacts: Option<ArtifactsConfig>,

    #[serde(default)]
    pub recompress: Option<bool>,

    #[serde(default)]
    pub recompress_level: Option<u32>,
}

// ---------------------------------------------------------------------------
// Mappings
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct MappingConfig {
    pub from: String,

    #[serde(default)]
    pub to: Option<String>,

    #[serde(default)]
    pub source: Option<String>,

    #[serde(default)]
    pub targets: Option<TargetsValue>,

    #[serde(default)]
    pub tags: Option<TagsConfig>,

    #[serde(default)]
    pub platforms: Option<Vec<String>>,

    #[serde(default)]
    pub skip_existing: Option<bool>,

    #[serde(default)]
    pub artifacts: Option<ArtifactsConfig>,

    #[serde(default)]
    pub recompress: Option<bool>,

    #[serde(default)]
    pub recompress_level: Option<u32>,

    #[serde(default)]
    pub bulk: Option<BulkConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct BulkConfig {
    pub from_prefix: String,
    pub to_prefix: String,
    pub names: Vec<String>,
}

// ---------------------------------------------------------------------------
// TargetsValue — single group name or inline list
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum TargetsValue {
    Group(String),
    List(Vec<String>),
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct TagsConfig {
    #[serde(default)]
    pub glob: Option<GlobOrList>,

    #[serde(default)]
    pub semver: Option<String>,

    #[serde(default)]
    pub semver_prerelease: Option<SemverPrerelease>,

    #[serde(default)]
    pub exclude: Option<GlobOrList>,

    #[serde(default)]
    pub sort: Option<SortOrder>,

    #[serde(default)]
    pub latest: Option<usize>,

    #[serde(default)]
    pub min_tags: Option<usize>,

    #[serde(default)]
    pub immutable_tags: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum GlobOrList {
    Single(String),
    List(Vec<String>),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SortOrder {
    Semver,
    Alpha,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SemverPrerelease {
    Include,
    Exclude,
    Only,
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ArtifactsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub include: Option<Vec<String>>,

    #[serde(default)]
    pub exclude: Option<Vec<String>>,

    #[serde(default)]
    pub require_artifacts: Option<bool>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// EnvVar policy
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EnvVarPolicy {
    #[serde(default)]
    pub allow: Vec<String>,
}

// ---------------------------------------------------------------------------
// Environment variable expansion
// ---------------------------------------------------------------------------

const BLOCKED_PATTERNS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "TOKEN",
    "PRIVATE_KEY",
    "CREDENTIAL",
    "API_KEY",
];

/// Check whether a variable name matches a blocked pattern.
fn is_blocked(var_name: &str, allow_list: &[String]) -> bool {
    if allow_list.iter().any(|a| a == var_name) {
        return false;
    }
    let upper = var_name.to_uppercase();
    BLOCKED_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

/// Expand `${VAR}`, `${VAR:-default}`, and `${VAR:?error}` expressions.
pub(crate) fn expand_env_vars(
    input: &str,
    allow_list: &[String],
    is_auth_field: bool,
) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            // consume '{'
            chars.next();
            // read until '}'
            let mut expr = String::new();
            let mut found_close = false;
            for c in chars.by_ref() {
                if c == '}' {
                    found_close = true;
                    break;
                }
                expr.push(c);
            }
            if !found_close {
                // malformed — push literal
                result.push_str("${");
                result.push_str(&expr);
                continue;
            }

            // Parse name, operator, argument
            if let Some(pos) = expr.find(":-") {
                let var_name = &expr[..pos];
                let default_val = &expr[pos + 2..];
                if !is_auth_field && is_blocked(var_name, allow_list) {
                    return Err(ConfigError::BlockedEnvVar(var_name.to_string()));
                }
                match std::env::var(var_name) {
                    Ok(val) if !val.is_empty() => result.push_str(&val),
                    _ => result.push_str(default_val),
                }
            } else if let Some(pos) = expr.find(":?") {
                let var_name = &expr[..pos];
                let err_msg = &expr[pos + 2..];
                if !is_auth_field && is_blocked(var_name, allow_list) {
                    return Err(ConfigError::BlockedEnvVar(var_name.to_string()));
                }
                match std::env::var(var_name) {
                    Ok(val) if !val.is_empty() => result.push_str(&val),
                    _ => {
                        return Err(ConfigError::EnvVarRequired(if err_msg.is_empty() {
                            var_name.to_string()
                        } else {
                            err_msg.to_string()
                        }));
                    }
                }
            } else {
                let var_name = &expr;
                if !is_auth_field && is_blocked(var_name, allow_list) {
                    return Err(ConfigError::BlockedEnvVar(var_name.to_string()));
                }
                if let Ok(val) = std::env::var(var_name) {
                    result.push_str(&val);
                }
            }
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

pub(crate) fn validate_tags(tags: &TagsConfig) -> Result<(), ConfigError> {
    if tags.latest.is_some() && tags.sort.is_none() {
        return Err(ConfigError::Validation(
            "tags.latest requires tags.sort to be set".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_artifacts(artifacts: &ArtifactsConfig) -> Result<(), ConfigError> {
    if !artifacts.enabled && artifacts.require_artifacts == Some(true) {
        return Err(ConfigError::Validation(
            "require_artifacts cannot be true when artifacts are disabled".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_mapping(mapping: &MappingConfig) -> Result<(), ConfigError> {
    if mapping.tags.is_none() {
        return Err(ConfigError::Validation(format!(
            "mapping '{}' is missing a tags block",
            mapping.from,
        )));
    }
    Ok(())
}

/// Resolve a `TargetsValue` into a list of registry names, producing clear
/// errors that distinguish unknown groups from unknown registries.
fn resolve_target_names(
    targets: &TargetsValue,
    config: &Config,
    known: &std::collections::HashSet<&str>,
    context: &str,
) -> Result<Vec<String>, ConfigError> {
    match targets {
        TargetsValue::Group(g) => {
            if let Some(members) = config.target_groups.get(g.as_str()) {
                Ok(members.clone())
            } else if known.contains(g.as_str()) {
                Ok(vec![g.clone()])
            } else {
                Err(ConfigError::Validation(format!(
                    "{context} references unknown target group or registry '{g}'",
                )))
            }
        }
        TargetsValue::List(list) => Ok(list.clone()),
    }
}

pub(crate) fn validate_references(config: &Config) -> Result<(), ConfigError> {
    let known: std::collections::HashSet<&str> =
        config.registries.keys().map(String::as_str).collect();

    // Check mapping source/targets references
    for mapping in &config.mappings {
        if let Some(ref src) = mapping.source {
            if !known.contains(src.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "mapping '{}' references unknown registry '{src}'",
                    mapping.from,
                )));
            }
        }
        if let Some(ref targets) = mapping.targets {
            let context = format!("mapping '{}'", mapping.from);
            let names = resolve_target_names(targets, config, &known, &context)?;
            for name in &names {
                if !known.contains(name.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "mapping '{}' references unknown registry '{name}'",
                        mapping.from,
                    )));
                }
            }
        }
    }

    // Check defaults source/targets
    if let Some(ref defaults) = config.defaults {
        if let Some(ref src) = defaults.source {
            if !known.contains(src.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "defaults references unknown registry '{src}'",
                )));
            }
        }
        if let Some(ref targets) = defaults.targets {
            let names = resolve_target_names(targets, config, &known, "defaults")?;
            for name in &names {
                if !known.contains(name.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "defaults references unknown registry '{name}'",
                    )));
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn deserialize_minimal_config() {
        let yaml = r#"
mappings:
  - from: library/nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.mappings.len(), 1);
        assert_eq!(config.mappings[0].from, "library/nginx");
        assert!(config.registries.is_empty());
    }

    #[test]
    fn deserialize_registry_with_ecr() {
        let yaml = r#"
registries:
  my-ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr
    ecr:
      auto_create: true
      defaults:
        image_tag_mutability: IMMUTABLE
        image_scanning_on_push: true
      overrides:
        - match: "dev-*"
          image_tag_mutability: MUTABLE
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let ecr_reg = &config.registries["my-ecr"];
        assert_eq!(ecr_reg.auth_type, Some(AuthType::Ecr));
        let ecr = ecr_reg.ecr.as_ref().unwrap();
        assert!(ecr.auto_create);
        assert_eq!(
            ecr.defaults.as_ref().unwrap().image_tag_mutability,
            Some(TagMutability::Immutable)
        );
        assert_eq!(ecr.overrides.len(), 1);
        assert_eq!(ecr.overrides[0].match_pattern, "dev-*");
        assert_eq!(
            ecr.overrides[0].image_tag_mutability,
            Some(TagMutability::Mutable)
        );
    }

    #[test]
    fn deserialize_tag_filter() {
        let yaml = r#"
mappings:
  - from: library/node
    tags:
      glob: "18.*"
      semver: ">=18.0.0"
      semver_prerelease: exclude
      exclude: "*-alpine"
      sort: semver
      latest: 5
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let tags = config.mappings[0].tags.as_ref().unwrap();
        assert!(matches!(tags.sort, Some(SortOrder::Semver)));
        assert_eq!(tags.latest, Some(5));
        assert!(matches!(
            tags.semver_prerelease,
            Some(SemverPrerelease::Exclude)
        ));
    }

    #[test]
    fn deserialize_glob_list() {
        let yaml = r#"
mappings:
  - from: lib/foo
    tags:
      glob:
        - "v1.*"
        - "v2.*"
      exclude:
        - "*.rc*"
        - "*.beta*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let tags = config.mappings[0].tags.as_ref().unwrap();
        match tags.glob.as_ref().unwrap() {
            GlobOrList::List(v) => assert_eq!(v.len(), 2),
            _ => panic!("expected list"),
        }
        match tags.exclude.as_ref().unwrap() {
            GlobOrList::List(v) => assert_eq!(v.len(), 2),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn deserialize_defaults_block() {
        let yaml = r#"
defaults:
  source: docker-hub
  targets:
    - ecr-prod
  tags:
    sort: semver
    latest: 10
  recompress: true
  recompress_level: 6
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let defaults = config.defaults.as_ref().unwrap();
        assert_eq!(defaults.source.as_deref(), Some("docker-hub"));
        assert!(defaults.recompress.unwrap());
        assert_eq!(defaults.recompress_level, Some(6));
    }

    #[test]
    fn deserialize_bulk_mapping() {
        let yaml = r#"
mappings:
  - from: bulk
    bulk:
      from_prefix: library/
      to_prefix: mirror/
      names:
        - nginx
        - redis
        - postgres
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let bulk = config.mappings[0].bulk.as_ref().unwrap();
        assert_eq!(bulk.from_prefix, "library/");
        assert_eq!(bulk.names.len(), 3);
    }

    #[test]
    fn deserialize_credentials_block() {
        let yaml = r#"
registries:
  ghcr:
    url: ghcr.io
    credentials:
      username: bot
      password: "ghp_abc123"
mappings:
  - from: myimage
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let creds = config.registries["ghcr"].credentials.as_ref().unwrap();
        assert_eq!(creds.username.as_deref(), Some("bot"));
        assert_eq!(creds.password.as_deref(), Some("ghp_abc123"));
    }

    #[test]
    fn deserialize_ecr_auto_create() {
        let yaml = r#"
registries:
  ecr:
    url: 111111111111.dkr.ecr.us-west-2.amazonaws.com
    ecr:
      auto_create: true
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let ecr = config.registries["ecr"].ecr.as_ref().unwrap();
        assert!(ecr.auto_create);
    }

    #[test]
    fn deserialize_target_groups() {
        let yaml = r#"
registries:
  ecr-east:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
  ecr-west:
    url: 111111111111.dkr.ecr.us-west-2.amazonaws.com
target_groups:
  all-ecr:
    - ecr-east
    - ecr-west
mappings:
  - from: nginx
    targets: all-ecr
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.target_groups["all-ecr"].len(), 2);
        match config.mappings[0].targets.as_ref().unwrap() {
            TargetsValue::Group(g) => assert_eq!(g, "all-ecr"),
            _ => panic!("expected group"),
        }
    }

    #[test]
    fn deserialize_log_format_and_level() {
        let yaml = r#"
log_format: json
log_level: debug
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_format, Some(LogFormat::Json));
        assert_eq!(config.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn deserialize_manifest_format() {
        let yaml = r#"
registries:
  hub:
    url: registry-1.docker.io
    manifest_format: oci
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.registries["hub"].manifest_format,
            Some(ManifestFormat::Oci)
        );
    }

    #[test]
    fn serialize_roundtrip() {
        let yaml = r#"
registries:
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr
mappings:
  - from: nginx
    tags:
      glob: "*"
      sort: semver
      latest: 5
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let serialized = serde_yaml::to_string(&config).unwrap();
        let roundtripped: Config = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(roundtripped.mappings.len(), config.mappings.len());
        assert_eq!(
            roundtripped.registries["ecr"].auth_type,
            Some(AuthType::Ecr)
        );
    }

    // -----------------------------------------------------------------------
    // Env var expansion tests
    // -----------------------------------------------------------------------

    #[test]
    fn expand_simple() {
        // SAFETY: test-only, run with --test-threads=1 if needed
        unsafe { std::env::set_var("OCYNC_TEST_HOST", "example.com") };
        let result = expand_env_vars("https://${OCYNC_TEST_HOST}/v2", &[], false).unwrap();
        assert_eq!(result, "https://example.com/v2");
        unsafe { std::env::remove_var("OCYNC_TEST_HOST") };
    }

    #[test]
    fn expand_with_default() {
        unsafe { std::env::remove_var("OCYNC_MISSING_DEFAULT") };
        let result = expand_env_vars("${OCYNC_MISSING_DEFAULT:-fallback}", &[], false).unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn expand_required_missing() {
        unsafe { std::env::remove_var("OCYNC_REQUIRED_VAR") };
        let result = expand_env_vars("${OCYNC_REQUIRED_VAR:?must be set}", &[], false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::EnvVarRequired(msg) => assert_eq!(msg, "must be set"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn blocked_secret_in_non_auth() {
        unsafe { std::env::set_var("MY_SECRET_KEY", "s3cret") };
        let result = expand_env_vars("${MY_SECRET_KEY}", &[], false);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::BlockedEnvVar(_)));
        unsafe { std::env::remove_var("MY_SECRET_KEY") };
    }

    #[test]
    fn allowed_in_auth_field() {
        unsafe { std::env::set_var("MY_SECRET_TOKEN", "tok123") };
        let result = expand_env_vars("${MY_SECRET_TOKEN}", &[], true).unwrap();
        assert_eq!(result, "tok123");
        unsafe { std::env::remove_var("MY_SECRET_TOKEN") };
    }

    #[test]
    fn allowed_via_override() {
        unsafe { std::env::set_var("CUSTOM_API_KEY", "key456") };
        let allow = vec!["CUSTOM_API_KEY".to_string()];
        let result = expand_env_vars("${CUSTOM_API_KEY}", &allow, false).unwrap();
        assert_eq!(result, "key456");
        unsafe { std::env::remove_var("CUSTOM_API_KEY") };
    }

    #[test]
    fn no_expansion_needed() {
        let result = expand_env_vars("plain string no vars", &[], false).unwrap();
        assert_eq!(result, "plain string no vars");
    }

    #[test]
    fn expand_malformed_unclosed_brace() {
        let result = expand_env_vars("prefix-${UNCLOSED", &[], false).unwrap();
        // Malformed expression is pushed as literal
        assert_eq!(result, "prefix-${UNCLOSED");
    }

    // -----------------------------------------------------------------------
    // Validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn latest_without_sort() {
        let tags = TagsConfig {
            latest: Some(5),
            sort: None,
            ..Default::default()
        };
        let err = validate_tags(&tags).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn missing_tags_block() {
        let mapping = MappingConfig {
            from: "nginx".to_string(),
            to: None,
            source: None,
            targets: None,
            tags: None,
            platforms: None,
            skip_existing: None,
            artifacts: None,
            recompress: None,
            recompress_level: None,
            bulk: None,
        };
        let err = validate_mapping(&mapping).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn require_artifacts_conflict() {
        let artifacts = ArtifactsConfig {
            enabled: false,
            include: None,
            exclude: None,
            require_artifacts: Some(true),
        };
        let err = validate_artifacts(&artifacts).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn unknown_registry_source_reference() {
        let yaml = r#"
registries:
  docker-hub:
    url: registry-1.docker.io
mappings:
  - from: nginx
    source: nonexistent
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_references(&config).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn unknown_registry_in_target_list() {
        let yaml = r#"
registries:
  ecr-east:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
mappings:
  - from: nginx
    targets:
      - ecr-east
      - ecr-west-missing
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_references(&config).unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("ecr-west-missing")),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn unknown_registry_via_group_members() {
        let yaml = r#"
registries:
  ecr-east:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
target_groups:
  all:
    - ecr-east
    - ghost-registry
mappings:
  - from: nginx
    targets: all
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_references(&config).unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("ghost-registry")),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn valid_references_pass() {
        let yaml = r#"
registries:
  hub:
    url: registry-1.docker.io
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
defaults:
  source: hub
mappings:
  - from: nginx
    source: hub
    targets:
      - ecr
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        validate_references(&config).unwrap();
    }

    #[test]
    fn unknown_defaults_target() {
        let yaml = r#"
registries:
  hub:
    url: registry-1.docker.io
defaults:
  targets:
    - nonexistent-ecr
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_references(&config).unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("nonexistent-ecr")),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn unknown_group_name_gives_clear_error() {
        let yaml = r#"
registries:
  hub:
    url: registry-1.docker.io
mappings:
  - from: nginx
    targets: typo-group
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_references(&config).unwrap_err();
        match err {
            ConfigError::Validation(msg) => {
                assert!(msg.contains("typo-group"));
                assert!(msg.contains("target group or registry"));
            }
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn invalid_auth_type_gives_serde_error() {
        let yaml = r#"
registries:
  hub:
    url: registry-1.docker.io
    auth_type: kerberos
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let err = serde_yaml::from_str::<Config>(yaml);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("kerberos") || msg.contains("unknown variant"));
    }

    #[test]
    fn invalid_log_format_gives_serde_error() {
        let yaml = r#"
log_format: xml
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let err = serde_yaml::from_str::<Config>(yaml);
        assert!(err.is_err());
    }
}
