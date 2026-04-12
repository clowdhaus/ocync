//! The `sync` subcommand -- runs all mappings from config.

use std::collections::HashMap;
use std::sync::Arc;

use ocync_distribution::RegistryClient;
use ocync_sync::SyncReport;
use ocync_sync::engine::{ResolvedMapping, SyncEngine, TargetEntry};
use ocync_sync::filter::FilterConfig;
use ocync_sync::progress::NullProgress;
use ocync_sync::retry::RetryConfig;

use crate::cli::config::{
    Config, GlobOrList, MappingConfig, TagsConfig, load_config, resolve_target_names,
};
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client};
use crate::{OutputFormat, SyncArgs};

/// Run the sync command: load config, resolve mappings, and execute.
pub(crate) async fn run(args: &SyncArgs) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config[0])?;

    let clients = build_clients(&config).await?;

    let mut mappings = Vec::new();
    for mapping in &config.mappings {
        match resolve_mapping(mapping, &config, &clients).await {
            Ok(Some(resolved)) => mappings.push(resolved),
            Ok(None) => {} // no tags after filtering, logged inside
            Err(err) => return Err(err),
        }
    }

    if args.dry_run {
        print_dry_run(&mappings);
        return Ok(ExitCode::Success);
    }

    let engine = SyncEngine::new(RetryConfig::default());
    let progress = NullProgress;
    let report = engine.run(mappings, &progress).await;

    write_output(args, &report)?;

    match report.exit_code() {
        0 => Ok(ExitCode::Success),
        1 => Ok(ExitCode::Failure),
        _ => Ok(ExitCode::Error),
    }
}

/// Build a `RegistryClient` for each named registry in config, keyed by name.
async fn build_clients(config: &Config) -> Result<HashMap<String, Arc<RegistryClient>>, CliError> {
    let mut clients = HashMap::with_capacity(config.registries.len());
    for (name, reg) in &config.registries {
        let hostname = bare_hostname(&reg.url);
        let client = build_registry_client(hostname, reg.auth_type.as_ref()).await?;
        clients.insert(name.clone(), Arc::new(client));
    }
    Ok(clients)
}

/// Resolve a single mapping config into a `ResolvedMapping`, or `None` if no
/// tags survive filtering.
///
/// Falls back to `defaults.source`, `defaults.targets`, and `defaults.tags`
/// when the mapping does not specify its own values.
async fn resolve_mapping(
    mapping: &MappingConfig,
    config: &Config,
    clients: &HashMap<String, Arc<RegistryClient>>,
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
            Ok(TargetEntry { name, client })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    // --- Fetch and filter tags ---
    let all_tags = source_client.list_tags(&mapping.from).await?;

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

    Ok(Some(ResolvedMapping {
        source_client,
        source_repo: mapping.from.clone(),
        target_repo,
        targets,
        tags: filtered,
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
        let target_names: Vec<&str> = mapping.targets.iter().map(|t| t.name.as_str()).collect();
        println!(
            "dry-run: {} -> {} ({} tag(s)) => [{}]",
            mapping.source_repo,
            mapping.target_repo,
            mapping.tags.len(),
            target_names.join(", "),
        );
        for tag in &mapping.tags {
            println!("  {tag}");
        }
    }
}

/// Write sync output in the requested format.
fn write_output(args: &SyncArgs, report: &SyncReport) -> Result<(), CliError> {
    let is_json = matches!(args.output_format, Some(OutputFormat::Json));

    if is_json {
        let json = serde_json::to_string_pretty(report)
            .map_err(|e| CliError::Input(format!("failed to serialize report: {e}")))?;

        if let Some(ref path) = args.output {
            std::fs::write(path, &json).map_err(|e| {
                CliError::Input(format!(
                    "failed to write output to '{}': {e}",
                    path.display()
                ))
            })?;
        } else {
            println!("{json}");
        }
    } else {
        print_summary(report);

        if let Some(ref path) = args.output {
            let json = serde_json::to_string_pretty(report)
                .map_err(|e| CliError::Input(format!("failed to serialize report: {e}")))?;
            std::fs::write(path, &json).map_err(|e| {
                CliError::Input(format!(
                    "failed to write output to '{}': {e}",
                    path.display()
                ))
            })?;
        }
    }

    Ok(())
}

/// Print a human-readable summary of the sync report.
fn print_summary(report: &SyncReport) {
    let s = &report.stats;
    println!(
        "sync complete: {} synced, {} skipped, {} failed ({}, {:.1}s)",
        s.images_synced,
        s.images_skipped,
        s.images_failed,
        format_bytes(s.bytes_transferred),
        report.duration.as_secs_f64(),
    );
}

/// Format a byte count as a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_bytes() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn format_bytes_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }

    #[test]
    fn format_bytes_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
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
}
