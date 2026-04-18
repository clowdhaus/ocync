//! The `sync` subcommand - runs all mappings from config.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ocync_distribution::auth::detect::{ProviderKind, detect_provider_kind};
use ocync_distribution::ecr::{BatchBlobChecker, BatchChecker};
use ocync_distribution::{RegistryClient, RepositoryName};
use ocync_sync::SyncReport;
use ocync_sync::cache::TransferStateCache;
use ocync_sync::engine::{
    DEFAULT_MAX_CONCURRENT_TRANSFERS, RegistryAlias, ResolvedMapping, SyncEngine, TagPair,
    TargetEntry,
};
use ocync_sync::filter::FilterConfig;
use ocync_sync::retry::RetryConfig;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;

use crate::SyncArgs;
use crate::cli::config::{
    AuthType, Config, GlobOrList, MappingConfig, TagsConfig, load_config, resolve_target_names,
};
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client};

/// Default cache TTL: 12 hours.
pub(crate) const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(12 * 3600);

/// Default cache file name within the cache directory.
const CACHE_FILE_NAME: &str = "transfer_state.bin";

/// Resolve the cache directory and file path from config.
///
/// Uses `global.cache_dir` if configured, otherwise places the cache
/// directory adjacent to the config file at `.ocync/cache/`.
pub(crate) fn resolve_cache_path(config: &Config, config_file: &Path) -> (PathBuf, PathBuf) {
    let cache_dir = config
        .global
        .as_ref()
        .and_then(|g| g.cache_dir.as_deref())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            config_file
                .parent()
                .unwrap_or(Path::new("."))
                .join(".ocync/cache")
        });
    let cache_path = cache_dir.join(CACHE_FILE_NAME);
    (cache_dir, cache_path)
}

/// Parse and return the cache TTL from config, defaulting to 12 hours.
pub(crate) fn resolve_cache_ttl(config: &Config) -> Duration {
    config
        .global
        .as_ref()
        .and_then(|g| g.cache_ttl.as_deref())
        .and_then(parse_duration)
        .unwrap_or(DEFAULT_CACHE_TTL)
}

/// Run the sync command: load config, resolve mappings, and execute.
///
/// The `shutdown` signal, if provided, will be forwarded to the engine for
/// graceful drain on SIGINT/SIGTERM.
pub(crate) async fn run(
    args: &SyncArgs,
    progress: &dyn ocync_sync::progress::ProgressReporter,
    shutdown: Option<&ShutdownSignal>,
    external_cache: Option<Rc<RefCell<TransferStateCache>>>,
) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;

    let clients = build_clients(&config).await?;
    let batch_checkers = build_batch_checkers(&config).await?;

    let mut mappings = Vec::new();
    for mapping in &config.mappings {
        match resolve_mapping(mapping, &config, &clients, &batch_checkers).await {
            Ok(Some(resolved)) => mappings.push(resolved),
            Ok(None) => {} // no tags after filtering, logged inside
            Err(err) => return Err(err),
        }
    }

    if args.dry_run {
        print_dry_run(&mappings);
        return Ok(ExitCode::Success);
    }

    let (cache_dir, cache_path) = resolve_cache_path(&config, &args.config);
    let cache_ttl = resolve_cache_ttl(&config);
    let (cache, should_persist) = match external_cache {
        Some(ext) => (ext, false),
        None => {
            let loaded = Rc::new(RefCell::new(TransferStateCache::load(
                &cache_path,
                cache_ttl,
            )));
            (loaded, true)
        }
    };

    // Enable disk staging when multiple targets OR multiple images share blobs.
    // Multi-target: pull once from source, push to N targets from disk.
    // Multi-image: pull once, push from staging when the same blob appears in
    // another image (cross-image source dedup).
    //
    // Trade-off: this is a conservative heuristic - disjoint mappings pay a
    // disk round-trip per blob for zero benefit. Tighter detection would
    // require manifest data (unavailable pre-discovery). The overhead is
    // small (local I/O) relative to the network savings when blobs overlap.
    let needs_staging = mappings.iter().any(|m| m.targets.len() > 1) || mappings.len() > 1;
    let staging = if needs_staging {
        let stage = BlobStage::new(cache_dir.join("blobs"));
        if let Err(e) = stage.cleanup_tmp_files() {
            tracing::warn!(error = %e, "failed to clean staging tmp files");
        }
        // Evict stale blobs from previous runs before starting new work.
        if let Some(limit) = config
            .global
            .as_ref()
            .and_then(|g| g.staging_size_limit.as_deref())
            .and_then(parse_size)
            && let Err(e) = stage.evict(limit)
        {
            tracing::warn!(error = %e, "failed to evict staged blobs");
        }
        stage
    } else {
        BlobStage::disabled()
    };

    let max_concurrent = config
        .global
        .as_ref()
        .map_or(DEFAULT_MAX_CONCURRENT_TRANSFERS, |g| {
            g.max_concurrent_transfers
        });
    let engine = SyncEngine::new(RetryConfig::default(), max_concurrent);
    let report = engine
        .run(mappings, cache.clone(), staging, progress, shutdown)
        .await;

    // Persist only when we own the cache (sync command). Watch mode persists on shutdown.
    if should_persist && let Err(e) = cache.borrow().persist(&cache_path) {
        tracing::error!(error = %e, "failed to persist transfer state cache");
    }

    write_output(&report, args.json)?;

    match report.exit_code() {
        0 => Ok(ExitCode::Success),
        1 => Ok(ExitCode::PartialFailure),
        _ => Ok(ExitCode::Failure),
    }
}

/// Parse a human-readable duration string into a [`Duration`].
///
/// Accepts:
/// - `"0"` - [`Duration::ZERO`]
/// - `"<N>s"` - N seconds
/// - `"<N>m"` - N minutes
/// - `"<N>h"` - N hours
/// - `"<N>d"` - N days
/// - `"<N>"` (no suffix) - N seconds
///
/// Returns `None` for unrecognised strings - callers must decide how to
/// handle invalid input rather than silently receiving a default.
fn parse_duration(s: &str) -> Option<Duration> {
    if s == "0" {
        return Some(Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let last = &s[s.len() - 1..];
    let (digits, multiplier) = match last {
        "s" => (&s[..s.len() - 1], 1u64),
        "m" => (&s[..s.len() - 1], 60),
        "h" => (&s[..s.len() - 1], 3600),
        "d" => (&s[..s.len() - 1], 86400),
        _ if s.chars().all(|c| c.is_ascii_digit()) => (s, 1),
        _ => return None,
    };
    digits
        .parse::<u64>()
        .ok()
        .map(|n| Duration::from_secs(n * multiplier))
}

/// Parse a human-readable size string into bytes.
///
/// Accepts `"0"`, `"<N>B"`, `"<N>KB"`, `"<N>MB"`, `"<N>GB"`, `"<N>TB"`.
/// Returns `None` for unrecognised strings.
fn parse_size(s: &str) -> Option<u64> {
    if s == "0" {
        return Some(0);
    }
    for (suffix, multiplier) in &[
        ("TB", 1_000_000_000_000u64),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("KB", 1_000),
        ("B", 1),
    ] {
        if let Some(digits) = s.strip_suffix(suffix) {
            return digits.parse::<u64>().ok().map(|n| n * multiplier);
        }
    }
    None
}

/// Build a `RegistryClient` for each named registry in config, keyed by name.
pub(crate) async fn build_clients(
    config: &Config,
) -> Result<HashMap<String, Arc<RegistryClient>>, CliError> {
    let mut clients = HashMap::with_capacity(config.registries.len());
    for (name, reg) in &config.registries {
        let hostname = bare_hostname(&reg.url);
        let client = build_registry_client(hostname, Some(reg)).await?;
        clients.insert(name.clone(), Arc::new(client));
    }
    Ok(clients)
}

/// Build batch blob checkers for ECR registries.
///
/// Automatically creates a [`BatchChecker`] for every registry detected
/// as ECR (via explicit `auth_type: ecr` or hostname auto-detection). No
/// user configuration is needed - if we know it's ECR, we use the batch API.
async fn build_batch_checkers(
    config: &Config,
) -> Result<HashMap<String, Rc<dyn BatchBlobChecker>>, CliError> {
    let mut checkers: HashMap<String, Rc<dyn BatchBlobChecker>> = HashMap::new();

    for (name, reg) in &config.registries {
        let hostname = bare_hostname(&reg.url);
        let is_ecr = reg.auth_type.as_ref().is_some_and(|a| *a == AuthType::Ecr)
            || detect_provider_kind(hostname) == Some(ProviderKind::Ecr);

        if !is_ecr {
            continue;
        }

        let checker = BatchChecker::from_hostname(hostname)
            .await
            .map_err(|e| CliError::Input(format!("ECR batch checker for '{name}': {e}")))?;
        checkers.insert(name.clone(), Rc::new(checker));
    }

    Ok(checkers)
}

/// Resolve a single mapping config into a `ResolvedMapping`, or `None` if no
/// tags survive filtering.
///
/// Falls back to `defaults.source`, `defaults.targets`, and `defaults.tags`
/// when the mapping does not specify its own values.
pub(crate) async fn resolve_mapping(
    mapping: &MappingConfig,
    config: &Config,
    clients: &HashMap<String, Arc<RegistryClient>>,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
) -> Result<Option<ResolvedMapping>, CliError> {
    // --- Source registry ---
    let source_name = mapping
        .source
        .as_deref()
        .or(config.defaults.as_ref().and_then(|d| d.source.as_deref()))
        .ok_or_else(|| {
            CliError::Input(format!(
                "mapping '{}': no source registry (set mapping.source or defaults.source)",
                mapping.from,
            ))
        })?;

    let source_client = clients.get(source_name).cloned().ok_or_else(|| {
        CliError::Input(format!(
            "mapping '{}': source registry '{}' not found in clients",
            mapping.from, source_name,
        ))
    })?;

    // --- Target registries ---
    let targets_value = mapping
        .targets
        .as_ref()
        .or(config.defaults.as_ref().and_then(|d| d.targets.as_ref()))
        .ok_or_else(|| {
            CliError::Input(format!(
                "mapping '{}': no target registries (set mapping.targets or defaults.targets)",
                mapping.from,
            ))
        })?;

    let known: std::collections::HashSet<&str> =
        config.registries.keys().map(String::as_str).collect();
    let context = format!("mapping '{}'", mapping.from);
    let target_names =
        resolve_target_names(targets_value, config, &known, &context).map_err(CliError::Config)?;

    let targets: Vec<TargetEntry> = target_names
        .into_iter()
        .map(|name| {
            let client = clients.get(&name).cloned().ok_or_else(|| {
                CliError::Input(format!(
                    "mapping '{}': target registry '{}' not found in clients",
                    mapping.from, name,
                ))
            })?;
            let batch_checker = batch_checkers.get(&name).cloned();
            Ok(TargetEntry {
                name: RegistryAlias::new(name),
                client,
                batch_checker,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    // --- Fetch and filter tags ---
    let source_repo_path = RepositoryName::new(&mapping.from);
    let all_tags = source_client.list_tags(&source_repo_path).await?;

    let tags_config = mapping
        .tags
        .as_ref()
        .or(config.defaults.as_ref().and_then(|d| d.tags.as_ref()));

    let filter = build_filter(tags_config);
    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    let filtered = filter.apply(&tag_refs)?;

    if filtered.is_empty() {
        tracing::warn!(
            from = %mapping.from,
            "no tags matched after filtering, skipping mapping"
        );
        return Ok(None);
    }

    // --- Target repo ---
    let target_repo = mapping.to.as_deref().unwrap_or(&mapping.from).to_owned();

    // --- Resolve platforms and skip_existing (mapping overrides defaults) ---
    let platform_strs = mapping
        .platforms
        .clone()
        .or_else(|| config.defaults.as_ref().and_then(|d| d.platforms.clone()));
    let platforms = platform_strs
        .map(|strs| {
            strs.iter()
                .map(|s| s.parse())
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    let skip_existing = mapping
        .skip_existing
        .or_else(|| config.defaults.as_ref().and_then(|d| d.skip_existing))
        .unwrap_or(false);

    let source_authority = source_client
        .registry_authority()
        .map_err(|e| CliError::Input(format!("mapping '{}': {e}", mapping.from)))?;

    Ok(Some(ResolvedMapping {
        source_authority,
        source_client,
        source_repo: mapping.from.clone().into(),
        target_repo: target_repo.into(),
        targets,
        tags: filtered.into_iter().map(TagPair::same).collect(),
        platforms,
        skip_existing,
    }))
}

/// Build a `FilterConfig` from a `TagsConfig`, falling back to defaults.
fn build_filter(tags: Option<&TagsConfig>) -> FilterConfig {
    let Some(tags) = tags else {
        return FilterConfig::default();
    };

    FilterConfig {
        glob: glob_or_list_to_vec(tags.glob.as_ref()),
        semver: tags.semver.clone(),
        semver_prerelease: tags.semver_prerelease,
        exclude: glob_or_list_to_vec(tags.exclude.as_ref()),
        sort: tags.sort,
        latest: tags.latest,
        min_tags: tags.min_tags,
    }
}

/// Flatten a `GlobOrList` into a `Vec<String>`.
fn glob_or_list_to_vec(g: Option<&GlobOrList>) -> Vec<String> {
    match g {
        Some(GlobOrList::Single(s)) => vec![s.clone()],
        Some(GlobOrList::List(v)) => v.clone(),
        None => Vec::new(),
    }
}

/// Print dry-run output showing what would be synced.
fn print_dry_run(mappings: &[ResolvedMapping]) {
    if mappings.is_empty() {
        println!("dry-run: no mappings to sync");
        return;
    }

    for mapping in mappings {
        let target_names: Vec<&str> = mapping.targets.iter().map(|t| &*t.name).collect();
        println!(
            "dry-run: {} -> {} ({} tag(s)) => [{}]",
            mapping.source_repo,
            mapping.target_repo,
            mapping.tags.len(),
            target_names.join(", "),
        );
        for tag_pair in &mapping.tags {
            if tag_pair.source == tag_pair.target {
                println!("  {}", tag_pair.source);
            } else {
                println!("  {} -> {}", tag_pair.source, tag_pair.target);
            }
        }
    }
}

/// Write sync output as JSON when `--json` is passed.
fn write_output(report: &SyncReport, json: bool) -> Result<(), CliError> {
    if json {
        let json = serde_json::to_string_pretty(report)
            .map_err(|e| CliError::Input(format!("failed to serialize report: {e}")))?;
        println!("{json}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_zero() {
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
    }

    #[test]
    fn parse_duration_seconds_suffix() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_duration_minutes_suffix() {
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(30 * 60)));
    }

    #[test]
    fn parse_duration_hours_suffix() {
        assert_eq!(parse_duration("12h"), Some(Duration::from_secs(12 * 3600)));
    }

    #[test]
    fn parse_duration_days_suffix() {
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86400)));
    }

    #[test]
    fn parse_duration_no_suffix_treated_as_seconds() {
        assert_eq!(parse_duration("60"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn parse_duration_invalid_returns_none() {
        assert_eq!(parse_duration("invalid"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("12hours"), None);
    }

    #[test]
    fn build_filter_none_returns_default() {
        let filter = build_filter(None);
        assert!(filter.glob.is_empty());
        assert!(filter.semver.is_none());
        assert!(filter.exclude.is_empty());
        assert!(filter.sort.is_none());
        assert!(filter.latest.is_none());
        assert!(filter.min_tags.is_none());
    }

    #[test]
    fn build_filter_single_glob() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("v1.*".into())),
            ..Default::default()
        };
        let filter = build_filter(Some(&tags));
        assert_eq!(filter.glob, vec!["v1.*"]);
    }

    #[test]
    fn build_filter_glob_list() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::List(vec!["v1.*".into(), "v2.*".into()])),
            ..Default::default()
        };
        let filter = build_filter(Some(&tags));
        assert_eq!(filter.glob, vec!["v1.*", "v2.*"]);
    }

    #[test]
    fn build_filter_exclude_patterns() {
        let tags = TagsConfig {
            exclude: Some(GlobOrList::List(vec!["*-rc*".into(), "*-beta*".into()])),
            ..Default::default()
        };
        let filter = build_filter(Some(&tags));
        assert_eq!(filter.exclude, vec!["*-rc*", "*-beta*"]);
    }

    #[test]
    fn build_filter_full() {
        use ocync_sync::filter::{SemverPrerelease, SortOrder};

        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("*".into())),
            semver: Some(">=1.0.0".into()),
            semver_prerelease: Some(SemverPrerelease::Exclude),
            exclude: Some(GlobOrList::Single("*-alpine".into())),
            sort: Some(SortOrder::Semver),
            latest: Some(5),
            min_tags: Some(1),
        };
        let filter = build_filter(Some(&tags));
        assert_eq!(filter.glob, vec!["*"]);
        assert_eq!(filter.semver.as_deref(), Some(">=1.0.0"));
        assert_eq!(filter.semver_prerelease, Some(SemverPrerelease::Exclude));
        assert_eq!(filter.exclude, vec!["*-alpine"]);
        assert_eq!(filter.sort, Some(SortOrder::Semver));
        assert_eq!(filter.latest, Some(5));
        assert_eq!(filter.min_tags, Some(1));
    }

    #[test]
    fn glob_or_list_to_vec_none() {
        assert!(glob_or_list_to_vec(None).is_empty());
    }

    #[test]
    fn glob_or_list_to_vec_single() {
        let g = GlobOrList::Single("pattern".into());
        assert_eq!(glob_or_list_to_vec(Some(&g)), vec!["pattern"]);
    }

    #[test]
    fn glob_or_list_to_vec_list() {
        let g = GlobOrList::List(vec!["a".into(), "b".into()]);
        assert_eq!(glob_or_list_to_vec(Some(&g)), vec!["a", "b"]);
    }

    // - parse_size -----------------------------------------------------------

    #[test]
    fn parse_size_zero() {
        assert_eq!(parse_size("0"), Some(0));
    }

    #[test]
    fn parse_size_bytes() {
        assert_eq!(parse_size("512B"), Some(512));
    }

    #[test]
    fn parse_size_kilobytes() {
        assert_eq!(parse_size("500KB"), Some(500_000));
    }

    #[test]
    fn parse_size_megabytes() {
        assert_eq!(parse_size("500MB"), Some(500_000_000));
    }

    #[test]
    fn parse_size_gigabytes() {
        assert_eq!(parse_size("2GB"), Some(2_000_000_000));
    }

    #[test]
    fn parse_size_terabytes() {
        assert_eq!(parse_size("1TB"), Some(1_000_000_000_000));
    }

    #[test]
    fn parse_size_invalid_returns_none() {
        assert_eq!(parse_size("2gigabytes"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
    }
}
