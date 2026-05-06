//! The `sync` subcommand - runs all mappings from config.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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
    DEFAULT_MAX_CONCURRENT_TRANSFERS, RegistryAlias, ResolvedArtifacts, ResolvedMapping,
    SyncEngine, TagPair, TargetEntry,
};
use ocync_sync::filter::{FilterConfig, build_glob_set, is_referrers_fallback_tag};
use ocync_sync::retry::RetryConfig;
use ocync_sync::shutdown::ShutdownSignal;
use ocync_sync::staging::BlobStage;

use crate::SyncArgs;
use crate::cli::config::{
    AuthType, Config, GlobOrList, MappingConfig, TagsConfig, load_config, resolve_target_names,
};
use crate::cli::output::{format_bytes, format_duration};
use crate::cli::{CliError, ExitCode, bare_hostname, build_registry_client};

/// Default cache TTL: 12 hours.
pub(crate) const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(12 * 3600);

/// Default cache file name within the cache directory.
const CACHE_FILE_NAME: &str = "transfer_state.bin";

/// Sample cap for the source-tag list shown in the no-tags-matched WARN.
/// Mirrors `dry_run::SAMPLE_CAP` so both surfaces show the same depth of
/// example data without overwhelming the log line.
const NO_TAGS_SAMPLE_CAP: usize = 5;

/// Outcome of resolving a single mapping. Either the mapping is ready for the
/// engine, or no source tag survived filtering and the caller decides whether
/// to log a WARN (sync mode: always; watch mode: only on transition).
///
/// The size disparity between variants is intentional: `ResolvedMapping` flows
/// directly into `Vec<ResolvedMapping>` for the engine, so boxing it would
/// just add a heap round-trip per success. The error variant is rare; we pay
/// the disparity instead of the allocation traffic.
#[allow(clippy::large_enum_variant)]
pub(crate) enum MappingResolution {
    Resolved(ResolvedMapping),
    NoMatchingTags(NoTagsInfo),
}

/// Diagnostic context for a mapping whose filter rejected every source tag.
///
/// Fields together let an operator see, in one log line, the size and
/// composition of the source repo (image tags vs OCI 1.1 referrer fallbacks),
/// the active filter clauses, and example image tag names so the cause is
/// obvious without spelunking.
pub(crate) struct NoTagsInfo {
    pub from: String,
    pub image_count: usize,
    pub artifact_count: usize,
    /// Active filter clauses (e.g. `semver >=1.0.0, latest=5`). `None` only
    /// when no filter is configured -- distinct from "filter description
    /// missing" so the formatter can render an explicit fallback string.
    pub filter_desc: Option<String>,
    /// Up to [`NO_TAGS_SAMPLE_CAP`] image-tag names. Excludes referrer
    /// fallback tags so the example list is meaningful on cosign-heavy
    /// repos like `cgr.dev/chainguard/*` (otherwise dominated by
    /// `sha256-<hex>(.sig|.sbom|.att)` entries).
    pub samples: Vec<String>,
}

impl NoTagsInfo {
    /// Total tags returned by `/v2/<repo>/tags/list`. Derived: image + artifact.
    fn source_total(&self) -> usize {
        self.image_count + self.artifact_count
    }

    /// True when the source had more image tags than `samples` shows.
    fn samples_truncated(&self) -> bool {
        self.image_count > self.samples.len()
    }
}

impl std::fmt::Display for NoTagsInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.source_total();
        let total_phrase = if self.artifact_count > 0 {
            format!(
                "{total} source tags ({} image tags, {} referrer artifacts)",
                self.image_count, self.artifact_count
            )
        } else {
            format!("{total} source tags")
        };
        let filter = self
            .filter_desc
            .as_deref()
            .unwrap_or("no filter configured");
        let samples = if self.samples.is_empty() {
            "<empty>".to_string()
        } else if self.samples_truncated() {
            format!("[{}, ...]", self.samples.join(", "))
        } else {
            format!("[{}]", self.samples.join(", "))
        };
        write!(
            f,
            "{}: 0 of {total_phrase} matched filter ({filter}); skipping. Source: {samples}",
            self.from
        )
    }
}

/// Per-process state that lets watch-mode log on transitions instead of
/// every cycle. Sync mode passes `None`. State lives in `watch::run` so it
/// spans loop iterations.
///
/// Tracks three pieces of cross-cycle context:
///
/// 1. No-tags failure set: mappings whose filter rejected every source tag.
/// 2. Per-mapping outcomes: the prior cycle's [`MappingOutcome`] keyed by
///    `mapping.from`, used to detect both repeated and recovery transitions.
/// 3. Per-cycle emit counter: bumped by every `observe_*` method that
///    reports a transition; the watch loop reads it to gate the idle
///    heartbeat.
#[derive(Debug, Default)]
pub(crate) struct WatchLogState {
    warned_no_tags: HashSet<String>,
    last_outcomes: HashMap<String, MappingOutcome>,
    cycle_emit_count: u32,
}

impl WatchLogState {
    pub(crate) fn begin_cycle(&mut self) {
        self.cycle_emit_count = 0;
    }

    pub(crate) fn cycle_emit_count(&self) -> u32 {
        self.cycle_emit_count
    }

    /// Record a no-match observation. Returns `true` on transition into the
    /// failure state (caller emits a WARN); `false` when already failing.
    fn observe_no_match(&mut self, from: &str) -> bool {
        let changed = self.warned_no_tags.insert(from.to_string());
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Record a successful resolution. Returns `true` when the mapping was
    /// previously in the failure set (caller emits a recovery INFO).
    fn observe_resolved(&mut self, from: &str) -> bool {
        let changed = self.warned_no_tags.remove(from);
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Record `outcome` as the latest result for `from`.
    ///
    /// Returns:
    /// - `None` when the outcome is identical to the prior cycle (suppress).
    /// - `Some(false)` on a non-recovery transition (emit normally).
    /// - `Some(true)` when transitioning from `failed > 0` to `failed == 0`
    ///   (emit with `[recovered]` marker).
    fn observe_mapping_outcome(&mut self, from: &str, outcome: &MappingOutcome) -> Option<bool> {
        use std::collections::hash_map::Entry;
        match self.last_outcomes.entry(from.to_string()) {
            Entry::Occupied(mut slot) => {
                let prev = *slot.get();
                if &prev == outcome {
                    return None;
                }
                slot.insert(*outcome);
                self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
                Some(prev.failed > 0 && outcome.failed == 0)
            }
            Entry::Vacant(slot) => {
                slot.insert(*outcome);
                self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
                Some(false)
            }
        }
    }

    /// Drop entries for mappings no longer in the active set so the state
    /// does not grow unbounded across edits to the config.
    fn retain_active<'a>(&mut self, active: impl IntoIterator<Item = &'a str>) {
        let active_set: HashSet<&str> = active.into_iter().collect();
        self.warned_no_tags
            .retain(|k| active_set.contains(k.as_str()));
        self.last_outcomes
            .retain(|k, _| active_set.contains(k.as_str()));
    }
}

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
///
/// Returns an error if the configured value cannot be parsed, rather than
/// silently falling back to the default.
pub(crate) fn resolve_cache_ttl(config: &Config) -> Result<Duration, CliError> {
    match config.global.as_ref().and_then(|g| g.cache_ttl.as_deref()) {
        Some(raw) => parse_duration(raw.trim()).ok_or_else(|| {
            CliError::Input(format!(
                "invalid cache_ttl '{raw}': accepted formats are \"0\", \"<N>s\", \"<N>m\", \"<N>h\", \"<N>d\", or \"<N>\" (seconds)"
            ))
        }),
        None => Ok(DEFAULT_CACHE_TTL),
    }
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
    verbose: bool,
    mut watch_log: Option<&mut WatchLogState>,
) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;

    let clients = build_clients(&config).await?;
    let batch_checkers = build_batch_checkers(&config).await?;

    let mut mappings = Vec::new();
    for mapping in &config.mappings {
        match resolve_mapping(mapping, &config, &clients, &batch_checkers, args.dry_run).await? {
            MappingResolution::Resolved(resolved) => mappings.push(resolved),
            MappingResolution::NoMatchingTags(info) => {
                let should_warn = match watch_log.as_mut() {
                    Some(state) => state.observe_no_match(&info.from),
                    None => true,
                };
                if should_warn {
                    emit_no_tags_warn(&info);
                }
            }
        }
    }

    if let Some(state) = watch_log.as_mut() {
        for resolved in &mappings {
            let from = resolved.source_repo.as_str();
            if state.observe_resolved(from) {
                tracing::info!(
                    from = %from,
                    "{from}: filter now matches at least one tag; resuming sync"
                );
            }
        }
        state.retain_active(config.mappings.iter().map(|m| m.from.as_str()));
    }

    if args.dry_run {
        crate::cli::commands::dry_run::print(&mappings, verbose);
        return Ok(ExitCode::Success);
    }

    let (cache_dir, cache_path) = resolve_cache_path(&config, &args.config);
    let cache_ttl = resolve_cache_ttl(&config)?;
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
        let staging_limit = match config
            .global
            .as_ref()
            .and_then(|g| g.staging_size_limit.as_deref())
        {
            Some(raw) => Some(parse_size(raw.trim()).ok_or_else(|| {
                CliError::Input(format!(
                    "invalid staging_size_limit '{raw}': accepted formats are \"0\", \"<N>B\", \"<N>KB\", \"<N>MB\", \"<N>GB\", \"<N>TB\""
                ))
            })?),
            None => None,
        };
        if let Some(limit) = staging_limit {
            if let Err(e) = stage.evict(limit) {
                tracing::warn!(error = %e, "failed to evict staged blobs");
            }
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
    // Capture per-mapping metadata before the engine consumes `mappings`.
    // Used to emit one INFO line per mapping after the engine returns,
    // grouped from the report's per-image outcomes.
    let descriptors: Vec<MappingDescriptor> = mappings
        .iter()
        .map(|m| MappingDescriptor {
            from: m.source_repo.as_str().to_string(),
            target_repo: m.target_repo.as_str().to_string(),
            target_names: m.targets.iter().map(|t| (*t.name).to_string()).collect(),
        })
        .collect();

    let engine = SyncEngine::new(RetryConfig::default(), max_concurrent);
    let report = engine
        .run(mappings, cache.clone(), staging, progress, shutdown)
        .await;

    // Persist only when we own the cache (sync command). Watch mode persists on shutdown.
    if should_persist {
        if let Err(e) = cache.borrow().persist(&cache_path) {
            tracing::error!(error = %e, "failed to persist transfer state cache");
        }
    }

    emit_mapping_outcomes(&descriptors, &report, watch_log.as_deref_mut());
    // Watch mode: suppress the cycle tail when no per-mapping line emitted
    // (steady-state idle); sync mode: always emit as the final marker.
    let cycle_had_activity = watch_log
        .as_deref()
        .is_none_or(|s| s.cycle_emit_count() > 0);
    if cycle_had_activity {
        emit_cycle_tail(&descriptors, &report);
    }

    write_output(&report, args.json)?;

    Ok(ExitCode::from_report(report.exit_code()))
}

/// Per-mapping metadata captured before the engine consumes `mappings`,
/// so we can join it with the engine's per-image report after the fact
/// to emit one log line per mapping (with source/target context).
struct MappingDescriptor {
    from: String,
    target_repo: String,
    target_names: Vec<String>,
}

/// Per-mapping aggregated outcome derived from [`SyncReport.images`].
/// Used for log emission and watch-mode change detection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MappingOutcome {
    pub synced: u64,
    pub skipped: u64,
    pub failed: u64,
    pub bytes: u64,
}

impl MappingOutcome {
    fn is_empty(&self) -> bool {
        self.synced == 0 && self.skipped == 0 && self.failed == 0
    }
}

/// Emit one INFO (or WARN, on failures) per mapping summarizing what its
/// configured tags did this cycle. In watch mode (when `watch_log` is
/// `Some`), suppress mappings whose outcome is unchanged from the prior
/// cycle so steady-state pods log only on transition.
fn emit_mapping_outcomes(
    descriptors: &[MappingDescriptor],
    report: &SyncReport,
    mut watch_log: Option<&mut WatchLogState>,
) {
    for d in descriptors {
        let outcome = aggregate_mapping_outcome(&d.from, &d.target_repo, report);
        // No images for this mapping in the report (e.g. the mapping was
        // resolved to zero tags by an upstream filter that the engine
        // never saw). The no-tags WARN already covered it; skip here.
        if outcome.is_empty() {
            continue;
        }
        let recovered = match watch_log.as_deref_mut() {
            Some(state) => match state.observe_mapping_outcome(&d.from, &outcome) {
                Some(r) => r,
                None => continue,
            },
            None => false,
        };
        let line = format_mapping_outcome(d, &outcome, recovered);
        // The `tracing::event!` macro requires a const-expression level, so
        // each per-mapping line goes through one of two near-identical
        // arms. Keep the structured field set in sync between them so log
        // aggregators don't see different shapes for warn vs info.
        if outcome.failed > 0 {
            tracing::warn!(
                from = %d.from,
                to = %d.target_repo,
                synced = outcome.synced,
                skipped = outcome.skipped,
                failed = outcome.failed,
                bytes = outcome.bytes,
                recovered,
                "{line}"
            );
        } else {
            tracing::info!(
                from = %d.from,
                to = %d.target_repo,
                synced = outcome.synced,
                skipped = outcome.skipped,
                failed = outcome.failed,
                bytes = outcome.bytes,
                recovered,
                "{line}"
            );
        }
    }
}

fn aggregate_mapping_outcome(
    source_repo: &str,
    target_repo: &str,
    report: &SyncReport,
) -> MappingOutcome {
    let src_prefix = format!("{source_repo}:");
    let tgt_prefix = format!("{target_repo}:");
    let mut o = MappingOutcome::default();
    for r in &report.images {
        if !(r.source.starts_with(&src_prefix) && r.target.starts_with(&tgt_prefix)) {
            continue;
        }
        match r.status {
            ocync_sync::ImageStatus::Synced => {
                o.synced += 1;
                o.bytes += r.bytes_transferred;
            }
            ocync_sync::ImageStatus::Skipped { .. } => o.skipped += 1,
            ocync_sync::ImageStatus::Failed { .. } => o.failed += 1,
        }
    }
    o
}

fn format_mapping_outcome(d: &MappingDescriptor, o: &MappingOutcome, recovered: bool) -> String {
    let mut parts = Vec::with_capacity(3);
    if o.synced > 0 {
        parts.push(format!("synced {}", o.synced));
    }
    if o.skipped > 0 {
        parts.push(format!("skipped {}", o.skipped));
    }
    if o.failed > 0 {
        parts.push(format!("failed {}", o.failed));
    }
    let counts = parts.join(", ");
    let bytes_clause = if o.bytes > 0 {
        format!(" ({})", format_bytes(o.bytes))
    } else {
        String::new()
    };
    let recovered_clause = if recovered { " [recovered]" } else { "" };
    // Multi-target mappings need the bracket to disambiguate which targets
    // the line refers to. Single-target mappings: omit -- the destination
    // is already in the `from -> to` arrow.
    let targets_clause = if d.target_names.len() > 1 {
        format!(" [{}]", d.target_names.join(", "))
    } else {
        String::new()
    };
    format!(
        "{} -> {}{targets_clause}: {counts}{bytes_clause}{recovered_clause}",
        d.from, d.target_repo
    )
}

/// One-line cycle tail rolling up totals across all mappings. The caller
/// is responsible for gating this in watch mode (skip on idle cycles).
fn emit_cycle_tail(descriptors: &[MappingDescriptor], report: &SyncReport) {
    let s = &report.stats;
    let line = format!(
        "summary: {} mappings | {} synced, {} skipped, {} failed | {} in {}",
        descriptors.len(),
        s.images_synced,
        s.images_skipped,
        s.images_failed,
        format_bytes(s.bytes_transferred),
        format_duration(report.duration),
    );
    if s.images_failed > 0 {
        tracing::warn!(
            mappings = descriptors.len(),
            synced = s.images_synced,
            skipped = s.images_skipped,
            failed = s.images_failed,
            bytes = s.bytes_transferred,
            "{line}"
        );
    } else {
        tracing::info!(
            mappings = descriptors.len(),
            synced = s.images_synced,
            skipped = s.images_skipped,
            failed = s.images_failed,
            bytes = s.bytes_transferred,
            "{line}"
        );
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
    let s = s.trim();
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
    let s = s.trim();
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

        let checker = BatchChecker::from_hostname(hostname, reg.aws_profile.as_deref())
            .await
            .map_err(|e| CliError::Input(format!("ECR batch checker for '{name}': {e}")))?;
        checkers.insert(name.clone(), Rc::new(checker));
    }

    Ok(checkers)
}

/// Resolve a single mapping config into a [`MappingResolution`].
///
/// Returns [`MappingResolution::Resolved`] when at least one tag survives the
/// filter pipeline, or [`MappingResolution::NoMatchingTags`] carrying the
/// diagnostic context the caller needs to render a WARN. Pulls fallbacks from
/// `defaults.source`, `defaults.targets`, and `defaults.tags`.
pub(crate) async fn resolve_mapping(
    mapping: &MappingConfig,
    config: &Config,
    clients: &HashMap<String, Arc<RegistryClient>>,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    with_report: bool,
) -> Result<MappingResolution, CliError> {
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

    let known: HashSet<&str> = config.registries.keys().map(String::as_str).collect();
    let context = format!("mapping '{}'", mapping.from);
    let target_names =
        resolve_target_names(targets_value, config, &known, &context).map_err(CliError::Config)?;

    let mut targets: Vec<TargetEntry> = target_names
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
                existing_tags: HashSet::new(),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    // --- Fetch and filter tags ---
    let source_repo_path = RepositoryName::new(&mapping.from)?;

    let tags_config = mapping
        .tags
        .as_ref()
        .or(config.defaults.as_ref().and_then(|d| d.tags.as_ref()));

    // Fast path: when the config specifies only exact tag names (no
    // wildcards, semver, latest, exclude), use them directly without
    // enumerating all tags from the source registry. This avoids
    // hundreds of paginated tags/list requests for repos with thousands
    // of tags.
    // The image/artifact partition + sample collection happen in the same
    // pass that prepares input for `select_filtered_tags`, so the filter and
    // the no-match WARN both see consistent counts. The pre-built `NoTagsInfo`
    // is only consumed when filtering yields zero tags.
    let (filtered, candidate_count, filter_report, no_tags_template): (
        Vec<String>,
        Option<usize>,
        Option<ocync_sync::filter::FilterReport>,
        Option<NoTagsInfo>,
    ) = if let Some(exact) = tags_config.and_then(|t| t.exact_tags()) {
        (exact, None, None, None)
    } else {
        let all_tags = source_client.list_tags(&source_repo_path).await?;
        let mut samples: Vec<String> = Vec::with_capacity(NO_TAGS_SAMPLE_CAP);
        let mut image_count = 0usize;
        for t in &all_tags {
            if !is_referrers_fallback_tag(t) {
                image_count += 1;
                if samples.len() < NO_TAGS_SAMPLE_CAP {
                    samples.push(t.clone());
                }
            }
        }
        let template = NoTagsInfo {
            from: mapping.from.clone(),
            image_count,
            artifact_count: all_tags.len() - image_count,
            filter_desc: describe_filter(tags_config),
            samples,
        };
        let (kept, count, report) = select_filtered_tags(tags_config, all_tags, with_report)?;
        (kept, count, report, Some(template))
    };

    if filtered.is_empty() {
        let info = no_tags_template.unwrap_or_else(|| NoTagsInfo {
            from: mapping.from.clone(),
            image_count: 0,
            artifact_count: 0,
            filter_desc: describe_filter(tags_config),
            samples: Vec::new(),
        });
        return Ok(MappingResolution::NoMatchingTags(info));
    }

    // --- Target repo ---
    let target_repo = mapping.to.as_deref().unwrap_or(&mapping.from).to_owned();

    // --- Resolve platforms (mapping overrides defaults) ---
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

    let source_authority = source_client
        .registry_authority()
        .map_err(|e| CliError::Input(format!("mapping '{}': {e}", mapping.from)))?;

    let head_first = config
        .registries
        .get(source_name)
        .map(|r| r.head_first)
        .unwrap_or(false);

    // --- Immutable tags optimization ---
    let immutable_pattern = tags_config.and_then(|t| t.immutable_tags.as_deref());
    let immutable_glob = if let Some(pattern) = immutable_pattern {
        let glob_set = build_glob_set(&[pattern.to_owned()])?;

        let target_repo_path = RepositoryName::new(&target_repo)?;
        for entry in &mut targets {
            match entry.client.list_tags(&target_repo_path).await {
                Ok(tags) => entry.existing_tags = tags.into_iter().collect(),
                Err(e) => {
                    tracing::warn!(
                        registry = %entry.name,
                        error = %e,
                        "failed to list target tags; immutable skip disabled for this target"
                    );
                }
            }
        }
        Some(glob_set)
    } else {
        None
    };

    // Resolve artifacts config (mapping overrides defaults).
    let artifacts = match mapping
        .artifacts
        .as_ref()
        .or(config.defaults.as_ref().and_then(|d| d.artifacts.as_ref()))
    {
        Some(c) => ResolvedArtifacts {
            enabled: c.enabled,
            include: c.include.clone(),
            exclude: c.exclude.clone(),
            require_artifacts: c.require_artifacts,
        },
        None => ResolvedArtifacts::default(),
    };

    Ok(MappingResolution::Resolved(ResolvedMapping {
        source_authority,
        source_client,
        source_repo: RepositoryName::new(mapping.from.clone())?,
        target_repo: RepositoryName::new(target_repo)?,
        targets,
        tags: filtered.into_iter().map(TagPair::same).collect(),
        platforms,
        head_first,
        immutable_glob,
        artifacts_config: Rc::new(artifacts),
        candidate_count,
        filter_report,
    }))
}

/// Emit a tracing WARN for a [`NoTagsInfo`] with both a human-readable
/// message (via [`Display`](std::fmt::Display)) and structured fields for
/// log aggregators.
fn emit_no_tags_warn(info: &NoTagsInfo) {
    tracing::warn!(
        from = %info.from,
        source_total = info.source_total(),
        image_count = info.image_count,
        artifact_count = info.artifact_count,
        filter = info.filter_desc.as_deref().unwrap_or(""),
        "{info}"
    );
}

/// Build a `FilterConfig` from a `TagsConfig`, falling back to defaults.
fn build_filter(tags: Option<&TagsConfig>) -> FilterConfig {
    let Some(tags) = tags else {
        return FilterConfig::default();
    };

    FilterConfig {
        include: glob_or_list_to_vec(tags.include.as_ref()),
        glob: glob_or_list_to_vec(tags.glob.as_ref()),
        semver: tags.semver.clone(),
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

/// Result of [`select_filtered_tags`]: kept tags, candidate count, and the
/// optional filter trace consumed by `--dry-run`.
type SelectionResult = (
    Vec<String>,
    Option<usize>,
    Option<ocync_sync::filter::FilterReport>,
);

/// Apply the filter pipeline to `all_tags` from `tags_config`, returning the
/// kept tags, the candidate count, and an optional `FilterReport`.
///
/// `with_report = true` (dry-run) calls
/// [`FilterConfig::apply_with_report`], which does NOT enforce `min_tags` --
/// the report carries the configured value so the formatter can render the
/// gap to the user. `with_report = false` (real-sync) calls
/// [`FilterConfig::apply`], which enforces `min_tags` and returns a
/// `BelowMinTags` error when violated.
///
/// Extracted from [`resolve_mapping`] so the report wire-up is testable
/// without spinning up a registry mock.
fn select_filtered_tags(
    tags_config: Option<&TagsConfig>,
    all_tags: Vec<String>,
    with_report: bool,
) -> Result<SelectionResult, CliError> {
    let n_candidates = all_tags.len();
    let filter = build_filter(tags_config);
    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    if with_report {
        let result = filter.apply_with_report(&tag_refs)?;
        Ok((result.kept, Some(n_candidates), Some(result.report)))
    } else {
        Ok((filter.apply(&tag_refs)?, Some(n_candidates), None))
    }
}

/// One-line summary of a [`TagsConfig`] suitable for log emission, e.g.
/// `semver >=1.0.0, latest=5`. Returns `None` when no filter applies.
///
/// Single source of truth: delegates to [`FilterConfig::describe`] after
/// the same conversion the engine uses, so dry-run stage labels and the
/// no-tags-matched WARN rationale cannot drift.
fn describe_filter(tags: Option<&TagsConfig>) -> Option<String> {
    build_filter(tags).describe()
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
    fn parse_duration_trims_whitespace() {
        assert_eq!(
            parse_duration("  12h  "),
            Some(Duration::from_secs(12 * 3600))
        );
        assert_eq!(parse_duration(" 30m "), Some(Duration::from_secs(30 * 60)));
    }

    #[test]
    fn resolve_cache_ttl_returns_error_for_invalid() {
        use crate::cli::config::{Config, GlobalConfig};

        let config = Config {
            global: Some(GlobalConfig {
                cache_ttl: Some("bogus".into()),
                ..Default::default()
            }),
            registries: Default::default(),
            target_groups: Default::default(),
            defaults: None,
            mappings: Vec::new(),
        };
        let result = resolve_cache_ttl(&config);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("invalid cache_ttl"), "got: {err_msg}");
    }

    #[test]
    fn resolve_cache_ttl_returns_default_when_absent() {
        use crate::cli::config::Config;

        let config = Config {
            global: None,
            registries: Default::default(),
            target_groups: Default::default(),
            defaults: None,
            mappings: Vec::new(),
        };
        assert_eq!(resolve_cache_ttl(&config).unwrap(), DEFAULT_CACHE_TTL);
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
        use ocync_sync::filter::SortOrder;

        let tags = TagsConfig {
            include: Some(GlobOrList::List(vec!["latest".into()])),
            glob: Some(GlobOrList::Single("*".into())),
            semver: Some(">=1.0.0".into()),
            exclude: Some(GlobOrList::Single("*-alpine".into())),
            sort: Some(SortOrder::Semver),
            latest: Some(5),
            min_tags: Some(1),
            immutable_tags: None,
            ..Default::default()
        };
        let filter = build_filter(Some(&tags));
        assert_eq!(filter.include, vec!["latest"]);
        assert_eq!(filter.glob, vec!["*"]);
        assert_eq!(filter.semver.as_deref(), Some(">=1.0.0"));
        assert_eq!(filter.exclude, vec!["*-alpine"]);
        assert_eq!(filter.sort, Some(SortOrder::Semver));
        assert_eq!(filter.latest, Some(5));
        assert_eq!(filter.min_tags, Some(1));
    }

    /// End-to-end: a `TagsConfig` with `include:` + `semver:` builds a
    /// `FilterConfig` that, when applied to a tag list, returns both the
    /// pinned literals and the version-range matches.
    #[test]
    fn build_filter_include_pin_plus_range_full_flow() {
        let tags_yaml = r#"
include: ["latest", "latest-dev"]
semver: ">=1.25.0"
sort: semver
latest: 5
"#;
        let tags: TagsConfig = serde_yaml::from_str(tags_yaml).expect("yaml parses");
        let filter = build_filter(Some(&tags));

        // Confirm the FilterConfig was built with the right include patterns.
        assert_eq!(
            filter.include,
            vec!["latest".to_string(), "latest-dev".to_string()]
        );

        // Apply against a synthesized tag list and verify both arms work.
        let candidate_tags = vec![
            "latest",
            "latest-dev",
            "1.25.5-r0",
            "1.25.4",
            "1.25.3",
            "1.25.2",
            "1.25.1",
            "1.25.0",
            "1.24.0",     // below range, drops
            "1.25.5-rc1", // RC, dropped by system-exclude
        ];
        let result = filter.apply(&candidate_tags).expect("filter applies");

        // Floats survive via include.
        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"latest-dev".to_string()));
        // Top 5 of the version range (1.25.5-r0 sorts above 1.25.5-rc1 if rc1
        // weren't dropped, but rc1 IS dropped, so we expect 1.25.0..1.25.5-r0).
        assert!(result.contains(&"1.25.5-r0".to_string()));
        // Below range: dropped.
        assert!(!result.contains(&"1.24.0".to_string()));
        // RC: dropped by system-exclude.
        assert!(!result.contains(&"1.25.5-rc1".to_string()));
        // 5 versions + 2 floats = 7.
        assert_eq!(result.len(), 7);
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

    #[test]
    fn parse_size_trims_whitespace() {
        assert_eq!(parse_size("  500MB  "), Some(500_000_000));
        assert_eq!(parse_size(" 2GB "), Some(2_000_000_000));
    }

    #[test]
    fn describe_filter_combines_semver_and_latest() {
        let tags = TagsConfig {
            semver: Some(">=1.0.0".into()),
            latest: Some(5),
            ..TagsConfig::default()
        };
        assert_eq!(
            describe_filter(Some(&tags)).as_deref(),
            Some("semver >=1.0.0, latest=5")
        );
    }

    #[test]
    fn describe_filter_returns_none_when_empty() {
        let tags = TagsConfig::default();
        assert!(describe_filter(Some(&tags)).is_none());
        assert!(describe_filter(None).is_none());
    }

    // -- NoTagsInfo Display ---------------------------------------------

    fn no_tags_info(
        from: &str,
        image_count: usize,
        artifact_count: usize,
        filter_desc: Option<&str>,
        samples: &[&str],
    ) -> NoTagsInfo {
        NoTagsInfo {
            from: from.into(),
            image_count,
            artifact_count,
            filter_desc: filter_desc.map(String::from),
            samples: samples.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn no_tags_warn_renders_simple_repo() {
        // 2 image tags, both shown -- no truncation, no artifact split.
        let info = no_tags_info(
            "library/nginx",
            2,
            0,
            Some("semver >=2.0"),
            &["v1.0", "v1.1"],
        );
        assert_eq!(
            info.to_string(),
            "library/nginx: 0 of 2 source tags matched filter (semver >=2.0); skipping. Source: [v1.0, v1.1]"
        );
    }

    /// Cosign-heavy repos: WARN must split image vs referrer counts so the
    /// 14289-tag chainguard case is not misread as 14289 missing image tags.
    #[test]
    fn no_tags_warn_splits_image_and_artifact_counts() {
        let info = no_tags_info(
            "chainguard/nginx",
            2,
            14287,
            Some("semver >=1.0.0, latest=5"),
            &["latest", "latest-dev"],
        );
        let msg = info.to_string();
        assert!(
            msg.contains(
                "0 of 14289 source tags (2 image tags, 14287 referrer artifacts) matched filter"
            ),
            "{msg}"
        );
    }

    /// Truncation appends `, ...` so the user knows the list is sampled.
    #[test]
    fn no_tags_warn_appends_ellipsis_when_truncated() {
        let info = NoTagsInfo {
            from: "library/alpine".into(),
            // image_count > samples.len() drives the truncation marker.
            image_count: 100,
            artifact_count: 0,
            filter_desc: Some("semver >=99.0".into()),
            samples: (0..5).map(|i| format!("v{i}")).collect(),
        };
        assert!(info.samples_truncated());
        let msg = info.to_string();
        assert!(msg.ends_with("Source: [v0, v1, v2, v3, v4, ...]"), "{msg}");
    }

    /// Empty samples render as `<empty>` and a missing filter description
    /// renders as `no filter configured` -- both ensure the message never
    /// has bare parens or `[]`.
    #[test]
    fn no_tags_warn_renders_empty_markers() {
        let info = no_tags_info("x/y", 0, 0, None, &[]);
        let msg = info.to_string();
        assert!(msg.contains("(no filter configured)"), "{msg}");
        assert!(msg.ends_with("Source: <empty>"), "{msg}");
    }

    // -- WatchLogState transitions --------------------------------------

    /// First observation triggers a WARN; repeats within the same failure
    /// run are suppressed; recovery clears the entry so a relapse warns
    /// again. Encodes the contract `run()` depends on.
    #[test]
    fn watch_log_state_emits_once_per_transition() {
        let mut state = WatchLogState::default();
        assert!(state.observe_no_match("repo-a"));
        assert!(!state.observe_no_match("repo-a"));
        assert!(state.observe_resolved("repo-a"));
        assert!(!state.observe_resolved("repo-a"));
        assert!(state.observe_no_match("repo-a"));
    }

    /// `retain_active` drops entries for mappings no longer in the config
    /// so the set does not grow unbounded across the watch process.
    #[test]
    fn watch_log_state_prunes_removed_mappings() {
        let mut state = WatchLogState::default();
        state.observe_no_match("repo-a");
        state.observe_no_match("repo-b");
        state.observe_no_match("repo-removed");

        state.retain_active(["repo-a", "repo-b"]);

        // After pruning, `repo-removed` re-warns (gap means transition);
        // surviving entries continue to suppress.
        assert!(!state.observe_no_match("repo-a"));
        assert!(!state.observe_no_match("repo-b"));
        assert!(state.observe_no_match("repo-removed"));
    }

    // -- per-mapping outcome aggregation + dedup ---------------------------

    fn img(
        source: &str,
        target: &str,
        status: ocync_sync::ImageStatus,
        bytes: u64,
    ) -> ocync_sync::ImageResult {
        use ocync_sync::{BlobTransferStats, ImageResult};
        ImageResult {
            image_id: uuid::Uuid::now_v7(),
            source: source.into(),
            target: target.into(),
            status,
            bytes_transferred: bytes,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::from_secs(1),
            artifacts_skipped: false,
        }
    }

    fn report_with(images: Vec<ocync_sync::ImageResult>) -> SyncReport {
        SyncReport {
            run_id: uuid::Uuid::now_v7(),
            images,
            stats: ocync_sync::SyncStats::default(),
            duration: Duration::from_secs(1),
        }
    }

    /// Aggregation groups by `source_repo:` + `target_repo:` prefix so
    /// images from a different mapping (same source repo, different target)
    /// don't bleed into this mapping's counts.
    #[test]
    fn aggregate_mapping_outcome_groups_by_source_and_target() {
        let report = report_with(vec![
            img(
                "library/alpine:3.20",
                "mirror/a:3.20",
                ocync_sync::ImageStatus::Synced,
                100,
            ),
            img(
                "library/alpine:3.21",
                "mirror/a:3.21",
                ocync_sync::ImageStatus::Synced,
                200,
            ),
            img(
                "library/alpine:3.21",
                "mirror/b:3.21",
                ocync_sync::ImageStatus::Skipped {
                    reason: ocync_sync::SkipReason::DigestMatch,
                },
                0,
            ),
        ]);
        let o = aggregate_mapping_outcome("library/alpine", "mirror/a", &report);
        assert_eq!(o.synced, 2);
        assert_eq!(o.skipped, 0);
        assert_eq!(o.bytes, 300);
    }

    /// Empty mappings (no images in the report) skip silently -- the
    /// no-tags WARN is the right surface for that case, not this one.
    #[test]
    fn empty_outcome_is_recognized() {
        let outcome = MappingOutcome::default();
        assert!(outcome.is_empty());
        let with_skip = MappingOutcome {
            skipped: 1,
            ..MappingOutcome::default()
        };
        assert!(!with_skip.is_empty());
    }

    /// First observation emits; identical follow-up suppresses; outcome
    /// change emits again. Mirrors the no-tags transition contract.
    #[test]
    fn watch_log_state_dedupes_identical_mapping_outcomes() {
        let mut state = WatchLogState::default();
        let steady = MappingOutcome {
            skipped: 5,
            ..MappingOutcome::default()
        };
        let active = MappingOutcome {
            synced: 1,
            skipped: 4,
            bytes: 1024,
            ..MappingOutcome::default()
        };

        // First observation: emit, no prior so not a recovery.
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &steady),
            Some(false)
        );
        // Same outcome twice: suppress.
        assert_eq!(state.observe_mapping_outcome("repo-a", &steady), None);
        assert_eq!(state.observe_mapping_outcome("repo-a", &steady), None);
        // Different outcome: emit, neither prior nor current was a failure
        // so not a recovery either.
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &active),
            Some(false)
        );
        assert_eq!(state.observe_mapping_outcome("repo-a", &active), None);
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &steady),
            Some(false)
        );
    }

    /// Recovery detection: prior outcome with `failed > 0` followed by an
    /// outcome with `failed == 0` returns `Some(true)` so the caller can
    /// attach the `[recovered]` marker. Cycle counter advances on each
    /// transition so the watch loop sees activity.
    #[test]
    fn watch_log_state_surfaces_failure_to_clean_transition() {
        let mut state = WatchLogState::default();
        let failing = MappingOutcome {
            failed: 1,
            ..MappingOutcome::default()
        };
        let healthy = MappingOutcome {
            synced: 1,
            ..MappingOutcome::default()
        };

        // First observation can never be a recovery (no prior).
        assert_eq!(state.observe_mapping_outcome("r", &failing), Some(false));
        assert_eq!(state.observe_mapping_outcome("r", &healthy), Some(true));
        assert_eq!(state.cycle_emit_count(), 2);
    }

    /// `retain_active` also prunes per-mapping outcome cache so a removed
    /// mapping doesn't keep its stale entry forever. Re-observation after
    /// pruning emits as a fresh first-observation.
    #[test]
    fn watch_log_state_retain_active_also_prunes_outcomes() {
        let mut state = WatchLogState::default();
        let outcome = MappingOutcome {
            skipped: 1,
            ..MappingOutcome::default()
        };
        state.observe_mapping_outcome("keep", &outcome);
        state.observe_mapping_outcome("drop", &outcome);

        state.retain_active(["keep"]);

        assert_eq!(state.observe_mapping_outcome("drop", &outcome), Some(false));
        assert_eq!(state.observe_mapping_outcome("keep", &outcome), None);
    }

    /// `format_mapping_outcome` omits zero counts, elides the bytes clause
    /// when nothing transferred, tags recovery transitions, and drops the
    /// `[targets]` bracket on single-target mappings (the destination is
    /// already in the `from -> to` arrow).
    #[test]
    fn format_mapping_outcome_single_target_omits_bracket() {
        let d = MappingDescriptor {
            from: "library/alpine".into(),
            target_repo: "mirror/alpine".into(),
            target_names: vec!["ttl".into()],
        };
        let synced_only = MappingOutcome {
            synced: 3,
            bytes: 1024,
            ..MappingOutcome::default()
        };
        assert_eq!(
            format_mapping_outcome(&d, &synced_only, false),
            "library/alpine -> mirror/alpine: synced 3 (1.0 KB)"
        );
        let skipped_only = MappingOutcome {
            skipped: 5,
            ..MappingOutcome::default()
        };
        assert_eq!(
            format_mapping_outcome(&d, &skipped_only, false),
            "library/alpine -> mirror/alpine: skipped 5"
        );
        let mixed = MappingOutcome {
            synced: 1,
            skipped: 2,
            failed: 1,
            bytes: 2048,
        };
        assert_eq!(
            format_mapping_outcome(&d, &mixed, false),
            "library/alpine -> mirror/alpine: synced 1, skipped 2, failed 1 (2.0 KB)"
        );
        assert_eq!(
            format_mapping_outcome(&d, &synced_only, true),
            "library/alpine -> mirror/alpine: synced 3 (1.0 KB) [recovered]"
        );
    }

    /// Multi-target mappings keep the `[targets]` bracket so the operator
    /// can see which destinations the outcome covers.
    #[test]
    fn format_mapping_outcome_multi_target_keeps_bracket() {
        let d = MappingDescriptor {
            from: "library/alpine".into(),
            target_repo: "mirror/alpine".into(),
            target_names: vec!["ecr-prod".into(), "ghcr-mirror".into()],
        };
        let synced = MappingOutcome {
            synced: 1,
            bytes: 1024,
            ..MappingOutcome::default()
        };
        assert_eq!(
            format_mapping_outcome(&d, &synced, false),
            "library/alpine -> mirror/alpine [ecr-prod, ghcr-mirror]: synced 1 (1.0 KB)"
        );
    }

    // -- select_filtered_tags wire-up tests ---------------------------------

    /// `with_report = true` produces a `Some(FilterReport)` whose
    /// `min_tags` and `include_kept` match the configured input. This is
    /// the wire-up that `--dry-run` depends on; a regression where
    /// `with_report` is hardcoded to `false` would fail this test.
    #[test]
    fn select_filtered_tags_with_report_populates_min_tags_and_include() {
        use ocync_sync::filter::SortOrder;

        let tags_config = TagsConfig {
            include: Some(GlobOrList::List(vec!["latest".into()])),
            semver: Some(">=1.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(2),
            min_tags: Some(5),
            ..Default::default()
        };
        let all_tags = vec![
            "latest".into(),
            "1.0.0".into(),
            "1.1.0".into(),
            "1.2.0".into(),
            "0.9.0-rc1".into(),
        ];
        let (kept, candidate_count, report) =
            select_filtered_tags(Some(&tags_config), all_tags, true).unwrap();

        // Wire-up: candidate count flows through.
        assert_eq!(candidate_count, Some(5));
        // Wire-up: report is Some.
        let report = report.expect("report present when with_report=true");
        // Report carries min_tags so the formatter can render the gap.
        assert_eq!(report.min_tags, Some(5));
        // Include rescued by name (latest fails semver but is in include:).
        assert_eq!(report.include_kept, vec!["latest".to_string()]);
        // Kept reflects union semantics: include + top-2 of pipeline
        // (1.2.0 and 1.1.0). 1.0.0 falls off via latest=2; 0.9.0-rc1 fails
        // semver and is dropped. 3 < min_tags=5, so real-sync would error.
        assert_eq!(kept.len(), 3);
        assert!(kept.contains(&"latest".to_string()));
    }

    /// `with_report = false` (real-sync hot path) produces `None` for the
    /// report so we don't pay drop-attribution cost on every watch cycle.
    #[test]
    fn select_filtered_tags_without_report_returns_none() {
        let tags_config = TagsConfig {
            glob: Some(GlobOrList::Single("*".into())),
            ..Default::default()
        };
        let all_tags = vec!["1.0".into(), "2.0".into()];
        let (kept, candidate_count, report) =
            select_filtered_tags(Some(&tags_config), all_tags, false).unwrap();
        assert_eq!(candidate_count, Some(2));
        assert_eq!(kept.len(), 2);
        assert!(report.is_none());
    }

    /// `with_report = false` enforces `min_tags` (real-sync errors when
    /// the filter doesn't yield enough tags). The dry-run path does NOT
    /// (covered separately).
    #[test]
    fn select_filtered_tags_without_report_enforces_min_tags() {
        let tags_config = TagsConfig {
            min_tags: Some(10),
            ..Default::default()
        };
        let all_tags = vec!["1.0".into(), "2.0".into()];
        let result = select_filtered_tags(Some(&tags_config), all_tags, false);
        assert!(
            result.is_err(),
            "expected BelowMinTags error from real-sync path"
        );
    }

    /// `with_report = true` does NOT enforce `min_tags`. The dry-run formatter
    /// surfaces the gap in its output instead of suppressing the report.
    #[test]
    fn select_filtered_tags_with_report_does_not_enforce_min_tags() {
        let tags_config = TagsConfig {
            min_tags: Some(10),
            ..Default::default()
        };
        let all_tags = vec!["1.0".into(), "2.0".into()];
        let (kept, candidate_count, report) =
            select_filtered_tags(Some(&tags_config), all_tags, true).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(candidate_count, Some(2));
        let report = report.expect("dry-run path returns report even when min_tags would error");
        assert_eq!(report.min_tags, Some(10));
    }

    /// Full wire-up: `select_filtered_tags` + `ResolvedMapping` construction +
    /// `dry_run::write_to`, ensuring the report flows from filter through to
    /// printed output. A regression where `filter_report` drops on the floor
    /// between `resolve_mapping` and the formatter would fail this test.
    #[test]
    fn dry_run_full_wire_up_renders_min_tags_failure() {
        use ocync_distribution::spec::RegistryAuthority;
        use ocync_sync::engine::{RegistryAlias, ResolvedArtifacts, ResolvedMapping, TargetEntry};
        use std::collections::HashSet as Set;

        let tags_config = TagsConfig {
            semver: Some(">=2.0".into()),
            min_tags: Some(5),
            ..Default::default()
        };
        let all_tags: Vec<String> = (0..10).map(|i| format!("1.{i}.0")).collect();
        let (kept, candidate_count, filter_report) =
            select_filtered_tags(Some(&tags_config), all_tags, true).unwrap();
        assert_eq!(kept.len(), 0); // all dropped by semver >=2.0
        assert!(filter_report.is_some());

        let client = Arc::new(
            ocync_distribution::RegistryClientBuilder::new(
                url::Url::parse("http://127.0.0.1").unwrap(),
            )
            .build()
            .unwrap(),
        );
        let mapping = ResolvedMapping {
            source_authority: RegistryAuthority::new("source.test:443"),
            source_client: client.clone(),
            source_repo: RepositoryName::new("repo").unwrap(),
            target_repo: RepositoryName::new("repo").unwrap(),
            targets: vec![TargetEntry {
                name: RegistryAlias::new("target"),
                client,
                batch_checker: None,
                existing_tags: Set::new(),
            }],
            tags: kept.into_iter().map(TagPair::same).collect(),
            platforms: None,
            head_first: false,
            immutable_glob: None,
            artifacts_config: Rc::new(ResolvedArtifacts::default()),
            candidate_count,
            filter_report,
        };

        let mut buf: Vec<u8> = Vec::new();
        crate::cli::commands::dry_run::write_to(&mut buf, &[mapping], false).unwrap();
        let out = String::from_utf8(buf).unwrap();

        // The end-to-end output surfaces the BelowMinTags warning.
        assert!(out.contains("min_tags: 5"), "{out}");
        assert!(out.contains("FAIL"), "{out}");
        assert!(out.contains("BelowMinTags"), "{out}");
        // And carries the full pipeline trace.
        assert!(out.contains("source tags: 10"), "{out}");
        assert!(out.contains("dropped (10):"), "{out}");
    }
}
