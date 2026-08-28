//! The `sync` subcommand - runs all mappings from config.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use serde::Serialize;

use crate::SyncArgs;
use crate::cli::config::{
    AuthType, Config, GlobOrList, MappingConfig, RegistryConfig, TagsConfig, load_config,
    resolve_target_names,
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

/// Cadence of the progress line emitted while the run is preparing.
///
/// Everything before the engine starts is sequential and network-bound: each
/// client build mints a token for ECR, GAR, and ACR, and each mapping lists a
/// repository's tags. No per-image output exists yet, so without this a large
/// config runs for minutes with nothing in the log.
const PREPARE_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// A step of the pre-engine phase.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum PreparePhase {
    /// Building a client per referenced registry.
    #[default]
    Registries,
    /// Building an ECR blob-visibility checker per target registry.
    BatchCheckers,
    /// Listing and filtering tags, one mapping at a time.
    Mappings,
}

impl fmt::Display for PreparePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registries => f.write_str("registries"),
            Self::BatchCheckers => f.write_str("batch checkers"),
            Self::Mappings => f.write_str("mappings"),
        }
    }
}

/// Snapshot of the pre-engine phase, read by the progress ticker.
#[derive(Debug, Clone, Copy, Default)]
struct PrepareProgress {
    /// Which pre-engine step is running.
    phase: PreparePhase,
    /// Items finished in this step.
    done: usize,
    /// Items this step will process.
    total: usize,
}

/// Counter the pre-engine steps advance and the progress ticker reads.
///
/// A plain [`Cell`] rather than a channel: the runtime is `current_thread`, so
/// the ticker and the work it describes never run concurrently.
#[derive(Debug, Default)]
pub(crate) struct PrepareTracker {
    state: Cell<PrepareProgress>,
}

impl PrepareTracker {
    /// Start a new step, resetting the item counter.
    pub(crate) fn begin(&self, phase: PreparePhase, total: usize) {
        self.state.set(PrepareProgress {
            phase,
            done: 0,
            total,
        });
    }

    /// Record one item finished in the current step.
    pub(crate) fn advance(&self) {
        let mut p = self.state.get();
        p.done += 1;
        self.state.set(p);
    }
}

/// Drive `work` to completion, logging its progress every
/// [`PREPARE_PROGRESS_INTERVAL`].
///
/// The ticker fires on wall-clock, not on item boundaries, so a single slow
/// mapping still reports. A boundary-driven throttle does not: measured on a
/// three-mapping config it emitted nothing across an 8.6 second run.
pub(crate) async fn with_prepare_progress<F: Future>(
    tracker: &PrepareTracker,
    work: F,
) -> F::Output {
    tick_while(tracker, PREPARE_PROGRESS_INTERVAL, work, |p, elapsed| {
        tracing::info!(
            phase = %p.phase,
            done = p.done,
            total = p.total,
            elapsed_secs = elapsed.as_secs(),
            "preparing sync"
        );
    })
    .await
}

/// Poll `work` to completion, calling `emit` with the tracker's state once per
/// `interval` until it finishes.
///
/// Split from [`with_prepare_progress`] so the cadence is testable against a
/// recording sink rather than a tracing subscriber.
async fn tick_while<F: Future>(
    tracker: &PrepareTracker,
    interval: Duration,
    work: F,
    mut emit: impl FnMut(PrepareProgress, Duration),
) -> F::Output {
    let mut work = std::pin::pin!(work);
    let started = Instant::now();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` yields immediately; push the first tick out a full period so
    // a fast run stays silent.
    ticker.reset();
    loop {
        tokio::select! {
            // `work` first: a tick that becomes ready on the same poll as a
            // finished future is dropped rather than logged after the fact.
            biased;
            out = &mut work => return out,
            _ = ticker.tick() => emit(tracker.state.get(), started.elapsed()),
        }
    }
}

/// Registry clients keyed by config alias.
///
/// A registry whose auth provider could not be built is stored as its failure
/// rather than dropped, so the error surfaces only on the mappings that
/// actually reference it.
pub(crate) type ClientMap = HashMap<String, Result<Arc<RegistryClient>, ClientInitError>>;

/// Why a registry's client could not be built.
///
/// Carries the classification alongside the message: recovering it later from
/// a stringified error is impossible, and losing it downgrades a wholly denied
/// run from the auth exit code to the generic failure code.
#[derive(Debug)]
pub(crate) struct ClientInitError {
    /// The original error, already formatted.
    message: String,
    /// Whether the cause was a credential failure.
    auth: bool,
}

/// Which side of a mapping a registry sits on, for error messages.
#[derive(Debug, Clone, Copy)]
enum RegistryRole {
    Source,
    Target,
}

impl fmt::Display for RegistryRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => f.write_str("source"),
            Self::Target => f.write_str("target"),
        }
    }
}

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

/// A target a mapping could not use.
///
/// The mapping still syncs to its other targets, so this is not an
/// [`UnresolvedMapping`]; it is a hole in an otherwise successful run and has
/// to be visible as one.
#[derive(Debug, Serialize)]
pub(crate) struct DroppedTarget {
    /// Source repository of the mapping that lost the target.
    pub from: String,
    /// Target registry alias that was dropped.
    pub registry: String,
    /// Why the target was unusable.
    pub error: String,
}

/// A mapping that never became transferable work.
///
/// Resolution failures are per-mapping: an unreachable registry or a 403 on
/// one repository's tag listing must not stop the mappings behind it.
#[derive(Debug, Serialize)]
pub(crate) struct UnresolvedMapping {
    /// Source repository path from the mapping config.
    pub from: String,
    /// Why the mapping could not be resolved.
    pub error: String,
    /// What the failure would have exited with had it aborted the run.
    ///
    /// Kept out of the JSON document. A bool would collapse every non-auth
    /// classification into the generic failure code, retiring exit 3 and with
    /// it the `min_tags` tripwire, whose whole job is to stop a run.
    #[serde(skip)]
    pub code: ExitCode,
}

/// Fold a run's failure classifications into one exit code.
///
/// A specific code survives only when every failure agrees on it: mixed causes
/// report the generic failure, because no single one names what to fix.
pub(crate) fn shared_failure_code(codes: impl IntoIterator<Item = ExitCode>) -> ExitCode {
    let mut codes = codes.into_iter();
    let Some(first) = codes.next() else {
        return ExitCode::Failure;
    };
    if matches!(first, ExitCode::AuthError | ExitCode::ConfigError) && codes.all(|c| c == first) {
        first
    } else {
        ExitCode::Failure
    }
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

impl fmt::Display for NoTagsInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    /// [`target_key`] for every target currently reported down.
    dropped_targets: HashSet<String>,
    /// Mappings currently reported as unresolvable.
    unresolved: HashSet<String>,
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

    /// Record a dropped target. Returns `true` on transition into the dropped
    /// state (caller emits a WARN); `false` while the same target stays down.
    ///
    /// Keyed on mapping plus registry so two mappings losing the same mirror
    /// each report once, and a second mirror going down still reports.
    fn observe_dropped_target(&mut self, from: &str, registry: &str) -> bool {
        let changed = self.dropped_targets.insert(target_key(from, registry));
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Forget any target of `from` absent from `still_down`, so a mirror that
    /// recovered warns again if it fails later.
    ///
    /// Reconciling rather than clearing wholesale is what makes a partial
    /// recovery work: clearing only when nothing is down leaves a recovered
    /// mirror suppressed for as long as any sibling stays down.
    fn forget_recovered_targets(&mut self, from: &str, still_down: &HashSet<&str>) {
        let prefix = target_key(from, "");
        self.dropped_targets
            .retain(|key| match key.strip_prefix(&prefix) {
                // Another mapping's key.
                None => true,
                Some(registry) => still_down.contains(registry),
            });
    }

    /// Record a mapping that could not be resolved. Returns `true` on
    /// transition into the failure state (caller emits the ERROR).
    fn observe_mapping_unresolved(&mut self, from: &str) -> bool {
        // A mapping that stops resolving reports no outcome this cycle, so a
        // stale entry here would suppress the recovery line when it returns
        // with identical counts.
        self.last_outcomes.remove(from);
        let changed = self.unresolved.insert(from.to_string());
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Record a mapping that resolved, re-arming its unresolved ERROR.
    fn observe_mapping_resolved(&mut self, from: &str) {
        self.unresolved.remove(from);
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
        self.unresolved.retain(|k| active_set.contains(k.as_str()));
        // Composite keys need a prefix split, which is why this cannot join
        // the retains above. A mapping removed and later re-added would
        // otherwise keep a stale key and never warn about its dead mirror.
        self.dropped_targets.retain(|key| {
            key.split_once('\u{0}')
                .is_some_and(|(from, _)| active_set.contains(from))
        });
    }
}

/// Key a dropped target by mapping and registry.
///
/// NUL-separated because neither a repository path nor a registry alias can
/// contain it, so the split back is unambiguous.
fn target_key(from: &str, registry: &str) -> String {
    format!("{from}\u{0}{registry}")
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

    // Client construction, batch-checker setup, and mapping resolution are all
    // sequential network work with no output of their own. One ticker spans
    // the lot so the run is never silent for longer than the interval.
    let tracker = PrepareTracker::default();
    let resolution = with_prepare_progress(&tracker, async {
        let clients = build_clients(&config, &tracker).await;
        let batch_checkers = build_batch_checkers(&config, &clients, &tracker).await;
        resolve_all(
            &config,
            &clients,
            &batch_checkers,
            args.dry_run,
            watch_log.as_deref_mut(),
            &tracker,
        )
        .await
    })
    .await;
    let Resolution {
        resolved: mappings,
        unresolved,
        dropped_targets,
        no_match,
    } = resolution;

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
        crate::cli::commands::dry_run::print(&mappings, &unresolved, &dropped_targets, verbose);
        return Ok(dry_run_exit_code(
            mappings.len() + no_match,
            &unresolved,
            &dropped_targets,
        ));
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
        .map(|m| {
            let from = m.source_repo.as_str().to_string();
            MappingDescriptor {
                target_repo: m.target_repo.as_str().to_string(),
                target_names: m.targets.iter().map(|t| (*t.name).to_string()).collect(),
                // Taken from the dropped list rather than `m.targets`, which
                // is already filtered: without it the bracket disappears at
                // exactly the moment a registry is missing from it.
                dropped_names: dropped_targets
                    .iter()
                    .filter(|d| d.from == from)
                    .map(|d| d.registry.clone())
                    .collect(),
                from,
            }
        })
        .collect();

    // Anything dropped before the engine started is absent from the mappings
    // it sees, and the prune cannot tell that from a deletion.
    let complete = unresolved.is_empty() && dropped_targets.is_empty();
    let engine =
        SyncEngine::new(RetryConfig::default(), max_concurrent).with_cache_pruning(complete);
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
    // Unresolved mappings always force the tail -- they produce no per-mapping
    // line, so without this a cycle where every mapping 403'd would go quiet.
    let cycle_had_activity = !unresolved.is_empty()
        || !dropped_targets.is_empty()
        || watch_log
            .as_deref()
            .is_none_or(|s| s.cycle_emit_count() > 0);
    if cycle_had_activity {
        emit_cycle_tail(
            &descriptors,
            unresolved.len(),
            dropped_targets.len(),
            &report,
        );
    }

    write_output(&report, &unresolved, &dropped_targets, args.json)?;

    Ok(combined_exit_code(
        &report,
        &unresolved,
        &dropped_targets,
        no_match,
    ))
}

/// Exit code for a dry run, which produces no [`SyncReport`] to fold into.
fn dry_run_exit_code(
    resolved: usize,
    unresolved: &[UnresolvedMapping],
    dropped: &[DroppedTarget],
) -> ExitCode {
    if unresolved.is_empty() {
        // A mapping that resolved but lost a target is still short of what
        // the config asked for.
        return if dropped.is_empty() {
            ExitCode::Success
        } else {
            ExitCode::PartialFailure
        };
    }
    if resolved > 0 {
        return ExitCode::PartialFailure;
    }
    total_failure_code(unresolved)
}

/// Exit code for a run where nothing succeeded.
///
/// Only unresolved mappings are consulted. A dropped target never reaches here
/// (the caller returns `PartialFailure` first, since something did resolve),
/// so it cannot make an all-denied run report as anything else.
///
/// A run denied everywhere exits with the auth code rather than the generic
/// failure code, so an operator can tell "fix the credentials" from "fix
/// something else" without reading the log.
fn total_failure_code(unresolved: &[UnresolvedMapping]) -> ExitCode {
    shared_failure_code(unresolved.iter().map(|u| u.code))
}

/// Fold mapping-level resolution failures into the run's exit code.
///
/// A mapping that never reached the engine produces no [`ocync_sync::ImageResult`],
/// so [`SyncReport::exit_code`] cannot see it. Counting each as a failure is
/// what keeps a run whose mappings all 403'd from exiting 0.
fn combined_exit_code(
    report: &SyncReport,
    unresolved: &[UnresolvedMapping],
    dropped: &[DroppedTarget],
    no_match: usize,
) -> ExitCode {
    if unresolved.is_empty() {
        // A dropped target means images never reached a registry the config
        // named, so an otherwise clean run is still only partial.
        return match ExitCode::from_report(report.exit_code()) {
            ExitCode::Success if !dropped.is_empty() => ExitCode::PartialFailure,
            other => other,
        };
    }
    // A mapping whose filter matched nothing is healthy with nothing to do, so
    // it counts as the run working, even though it contributes no image.
    if report.has_success() || no_match > 0 {
        return ExitCode::PartialFailure;
    }
    // Images that ran and failed for non-auth reasons make this a generic
    // failure regardless of why the unresolved mappings were dropped.
    if !report.images.is_empty() {
        return ExitCode::Failure;
    }
    total_failure_code(unresolved)
}

/// Per-mapping metadata captured before the engine consumes `mappings`,
/// so we can join it with the engine's per-image report after the fact
/// to emit one log line per mapping (with source/target context).
struct MappingDescriptor {
    from: String,
    target_repo: String,
    target_names: Vec<String>,
    /// Registries the config named that this mapping could not use.
    dropped_names: Vec<String>,
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
        // `from` / `to` are intentionally NOT structured fields here -- the
        // message already names them as `from -> to`, and tracing's text
        // formatter would otherwise tail the line with a redundant
        // `from=... to=...` block. The count fields remain because they
        // carry zero values the terse message elides.
        //
        // The two arms differ only in level. `tracing::event!` would let
        // us pick at runtime, but it requires a const-expression level.
        if outcome.failed > 0 {
            tracing::warn!(
                synced = outcome.synced,
                skipped = outcome.skipped,
                failed = outcome.failed,
                bytes = outcome.bytes,
                recovered,
                "{line}"
            );
        } else {
            tracing::info!(
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
    // Shown whenever more than one registry was configured, which includes the
    // case where only one survived: that is precisely when the operator needs
    // to see which.
    let targets_clause = if d.target_names.len() + d.dropped_names.len() > 1 {
        let mut names = d.target_names.join(", ");
        if !d.dropped_names.is_empty() {
            names.push_str(&format!(", {} unreachable", d.dropped_names.join(", ")));
        }
        format!(" [{names}]")
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
fn emit_cycle_tail(
    descriptors: &[MappingDescriptor],
    unresolved_count: usize,
    dropped_count: usize,
    report: &SyncReport,
) {
    let line = format_cycle_tail(descriptors.len(), unresolved_count, dropped_count, report);
    // Counts are already in the message; structured fields would be a
    // verbatim restatement in text output. JSON aggregators parse the
    // message (or use the SyncReport via `--json`).
    if report.stats.images_failed > 0 || unresolved_count > 0 || dropped_count > 0 {
        tracing::warn!("{line}");
    } else {
        tracing::info!("{line}");
    }
}

/// Render the cycle tail. Split from [`emit_cycle_tail`] so the wording is
/// testable without a tracing subscriber.
fn format_cycle_tail(
    mapping_count: usize,
    unresolved_count: usize,
    dropped_count: usize,
    report: &SyncReport,
) -> String {
    let s = &report.stats;
    // Neither of these reached the engine, so they are absent from every
    // count above and need their own clauses.
    let unresolved_clause = if unresolved_count > 0 {
        format!(" | {unresolved_count} unresolved")
    } else {
        String::new()
    };
    let dropped_clause = if dropped_count > 0 {
        let plural = if dropped_count == 1 {
            "target"
        } else {
            "targets"
        };
        format!(" | {dropped_count} {plural} dropped")
    } else {
        String::new()
    };
    format!(
        "summary: {mapping_count} mappings | {} synced, {} skipped, {} failed{unresolved_clause}{dropped_clause} | {} in {}",
        s.images_synced,
        s.images_skipped,
        s.images_failed,
        format_bytes(s.bytes_transferred),
        format_duration(report.duration),
    )
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

/// Build a `RegistryClient` for each registry at least one mapping references,
/// keyed by config alias.
///
/// Auth setup reaches the network (ECR, GAR, ACR all mint a token), so any one
/// registry can fail. Record the failure against that alias instead of
/// aborting: registries the failing one shares no mapping with still sync, and
/// the mappings that do reference it fail with the original error text.
pub(crate) async fn build_clients(config: &Config, tracker: &PrepareTracker) -> ClientMap {
    let referenced = referenced_registries(config);
    tracker.begin(PreparePhase::Registries, referenced.len());
    let mut clients = ClientMap::with_capacity(referenced.len());
    for (name, reg) in &config.registries {
        if !referenced.contains(name) {
            continue;
        }
        let hostname = bare_hostname(&reg.url);
        let entry = match build_registry_client(hostname, Some(reg)).await {
            Ok(client) => Ok(Arc::new(client)),
            Err(err) => {
                tracing::error!(
                    registry = %name,
                    error = %err,
                    "registry client setup failed; mappings using this registry will be skipped"
                );
                Err(ClientInitError {
                    auth: matches!(err.exit_code(), ExitCode::AuthError),
                    message: err.to_string(),
                })
            }
        };
        clients.insert(name.clone(), entry);
        tracker.advance();
    }
    clients
}

/// Registry aliases at least one mapping actually references.
///
/// Building a client mints a token for ECR, GAR, and ACR, so a registry no
/// mapping uses costs an auth round trip against the rate-limit budget and, on
/// failure, logs an error nothing depends on. A target value that does not
/// resolve is left out here; `resolve_mapping` reports it per mapping.
fn referenced_registries(config: &Config) -> HashSet<String> {
    let known: HashSet<&str> = config.registries.keys().map(String::as_str).collect();
    let defaults = config.defaults.as_ref();
    let mut used = HashSet::new();
    for mapping in &config.mappings {
        if let Some(source) = mapping
            .source
            .as_deref()
            .or(defaults.and_then(|d| d.source.as_deref()))
            // Only aliases that exist: an unknown one is reported per mapping
            // during resolution, and counting it here inflates the progress
            // total past the number of clients actually built.
            .filter(|name| known.contains(name))
        {
            used.insert(source.to_owned());
        }
        if let Ok(names) = mapping_target_names(mapping, config, &known) {
            used.extend(names);
        }
    }
    used
}

/// Registry aliases named as a target by at least one mapping.
fn referenced_targets(config: &Config) -> HashSet<String> {
    let known: HashSet<&str> = config.registries.keys().map(String::as_str).collect();
    config
        .mappings
        .iter()
        .filter_map(|m| mapping_target_names(m, config, &known).ok())
        .flatten()
        .collect()
}

/// Target registry aliases a mapping names, with groups expanded.
///
/// Shared with [`resolve_mapping`] so the set built here cannot drift from the
/// set resolved there: a registry present in one and absent from the other
/// would surface as a misleading "not found in clients".
fn mapping_target_names(
    mapping: &MappingConfig,
    config: &Config,
    known: &HashSet<&str>,
) -> Result<Vec<String>, CliError> {
    let Some(targets) = mapping
        .targets
        .as_ref()
        .or(config.defaults.as_ref().and_then(|d| d.targets.as_ref()))
    else {
        return Err(CliError::Input(format!(
            "mapping '{}': no target registries (set mapping.targets or defaults.targets)",
            mapping.from,
        )));
    };
    let context = format!("mapping '{}'", mapping.from);
    resolve_target_names(targets, config, known, &context).map_err(CliError::Config)
}

/// Look up a usable client for `name`, turning a missing or failed registry
/// into a mapping-scoped error rather than a run-wide one.
fn client_for(
    clients: &ClientMap,
    name: &str,
    mapping_from: &str,
    role: RegistryRole,
) -> Result<Arc<RegistryClient>, CliError> {
    match clients.get(name) {
        Some(Ok(client)) => Ok(Arc::clone(client)),
        Some(Err(err)) => {
            let msg = format!(
                "mapping '{mapping_from}': {role} registry '{name}' is unavailable: {}",
                err.message
            );
            // A denied registry stays a denial here, so a run that failed only
            // on credentials still exits with the auth code.
            Err(if err.auth {
                CliError::Auth(msg)
            } else {
                CliError::Input(msg)
            })
        }
        None => Err(CliError::Input(format!(
            "mapping '{mapping_from}': {role} registry '{name}' not found in clients"
        ))),
    }
}

/// Build batch blob checkers for ECR registries.
///
/// Creates a [`BatchChecker`] for every registry that a mapping names as a
/// *target*, whose client built successfully, and that is detected as ECR (via
/// explicit `auth_type: ecr` or hostname auto-detection). No user
/// configuration is needed - if we know it's ECR, we use the batch API.
///
/// Targets only: the map is read in exactly one place, keyed by target alias,
/// so a checker for a source-only registry is an AWS round trip per cycle that
/// nothing can consume.
async fn build_batch_checkers(
    config: &Config,
    clients: &ClientMap,
    tracker: &PrepareTracker,
) -> HashMap<String, Rc<dyn BatchBlobChecker>> {
    let mut checkers: HashMap<String, Rc<dyn BatchBlobChecker>> = HashMap::new();

    // The client map is already the referenced set, so deriving it from there
    // avoids re-resolving every mapping's target groups a second time. Only
    // registries whose client actually built are worth a checker: the rest
    // have already failed and would just repeat the round trip.
    let targets = referenced_targets(config);
    let usable: Vec<(&String, &RegistryConfig)> = config
        .registries
        .iter()
        .filter(|(name, _)| {
            targets.contains(name.as_str()) && matches!(clients.get(*name), Some(Ok(_)))
        })
        .collect();
    tracker.begin(PreparePhase::BatchCheckers, usable.len());
    for (name, reg) in usable {
        let hostname = bare_hostname(&reg.url);
        // Explicit non-Ecr auth_type is a hard opt-out: don't try to build an
        // AWS-SDK-backed batch checker for a registry the user has declared
        // is not ECR, even if the hostname pattern matches. Only fall
        // through to hostname auto-detection when no `auth_type` is set.
        let is_ecr = match reg.auth_type.as_ref() {
            Some(AuthType::Ecr) => true,
            Some(_) => false,
            None => detect_provider_kind(hostname) == Some(ProviderKind::Ecr),
        };

        if !is_ecr {
            tracker.advance();
            continue;
        }

        // The checker only gates the manifest commit on ECR's blob-visibility
        // API. Losing it costs an optimization, not correctness -- `with_retry`
        // still catches the `BLOB_UPLOAD_UNKNOWN` this would have avoided -- so
        // a failure here degrades rather than aborting the run.
        match BatchChecker::from_hostname(hostname, reg.aws_profile.as_deref()).await {
            Ok(checker) => {
                checkers.insert(name.clone(), Rc::new(checker));
            }
            Err(e) => tracing::warn!(
                registry = %name,
                error = %e,
                "ECR batch checker unavailable; manifest commits fall back to retry-on-conflict"
            ),
        }
        tracker.advance();
    }

    checkers
}

/// What resolving a whole config produced.
pub(crate) struct Resolution {
    /// Mappings ready for the engine.
    pub resolved: Vec<ResolvedMapping>,
    /// Mappings that produced no work at all.
    pub unresolved: Vec<UnresolvedMapping>,
    /// Targets dropped from otherwise usable mappings.
    pub dropped_targets: Vec<DroppedTarget>,
    /// Mappings whose filter legitimately matched nothing.
    ///
    /// Healthy, but they contribute no images, so without counting them a run
    /// that was half fine and half denied looks wholly denied.
    pub no_match: usize,
}

/// Resolve every configured mapping, isolating per-mapping failures.
///
/// One unreachable registry, one 403 on a tag listing, or one bad repository
/// name must not cost the run every other mapping. Each failure is logged and
/// recorded; resolution continues. Returns the mappings that are ready for the
/// engine plus one [`UnresolvedMapping`] per mapping that is not.
async fn resolve_all(
    config: &Config,
    clients: &ClientMap,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    with_report: bool,
    mut watch_log: Option<&mut WatchLogState>,
    tracker: &PrepareTracker,
) -> Resolution {
    tracker.begin(PreparePhase::Mappings, config.mappings.len());
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    let mut dropped_targets = Vec::new();
    let mut no_match = 0usize;

    for mapping in &config.mappings {
        // Dropped targets come back whatever the outcome, so a mapping that
        // loses a mirror and then fails on its tag listing still reports the
        // outage. Reconciled against the previous cycle before anything else,
        // so a mirror that recovered re-arms even if the mapping then failed.
        let (outcome, dropped) =
            resolve_mapping(mapping, config, clients, batch_checkers, with_report).await;
        reconcile_dropped_targets(&mapping.from, &dropped, watch_log.as_deref_mut());
        dropped_targets.extend(dropped);

        match outcome {
            Ok(MappingResolution::Resolved(m)) => {
                if let Some(state) = watch_log.as_deref_mut() {
                    state.observe_mapping_resolved(&mapping.from);
                }
                resolved.push(m);
            }
            Ok(MappingResolution::NoMatchingTags(info)) => {
                if let Some(state) = watch_log.as_deref_mut() {
                    state.observe_mapping_resolved(&mapping.from);
                }
                let should_warn = match watch_log.as_deref_mut() {
                    Some(state) => state.observe_no_match(&info.from),
                    None => true,
                };
                if should_warn {
                    emit_no_tags_warn(&info);
                }
                // A filter that legitimately matches nothing is a healthy
                // mapping with nothing to do, not a failure.
                no_match += 1;
            }
            Err(err) => {
                // Gated like every other watch surface: without it a
                // permanently broken mapping logs an ERROR every cycle while
                // `cycle_emit_count` stays at zero, so watch prints "no state
                // changes" beside the error stream it just produced.
                let should_log = match watch_log.as_deref_mut() {
                    Some(state) => state.observe_mapping_unresolved(&mapping.from),
                    None => true,
                };
                if should_log {
                    log_unresolved_mapping(&mapping.from, &err);
                }
                unresolved.push(UnresolvedMapping {
                    from: mapping.from.clone(),
                    code: err.exit_code(),
                    error: err.to_string(),
                });
            }
        }
        tracker.advance();
    }

    Resolution {
        resolved,
        unresolved,
        dropped_targets,
        no_match,
    }
}

/// WARN once per target that goes down, not once per watch cycle.
///
/// Reconciles rather than accumulates: a mirror that came back is forgotten
/// even while a sibling stays down, so when it fails again it warns again.
/// Accumulating suppressed the second outage until every target of the mapping
/// was simultaneously healthy, which a flapping fleet may never reach.
fn reconcile_dropped_targets(
    from: &str,
    dropped: &[DroppedTarget],
    watch_log: Option<&mut WatchLogState>,
) {
    let Some(state) = watch_log else {
        for target in dropped {
            warn_dropped_target(target);
        }
        return;
    };
    let now: HashSet<&str> = dropped.iter().map(|t| t.registry.as_str()).collect();
    state.forget_recovered_targets(from, &now);
    for target in dropped {
        if state.observe_dropped_target(from, &target.registry) {
            warn_dropped_target(target);
        }
    }
}

fn warn_dropped_target(target: &DroppedTarget) {
    tracing::warn!(
        from = %target.from,
        registry = %target.registry,
        error = %target.error,
        "target registry unavailable; syncing the mapping's remaining targets"
    );
}

/// Report a mapping that could not be turned into work.
///
/// Shared with `analyze`, which drops the same mapping for the same reasons
/// and should say so the same way.
pub(crate) fn log_unresolved_mapping(from: &str, err: &CliError) {
    tracing::error!(
        from = %from,
        error = %err,
        "mapping could not be resolved; continuing with the remaining mappings"
    );
}

/// Build a mapping's target entries, keeping the ones whose registry works.
///
/// One unusable target must not take its siblings with it: a mapping that fans
/// out to three registries still syncs to the two that work. Mirrors the
/// per-target degradation the immutable-tag listing already does.
///
/// Returns the usable entries, the dropped ones, and the last error, which the
/// caller needs when nothing is left. Reporting is the caller's job: only it
/// knows whether any target survived, and `sync` and `analyze` word it
/// differently.
fn build_targets(
    target_names: Vec<String>,
    clients: &ClientMap,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    from: &str,
    dropped: &mut Vec<DroppedTarget>,
) -> (Vec<TargetEntry>, Option<CliError>) {
    let mut targets = Vec::with_capacity(target_names.len());
    let mut worst_err: Option<CliError> = None;
    for name in target_names {
        match client_for(clients, &name, from, RegistryRole::Target) {
            Ok(client) => targets.push(TargetEntry {
                batch_checker: batch_checkers.get(&name).cloned(),
                name: RegistryAlias::new(name),
                client,
                existing_tags: HashSet::new(),
            }),
            Err(err) => {
                dropped.push(DroppedTarget {
                    from: from.to_owned(),
                    registry: name,
                    error: err.to_string(),
                });
                // Keep a denial over anything else rather than the last one
                // seen: target order comes straight from the config file, and
                // the same outage should not exit 2 or 4 depending on how the
                // list happens to be written.
                let replace = match &worst_err {
                    None => true,
                    Some(existing) => {
                        !matches!(existing.exit_code(), ExitCode::AuthError)
                            && matches!(err.exit_code(), ExitCode::AuthError)
                    }
                };
                if replace {
                    worst_err = Some(err);
                }
            }
        }
    }
    (targets, worst_err)
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
    clients: &ClientMap,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    with_report: bool,
) -> (Result<MappingResolution, CliError>, Vec<DroppedTarget>) {
    // Targets are resolved first and returned unconditionally: every `?` in
    // the rest of resolution would otherwise discard them, and a mapping that
    // loses a mirror and then hits a rate limit on its tag listing would
    // report the outage nowhere.
    let mut dropped = Vec::new();
    let result = resolve_mapping_inner(
        mapping,
        config,
        clients,
        batch_checkers,
        with_report,
        &mut dropped,
    )
    .await;
    (result, dropped)
}

async fn resolve_mapping_inner(
    mapping: &MappingConfig,
    config: &Config,
    clients: &ClientMap,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    with_report: bool,
    dropped: &mut Vec<DroppedTarget>,
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

    let source_client = client_for(clients, source_name, &mapping.from, RegistryRole::Source)?;

    // --- Target registries ---
    let known: HashSet<&str> = config.registries.keys().map(String::as_str).collect();
    let target_names = mapping_target_names(mapping, config, &known)?;

    let (mut targets, last_target_err) = build_targets(
        target_names,
        clients,
        batch_checkers,
        &mapping.from,
        dropped,
    );
    // Every named target failed, so the mapping has nowhere to sync. Surface
    // the underlying cause rather than a generic "no targets". A config that
    // names no targets at all is left alone: that produced a zero-target
    // mapping before, and the engine handles it.
    if targets.is_empty()
        && let Some(err) = last_target_err
    {
        return Err(err);
    }
    if targets.is_empty() {
        // Reachable via `targets: []`. The engine treats it as a degenerate
        // mapping, but it still costs a source manifest pull per tag, so say
        // so rather than letting it look like work.
        tracing::warn!(
            from = %mapping.from,
            "mapping names no target registries; nothing will be pushed"
        );
    }

    // --- Fetch and filter tags ---
    let source_repo_path = RepositoryName::new(&mapping.from)?;

    let mapping_tags = mapping.tags.as_ref();
    let defaults_tags = config.defaults.as_ref().and_then(|d| d.tags.as_ref());

    // Fast path: when the config specifies only exact tag names (no
    // wildcards, semver, latest, exclude), use them directly without
    // enumerating all tags from the source registry. The fast path is
    // gated on the mapping having no `defaults.tags` block in play --
    // otherwise inherited filters (notably `defaults.exclude`) would be
    // skipped silently.
    // The image/artifact partition + sample collection happen in the same
    // pass that prepares input for `select_filtered_tags`, so the filter and
    // the no-match WARN both see consistent counts. The pre-built `NoTagsInfo`
    // is only consumed when filtering yields zero tags.
    let (filtered, candidate_count, filter_report, no_tags_template): (
        Vec<String>,
        Option<usize>,
        Option<ocync_sync::filter::FilterReport>,
        Option<NoTagsInfo>,
    ) = if let Some(exact) = mapping_tags
        .filter(|_| defaults_tags.is_none())
        .and_then(|t| t.exact_tags())
    {
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
            filter_desc: describe_filter(mapping_tags, defaults_tags),
            samples,
        };
        let (kept, count, report) =
            select_filtered_tags(mapping_tags, defaults_tags, all_tags, with_report)?;
        (kept, count, report, Some(template))
    };

    if filtered.is_empty() {
        let info = no_tags_template.unwrap_or_else(|| NoTagsInfo {
            from: mapping.from.clone(),
            image_count: 0,
            artifact_count: 0,
            filter_desc: describe_filter(mapping_tags, defaults_tags),
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
    let immutable_pattern = resolve_immutable_pattern(mapping_tags, defaults_tags);
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
    // `from` is omitted as a structured field -- the message renders it
    // first, so the text formatter would otherwise tail with a redundant
    // `from=...`. Counts and filter remain (numeric, terse, useful for
    // both grep and JSON aggregation).
    tracing::warn!(
        source_total = info.source_total(),
        image_count = info.image_count,
        artifact_count = info.artifact_count,
        filter = info.filter_desc.as_deref().unwrap_or(""),
        "{info}"
    );
}

/// Build a `FilterConfig` from a mapping `TagsConfig` plus an optional
/// `defaults.tags` block. Field-level merge: any field set on the mapping
/// wins; unset fields fall through to `defaults`. The exclude lists are
/// kept separate by source -- mapping exclude is the hard tier (blocks
/// `include:`), defaults exclude is the soft tier (bypassable by `include:`).
fn build_filter(mapping: Option<&TagsConfig>, defaults: Option<&TagsConfig>) -> FilterConfig {
    let pick_glob = |get: fn(&TagsConfig) -> Option<&GlobOrList>| -> Vec<String> {
        mapping
            .and_then(get)
            .or_else(|| defaults.and_then(get))
            .map(glob_or_list_to_vec_owned)
            .unwrap_or_default()
    };

    FilterConfig {
        include: pick_glob(|t| t.include.as_ref()),
        glob: pick_glob(|t| t.glob.as_ref()),
        semver: mapping
            .and_then(|t| t.semver.clone())
            .or_else(|| defaults.and_then(|t| t.semver.clone())),
        defaults_exclude: defaults
            .and_then(|t| t.exclude.as_ref())
            .map(glob_or_list_to_vec_owned)
            .unwrap_or_default(),
        exclude: mapping
            .and_then(|t| t.exclude.as_ref())
            .map(glob_or_list_to_vec_owned)
            .unwrap_or_default(),
        sort: mapping
            .and_then(|t| t.sort)
            .or_else(|| defaults.and_then(|t| t.sort)),
        latest: mapping
            .and_then(|t| t.latest)
            .or_else(|| defaults.and_then(|t| t.latest)),
        min_tags: mapping
            .and_then(|t| t.min_tags)
            .or_else(|| defaults.and_then(|t| t.min_tags)),
    }
}

/// Resolve `immutable_tags` from a mapping + defaults pair: mapping wins,
/// then falls through to defaults. Lives outside [`build_filter`] because
/// `immutable_tags` is consumed by the skip-optimization path, not the
/// filter pipeline.
fn resolve_immutable_pattern<'a>(
    mapping: Option<&'a TagsConfig>,
    defaults: Option<&'a TagsConfig>,
) -> Option<&'a str> {
    mapping
        .and_then(|t| t.immutable_tags.as_deref())
        .or_else(|| defaults.and_then(|t| t.immutable_tags.as_deref()))
}

/// Flatten a [`GlobOrList`] into an owned `Vec<String>`. Used by the
/// merge path which already holds a borrow.
fn glob_or_list_to_vec_owned(g: &GlobOrList) -> Vec<String> {
    match g {
        GlobOrList::Single(s) => vec![s.clone()],
        GlobOrList::List(v) => v.clone(),
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
    mapping_tags: Option<&TagsConfig>,
    defaults_tags: Option<&TagsConfig>,
    all_tags: Vec<String>,
    with_report: bool,
) -> Result<SelectionResult, CliError> {
    let n_candidates = all_tags.len();
    let filter = build_filter(mapping_tags, defaults_tags);
    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    if with_report {
        let result = filter.apply_with_report(&tag_refs)?;
        Ok((result.kept, Some(n_candidates), Some(result.report)))
    } else {
        Ok((filter.apply(&tag_refs)?, Some(n_candidates), None))
    }
}

/// One-line summary of mapping + defaults tags suitable for log emission,
/// e.g. `semver >=1.0.0, latest=5`. Returns `None` when no filter applies.
///
/// Single source of truth: delegates to [`FilterConfig::describe`] after
/// the same merge the engine uses, so dry-run stage labels and the
/// no-tags-matched WARN rationale cannot drift.
fn describe_filter(
    mapping_tags: Option<&TagsConfig>,
    defaults_tags: Option<&TagsConfig>,
) -> Option<String> {
    build_filter(mapping_tags, defaults_tags).describe()
}

/// The `--json` document: the engine's report plus the mappings that never
/// reached it.
///
/// Without the extra key a consumer that only reads the report would see a
/// clean run when mappings were dropped before the engine started.
#[derive(Serialize)]
struct SyncOutput<'a> {
    #[serde(flatten)]
    report: &'a SyncReport,
    /// Mappings dropped during resolution. Absent when every mapping resolved,
    /// so the document shape is unchanged on a clean run.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    unresolved_mappings: &'a [UnresolvedMapping],
    /// Targets a resolved mapping could not use. Absent when none were
    /// dropped, so the document shape is unchanged on a clean run.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    dropped_targets: &'a [DroppedTarget],
}

/// Write sync output as JSON when `--json` is passed.
fn write_output(
    report: &SyncReport,
    unresolved: &[UnresolvedMapping],
    dropped: &[DroppedTarget],
    json: bool,
) -> Result<(), CliError> {
    if json {
        let output = SyncOutput {
            report,
            unresolved_mappings: unresolved,
            dropped_targets: dropped,
        };
        let json = serde_json::to_string_pretty(&output)
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
        let filter = build_filter(None, None);
        assert!(filter.glob.is_empty());
        assert!(filter.semver.is_none());
        assert!(filter.exclude.is_empty());
        assert!(filter.defaults_exclude.is_empty());
        assert!(filter.sort.is_none());
        assert!(filter.latest.is_none());
        assert!(filter.min_tags.is_none());
    }

    /// `defaults.tags.exclude:` reaches a mapping that has its own `tags:`
    /// block. Today's `or` resolution would drop it. After the merge, the
    /// patterns land in `FilterConfig.defaults_exclude` (the soft tier).
    #[test]
    fn build_filter_defaults_exclude_reaches_mapping_with_own_tags() {
        let mapping = TagsConfig {
            semver: Some(">=1.0".into()),
            ..Default::default()
        };
        let defaults = TagsConfig {
            exclude: Some(GlobOrList::List(vec!["*-dev".into(), "*-r[0-9]*".into()])),
            ..Default::default()
        };
        let filter = build_filter(Some(&mapping), Some(&defaults));
        assert_eq!(filter.semver.as_deref(), Some(">=1.0"));
        assert_eq!(filter.defaults_exclude, vec!["*-dev", "*-r[0-9]*"]);
        assert!(filter.exclude.is_empty(), "mapping had no exclude");
    }

    /// `mapping.tags.exclude:` lands in the hard tier; `defaults.exclude`
    /// lands in the soft tier. Both apply (concat semantics).
    #[test]
    fn build_filter_mapping_exclude_is_hard_tier() {
        let mapping = TagsConfig {
            exclude: Some(GlobOrList::Single("*-slim".into())),
            ..Default::default()
        };
        let defaults = TagsConfig {
            exclude: Some(GlobOrList::Single("*-dev".into())),
            ..Default::default()
        };
        let filter = build_filter(Some(&mapping), Some(&defaults));
        assert_eq!(filter.exclude, vec!["*-slim"]);
        assert_eq!(filter.defaults_exclude, vec!["*-dev"]);
    }

    /// When mapping has no `tags:` block at all, defaults' filter fields
    /// flow through. `defaults.exclude` still goes to the soft tier --
    /// the source decides the tier, not whether the mapping was set.
    #[test]
    fn build_filter_inherits_defaults_when_mapping_unset() {
        let defaults = TagsConfig {
            semver: Some(">=2.0".into()),
            exclude: Some(GlobOrList::Single("*-dev".into())),
            ..Default::default()
        };
        let filter = build_filter(None, Some(&defaults));
        assert_eq!(filter.semver.as_deref(), Some(">=2.0"));
        assert_eq!(filter.defaults_exclude, vec!["*-dev"]);
        assert!(filter.exclude.is_empty());
    }

    #[test]
    fn build_filter_single_glob() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::Single("v1.*".into())),
            ..Default::default()
        };
        let filter = build_filter(Some(&tags), None);
        assert_eq!(filter.glob, vec!["v1.*"]);
    }

    #[test]
    fn build_filter_glob_list() {
        let tags = TagsConfig {
            glob: Some(GlobOrList::List(vec!["v1.*".into(), "v2.*".into()])),
            ..Default::default()
        };
        let filter = build_filter(Some(&tags), None);
        assert_eq!(filter.glob, vec!["v1.*", "v2.*"]);
    }

    #[test]
    fn build_filter_exclude_patterns() {
        let tags = TagsConfig {
            exclude: Some(GlobOrList::List(vec!["*-rc*".into(), "*-beta*".into()])),
            ..Default::default()
        };
        let filter = build_filter(Some(&tags), None);
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
        let filter = build_filter(Some(&tags), None);
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
        let filter = build_filter(Some(&tags), None);

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

    /// Every fall-through field on the merge model: `sort`, `latest`,
    /// `min_tags`, `include`, `glob`, `semver`. When the mapping leaves
    /// each unset, the value comes from `defaults.tags`; when set, the
    /// mapping wins. One test, six pairs of assertions, no scaffolding.
    #[test]
    fn merge_inherits_all_fall_through_fields() {
        use ocync_sync::filter::SortOrder;

        let defaults = TagsConfig {
            include: Some(GlobOrList::Single("latest".into())),
            glob: Some(GlobOrList::Single("v*".into())),
            semver: Some(">=1.0".into()),
            sort: Some(SortOrder::Semver),
            latest: Some(10),
            min_tags: Some(2),
            immutable_tags: Some("v?[0-9]*.[0-9]*.[0-9]*".into()),
            ..Default::default()
        };

        // 1. Mapping unset on everything: defaults flow through.
        let empty_mapping = TagsConfig::default();
        let inherited = build_filter(Some(&empty_mapping), Some(&defaults));
        assert_eq!(inherited.include, vec!["latest"]);
        assert_eq!(inherited.glob, vec!["v*"]);
        assert_eq!(inherited.semver.as_deref(), Some(">=1.0"));
        assert_eq!(inherited.sort, Some(SortOrder::Semver));
        assert_eq!(inherited.latest, Some(10));
        assert_eq!(inherited.min_tags, Some(2));
        assert_eq!(
            resolve_immutable_pattern(Some(&empty_mapping), Some(&defaults)),
            Some("v?[0-9]*.[0-9]*.[0-9]*"),
        );

        // 2. Mapping sets every field: mapping wins on every field.
        let override_mapping = TagsConfig {
            include: Some(GlobOrList::Single("override".into())),
            glob: Some(GlobOrList::Single("override*".into())),
            semver: Some(">=2.0".into()),
            sort: Some(SortOrder::Alpha),
            latest: Some(3),
            min_tags: Some(1),
            immutable_tags: Some("override-pattern".into()),
            ..Default::default()
        };
        let overridden = build_filter(Some(&override_mapping), Some(&defaults));
        assert_eq!(overridden.include, vec!["override"]);
        assert_eq!(overridden.glob, vec!["override*"]);
        assert_eq!(overridden.semver.as_deref(), Some(">=2.0"));
        assert_eq!(overridden.sort, Some(SortOrder::Alpha));
        assert_eq!(overridden.latest, Some(3));
        assert_eq!(overridden.min_tags, Some(1));
        assert_eq!(
            resolve_immutable_pattern(Some(&override_mapping), Some(&defaults)),
            Some("override-pattern"),
        );
    }

    /// `resolve_immutable_pattern` falls back when only one side carries
    /// a pattern, and returns `None` when neither does.
    #[test]
    fn resolve_immutable_pattern_handles_partial_set() {
        let with_immutable = TagsConfig {
            immutable_tags: Some("v?[0-9]*".into()),
            ..Default::default()
        };
        let empty = TagsConfig::default();

        assert_eq!(
            resolve_immutable_pattern(Some(&with_immutable), None),
            Some("v?[0-9]*")
        );
        assert_eq!(
            resolve_immutable_pattern(None, Some(&with_immutable)),
            Some("v?[0-9]*")
        );
        assert_eq!(
            resolve_immutable_pattern(Some(&empty), Some(&with_immutable)),
            Some("v?[0-9]*")
        );
        assert_eq!(resolve_immutable_pattern(Some(&empty), Some(&empty)), None);
        assert_eq!(resolve_immutable_pattern(None, None), None);
    }

    /// Realistic Chainguard scenario: `defaults.exclude` filters dev and
    /// `-rN` revisions across the project, one mapping uses `include:` to
    /// rescue `latest-dev`, another adds a hard-tier mapping `exclude:`
    /// for `-slim` variants. Asserts the final keep set + dry-run drop
    /// attribution by tier.
    #[test]
    fn merge_chainguard_scenario_end_to_end() {
        let defaults_yaml = r#"
exclude: ["*-dev", "*-r[0-9]*"]
"#;
        let defaults: TagsConfig =
            serde_yaml::from_str(defaults_yaml).expect("defaults yaml parses");

        // Realistic cgr.dev tag list for a single repo: stable releases,
        // dev variants, package revisions, slim variants, an RC.
        let tags = vec![
            "1.27",
            "1.27-r0",
            "1.27-r1",
            "1.27-dev",
            "1.27-slim",
            "1.27-rc1",
            "latest",
            "latest-dev",
        ];

        // --- Mapping A: pure inheritance (no `tags:` block) ---
        // Today's bug: this mapping silently lost `defaults.exclude`. After
        // the fix, all dev/-rN variants drop, RC drops via built-in.
        let no_mapping_filter = build_filter(None, Some(&defaults));
        let kept_a = no_mapping_filter
            .apply(&tags)
            .expect("filter applies (mapping A)");
        assert_eq!(
            kept_a,
            vec![
                "1.27".to_string(),
                "1.27-slim".to_string(),
                "latest".to_string()
            ],
            "mapping A should keep stable + slim + latest only",
        );

        // --- Mapping B: `include: ["latest-dev"]` rescues from soft tier ---
        let mapping_b: TagsConfig = serde_yaml::from_str(
            r#"
include: ["latest-dev"]
"#,
        )
        .expect("mapping B yaml parses");
        let filter_b = build_filter(Some(&mapping_b), Some(&defaults));
        let kept_b = filter_b.apply(&tags).expect("filter applies (mapping B)");
        assert!(
            kept_b.contains(&"latest-dev".to_string()),
            "include: should rescue latest-dev from defaults.exclude"
        );
        assert!(
            !kept_b.contains(&"1.27-dev".to_string()),
            "non-included dev variants still drop"
        );
        assert!(
            !kept_b.contains(&"1.27-r0".to_string()),
            "include: doesn't rescue what it doesn't list"
        );

        // --- Mapping C: hard-tier `exclude: ["*-slim"]` stacks on defaults ---
        let mapping_c: TagsConfig = serde_yaml::from_str(
            r#"
exclude: ["*-slim"]
"#,
        )
        .expect("mapping C yaml parses");
        let filter_c = build_filter(Some(&mapping_c), Some(&defaults));
        let kept_c = filter_c.apply(&tags).expect("filter applies (mapping C)");
        assert_eq!(
            kept_c,
            vec!["1.27".to_string(), "latest".to_string()],
            "mapping C drops slim (hard tier) + dev/-rN (soft tier) + rc (built-in)",
        );

        // --- Mapping D: dry-run attribution proves the tier breakdown ---
        // Use mapping C's filter and verify each drop carries the right
        // DropKind. This is the operator-facing observability check.
        let report = filter_c
            .apply_with_report(&tags)
            .expect("apply_with_report succeeds");

        let mapping_drops: Vec<&String> = report
            .report
            .dropped
            .iter()
            .filter(|d| matches!(d.kind, ocync_sync::filter::DropKind::MappingExclude { .. }))
            .flat_map(|d| d.samples.iter())
            .collect();
        assert_eq!(
            mapping_drops,
            vec![&"1.27-slim".to_string()],
            "MappingExclude bucket carries only the slim variant",
        );

        let defaults_drops: HashSet<String> = report
            .report
            .dropped
            .iter()
            .filter(|d| matches!(d.kind, ocync_sync::filter::DropKind::DefaultsExclude { .. }))
            .flat_map(|d| d.samples.iter().cloned())
            .collect();
        let expected_defaults: HashSet<String> = ["1.27-r0", "1.27-r1", "1.27-dev", "latest-dev"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            defaults_drops, expected_defaults,
            "DefaultsExclude bucket carries dev + -rN variants"
        );

        let builtin_drops: Vec<&String> = report
            .report
            .dropped
            .iter()
            .filter(|d| matches!(d.kind, ocync_sync::filter::DropKind::BuiltinExclude))
            .flat_map(|d| d.samples.iter())
            .collect();
        assert_eq!(
            builtin_drops,
            vec![&"1.27-rc1".to_string()],
            "BuiltinExclude bucket carries the RC tag"
        );
    }

    /// YAML round-trip: parse a `defaults.tags` block + a per-mapping
    /// `tags:` block via the production deserializer and confirm the merge
    /// puts `defaults.exclude` patterns into the soft tier and mapping
    /// patterns into the hard tier. Catches future serde-aliasing or
    /// field-renaming regressions that bypass `build_filter`'s logic.
    #[test]
    fn merge_yaml_round_trip_separates_exclude_tiers() {
        let defaults_yaml = r#"
exclude: ["*-dev", "*-r[0-9]*"]
sort: semver
latest: 5
"#;
        let mapping_yaml = r#"
semver: ">=1.0"
exclude: ["*-slim"]
"#;
        let defaults: TagsConfig =
            serde_yaml::from_str(defaults_yaml).expect("defaults yaml parses");
        let mapping: TagsConfig = serde_yaml::from_str(mapping_yaml).expect("mapping yaml parses");

        let filter = build_filter(Some(&mapping), Some(&defaults));

        // defaults.exclude reaches the soft tier verbatim.
        assert_eq!(filter.defaults_exclude, vec!["*-dev", "*-r[0-9]*"]);
        // mapping.exclude is the hard tier, separate from defaults.
        assert_eq!(filter.exclude, vec!["*-slim"]);
        // mapping fields override; unset fields inherit from defaults.
        assert_eq!(filter.semver.as_deref(), Some(">=1.0"));
        assert_eq!(filter.sort, Some(ocync_sync::filter::SortOrder::Semver));
        assert_eq!(filter.latest, Some(5));

        // End-to-end behavior: 1.27-r0 dropped by defaults soft tier,
        // 1.27-slim dropped by mapping hard tier, 1.27 survives.
        let tags = vec!["1.27", "1.27-r0", "1.27-dev", "1.27-slim"];
        let kept = filter.apply(&tags).expect("filter applies");
        assert_eq!(kept, vec!["1.27".to_string()]);
    }

    /// End-to-end YAML proof of the build+runtime case: project-wide
    /// `defaults.tags.exclude` denies `*-dev` and `*-r[0-9]*`; one mapping
    /// uses a glob `include:` to rescue `-dev` for a bounded semver range.
    /// Catches regressions in either the filter pipeline or the merge layer.
    #[test]
    fn merge_glob_include_with_semver_yaml_e2e() {
        let defaults_yaml = r#"
exclude: ["*-dev", "*-r[0-9]*"]
sort: semver
latest: 10
"#;
        let mapping_yaml = r#"
semver: ">=8.0.0, <9.0.0"
include: ["*-dev"]
"#;
        let defaults: TagsConfig =
            serde_yaml::from_str(defaults_yaml).expect("defaults yaml parses");
        let mapping: TagsConfig = serde_yaml::from_str(mapping_yaml).expect("mapping yaml parses");

        let filter = build_filter(Some(&mapping), Some(&defaults));

        // Mapping include reaches the FilterConfig; defaults exclude lands
        // in the soft tier; semver/sort/latest inherit from defaults.
        assert_eq!(filter.include, vec!["*-dev"]);
        assert_eq!(filter.defaults_exclude, vec!["*-dev", "*-r[0-9]*"]);
        assert_eq!(filter.semver.as_deref(), Some(">=8.0.0, <9.0.0"));
        assert_eq!(filter.latest, Some(10));

        let tags = vec![
            "8.0.0",
            "8.5.0",
            "8.5.0-dev",  // rescued by include glob, in semver range -> kept
            "8.5.0-r3",   // not in include; soft tier drops -> dropped
            "10.0.0",     // out of semver range -> dropped
            "10.0.0-dev", // rescued by include glob, but out of semver -> dropped
            "7.0.0",      // below semver -> dropped
        ];
        let kept = filter.apply(&tags).expect("filter applies");
        assert!(kept.contains(&"8.0.0".to_string()));
        assert!(kept.contains(&"8.5.0".to_string()));
        assert!(
            kept.contains(&"8.5.0-dev".to_string()),
            "in-range -dev rescued from soft tier and admitted by semver"
        );
        assert!(
            !kept.contains(&"8.5.0-r3".to_string()),
            "-r counter still dropped by defaults soft tier"
        );
        assert!(
            !kept.contains(&"10.0.0-dev".to_string()),
            "out-of-range -dev rescued but dropped by semver"
        );
        assert!(!kept.contains(&"10.0.0".to_string()));
        assert!(!kept.contains(&"7.0.0".to_string()));
    }

    /// Per-mapping `include:` REPLACES `defaults.tags.include`, not concat.
    /// The merge resolution at `build_filter` picks the mapping value if
    /// set, else falls through to defaults. This pins the field-replace
    /// semantics so a future regression cannot quietly stack the lists.
    #[test]
    fn merge_mapping_include_replaces_defaults_include() {
        let defaults_yaml = r#"
include: ["latest", "latest-dev"]
"#;
        let mapping_yaml = r#"
include: ["1.25.0-rc1"]
"#;
        let defaults: TagsConfig =
            serde_yaml::from_str(defaults_yaml).expect("defaults yaml parses");
        let mapping: TagsConfig = serde_yaml::from_str(mapping_yaml).expect("mapping yaml parses");

        let filter_inherited = build_filter(None, Some(&defaults));
        assert_eq!(
            filter_inherited.include,
            vec!["latest", "latest-dev"],
            "no mapping -> defaults include flows through"
        );

        let filter_overridden = build_filter(Some(&mapping), Some(&defaults));
        assert_eq!(
            filter_overridden.include,
            vec!["1.25.0-rc1"],
            "mapping include replaces defaults include; the two are NOT concat"
        );
    }

    #[test]
    fn glob_or_list_to_vec_owned_single() {
        let g = GlobOrList::Single("pattern".into());
        assert_eq!(glob_or_list_to_vec_owned(&g), vec!["pattern"]);
    }

    #[test]
    fn glob_or_list_to_vec_owned_list() {
        let g = GlobOrList::List(vec!["a".into(), "b".into()]);
        assert_eq!(glob_or_list_to_vec_owned(&g), vec!["a", "b"]);
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
            describe_filter(Some(&tags), None).as_deref(),
            Some("semver >=1.0.0, latest=5")
        );
    }

    #[test]
    fn describe_filter_returns_none_when_empty() {
        let tags = TagsConfig::default();
        assert!(describe_filter(Some(&tags), None).is_none());
        assert!(describe_filter(None, None).is_none());
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
            dropped_names: Vec::new(),
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
    /// A mapping that lost a target still names its registries: the one line
    /// whose job is saying where the images went must not go quiet at the
    /// moment one of them is missing.
    #[test]
    fn format_mapping_outcome_names_an_unreachable_target() {
        let d = MappingDescriptor {
            from: "library/alpine".into(),
            target_repo: "mirror/alpine".into(),
            target_names: vec!["good".into()],
            dropped_names: vec!["broken".into()],
        };
        let outcome = MappingOutcome {
            synced: 4,
            ..Default::default()
        };

        let line = format_mapping_outcome(&d, &outcome, false);

        assert!(line.contains("[good, broken unreachable]"), "{line}");
    }

    /// can see which destinations the outcome covers.
    #[test]
    fn format_mapping_outcome_multi_target_keeps_bracket() {
        let d = MappingDescriptor {
            from: "library/alpine".into(),
            target_repo: "mirror/alpine".into(),
            target_names: vec!["ecr-prod".into(), "ghcr-mirror".into()],
            dropped_names: Vec::new(),
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
            select_filtered_tags(Some(&tags_config), None, all_tags, true).unwrap();

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
            select_filtered_tags(Some(&tags_config), None, all_tags, false).unwrap();
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
        let result = select_filtered_tags(Some(&tags_config), None, all_tags, false);
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
            select_filtered_tags(Some(&tags_config), None, all_tags, true).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(candidate_count, Some(2));
        let report = report.expect("dry-run path returns report even when min_tags would error");
        assert_eq!(report.min_tags, Some(10));
    }

    // -----------------------------------------------------------------
    // Failure isolation
    // -----------------------------------------------------------------

    /// Config with three mappings; the middle one names a registry that does
    /// not exist. Tag selection uses literal globs so resolution never touches
    /// the network.
    fn three_mapping_config() -> Config {
        serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
  dst:
    url: target.test
defaults:
  source: src
  targets: [dst]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
  - from: repo/two
    source: nonexistent
    tags:
      glob: ["v1"]
  - from: repo/three
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses")
    }

    fn test_client(url: &str) -> Arc<RegistryClient> {
        Arc::new(
            ocync_distribution::RegistryClientBuilder::new(url::Url::parse(url).unwrap())
                .build()
                .unwrap(),
        )
    }

    fn working_clients() -> ClientMap {
        ClientMap::from([
            ("src".to_string(), Ok(test_client("https://source.test"))),
            ("dst".to_string(), Ok(test_client("https://target.test"))),
        ])
    }

    /// A registry whose client could not be built, as `build_clients` records it.
    fn broken_client(message: &str, auth: bool) -> Result<Arc<RegistryClient>, ClientInitError> {
        Err(ClientInitError {
            message: message.to_string(),
            auth,
        })
    }

    async fn resolve(config: &Config, clients: &ClientMap) -> Resolution {
        resolve_all(
            config,
            clients,
            &HashMap::new(),
            false,
            None,
            &PrepareTracker::default(),
        )
        .await
    }

    /// A mapping that cannot be resolved must not cost the run the mappings
    /// behind it: the mapping *after* the failure is the one that proves it.
    #[tokio::test]
    async fn resolve_all_isolates_a_failing_mapping() {
        let config = three_mapping_config();
        let Resolution {
            resolved,
            unresolved,
            ..
        } = resolve(&config, &working_clients()).await;

        assert_eq!(unresolved.len(), 1, "only the bad mapping should fail");
        assert_eq!(unresolved[0].from, "repo/two");

        // The negative half: the mapping *after* the failure still resolved.
        let names: Vec<&str> = resolved.iter().map(|m| m.source_repo.as_str()).collect();
        assert_eq!(names, vec!["repo/one", "repo/three"]);
    }

    /// A registry whose client could not be built fails only the mappings that
    /// reference it, and the original setup error reaches the caller.
    #[tokio::test]
    async fn resolve_all_scopes_a_failed_registry_to_its_mappings() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
  broken:
    url: broken.test
  dst:
    url: target.test
defaults:
  source: src
  targets: [dst]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
  - from: repo/two
    source: broken
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let clients = ClientMap::from([
            ("src".to_string(), Ok(test_client("https://source.test"))),
            (
                "broken".to_string(),
                broken_client("ECR auth setup for 'broken.test': 403 Forbidden", true),
            ),
            ("dst".to_string(), Ok(test_client("https://target.test"))),
        ]);

        let Resolution {
            resolved,
            unresolved,
            dropped_targets: _dropped,
            ..
        } = resolve(&config, &clients).await;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_repo.as_str(), "repo/one");
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].from, "repo/two");
        assert!(
            unresolved[0].error.contains("403 Forbidden"),
            "the setup error must survive to the mapping failure: {}",
            unresolved[0].error
        );
        // The classification has to survive too, or a wholly denied run
        // silently drops from the auth exit code to the generic one.
        assert!(
            matches!(unresolved[0].code, ExitCode::AuthError),
            "a denied registry must stay classified as a denial"
        );
    }

    /// Every mapping resolving means no failures recorded -- the isolation
    /// path must not manufacture failures on a clean config.
    #[tokio::test]
    async fn resolve_all_reports_no_failures_when_all_mappings_resolve() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
  dst:
    url: target.test
defaults:
  source: src
  targets: [dst]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let Resolution {
            resolved,
            unresolved,
            ..
        } = resolve(&config, &working_clients()).await;

        assert_eq!(resolved.len(), 1);
        assert!(unresolved.is_empty());
    }

    /// A mapping fanning out to several targets keeps the ones that work.
    ///
    /// This is the same principle as per-mapping isolation, one level down: a
    /// broken mirror must not stop the other mirrors from receiving the image.
    #[tokio::test]
    async fn resolve_all_keeps_the_working_targets_when_one_is_unavailable() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
  good:
    url: good.test
  broken:
    url: broken.test
defaults:
  source: src
  targets: [good, broken]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let clients = ClientMap::from([
            ("src".to_string(), Ok(test_client("https://source.test"))),
            ("good".to_string(), Ok(test_client("https://good.test"))),
            (
                "broken".to_string(),
                broken_client("ECR auth setup for 'broken.test': 403 Forbidden", true),
            ),
        ]);

        let Resolution {
            resolved,
            unresolved,
            dropped_targets: dropped,
            ..
        } = resolve(&config, &clients).await;

        assert!(
            unresolved.is_empty(),
            "one dead target must not unresolve the whole mapping"
        );
        assert_eq!(resolved.len(), 1);
        let targets: Vec<&str> = resolved[0].targets.iter().map(|t| &*t.name).collect();
        assert_eq!(targets, vec!["good"], "the healthy target must survive");

        // Surviving is not enough: a target the images never reached has to
        // stay visible, or the run reports a clean success it did not have.
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].registry, "broken");
        assert_eq!(dropped[0].from, "repo/one");
        assert!(dropped[0].error.contains("403 Forbidden"));
    }

    /// A dropped target is a hole in an otherwise clean run, so it must move
    /// the exit code off success and appear in both report surfaces.
    #[test]
    fn a_dropped_target_is_visible_in_every_surface() {
        let report = report_from(vec![ocync_sync::ImageStatus::Synced]);
        let dropped = [DroppedTarget {
            from: "repo/one".into(),
            registry: "broken".into(),
            error: "403 Forbidden".into(),
        }];

        assert_eq!(
            combined_exit_code(&report, &[], &dropped, 0),
            ExitCode::PartialFailure,
            "an unreached target must not report as a clean run"
        );
        assert_eq!(
            combined_exit_code(&report, &[], &[], 0),
            ExitCode::Success,
            "and must not fire when nothing was dropped"
        );
        assert_eq!(
            dry_run_exit_code(1, &[], &dropped),
            ExitCode::PartialFailure
        );

        let line = format_cycle_tail(1, 0, dropped.len(), &report);
        assert!(line.contains("1 target dropped"), "{line}");

        let value = serde_json::to_value(SyncOutput {
            report: &report,
            unresolved_mappings: &[],
            dropped_targets: &dropped,
        })
        .expect("serializes");
        assert_eq!(value["dropped_targets"][0]["registry"], "broken");
    }

    /// Losing every target is different from losing one: there is nowhere left
    /// to sync, so the mapping is unresolvable and keeps its cause.
    #[tokio::test]
    async fn resolve_all_fails_the_mapping_when_every_target_is_gone() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
  broken:
    url: broken.test
defaults:
  source: src
  targets: [broken]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let clients = ClientMap::from([
            ("src".to_string(), Ok(test_client("https://source.test"))),
            (
                "broken".to_string(),
                broken_client("ECR auth setup for 'broken.test': 403 Forbidden", true),
            ),
        ]);

        let Resolution {
            resolved,
            unresolved,
            dropped_targets: _dropped,
            ..
        } = resolve(&config, &clients).await;

        assert!(resolved.is_empty());
        assert_eq!(unresolved.len(), 1);
        assert!(
            unresolved[0].error.contains("403 Forbidden"),
            "the cause must survive, not a generic 'no targets': {}",
            unresolved[0].error
        );
        assert!(matches!(unresolved[0].code, ExitCode::AuthError));
    }

    /// The guard, not just the helper: `build_clients` must not construct a
    /// client for a registry no mapping names. `static_token` resolves
    /// offline, so this stays a unit test.
    #[tokio::test]
    async fn build_clients_skips_registries_no_mapping_references() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
    auth_type: static_token
    token: t
  dst:
    url: target.test
    auth_type: static_token
    token: t
  unused:
    url: unused.test
    auth_type: static_token
    token: t
defaults:
  source: src
  targets: [dst]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let clients = build_clients(&config, &PrepareTracker::default()).await;

        let mut names: Vec<&str> = clients.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["dst", "src"], "'unused' must not be built");
    }

    /// Building a client mints a token for ECR, GAR, and ACR, so a registry no
    /// mapping references must not be contacted at all.
    #[test]
    fn referenced_registries_ignores_unused_entries() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
  dst:
    url: target.test
  unused:
    url: unused.test
target_groups:
  mirrors: [dst]
defaults:
  source: src
mappings:
  - from: repo/one
    targets: mirrors
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let used = referenced_registries(&config);

        assert!(used.contains("src"));
        assert!(used.contains("dst"), "target groups must be expanded");
        assert!(
            !used.contains("unused"),
            "a registry no mapping names must not be built"
        );
    }

    /// The prepare ticker must fire on wall-clock, not on item boundaries.
    ///
    /// The throttle this replaced was evaluated only after a mapping finished,
    /// so a config with few mappings emitted nothing at all: measured on a
    /// three-mapping config, zero lines across an 8.6 second run.
    #[tokio::test(start_paused = true)]
    async fn prepare_ticker_reports_a_single_slow_step() {
        let tracker = PrepareTracker::default();
        tracker.begin(PreparePhase::Mappings, 1);
        let seen = RefCell::new(Vec::new());

        // One item that never completes within an interval: the boundary
        // throttle scored exactly this case as silent.
        tick_while(
            &tracker,
            Duration::from_secs(5),
            tokio::time::sleep(Duration::from_secs(17)),
            |p, _| seen.borrow_mut().push(p.total),
        )
        .await;

        assert_eq!(
            seen.borrow().len(),
            3,
            "17s of work at a 5s cadence must report three times"
        );
    }

    /// The negative half: work that finishes inside one interval stays silent,
    /// so a fast config gains no noise.
    #[tokio::test(start_paused = true)]
    async fn prepare_ticker_stays_silent_for_fast_work() {
        let tracker = PrepareTracker::default();
        let seen = RefCell::new(0usize);

        tick_while(
            &tracker,
            Duration::from_secs(5),
            tokio::time::sleep(Duration::from_secs(1)),
            |_, _| *seen.borrow_mut() += 1,
        )
        .await;

        assert_eq!(*seen.borrow(), 0);
    }

    /// The ticker reads live counters, not a snapshot taken when it started.
    #[tokio::test(start_paused = true)]
    async fn prepare_ticker_reports_progress_as_it_advances() {
        let tracker = PrepareTracker::default();
        tracker.begin(PreparePhase::Registries, 2);
        let seen = RefCell::new(Vec::new());

        tick_while(
            &tracker,
            Duration::from_secs(5),
            async {
                tokio::time::sleep(Duration::from_secs(7)).await;
                tracker.advance();
                tokio::time::sleep(Duration::from_secs(7)).await;
            },
            |p, _| seen.borrow_mut().push(p.done),
        )
        .await;

        assert_eq!(
            *seen.borrow(),
            vec![0, 1],
            "the second line must show the item finished in between"
        );
    }

    /// The progress line has to reach its total. The counter previously
    /// advanced only for ECR registries, so any config without one reported
    /// `done=0 total=N` forever.
    #[tokio::test]
    async fn batch_checker_progress_reaches_its_total_without_ecr() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
    auth_type: static_token
    token: t
  dst:
    url: target.test
    auth_type: static_token
    token: t
defaults:
  source: src
  targets: [dst]
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let tracker = PrepareTracker::default();
        let clients = build_clients(&config, &tracker).await;
        let checkers = build_batch_checkers(&config, &clients, &tracker).await;

        assert!(checkers.is_empty(), "neither registry is ECR");
        let p = tracker.state.get();
        assert_eq!(
            p.total, 1,
            "only the target registry is a checker candidate; the map is read by target alias"
        );
        assert_eq!(p.done, p.total, "every registry considered must be counted");
    }

    /// A watch cycle re-reporting the same dead mirror is noise; a different
    /// mirror going down is not.
    #[test]
    fn dropped_target_warnings_fire_once_per_transition() {
        let mut state = WatchLogState::default();

        assert!(state.observe_dropped_target("repo/one", "mirror-a"));
        assert!(
            !state.observe_dropped_target("repo/one", "mirror-a"),
            "the same target must not warn twice"
        );
        assert!(
            state.observe_dropped_target("repo/one", "mirror-b"),
            "a second mirror going down is a new transition"
        );
        assert!(
            state.observe_dropped_target("repo/two", "mirror-a"),
            "the same mirror under another mapping is its own transition"
        );

        // Partial recovery: mirror-a is back, mirror-b is still down. The
        // all-or-nothing clear this replaced left mirror-a suppressed for as
        // long as any sibling stayed down.
        let still_down: HashSet<&str> = ["mirror-b"].into_iter().collect();
        state.forget_recovered_targets("repo/one", &still_down);
        assert!(
            state.observe_dropped_target("repo/one", "mirror-a"),
            "a recovered mirror must warn again when it fails again"
        );
        assert!(
            !state.observe_dropped_target("repo/one", "mirror-b"),
            "the mirror that never recovered must stay quiet"
        );
        assert!(
            !state.observe_dropped_target("repo/two", "mirror-a"),
            "another mapping's state is untouched"
        );
    }

    /// A mapping dropped from config must not leave state behind, or
    /// re-adding it later silently suppresses its next outage.
    #[test]
    fn retain_active_prunes_dropped_targets_and_unresolved() {
        let mut state = WatchLogState::default();
        state.observe_dropped_target("repo/gone", "mirror-a");
        state.observe_dropped_target("repo/stays", "mirror-a");
        state.observe_mapping_unresolved("repo/gone");
        state.observe_mapping_unresolved("repo/stays");

        state.retain_active(["repo/stays"]);

        assert!(
            state.observe_dropped_target("repo/gone", "mirror-a"),
            "the removed mapping's target key must be gone"
        );
        assert!(
            !state.observe_dropped_target("repo/stays", "mirror-a"),
            "the surviving mapping keeps its state"
        );
        assert!(state.observe_mapping_unresolved("repo/gone"));
        assert!(!state.observe_mapping_unresolved("repo/stays"));
    }

    /// A mapping that stops resolving must not keep its last outcome, or the
    /// recovery cycle reports nothing when the counts come back identical.
    #[test]
    fn becoming_unresolved_clears_the_last_outcome() {
        let mut state = WatchLogState::default();
        let outcome = MappingOutcome {
            synced: 0,
            skipped: 5,
            failed: 0,
            bytes: 0,
        };
        assert!(
            state
                .observe_mapping_outcome("repo/one", &outcome)
                .is_some()
        );
        // Same counts again: suppressed, as designed.
        assert!(
            state
                .observe_mapping_outcome("repo/one", &outcome)
                .is_none()
        );

        state.observe_mapping_unresolved("repo/one");
        state.observe_mapping_resolved("repo/one");

        assert!(
            state
                .observe_mapping_outcome("repo/one", &outcome)
                .is_some(),
            "recovery must report even when the counts are unchanged"
        );
    }

    /// A mapping naming no targets at all keeps its old zero-target shape
    /// rather than becoming an error, which is the case the engine documents
    /// as degenerate but handles.
    #[tokio::test]
    async fn a_mapping_with_no_named_targets_still_resolves() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
defaults:
  source: src
  targets: []
mappings:
  - from: repo/one
    tags:
      glob: ["v1"]
"#,
        )
        .expect("config yaml parses");

        let clients =
            ClientMap::from([("src".to_string(), Ok(test_client("https://source.test")))]);
        let Resolution {
            resolved,
            unresolved,
            dropped_targets: dropped,
            ..
        } = resolve(&config, &clients).await;

        assert_eq!(resolved.len(), 1, "no targets is not a resolution failure");
        assert!(resolved[0].targets.is_empty());
        assert!(unresolved.is_empty());
        assert!(
            dropped.is_empty(),
            "nothing was dropped; the config named nothing"
        );
    }

    // -----------------------------------------------------------------
    // Exit codes
    // -----------------------------------------------------------------

    fn report_from(statuses: Vec<ocync_sync::ImageStatus>) -> SyncReport {
        report_with(
            statuses
                .into_iter()
                .map(|status| img("src/repo:v1", "dst/repo:v1", status, 0))
                .collect(),
        )
    }

    fn unresolved_mapping(from: &str, auth: bool) -> UnresolvedMapping {
        UnresolvedMapping {
            from: from.into(),
            error: "boom".into(),
            code: if auth {
                ExitCode::AuthError
            } else {
                ExitCode::Failure
            },
        }
    }

    /// An unresolved mapping produces no `ImageResult`, so the report alone
    /// would score the run a clean success. It must not.
    #[test]
    fn combined_exit_code_downgrades_a_clean_report_with_unresolved_mappings() {
        let report = report_from(vec![ocync_sync::ImageStatus::Synced]);
        assert_eq!(combined_exit_code(&report, &[], &[], 0), ExitCode::Success);
        assert_eq!(
            combined_exit_code(&report, &[unresolved_mapping("repo/two", false)], &[], 0),
            ExitCode::PartialFailure
        );
    }

    /// Nothing synced and mappings unresolved is a total failure, not partial.
    #[test]
    fn combined_exit_code_is_failure_when_nothing_succeeded() {
        let empty = report_with(Vec::new());
        let failures = [
            unresolved_mapping("repo/one", false),
            unresolved_mapping("repo/two", true),
        ];
        assert_eq!(
            combined_exit_code(&empty, &failures, &[], 0),
            ExitCode::Failure
        );
    }

    /// A skipped image still counts as the run having done its job, so an
    /// unresolved mapping alongside it is a partial failure.
    #[test]
    fn combined_exit_code_treats_skipped_images_as_success() {
        let report = report_from(vec![ocync_sync::ImageStatus::Skipped {
            reason: ocync_sync::SkipReason::DigestMatch,
        }]);
        assert_eq!(
            combined_exit_code(&report, &[unresolved_mapping("repo/two", false)], &[], 0),
            ExitCode::PartialFailure
        );
    }

    /// Isolating failures must not silently retire the auth exit code: a run
    /// denied everywhere still reports 4, not the generic 2.
    #[test]
    fn combined_exit_code_keeps_the_auth_code_when_every_mapping_was_denied() {
        let empty = report_with(Vec::new());
        let denied = [
            unresolved_mapping("repo/one", true),
            unresolved_mapping("repo/two", true),
        ];
        assert_eq!(
            combined_exit_code(&empty, &denied, &[], 0),
            ExitCode::AuthError
        );
    }

    /// One non-auth failure in the set is enough to make it a generic failure.
    #[test]
    fn combined_exit_code_does_not_claim_auth_for_a_mixed_failure_set() {
        let empty = report_with(Vec::new());
        let mixed = [
            unresolved_mapping("repo/one", true),
            unresolved_mapping("repo/two", false),
        ];
        assert_eq!(
            combined_exit_code(&empty, &mixed, &[], 0),
            ExitCode::Failure
        );
    }

    #[test]
    fn dry_run_exit_code_maps_resolution_counts() {
        assert_eq!(dry_run_exit_code(3, &[], &[]), ExitCode::Success);
        assert_eq!(
            dry_run_exit_code(2, &[unresolved_mapping("repo/two", false)], &[]),
            ExitCode::PartialFailure
        );
        assert_eq!(
            dry_run_exit_code(0, &[unresolved_mapping("repo/two", false)], &[]),
            ExitCode::Failure
        );
    }

    /// `--dry-run` returns before the engine, so it never sees a `SyncReport`
    /// and has to apply the auth rule itself. Missing that here silently
    /// downgraded a denied dry run from 4 to 2.
    #[test]
    fn dry_run_exit_code_keeps_the_auth_code_when_every_mapping_was_denied() {
        let denied = [
            unresolved_mapping("repo/one", true),
            unresolved_mapping("repo/two", true),
        ];
        assert_eq!(dry_run_exit_code(0, &denied, &[]), ExitCode::AuthError);
        // Something resolved, so the run was only partly denied.
        assert_eq!(dry_run_exit_code(1, &denied, &[]), ExitCode::PartialFailure);
    }

    // -----------------------------------------------------------------
    // Reporting surfaces
    // -----------------------------------------------------------------

    #[test]
    fn cycle_tail_names_unresolved_mappings() {
        let report = report_with(Vec::new());
        let line = format_cycle_tail(4, 2, 0, &report);
        assert!(line.contains("2 unresolved"), "{line}");
    }

    #[test]
    fn cycle_tail_omits_the_clause_when_everything_resolved() {
        let report = report_with(Vec::new());
        let line = format_cycle_tail(4, 0, 0, &report);
        assert!(!line.contains("unresolved"), "{line}");
    }

    /// `--json` consumers must be able to see mappings that never reached the
    /// engine; otherwise the isolation change hides failures from automation.
    #[test]
    fn json_output_carries_unresolved_mappings() {
        let report = report_with(Vec::new());
        let failures = vec![UnresolvedMapping {
            from: "repo/two".into(),
            error: "403 Forbidden".into(),
            code: ExitCode::AuthError,
        }];
        let value = serde_json::to_value(SyncOutput {
            report: &report,
            unresolved_mappings: &failures,
            dropped_targets: &[],
        })
        .expect("serializes");

        assert_eq!(value["unresolved_mappings"][0]["from"], "repo/two");
        assert_eq!(value["unresolved_mappings"][0]["error"], "403 Forbidden");
        // The classification drives the exit code only; not part of the document.
        assert!(
            value["unresolved_mappings"][0].get("auth").is_none(),
            "{value}"
        );
        // Flattened, not nested: the report's own keys stay at the top level.
        assert!(value.get("stats").is_some(), "{value}");
    }

    /// The key is absent on a clean run so the document shape is unchanged.
    #[test]
    fn json_output_omits_unresolved_mappings_when_empty() {
        let report = report_with(Vec::new());
        let value = serde_json::to_value(SyncOutput {
            report: &report,
            unresolved_mappings: &[],
            dropped_targets: &[],
        })
        .expect("serializes");

        assert!(value.get("unresolved_mappings").is_none(), "{value}");
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
            select_filtered_tags(Some(&tags_config), None, all_tags, true).unwrap();
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
        crate::cli::commands::dry_run::write_to(&mut buf, &[mapping], &[], &[], false).unwrap();
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
