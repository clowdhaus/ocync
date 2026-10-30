//! Configuration file schema types and YAML deserialization.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
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
    for (name, registry) in &config.registries {
        validate_registry(name, registry)?;
    }
    let has_default_tags = config.defaults.as_ref().is_some_and(|d| d.tags.is_some());
    for mapping in &config.mappings {
        validate_mapping(mapping, has_default_tags)?;
        if let Some(ref tags) = mapping.tags {
            validate_tags(&format!("mapping '{}'", mapping.from), tags)?;
        }
    }
    if let Some(ref defaults) = config.defaults {
        if let Some(ref tags) = defaults.tags {
            validate_tags("defaults", tags)?;
        }
        if let Some(ref platforms) = defaults.platforms {
            validate_platforms("defaults", platforms)?;
        }
        if let Some(ref artifacts) = defaults.artifacts {
            validate_artifacts("defaults", artifacts)?;
        }
    }
    for mapping in &config.mappings {
        let effective = mapping
            .artifacts
            .as_ref()
            .or(config.defaults.as_ref().and_then(|d| d.artifacts.as_ref()));
        if let Some(artifacts) = effective {
            validate_artifacts(&mapping.from, artifacts)?;
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

/// Top-level configuration defining registries, mappings, and sync behavior.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(crate) struct Config {
    /// Global engine settings applied across all syncs.
    #[serde(default)]
    pub global: Option<GlobalConfig>,

    /// Named registry definitions keyed by alias.
    #[serde(default)]
    pub registries: HashMap<String, RegistryConfig>,

    /// Named groups of registry aliases for multi-target fanout.
    #[serde(default)]
    pub target_groups: HashMap<String, Vec<String>>,

    /// Default source, targets, tags, and platforms inherited by all mappings.
    #[serde(default)]
    pub defaults: Option<DefaultsConfig>,

    /// Image mapping rules defining what to sync and where.
    pub mappings: Vec<MappingConfig>,
}

// ---------------------------------------------------------------------------
// Global config
// ---------------------------------------------------------------------------

/// Global engine settings that apply across all sync operations.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub(crate) struct GlobalConfig {
    /// Maximum concurrent image syncs (default: 10).
    ///
    /// Default derived as `blob_upload_concurrency / BLOB_CONCURRENCY = 64
    /// / 6`, sized so the per-target HTTP/2 stream budget is not exhausted
    /// (typical advertised `SETTINGS_MAX_CONCURRENT_STREAMS` is 100-128
    /// across major container registries).
    #[serde(
        default = "default_max_concurrent_transfers",
        deserialize_with = "transfers_at_least_one"
    )]
    pub max_concurrent_transfers: NonZeroUsize,

    /// Cache directory for persistent cache and blob staging.
    ///
    /// Defaults to a directory next to the config file when not specified.
    pub cache_dir: Option<String>,

    /// Warm cache TTL as a human-readable duration (e.g. "12h", "30m").
    ///
    /// `"0"` disables TTL-based expiry (cache never expires by age; lazy
    /// invalidation only). Defaults to `"12h"` when not specified.
    #[schemars(with = "Option<String>")]
    pub cache_ttl: Option<CacheTtl>,

    /// Disk staging size limit as a human-readable size (e.g. "2GB", "500MB").
    ///
    /// Uses SI decimal prefixes: 1 GB = 1,000,000,000 bytes.
    /// `0` disables disk staging. When absent, no eviction is performed.
    #[schemars(with = "Option<String>")]
    pub staging_size_limit: Option<ByteSize>,
}

/// A count that must be at least 1, so a 0 reads as the rule it broke,
/// naming `field`, rather than as a type mismatch.
fn nonzero<E: serde::de::Error>(field: &str, n: usize) -> Result<NonZeroUsize, E> {
    NonZeroUsize::new(n).ok_or_else(|| E::custom(format!("{field} must be at least 1")))
}

/// Reads `max_concurrent_transfers` through [`nonzero`].
fn transfers_at_least_one<'de, D>(deserializer: D) -> Result<NonZeroUsize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    nonzero(
        "max_concurrent_transfers",
        usize::deserialize(deserializer)?,
    )
}

/// Reads a registry's optional `max_concurrent` through [`nonzero`].
fn registry_cap_at_least_one<'de, D>(deserializer: D) -> Result<Option<NonZeroUsize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<usize>::deserialize(deserializer)?
        .map(|n| nonzero("max_concurrent", n))
        .transpose()
}

fn default_max_concurrent_transfers() -> NonZeroUsize {
    // Mirror the engine's DEFAULT_MAX_CONCURRENT_TRANSFERS. The previous
    // value of 50 multiplied by BLOB_CONCURRENCY=6 (= 300 streams) blew
    // past the per-connection HTTP/2 stream budget on every major
    // registry; see DEFAULT_BLOB_UPLOAD_CONCURRENCY in
    // ocync-distribution for the derivation. Keep these two in lockstep.
    ocync_sync::engine::DEFAULT_MAX_CONCURRENT_TRANSFERS
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            max_concurrent_transfers: default_max_concurrent_transfers(),
            cache_dir: None,
            cache_ttl: None,
            staging_size_limit: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Authentication method for a registry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthType {
    /// AWS ECR token exchange.
    Ecr,
    /// Google Artifact Registry native auth (ADC).
    Gar,
    /// Google Container Registry native auth (ADC).
    Gcr,
    /// Azure Container Registry native auth (AAD).
    Acr,
    /// GitHub Container Registry (`GITHUB_TOKEN`).
    Ghcr,
    /// Anonymous (token exchange only).
    Anonymous,
    /// HTTP basic auth.
    Basic,
    /// Pre-obtained bearer token (PAT, CI token).
    #[serde(alias = "token")]
    StaticToken,
    /// Docker config.json credential store.
    DockerConfig,
}

/// Per-registry settings: URL, auth method, concurrency, and credentials.
#[derive(Deserialize, Serialize, JsonSchema)]
pub(crate) struct RegistryConfig {
    /// Registry base URL (e.g. `registry-1.docker.io`, `123456789012.dkr.ecr.us-east-1.amazonaws.com`).
    /// HTTPS unless written with `http://`, which uses plain HTTP (local registries only).
    pub url: String,

    /// Authentication method to use for this registry.
    #[serde(default)]
    pub auth_type: Option<AuthType>,

    /// Per-registry aggregate concurrency cap (default: 50).
    ///
    /// Limits the total number of simultaneous in-flight HTTP requests to this
    /// registry across all action types. This is independent of the global
    /// `max_concurrent_transfers` (which caps image-level parallelism).
    #[serde(default, deserialize_with = "registry_cap_at_least_one")]
    pub max_concurrent: Option<NonZeroUsize>,

    /// Credentials for Basic auth (`auth_type: basic`).
    #[serde(default)]
    pub credentials: Option<BasicCredentials>,

    /// Bearer token for static token auth (`auth_type: static_token`).
    #[serde(default)]
    pub token: Option<String>,

    /// HEAD-check targets before pulling full source manifests on cache miss.
    ///
    /// When enabled, the engine issues a manifest HEAD against all targets
    /// before performing a full source manifest GET. If every target already
    /// holds the same digest as the source HEAD, the expensive GET is skipped.
    /// This conserves rate-limit tokens on source registries with aggressive
    /// quotas (e.g., Docker Hub).
    ///
    /// Applies to both `sync` (which is the original target) and `copy`
    /// (single-image variant); in `copy` the optimization fires when the
    /// destination tag already resolves to the same source digest.
    #[serde(default)]
    pub head_first: bool,

    /// Named AWS profile for ECR credential resolution.
    ///
    /// When set, ECR auth for this registry resolves credentials and
    /// settings from this profile instead of the one `AWS_PROFILE` selects.
    /// Only valid with explicit `auth_type: ecr`.
    ///
    /// Use this when the registry requires credentials distinct from the
    /// ambient identity (for example, a third-party ECR accessed with static
    /// IAM-user keys while the ambient chain serves the rest of the workload).
    #[serde(default)]
    pub aws_profile: Option<String>,
}

impl std::fmt::Debug for RegistryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryConfig")
            .field("url", &self.url)
            .field("auth_type", &self.auth_type)
            .field("max_concurrent", &self.max_concurrent)
            .field("head_first", &self.head_first)
            .field("credentials", &self.credentials)
            .field("token", &"[REDACTED]")
            .field("aws_profile", &self.aws_profile)
            .finish()
    }
}

/// Credentials for HTTP Basic authentication.
#[derive(Deserialize, Serialize, JsonSchema)]
pub(crate) struct BasicCredentials {
    /// Username for authentication.
    pub username: String,
    /// Password or access token.
    pub password: String,
}

impl std::fmt::Debug for BasicCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicCredentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Default values inherited by all mappings unless individually overridden.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(crate) struct DefaultsConfig {
    /// Default source registry alias for all mappings.
    #[serde(default)]
    pub source: Option<String>,

    /// Default target registry alias or group for all mappings.
    #[serde(default)]
    pub targets: Option<TargetsValue>,

    /// Default tag filter rules inherited by mappings without their own.
    #[serde(default)]
    pub tags: Option<TagsConfig>,

    /// Platform filter applied to all mappings unless overridden.
    ///
    /// Each entry must be `os/arch` or `os/arch/variant` (e.g. `linux/amd64`,
    /// `linux/arm/v7`).
    #[serde(default)]
    pub platforms: Option<Vec<String>>,

    /// Artifact sync configuration applied to all mappings unless overridden.
    #[serde(default)]
    pub artifacts: Option<ArtifactsConfig>,
}

// ---------------------------------------------------------------------------
// Mappings
// ---------------------------------------------------------------------------

/// A single image mapping rule: source repo, target registries, and tag filters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(crate) struct MappingConfig {
    /// Source repository path (e.g. `library/nginx`).
    pub from: String,

    /// Destination repository path; defaults to `from` when absent.
    #[serde(default)]
    pub to: Option<String>,

    /// Source registry alias, overriding `defaults.source`.
    #[serde(default)]
    pub source: Option<String>,

    /// Target registry alias or group, overriding `defaults.targets`.
    #[serde(default)]
    pub targets: Option<TargetsValue>,

    /// Tag filter rules for this mapping, overriding `defaults.tags`.
    #[serde(default)]
    pub tags: Option<TagsConfig>,

    /// Platform filter for this mapping, overriding any value in `defaults`.
    ///
    /// Each entry must be `os/arch` or `os/arch/variant` (e.g. `linux/amd64`,
    /// `linux/arm/v7`).
    #[serde(default)]
    pub platforms: Option<Vec<String>>,

    /// Artifact sync configuration for this mapping, overriding `defaults.artifacts`.
    #[serde(default)]
    pub artifacts: Option<ArtifactsConfig>,
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// Configuration for OCI artifact (signatures, SBOMs, attestations) sync.
///
/// Controls whether referrers are discovered and transferred alongside their
/// parent image manifests.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub(crate) struct ArtifactsConfig {
    /// Whether artifact sync is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Only sync artifacts whose artifact type matches one of these MIME types.
    #[serde(default)]
    pub include: Vec<String>,

    /// Exclude artifacts whose artifact type matches one of these MIME types.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// When true, every synced image must have at least one referrer.
    /// Images without referrers cause a sync failure instead of silently
    /// producing unsigned images at the target.
    #[serde(default)]
    pub require_artifacts: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// TargetsValue - single group name or inline list
// ---------------------------------------------------------------------------

/// Target specification: either a named group or an inline list of registry aliases.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum TargetsValue {
    /// A single target group name defined in `target_groups`.
    Group(String),
    /// An inline list of registry aliases.
    List(Vec<String>),
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// Tag filter and selection rules for a mapping.
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub(crate) struct TagsConfig {
    /// Include tags matching one or more glob patterns.
    #[serde(default)]
    pub glob: Option<GlobOrList>,

    /// Include tags matching a semver range (e.g. `>=1.0, <2.0`).
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub semver: Option<Range>,

    /// Always-include patterns. Behavior depends on pattern shape: literals
    /// (no `*`, `?`, `[`) bypass the entire pipeline and are kept verbatim
    /// (use to pin floating tags like `latest` or specific RCs). Globs
    /// rescue matching tags from `glob:` and the soft-tier excludes
    /// (built-in defaults + `defaults.tags.exclude:`), then run through
    /// `semver:`, hard `exclude:`, `sort:`, and `latest:` like any other
    /// pipeline tag. Same syntax as `exclude:`.
    #[serde(default)]
    pub include: Option<GlobOrList>,

    /// Removed in 2026-05; deserializing a config that still sets this field
    /// triggers a fail-loud migration error pointing at `include:`.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub semver_prerelease: Option<RemovedSemverPrerelease>,

    /// Exclude tags matching one or more glob patterns.
    #[serde(default)]
    pub exclude: Option<GlobOrList>,

    /// Sort order applied before `latest` truncation.
    #[serde(default)]
    pub sort: Option<SortOrder>,

    /// Keep only the N most recent tags after sorting.
    #[serde(default)]
    pub latest: Option<usize>,

    /// Minimum number of tags to retain regardless of filters.
    #[serde(default)]
    pub min_tags: Option<usize>,

    /// Glob pattern for immutable tags (e.g. `"v?[0-9]*.[0-9]*.[0-9]*"`).
    ///
    /// When a tag matches this pattern AND already exists in the target's tag
    /// list, the tag is skipped with zero API calls. Semver tags are
    /// conventionally immutable; this avoids redundant HEAD checks.
    #[serde(default)]
    pub immutable_tags: Option<String>,
}

impl TagsConfig {
    /// Returns the exact tag names if this config requires no enumeration.
    ///
    /// Returns `Some` when `glob` contains only literal strings (no wildcard
    /// characters) and no other filter fields (`semver`, `latest`, `sort`,
    /// `min_tags`, `exclude`, `include`) are set. In this case the tags can
    /// be used directly without listing all tags from the registry.
    pub(crate) fn exact_tags(&self) -> Option<Vec<String>> {
        // Any field that requires the full tag list forces enumeration.
        if self.semver.is_some()
            || self.latest.is_some()
            || self.sort.is_some()
            || self.min_tags.is_some()
            || self.exclude.is_some()
            || self.include.is_some()
        {
            return None;
        }
        let patterns = match &self.glob {
            Some(GlobOrList::Single(s)) => vec![s.clone()],
            Some(GlobOrList::List(v)) => v.clone(),
            None => return None, // No glob = sync all tags = must enumerate.
        };
        if patterns.iter().all(|p| is_literal_pattern(p)) {
            Some(patterns)
        } else {
            None
        }
    }
}

/// A glob pattern: either a single string or a list of patterns.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum GlobOrList {
    /// A single glob pattern string.
    Single(String),
    /// Multiple glob pattern strings.
    List(Vec<String>),
}

use ocync_sync::filter::{SortOrder, is_literal_pattern};
use ocync_sync::version::Range;

/// Placeholder type for the removed `tags.semver_prerelease` field. Triggers
/// a custom Deserialize error if the field appears in user config, with a
/// migration hint pointing at the new `include:` mechanism.
#[derive(Debug)]
pub(crate) struct RemovedSemverPrerelease;

impl<'de> Deserialize<'de> for RemovedSemverPrerelease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Consume the value so the deserializer is in a clean state.
        let _: serde::de::IgnoredAny = Deserialize::deserialize(deserializer)?;
        Err(<D::Error as serde::de::Error>::custom(
            "tags.semver_prerelease has been removed. \
             For the previous 'exclude' default (the common case), simply delete this field; \
             the system-exclude default drops *-rc*, *-alpha*, *-beta*, *-pre*, *-snapshot*, *-nightly* automatically. \
             For 'include' mode, replace this field with: include: [\"*-rc*\", \"*-alpha*\", \"*-beta*\", \"*-pre*\", \"*-snapshot*\", \"*-nightly*\"]. \
             For 'only' mode, restrict via glob: e.g., glob: [\"*-rc*\", \"*-alpha*\", \"*-beta*\"] plus include: with the same patterns.",
        ))
    }
}

impl JsonSchema for RemovedSemverPrerelease {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RemovedSemverPrerelease".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!(false)
    }
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
fn is_blocked(var_name: &str) -> bool {
    let upper = var_name.to_uppercase();
    BLOCKED_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

/// Expand `${VAR}`, `${VAR:-default}`, and `${VAR:?error}` expressions.
///
/// Only the `${...}` form is supported - bare `$VAR` without braces is
/// treated as literal text. This avoids ambiguity with shell-like strings.
///
/// When `allow_secrets` is false, variables matching [`BLOCKED_PATTERNS`]
/// (SECRET, TOKEN, etc.) are rejected to prevent accidental secret leakage
/// into display output. This is used by `expand_config_for_display` when
/// `--show-secrets` is not set. Config loading always passes `true` because
/// it operates on raw YAML text without field-level context - blocking
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
// Duration and size values
// ---------------------------------------------------------------------------

/// The warm cache TTL, parsed when the config is read so an out-of-range
/// value is a config error rather than a failure at use.
///
/// Accepts an unsigned integer with an optional unit suffix: `s`, `m`, `h`
/// or `d`. A bare integer is seconds and `"0"` disables TTL expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct CacheTtl(Duration);

impl CacheTtl {
    /// The TTL as a [`Duration`].
    pub(crate) fn get(self) -> Duration {
        self.0
    }
}

impl TryFrom<String> for CacheTtl {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let invalid =
            || format!("cache_ttl '{text}' is not a valid duration (e.g. \"12h\", \"30m\", \"0\")");
        let (digits, seconds_per_unit) = match text.chars().last() {
            Some('s') => (&text[..text.len() - 1], 1u64),
            Some('m') => (&text[..text.len() - 1], 60),
            Some('h') => (&text[..text.len() - 1], 3600),
            Some('d') => (&text[..text.len() - 1], 86400),
            _ => (text.as_str(), 1),
        };
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse::<u64>().ok())
            .flatten()
            .and_then(|n| n.checked_mul(seconds_per_unit))
            .map(|secs| Self(Duration::from_secs(secs)))
            .ok_or_else(invalid)
    }
}

impl From<CacheTtl> for String {
    fn from(ttl: CacheTtl) -> String {
        format!("{}s", ttl.0.as_secs())
    }
}

/// A byte count, parsed when the config is read so an out-of-range value is
/// a config error rather than a failure at use.
///
/// Accepts `"0"` or an unsigned integer with a suffix `B`, `KB`, `MB`, `GB`
/// or `TB`, using SI decimal prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ByteSize(u64);

impl ByteSize {
    /// The size in bytes.
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<String> for ByteSize {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let invalid = || {
            format!(
                "staging_size_limit '{text}' is not a valid size (e.g. \"2GB\", \"500MB\", \"0\")"
            )
        };
        if text == "0" {
            return Ok(Self(0));
        }
        // Longest suffix first, as "B" ends every other one.
        let (digits, bytes_per_unit) = [
            ("TB", 1_000_000_000_000u64),
            ("GB", 1_000_000_000),
            ("MB", 1_000_000),
            ("KB", 1_000),
            ("B", 1),
        ]
        .into_iter()
        .find_map(|(suffix, unit)| text.strip_suffix(suffix).map(|digits| (digits, unit)))
        .ok_or_else(invalid)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse::<u64>().ok())
            .flatten()
            .and_then(|n| n.checked_mul(bytes_per_unit))
            .map(Self)
            .ok_or_else(invalid)
    }
}

impl From<ByteSize> for String {
    fn from(size: ByteSize) -> String {
        format!("{}B", size.0)
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Validate per-registry settings.
fn validate_registry(name: &str, registry: &RegistryConfig) -> Result<(), ConfigError> {
    if let Some(ref auth_type) = registry.auth_type {
        match auth_type {
            AuthType::Basic => {
                if registry.credentials.is_none() {
                    return Err(ConfigError::Validation(format!(
                        "registries.{name}: auth_type 'basic' requires 'credentials' \
                         with 'username' and 'password'"
                    )));
                }
            }
            AuthType::StaticToken => {
                if registry.token.is_none() {
                    return Err(ConfigError::Validation(format!(
                        "registries.{name}: auth_type 'token' requires a 'token' field"
                    )));
                }
            }
            AuthType::Ecr
            | AuthType::Gar
            | AuthType::Gcr
            | AuthType::Acr
            | AuthType::Ghcr
            | AuthType::Anonymous
            | AuthType::DockerConfig => {}
        }
    }

    if let Some(ref profile) = registry.aws_profile {
        if profile.is_empty() {
            return Err(ConfigError::Validation(format!(
                "registries.{name}: aws_profile must not be empty"
            )));
        }
        match registry.auth_type {
            Some(AuthType::Ecr) => {}
            _ => {
                return Err(ConfigError::Validation(format!(
                    "registries.{name}: aws_profile requires explicit 'auth_type: ecr'"
                )));
            }
        }
    }

    Ok(())
}

/// Validate artifact sync configuration.
///
/// `require_artifacts: true` with `enabled: false` is contradictory - you
/// cannot require artifacts while disabling their transfer.
fn validate_artifacts(context: &str, artifacts: &ArtifactsConfig) -> Result<(), ConfigError> {
    if artifacts.require_artifacts && !artifacts.enabled {
        return Err(ConfigError::Validation(format!(
            "{context}: require_artifacts is true but artifacts.enabled is false \
             (cannot require artifacts while disabling their transfer)"
        )));
    }
    if !artifacts.enabled {
        tracing::debug!(
            context,
            "artifacts.enabled is false: signatures and SBOMs will be stripped from synced images"
        );
    }
    Ok(())
}

fn validate_tags(context: &str, tags: &TagsConfig) -> Result<(), ConfigError> {
    if tags.latest.is_some() && tags.sort.is_none() {
        return Err(ConfigError::Validation(
            "tags.latest requires tags.sort to be set".to_string(),
        ));
    }
    if tags.semver.is_some() && tags.latest.is_none() {
        tracing::debug!(
            context,
            "tags.semver is set without tags.latest: every tag matching the version range will sync. \
             For long-running mirrors of images with many tags, consider adding 'latest: N' (with 'sort: semver') \
             to cap output size and reduce bytes transferred"
        );
    }
    Ok(())
}

/// Validate a mapping's tags block, falling back to defaults when present.
///
/// A mapping must have its own `tags` block OR inherit from `defaults.tags`.
/// If neither is present, this returns a validation error.
///
/// Platform strings are also validated: each entry must be `os/arch` or
/// `os/arch/variant`.
fn validate_mapping(mapping: &MappingConfig, has_default_tags: bool) -> Result<(), ConfigError> {
    if mapping.tags.is_none() && !has_default_tags {
        return Err(ConfigError::Validation(format!(
            "mapping '{}' is missing a tags block (and no defaults.tags is set)",
            mapping.from,
        )));
    }
    if let Some(ref platforms) = mapping.platforms {
        validate_platforms(&mapping.from, platforms)?;
    }
    Ok(())
}

/// Validate that every platform string has the form `os/arch` or
/// `os/arch/variant`.
fn validate_platforms(mapping_from: &str, platforms: &[String]) -> Result<(), ConfigError> {
    if platforms.is_empty() {
        return Err(ConfigError::Validation(format!(
            "mapping '{mapping_from}': platforms list must not be empty",
        )));
    }
    for platform in platforms {
        let parts: Vec<&str> = platform.splitn(3, '/').collect();
        if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
            return Err(ConfigError::Validation(format!(
                "mapping '{mapping_from}': invalid platform '{platform}' \
                 (expected os/arch or os/arch/variant)",
            )));
        }
    }
    Ok(())
}

/// Resolve a `TargetsValue` into a list of registry names, producing clear
/// errors that distinguish unknown groups from unknown registries.
pub(crate) fn resolve_target_names(
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

fn validate_references(config: &Config) -> Result<(), ConfigError> {
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

    const SCHEMA_JSON_PATH: &str = "docs/public/config.schema.json";

    fn generate_schema_json() -> String {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema).unwrap()
    }

    /// Verify the committed JSON schema matches the current config types.
    ///
    /// If this test fails, regenerate with:
    /// ```bash
    /// cargo test --package ocync -- update_json_schema --ignored
    /// ```
    #[test]
    fn json_schema_up_to_date() {
        let expected = generate_schema_json();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let json_path = root.join(SCHEMA_JSON_PATH);
        let committed_json = std::fs::read_to_string(&json_path).unwrap_or_default();
        assert_eq!(
            committed_json.trim(),
            expected.trim(),
            "config.schema.json is out of date. Run: cargo test --package ocync -- update_json_schema --ignored"
        );
    }

    #[test]
    #[ignore]
    fn update_json_schema() {
        let json = generate_schema_json();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let json_path = root.join(SCHEMA_JSON_PATH);
        std::fs::write(&json_path, format!("{json}\n")).unwrap();
    }

    // - Deserialization ----------------------------------------------------

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
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let reg = &config.registries["my-ecr"];
        assert_eq!(reg.auth_type, Some(AuthType::Ecr));
    }

    #[test]
    fn deserialize_tag_filter() {
        let yaml = r#"
mappings:
  - from: library/node
    tags:
      glob: "18.*"
      semver: ">=18.0.0"
      include: "*-rc*"
      exclude: "*-alpine"
      sort: semver
      latest: 5
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let tags = config.mappings[0].tags.as_ref().unwrap();
        assert!(matches!(tags.sort, Some(SortOrder::Semver)));
        assert_eq!(tags.latest, Some(5));
        assert!(matches!(&tags.include, Some(GlobOrList::Single(s)) if s == "*-rc*"));
    }

    #[test]
    fn deserialize_semver_prerelease_removed_errors() {
        let yaml = r#"
mappings:
  - from: library/node
    tags:
      glob: "18.*"
      semver_prerelease: exclude
"#;
        let err = serde_yaml::from_str::<Config>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("semver_prerelease has been removed"),
            "expected migration hint in error, got: {msg}"
        );
        assert!(
            msg.contains("delete this field"),
            "expected delete-this-field guidance in error, got: {msg}"
        );
        assert!(
            msg.contains("system-exclude default"),
            "expected system-exclude default mention in error, got: {msg}"
        );
        assert!(
            msg.contains("include:"),
            "expected include: recommendation in error, got: {msg}"
        );
    }

    #[test]
    fn accepts_include_field() {
        let yaml = r#"
registries:
  src: { url: docker.io }
defaults:
  source: src
mappings:
  - from: lib/redis
    tags:
      include: ["latest", "latest-dev"]
      semver: ">=1.0"
"#;
        let cfg = serde_yaml::from_str::<Config>(yaml).expect("config parses");
        let mapping = &cfg.mappings[0];
        let include = mapping.tags.as_ref().unwrap().include.as_ref().unwrap();
        let list = match include {
            GlobOrList::List(l) => l,
            GlobOrList::Single(s) => panic!("expected list, got single: {}", s),
        };
        assert_eq!(list, &vec!["latest".to_string(), "latest-dev".to_string()]);
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

    // - Env var expansion --------------------------------------------------

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

    // - Validation ---------------------------------------------------------

    #[test]
    fn latest_without_sort() {
        let tags = TagsConfig {
            latest: Some(5),
            sort: None,
            ..Default::default()
        };
        let err = validate_tags("test", &tags).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn missing_tags_block_no_defaults() {
        let mapping = MappingConfig {
            from: "nginx".to_string(),
            to: None,
            source: None,
            targets: None,
            tags: None,
            platforms: None,
            artifacts: None,
        };
        let err = validate_mapping(&mapping, false).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn missing_tags_block_with_defaults() {
        let mapping = MappingConfig {
            from: "nginx".to_string(),
            to: None,
            source: None,
            targets: None,
            tags: None,
            platforms: None,
            artifacts: None,
        };
        validate_mapping(&mapping, true).unwrap();
    }

    #[test]
    fn invalid_platform_format_rejected() {
        let mapping = MappingConfig {
            from: "nginx".to_string(),
            to: None,
            source: None,
            targets: None,
            tags: Some(TagsConfig {
                glob: Some(GlobOrList::Single("*".to_string())),
                ..Default::default()
            }),
            platforms: Some(vec!["linux-amd64".to_string()]),
            artifacts: None,
        };
        let err = validate_mapping(&mapping, false).unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("linux-amd64")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn valid_platform_formats_accepted() {
        for platform in &[
            "linux/amd64",
            "linux/arm64",
            "linux/arm/v7",
            "windows/amd64",
        ] {
            let mapping = MappingConfig {
                from: "nginx".to_string(),
                to: None,
                source: None,
                targets: None,
                tags: Some(TagsConfig {
                    glob: Some(GlobOrList::Single("*".to_string())),
                    ..Default::default()
                }),
                platforms: Some(vec![platform.to_string()]),
                artifacts: None,
            };
            validate_mapping(&mapping, false)
                .unwrap_or_else(|e| panic!("expected valid platform '{platform}': {e}"));
        }
    }

    #[test]
    fn empty_platform_parts_rejected() {
        for bad in &["/", "/amd64", "linux/", "//", "linux//v8"] {
            let err = validate_platforms("test", &[bad.to_string()]).unwrap_err();
            match err {
                ConfigError::Validation(msg) => {
                    assert!(msg.contains(bad), "expected '{bad}' in: {msg}")
                }
                other => panic!("expected Validation for '{bad}', got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_platforms_list_rejected() {
        let err = validate_platforms("test", &[]).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn deserialize_mapping_with_platforms() {
        let yaml = r#"
mappings:
  - from: library/nginx
    platforms:
      - linux/amd64
      - linux/arm64
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let m = &config.mappings[0];
        assert_eq!(
            m.platforms,
            Some(vec!["linux/amd64".to_string(), "linux/arm64".to_string()])
        );
    }

    #[test]
    fn deserialize_defaults_with_platforms() {
        let yaml = r#"
defaults:
  platforms:
    - linux/amd64
mappings:
  - from: library/nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let d = config.defaults.as_ref().unwrap();
        assert_eq!(d.platforms, Some(vec!["linux/amd64".to_string()]));
    }

    #[test]
    fn invalid_platform_in_defaults_rejected() {
        let yaml = r#"
defaults:
  platforms:
    - linux-amd64
mappings:
  - from: library/nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let defaults = config.defaults.as_ref().unwrap();
        let err = validate_platforms("defaults", defaults.platforms.as_ref().unwrap()).unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("linux-amd64")),
            other => panic!("expected Validation, got {other:?}"),
        }
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

    // - GlobalConfig -------------------------------------------------------

    #[test]
    fn deserialize_global_all_fields() {
        let yaml = r#"
global:
  max_concurrent_transfers: 20
  cache_dir: /tmp/ocync-cache
  cache_ttl: 6h
  staging_size_limit: 1GB
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let global = config.global.as_ref().unwrap();
        assert_eq!(global.max_concurrent_transfers.get(), 20);
        assert_eq!(global.cache_dir.as_deref(), Some("/tmp/ocync-cache"));
        assert_eq!(
            global.cache_ttl.map(CacheTtl::get),
            Some(Duration::from_secs(6 * 3600))
        );
        assert_eq!(
            global.staging_size_limit.map(ByteSize::get),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn global_defaults_when_section_absent() {
        let yaml = r#"
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.global.is_none());
        // When global is absent consumers should use the field defaults.
        let global = GlobalConfig::default();
        assert_eq!(
            global.max_concurrent_transfers,
            default_max_concurrent_transfers()
        );
        assert!(global.cache_dir.is_none());
        assert!(global.cache_ttl.is_none());
        assert!(global.staging_size_limit.is_none());
    }

    #[test]
    fn global_max_concurrent_transfers_default_when_section_present() {
        let yaml = r#"
global:
  cache_dir: /tmp
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let global = config.global.as_ref().unwrap();
        assert_eq!(
            global.max_concurrent_transfers,
            ocync_sync::engine::DEFAULT_MAX_CONCURRENT_TRANSFERS,
        );
    }

    #[test]
    fn global_max_concurrent_transfers_zero_is_refused() {
        let yaml = r#"
global:
  max_concurrent_transfers: 0
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let err = serde_yaml::from_str::<Config>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_concurrent_transfers"), "{err}");
        assert!(err.contains("must be at least 1"), "{err}");
    }

    #[test]
    fn semver_range_is_parsed_when_config_is_read() {
        let yaml = |range: &str| {
            format!("mappings:\n  - from: nginx\n    tags:\n      semver: \"{range}\"\n")
        };
        let config: Config = serde_yaml::from_str(&yaml(">=1.0, <2.0")).unwrap();
        let tags = config.mappings[0].tags.as_ref().unwrap();
        assert_eq!(tags.semver.as_ref().unwrap().to_string(), ">=1.0, <2.0");

        let err = serde_yaml::from_str::<Config>(&yaml("not_valid"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid version range 'not_valid'"), "{err}");
    }

    /// A TTL or size that is malformed, empty or too large for 64 bits is
    /// refused when the config is read, and that read is a config error
    /// (exit 3). Breaks if a value passes the read and fails at use, or an
    /// overflow wraps.
    #[test]
    fn unusable_ttl_and_size_are_config_errors_at_load() {
        let cases = [
            ("cache_ttl", "not-a-duration"),
            ("cache_ttl", ""),
            ("cache_ttl", "12hours"),
            ("cache_ttl", "99999999999999999999"),
            ("cache_ttl", "1000000000000000d"),
            ("staging_size_limit", "2gigabytes"),
            ("staging_size_limit", ""),
            ("staging_size_limit", "99999999999999999999B"),
            ("staging_size_limit", "99999999999TB"),
        ];
        for (field, value) in cases {
            let path = std::env::temp_dir().join("ocync_config_unusable_ttl_and_size.yaml");
            std::fs::write(
                &path,
                format!(
                    "global:\n  {field}: \"{value}\"\nmappings:\n  - from: nginx\n    tags:\n      glob: \"*\"\n"
                ),
            )
            .unwrap();
            let err = load_config(&path).unwrap_err();
            assert!(err.to_string().contains(field), "{field} {value}: {err}");
            assert_eq!(
                crate::cli::CliError::from(err).exit_code(),
                crate::cli::ExitCode::ConfigError,
                "{field} {value}"
            );
        }
    }

    #[test]
    fn valid_ttl_forms_parse_to_their_durations() {
        let cases = [
            ("0", 0),
            ("60", 60),
            ("1s", 1),
            ("30m", 1_800),
            ("12h", 43_200),
            ("7d", 604_800),
        ];
        for (text, secs) in cases {
            let ttl = CacheTtl::try_from(text.to_string()).unwrap();
            assert_eq!(ttl.get(), Duration::from_secs(secs), "{text}");
        }
    }

    #[test]
    fn valid_size_forms_parse_to_their_bytes() {
        let cases = [
            ("0", 0),
            ("512B", 512),
            ("500KB", 500_000),
            ("500MB", 500_000_000),
            ("2GB", 2_000_000_000),
            ("1TB", 1_000_000_000_000),
        ];
        for (text, bytes) in cases {
            assert_eq!(
                ByteSize::try_from(text.to_string()).unwrap().get(),
                bytes,
                "{text}"
            );
        }
    }

    // - Per-registry fields ------------------------------------------------

    #[test]
    fn deserialize_registry_max_concurrent() {
        let yaml = r#"
registries:
  chainguard:
    url: cgr.dev
    max_concurrent: 10
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let reg = &config.registries["chainguard"];
        assert_eq!(reg.max_concurrent.map(NonZeroUsize::get), Some(10));
    }

    #[test]
    fn registry_max_concurrent_defaults_none() {
        let yaml = r#"
registries:
  hub:
    url: registry-1.docker.io
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.registries["hub"].max_concurrent.is_none());
    }

    #[test]
    fn registry_max_concurrent_zero_is_refused() {
        let yaml = r#"
registries:
  example:
    url: example.com
    max_concurrent: 0
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let err = serde_yaml::from_str::<Config>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_concurrent"), "{err}");
        assert!(err.contains("must be at least 1"), "{err}");
    }

    // - Auth-type validation -------------------------------------------------

    #[test]
    fn validate_basic_without_credentials_is_error() {
        let yaml = r#"
registries:
  ghcr:
    url: ghcr.io
    auth_type: basic
mappings:
  - from: library/nginx
    source: ghcr
    targets: [ghcr]
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_registry("ghcr", config.registries.get("ghcr").unwrap());
        assert!(err.is_err());
        assert!(
            err.unwrap_err().to_string().contains("credentials"),
            "error should mention credentials"
        );
    }

    #[test]
    fn validate_basic_with_credentials_is_ok() {
        let yaml = r#"
registries:
  ghcr:
    url: ghcr.io
    auth_type: basic
    credentials:
      username: myuser
      password: mypass
mappings:
  - from: library/nginx
    source: ghcr
    targets: [ghcr]
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let result = validate_registry("ghcr", config.registries.get("ghcr").unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_token_without_token_field_is_error() {
        let yaml = r#"
registries:
  quay:
    url: quay.io
    auth_type: token
mappings:
  - from: library/nginx
    source: quay
    targets: [quay]
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = validate_registry("quay", config.registries.get("quay").unwrap());
        assert!(err.is_err());
        assert!(
            err.unwrap_err().to_string().contains("token"),
            "error should mention token"
        );
    }

    #[test]
    fn validate_token_with_token_field_is_ok() {
        let yaml = r#"
registries:
  quay:
    url: quay.io
    auth_type: token
    token: ghp_abc123
mappings:
  - from: library/nginx
    source: quay
    targets: [quay]
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let result = validate_registry("quay", config.registries.get("quay").unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn credentials_config_deserializes() {
        let yaml = r#"
registries:
  private:
    url: registry.example.com
    auth_type: basic
    credentials:
      username: user
      password: pass
mappings:
  - from: myapp
    source: private
    targets: [private]
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let reg = config.registries.get("private").unwrap();
        let creds = reg.credentials.as_ref().unwrap();
        assert_eq!(creds.username, "user");
        assert_eq!(creds.password, "pass");
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let creds = BasicCredentials {
            username: "admin".to_string(),
            password: "super-secret".to_string(),
        };
        let debug_output = format!("{creds:?}");
        assert!(debug_output.contains("admin"));
        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("super-secret"));
    }

    #[test]
    fn registry_debug_redacts_token() {
        let registry = RegistryConfig {
            url: "example.com".to_string(),
            auth_type: Some(AuthType::StaticToken),
            max_concurrent: None,
            credentials: None,
            token: Some("secret-bearer-token".to_string()),
            head_first: false,
            aws_profile: None,
        };
        let debug_output = format!("{registry:?}");
        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("secret-bearer-token"));
    }

    #[test]
    fn parses_registry_with_aws_profile() {
        let yaml = r#"
registries:
  vendor:
    url: 222222222222.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr
    aws_profile: vendor
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let reg = &config.registries["vendor"];
        assert_eq!(reg.auth_type, Some(AuthType::Ecr));
        assert_eq!(reg.aws_profile.as_deref(), Some("vendor"));
    }

    #[test]
    fn registry_aws_profile_defaults_to_none() {
        let yaml = r#"
registries:
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.registries["ecr"].aws_profile, None);
    }

    // - ArtifactsConfig -------------------------------------------------------

    #[test]
    fn deserialize_artifacts_config() {
        let yaml = r#"
defaults:
  artifacts:
    enabled: true
    include:
      - "application/vnd.dev.cosign.artifact.sig.v1+json"
    exclude:
      - "application/spdx+json"
    require_artifacts: false
mappings:
  - from: nginx
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let artifacts = config
            .defaults
            .as_ref()
            .unwrap()
            .artifacts
            .as_ref()
            .unwrap();
        assert!(artifacts.enabled);
        assert_eq!(artifacts.include.len(), 1);
        assert_eq!(artifacts.exclude.len(), 1);
        assert!(!artifacts.require_artifacts);
    }

    #[test]
    fn require_artifacts_with_disabled_is_error() {
        let artifacts = ArtifactsConfig {
            enabled: false,
            include: Vec::new(),
            exclude: Vec::new(),
            require_artifacts: true,
        };
        let err = validate_artifacts("test-mapping", &artifacts).unwrap_err();
        match err {
            ConfigError::Validation(msg) => {
                assert!(msg.contains("require_artifacts"), "msg: {msg}");
                assert!(msg.contains("enabled"), "msg: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn require_artifacts_with_enabled_is_ok() {
        let artifacts = ArtifactsConfig {
            enabled: true,
            include: Vec::new(),
            exclude: Vec::new(),
            require_artifacts: true,
        };
        validate_artifacts("test-mapping", &artifacts).unwrap();
    }

    #[test]
    fn disabled_artifacts_without_require_is_ok() {
        let artifacts = ArtifactsConfig {
            enabled: false,
            include: Vec::new(),
            exclude: Vec::new(),
            require_artifacts: false,
        };
        validate_artifacts("test-mapping", &artifacts).unwrap();
    }

    // - TagsConfig::exact_tags() -------------------------------------------

    #[test]
    fn exact_tags_plain_list() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::List(vec!["latest".into(), "3.20".into()])),
            ..Default::default()
        };
        assert_eq!(
            tags.exact_tags(),
            Some(vec!["latest".into(), "3.20".into()])
        );
    }

    #[test]
    fn exact_tags_single() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("v1.0.0".into())),
            ..Default::default()
        };
        assert_eq!(tags.exact_tags(), Some(vec!["v1.0.0".into()]));
    }

    #[test]
    fn exact_tags_with_wildcard_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("v1.*".into())),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_with_semver_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("latest".into())),
            semver: Some(Range::parse(">=1.0").unwrap()),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_with_latest_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("latest".into())),
            latest: Some(5),
            sort: Some(SortOrder::Semver),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_no_glob_returns_none() {
        let tags = TagsConfig::default();
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_question_mark_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("v1.?".into())),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_bracket_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("[0-9]*".into())),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_with_exclude_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::List(vec!["v1.0".into(), "v2.0".into()])),
            exclude: Some(GlobOrList::Single("v2.0".into())),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    #[test]
    fn exact_tags_with_include_returns_none() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::List(vec!["v2.13.0".into()])),
            include: Some(GlobOrList::Single("latest".into())),
            ..Default::default()
        };
        assert!(tags.exact_tags().is_none());
    }

    // - ArtifactsConfig -------------------------------------------------------

    #[test]
    fn artifacts_defaults_enabled() {
        let yaml = r#"
mappings:
  - from: nginx
    artifacts:
      require_artifacts: false
    tags:
      glob: "*"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let artifacts = config.mappings[0].artifacts.as_ref().unwrap();
        assert!(artifacts.enabled, "enabled should default to true");
    }

    // - aws_profile validation -----------------------------------------------

    #[test]
    fn aws_profile_with_ecr_auth_type_passes() {
        let registry = RegistryConfig {
            url: "123456789012.dkr.ecr.us-east-1.amazonaws.com".to_string(),
            auth_type: Some(AuthType::Ecr),
            max_concurrent: None,
            credentials: None,
            token: None,
            head_first: false,
            aws_profile: Some("vendor".to_string()),
        };
        validate_registry("vendor", &registry).unwrap();
    }

    #[test]
    fn aws_profile_without_auth_type_fails() {
        let registry = RegistryConfig {
            url: "123456789012.dkr.ecr.us-east-1.amazonaws.com".to_string(),
            auth_type: None,
            max_concurrent: None,
            credentials: None,
            token: None,
            head_first: false,
            aws_profile: Some("vendor".to_string()),
        };
        let err = validate_registry("vendor", &registry)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("aws_profile"),
            "error must mention aws_profile: {err}"
        );
        assert!(
            err.contains("auth_type"),
            "error must mention auth_type: {err}"
        );
        assert!(err.contains("ecr"), "error must mention ecr: {err}");
    }

    #[test]
    fn aws_profile_with_non_ecr_auth_type_fails() {
        let registry = RegistryConfig {
            url: "ghcr.io".to_string(),
            auth_type: Some(AuthType::Basic),
            max_concurrent: None,
            credentials: Some(BasicCredentials {
                username: "u".into(),
                password: "p".into(),
            }),
            token: None,
            head_first: false,
            aws_profile: Some("vendor".to_string()),
        };
        let err = validate_registry("vendor", &registry)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("aws_profile"),
            "error must mention aws_profile: {err}"
        );
        assert!(err.contains("ecr"), "error must mention ecr: {err}");
    }

    #[test]
    fn aws_profile_empty_string_fails() {
        let registry = RegistryConfig {
            url: "123456789012.dkr.ecr.us-east-1.amazonaws.com".to_string(),
            auth_type: Some(AuthType::Ecr),
            max_concurrent: None,
            credentials: None,
            token: None,
            head_first: false,
            aws_profile: Some(String::new()),
        };
        let err = validate_registry("vendor", &registry)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("aws_profile"),
            "error must mention aws_profile: {err}"
        );
        assert!(err.contains("empty"), "error must mention empty: {err}");
    }
}
