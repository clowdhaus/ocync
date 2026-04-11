#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
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
pub struct Config {
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
    pub log_format: Option<String>,

    #[serde(default)]
    pub log_level: Option<String>,

    #[serde(default)]
    pub env_var_policy: Option<EnvVarPolicy>,
}

// ---------------------------------------------------------------------------
// Global
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub struct GlobalConfig {
    #[serde(default = "default_max_concurrent_transfers")]
    pub max_concurrent_transfers: u32,
}

fn default_max_concurrent_transfers() -> u32 {
    50
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub struct RegistryConfig {
    pub url: String,

    #[serde(default)]
    pub auth_type: Option<String>,

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
    pub manifest_format: Option<String>,

    #[serde(default)]
    pub ecr: Option<EcrConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub pull: Option<u32>,

    #[serde(default)]
    pub push: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CredentialsConfig {
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
pub struct EcrConfig {
    #[serde(default)]
    pub auto_create: bool,

    #[serde(default)]
    pub defaults: Option<EcrDefaults>,

    #[serde(default)]
    pub overrides: Vec<EcrOverride>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EcrDefaults {
    #[serde(default)]
    pub image_tag_mutability: Option<String>,

    #[serde(default)]
    pub image_scanning_on_push: Option<bool>,

    #[serde(default)]
    pub repository_policy_file: Option<String>,

    #[serde(default)]
    pub lifecycle_policy_file: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EcrOverride {
    /// Named `match_pattern` to avoid the `match` keyword.
    #[serde(rename = "match")]
    pub match_pattern: String,

    #[serde(default)]
    pub image_tag_mutability: Option<String>,

    #[serde(default)]
    pub lifecycle_policy_file: Option<String>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub struct DefaultsConfig {
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
pub struct MappingConfig {
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
pub struct BulkConfig {
    pub from_prefix: String,
    pub to_prefix: String,
    pub names: Vec<String>,
}

// ---------------------------------------------------------------------------
// TargetsValue — single group name or inline list
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TargetsValue {
    Group(String),
    List(Vec<String>),
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct TagsConfig {
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
pub enum GlobOrList {
    Single(String),
    List(Vec<String>),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Semver,
    Alpha,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SemverPrerelease {
    Include,
    Exclude,
    Only,
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub struct ArtifactsConfig {
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
pub struct EnvVarPolicy {
    #[serde(default)]
    pub allow: Vec<String>,
}

// ---------------------------------------------------------------------------
// ResolvedMapping — fully merged with defaults
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ResolvedMapping {
    pub from: String,
    pub to: Option<String>,
    pub source: String,
    pub targets: Vec<String>,
    pub tags: TagsConfig,
    pub platforms: Option<Vec<String>>,
    pub skip_existing: bool,
    pub artifacts: Option<ArtifactsConfig>,
    pub recompress: bool,
    pub recompress_level: Option<u32>,
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
pub fn expand_env_vars(
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
                // malformed — just push literal
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

pub fn validate_tags(tags: &TagsConfig) -> Result<(), ConfigError> {
    if tags.latest.is_some() && tags.sort.is_none() {
        return Err(ConfigError::Validation(
            "tags.latest requires tags.sort to be set".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_artifacts(artifacts: &ArtifactsConfig) -> Result<(), ConfigError> {
    if !artifacts.enabled && artifacts.require_artifacts == Some(true) {
        return Err(ConfigError::Validation(
            "require_artifacts cannot be true when artifacts are disabled".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_mapping(mapping: &MappingConfig) -> Result<(), ConfigError> {
    if mapping.tags.is_none() {
        return Err(ConfigError::Validation(format!(
            "mapping '{}' is missing a tags block",
            mapping.from,
        )));
    }
    Ok(())
}

pub fn validate_references(config: &Config) -> Result<(), ConfigError> {
    // Collect known registry names
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
            let names: Vec<&str> = match targets {
                TargetsValue::Group(g) => {
                    // group reference — check group members if group exists
                    if let Some(members) = config.target_groups.get(g.as_str()) {
                        members.iter().map(String::as_str).collect()
                    } else {
                        // could be a direct registry name
                        vec![g.as_str()]
                    }
                }
                TargetsValue::List(list) => list.iter().map(String::as_str).collect(),
            };
            for name in names {
                if !known.contains(name) {
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
    // Deserialization tests (9+)
    // -----------------------------------------------------------------------

    #[test]
    fn test_deserialize_minimal_config() {
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
    fn test_deserialize_registry_with_ecr() {
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
        let ecr = ecr_reg.ecr.as_ref().unwrap();
        assert!(ecr.auto_create);
        assert_eq!(
            ecr.defaults
                .as_ref()
                .unwrap()
                .image_tag_mutability
                .as_deref(),
            Some("IMMUTABLE")
        );
        assert_eq!(ecr.overrides.len(), 1);
        assert_eq!(ecr.overrides[0].match_pattern, "dev-*");
    }

    #[test]
    fn test_deserialize_tag_filter() {
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
    fn test_deserialize_glob_list() {
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
    fn test_deserialize_defaults_block() {
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
    fn test_deserialize_bulk_mapping() {
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
    fn test_deserialize_credentials_block() {
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
    fn test_deserialize_ecr_auto_create() {
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
    fn test_deserialize_target_groups() {
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

    // -----------------------------------------------------------------------
    // Env var expansion tests (7)
    // -----------------------------------------------------------------------

    #[test]
    fn test_expand_simple() {
        // SAFETY: test-only, run with --test-threads=1 if needed
        unsafe { std::env::set_var("OCYNC_TEST_HOST", "example.com") };
        let result = expand_env_vars("https://${OCYNC_TEST_HOST}/v2", &[], false).unwrap();
        assert_eq!(result, "https://example.com/v2");
        unsafe { std::env::remove_var("OCYNC_TEST_HOST") };
    }

    #[test]
    fn test_expand_with_default() {
        unsafe { std::env::remove_var("OCYNC_MISSING_DEFAULT") };
        let result = expand_env_vars("${OCYNC_MISSING_DEFAULT:-fallback}", &[], false).unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn test_expand_required_missing() {
        unsafe { std::env::remove_var("OCYNC_REQUIRED_VAR") };
        let result = expand_env_vars("${OCYNC_REQUIRED_VAR:?must be set}", &[], false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::EnvVarRequired(msg) => assert_eq!(msg, "must be set"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn test_blocked_secret_in_non_auth() {
        unsafe { std::env::set_var("MY_SECRET_KEY", "s3cret") };
        let result = expand_env_vars("${MY_SECRET_KEY}", &[], false);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::BlockedEnvVar(_)));
        unsafe { std::env::remove_var("MY_SECRET_KEY") };
    }

    #[test]
    fn test_allowed_in_auth_field() {
        unsafe { std::env::set_var("MY_SECRET_TOKEN", "tok123") };
        let result = expand_env_vars("${MY_SECRET_TOKEN}", &[], true).unwrap();
        assert_eq!(result, "tok123");
        unsafe { std::env::remove_var("MY_SECRET_TOKEN") };
    }

    #[test]
    fn test_allowed_via_override() {
        unsafe { std::env::set_var("CUSTOM_API_KEY", "key456") };
        let allow = vec!["CUSTOM_API_KEY".to_string()];
        let result = expand_env_vars("${CUSTOM_API_KEY}", &allow, false).unwrap();
        assert_eq!(result, "key456");
        unsafe { std::env::remove_var("CUSTOM_API_KEY") };
    }

    #[test]
    fn test_no_expansion_needed() {
        let result = expand_env_vars("plain string no vars", &[], false).unwrap();
        assert_eq!(result, "plain string no vars");
    }

    // -----------------------------------------------------------------------
    // Validation tests (4)
    // -----------------------------------------------------------------------

    #[test]
    fn test_latest_without_sort() {
        let tags = TagsConfig {
            latest: Some(5),
            sort: None,
            ..Default::default()
        };
        let err = validate_tags(&tags).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn test_missing_tags_block() {
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
    fn test_require_artifacts_conflict() {
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
    fn test_unknown_registry_reference() {
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
}
