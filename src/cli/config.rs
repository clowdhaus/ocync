//! Configuration file schema types and YAML deserialization.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load, expand env vars, and validate a config file from disk.
///
/// Reads the raw YAML, expands `${VAR}` expressions in the text, then
/// deserializes and validates. Expanding before deserialization ensures
/// that registry URLs and references containing env vars are resolved
/// before structural validation runs.
pub(crate) fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Parse(format!("failed to read {}: {e}", path.display())))?;

    let expanded = expand_env_vars(&raw, true)?;

    let config: Config = serde_yaml::from_str(&expanded)
        .map_err(|e| ConfigError::Parse(format!("failed to parse {}: {e}", path.display())))?;

    // Structural validation.
    for mapping in &config.mappings {
        validate_mapping(mapping)?;
        if let Some(ref tags) = mapping.tags {
            validate_tags(tags)?;
        }
    }
    validate_references(&config)?;

    Ok(config)
}

/// Read a config file and expand env vars for display.
///
/// Returns the raw YAML with `${VAR}` expressions expanded. When
/// `show_secrets` is false, the expansion blocks secret-pattern variables.
/// When `show_secrets` is true, all variables are expanded freely.
pub(crate) fn expand_config_for_display(
    path: &Path,
    show_secrets: bool,
) -> Result<String, ConfigError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Parse(format!("failed to read {}: {e}", path.display())))?;

    expand_env_vars(&raw, show_secrets)
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("config parse error: {0}")]
    Parse(String),

    #[error("required environment variable not set: {0}")]
    EnvVarRequired(String),

    #[error("blocked secret-pattern environment variable: {0}")]
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
}

// ---------------------------------------------------------------------------
// Registry
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

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RegistryConfig {
    pub url: String,

    #[serde(default)]
    pub auth_type: Option<AuthType>,

    #[serde(default)]
    pub ecr: Option<EcrConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EcrConfig {
    #[serde(default)]
    pub auto_create: bool,
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
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum GlobOrList {
    Single(String),
    List(Vec<String>),
}

use ocync_sync::filter::{SemverPrerelease, SortOrder};

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
fn is_blocked(var_name: &str) -> bool {
    let upper = var_name.to_uppercase();
    BLOCKED_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

/// Expand `${VAR}`, `${VAR:-default}`, and `${VAR:?error}` expressions.
///
/// Only the `${...}` form is supported — bare `$VAR` without braces is
/// treated as literal text. This avoids ambiguity with shell-like strings.
///
/// When `allow_secrets` is false, variables matching [`BLOCKED_PATTERNS`]
/// (SECRET, TOKEN, etc.) are rejected to prevent accidental secret leakage
/// into display output. This is used by `expand_config_for_display` when
/// `--show-secrets` is not set. Config loading always passes `true` because
/// it operates on raw YAML text without field-level context — blocking
/// secret-patterned vars during loading would reject legitimate auth config.
pub(crate) fn expand_env_vars(input: &str, allow_secrets: bool) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next();
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
                result.push_str("${");
                result.push_str(&expr);
                continue;
            }

            if let Some(pos) = expr.find(":-") {
                let var_name = &expr[..pos];
                let default_val = &expr[pos + 2..];
                if !allow_secrets && is_blocked(var_name) {
                    return Err(ConfigError::BlockedEnvVar(var_name.to_string()));
                }
                match std::env::var(var_name) {
                    Ok(val) if !val.is_empty() => result.push_str(&val),
                    _ => result.push_str(default_val),
                }
            } else if let Some(pos) = expr.find(":?") {
                let var_name = &expr[..pos];
                let err_msg = &expr[pos + 2..];
                if !allow_secrets && is_blocked(var_name) {
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
                if !allow_secrets && is_blocked(var_name) {
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
    if tags.semver_prerelease.is_some() && tags.semver.is_none() {
        return Err(ConfigError::Validation(
            "tags.semver_prerelease requires tags.semver to be set".to_string(),
        ));
    }
    Ok(())
}

// TODO(defaults): When defaults merging is implemented, allow mappings to omit
// `tags` and inherit from `defaults.tags`. This validation must become context-aware.
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

    // -- Deserialization ----------------------------------------------------

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
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let reg = &config.registries["my-ecr"];
        assert_eq!(reg.auth_type, Some(AuthType::Ecr));
        assert!(reg.ecr.as_ref().unwrap().auto_create);
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
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let defaults = config.defaults.as_ref().unwrap();
        assert_eq!(defaults.source.as_deref(), Some("docker-hub"));
        assert!(defaults.tags.is_some());
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
    }

    // -- Env var expansion --------------------------------------------------

    #[test]
    fn expand_simple() {
        unsafe { std::env::set_var("OCYNC_TEST_HOST", "example.com") };
        let result = expand_env_vars("https://${OCYNC_TEST_HOST}/v2", false).unwrap();
        assert_eq!(result, "https://example.com/v2");
        unsafe { std::env::remove_var("OCYNC_TEST_HOST") };
    }

    #[test]
    fn expand_with_default() {
        unsafe { std::env::remove_var("OCYNC_MISSING_DEFAULT") };
        let result = expand_env_vars("${OCYNC_MISSING_DEFAULT:-fallback}", false).unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn expand_required_missing() {
        unsafe { std::env::remove_var("OCYNC_REQUIRED_VAR") };
        let result = expand_env_vars("${OCYNC_REQUIRED_VAR:?must be set}", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::EnvVarRequired(msg) => assert_eq!(msg, "must be set"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn blocked_secret_when_secrets_disallowed() {
        unsafe { std::env::set_var("MY_SECRET_KEY", "s3cret") };
        let result = expand_env_vars("${MY_SECRET_KEY}", false);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::BlockedEnvVar(_)));
        unsafe { std::env::remove_var("MY_SECRET_KEY") };
    }

    #[test]
    fn allowed_when_secrets_allowed() {
        unsafe { std::env::set_var("MY_SECRET_TOKEN", "tok123") };
        let result = expand_env_vars("${MY_SECRET_TOKEN}", true).unwrap();
        assert_eq!(result, "tok123");
        unsafe { std::env::remove_var("MY_SECRET_TOKEN") };
    }

    #[test]
    fn no_expansion_needed() {
        let result = expand_env_vars("plain string no vars", false).unwrap();
        assert_eq!(result, "plain string no vars");
    }

    #[test]
    fn expand_malformed_unclosed_brace() {
        let result = expand_env_vars("prefix-${UNCLOSED", false).unwrap();
        assert_eq!(result, "prefix-${UNCLOSED");
    }

    #[test]
    fn expand_value_with_colon_in_yaml() {
        // A var value containing ":" should not break YAML structure when the
        // field is already quoted in the YAML source.
        unsafe { std::env::set_var("OCYNC_TEST_COLON", "host:8080") };
        let yaml = "url: \"${OCYNC_TEST_COLON}\"";
        let expanded = expand_env_vars(yaml, true).unwrap();
        assert_eq!(expanded, "url: \"host:8080\"");
        // Verify it parses as valid YAML.
        let map: HashMap<String, String> = serde_yaml::from_str(&expanded).unwrap();
        assert_eq!(map["url"], "host:8080");
        unsafe { std::env::remove_var("OCYNC_TEST_COLON") };
    }

    #[test]
    fn expand_value_with_hash_in_yaml() {
        // A var value containing "#" should not be treated as a YAML comment
        // when the field is quoted.
        unsafe { std::env::set_var("OCYNC_TEST_HASH", "value#with#hashes") };
        let yaml = "key: \"${OCYNC_TEST_HASH}\"";
        let expanded = expand_env_vars(yaml, true).unwrap();
        let map: HashMap<String, String> = serde_yaml::from_str(&expanded).unwrap();
        assert_eq!(map["key"], "value#with#hashes");
        unsafe { std::env::remove_var("OCYNC_TEST_HASH") };
    }

    #[test]
    fn expand_unquoted_colon_value_breaks_yaml() {
        // Demonstrates the known limitation: unquoted values containing ":"
        // can produce invalid YAML. Users must quote values in their config.
        unsafe { std::env::set_var("OCYNC_TEST_BARE_COLON", "host:8080") };
        let yaml = "url: ${OCYNC_TEST_BARE_COLON}";
        let expanded = expand_env_vars(yaml, true).unwrap();
        // The expansion itself succeeds, but YAML parsing may interpret
        // "host:8080" differently (as a mapping key).
        assert_eq!(expanded, "url: host:8080");
        unsafe { std::env::remove_var("OCYNC_TEST_BARE_COLON") };
    }

    // -- Validation ---------------------------------------------------------

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
        };
        let err = validate_mapping(&mapping).unwrap_err();
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
}
