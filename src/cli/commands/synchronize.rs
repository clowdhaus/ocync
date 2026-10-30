//! The `sync` subcommand - runs all mappings from config.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use ocync_distribution::AwsCredentialProviders;
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
    CacheTtl, Config, GlobOrList, MappingConfig, RegistryConfig, StagingLimit, TagsConfig,
    load_config, resolve_target_names,
};
use crate::cli::output::{format_bytes, format_duration};
use crate::cli::{
    CliError, ExitCode, Registry, RunCredentials, bare_hostname, build_registry_client,
    is_ecr_registry,
};

/// Default cache TTL: 12 hours.
pub(crate) const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(12 * 3600);

const CACHE_FILE_NAME: &str = "transfer_state.bin";

/// Sample cap for the source-tag list in the no-tags-matched WARN. Matches
/// `dry_run::SAMPLE_CAP` so both surfaces show the same depth.
const NO_TAGS_SAMPLE_CAP: usize = 5;

/// Cadence of the progress line emitted while the run is preparing.
///
/// Everything before the engine starts is network-bound (token mints, tag
/// listings) and has no per-image output, so without this a large config runs
/// for minutes with nothing in the log.
const PREPARE_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// Mappings whose resolution may be in flight at once.
///
/// Running them one at a time makes the prepare phase cost the sum of every
/// registry's latency rather than its maximum. This caps open mappings, not
/// load on any registry: every request underneath still passes that registry's
/// AIMD window, so a wide config cannot burst against a single host.
pub(crate) const PREPARE_MAPPING_CONCURRENCY: usize = 16;

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
    /// Reading image manifests, one per resolved tag. `analyze` only.
    Images,
}

impl fmt::Display for PreparePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registries => f.write_str("registries"),
            Self::BatchCheckers => f.write_str("batch checkers"),
            Self::Mappings => f.write_str("mappings"),
            Self::Images => f.write_str("images"),
        }
    }
}

/// Counts for the pre-engine phase, held in a [`Cell`].
#[derive(Debug, Clone, Copy, Default)]
struct PrepareCounts {
    phase: PreparePhase,
    done: usize,
    total: usize,
}

/// An item the current step has started and not yet finished.
#[derive(Debug)]
struct InFlightItem {
    id: u64,
    name: String,
    /// `tokio`'s clock, not `std`'s, so a paused runtime still ages items.
    started: tokio::time::Instant,
}

/// What the progress ticker reports for one tick.
#[derive(Debug, Clone, Default)]
pub(crate) struct PrepareProgress {
    /// Which pre-engine step is running.
    pub(crate) phase: PreparePhase,
    /// Items finished in this step.
    pub(crate) done: usize,
    /// Items this step will process.
    pub(crate) total: usize,
    /// Items started and not yet finished.
    pub(crate) in_flight: usize,
    /// The in-flight item running longest, and for how long.
    ///
    /// Names the item holding the step up, so a step stuck at one `done` count
    /// can be told apart as a huge tag listing or a wedged registry.
    pub(crate) slowest: Option<(String, Duration)>,
}

/// Counters and in-flight set the pre-engine steps update and the progress
/// ticker reads.
///
/// The runtime is `current_thread`, so the ticker and the work it describes
/// never run concurrently; no borrow here is held across an await.
#[derive(Debug, Default)]
pub(crate) struct PrepareTracker {
    counts: Cell<PrepareCounts>,
    in_flight: RefCell<Vec<InFlightItem>>,
    next_id: Cell<u64>,
}

impl PrepareTracker {
    /// Start a new step, resetting the counter and in-flight set.
    pub(crate) fn begin(&self, phase: PreparePhase, total: usize) {
        self.counts.set(PrepareCounts {
            phase,
            done: 0,
            total,
        });
        self.in_flight.borrow_mut().clear();
    }

    /// Mark `name` in flight until the returned guard drops.
    ///
    /// A guard rather than paired calls: every early exit must both retire the
    /// item and advance the counter, or progress stalls short of its total.
    pub(crate) fn track(&self, name: &str) -> InFlightGuard<'_> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.in_flight.borrow_mut().push(InFlightItem {
            id,
            name: name.to_owned(),
            started: tokio::time::Instant::now(),
        });
        InFlightGuard { tracker: self, id }
    }

    /// The current step, its counts, and what is still running.
    pub(crate) fn snapshot(&self) -> PrepareProgress {
        let counts = self.counts.get();
        let in_flight = self.in_flight.borrow();
        let now = tokio::time::Instant::now();
        let slowest = in_flight
            .iter()
            // Earliest start wins: names the item holding the step up.
            .min_by_key(|item| item.started)
            .map(|item| {
                (
                    item.name.clone(),
                    now.saturating_duration_since(item.started),
                )
            });
        PrepareProgress {
            phase: counts.phase,
            done: counts.done,
            total: counts.total,
            in_flight: in_flight.len(),
            slowest,
        }
    }

    /// Retire an in-flight item and count it finished.
    fn finish(&self, id: u64) {
        self.in_flight.borrow_mut().retain(|item| item.id != id);
        let mut counts = self.counts.get();
        counts.done += 1;
        self.counts.set(counts);
    }
}

/// Marks one item in flight while held.
#[derive(Debug)]
pub(crate) struct InFlightGuard<'a> {
    tracker: &'a PrepareTracker,
    id: u64,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.tracker.finish(self.id);
    }
}

/// Drive `work` to completion, logging its progress every
/// [`PREPARE_PROGRESS_INTERVAL`].
///
/// The ticker fires on wall-clock, not on item boundaries, so a single slow
/// mapping still reports.
pub(crate) async fn with_prepare_progress<F: Future>(
    tracker: &PrepareTracker,
    what: &'static str,
    work: F,
) -> F::Output {
    tick_while(tracker, PREPARE_PROGRESS_INTERVAL, work, |p, elapsed| {
        // Between steps nothing is in flight; `in_flight=0` explains the `-`.
        let (slowest, slowest_secs) = p
            .slowest
            .as_ref()
            .map(|(name, age)| (name.as_str(), age.as_secs()))
            .unwrap_or(("-", 0));
        tracing::info!(
            phase = %p.phase,
            done = p.done,
            total = p.total,
            in_flight = p.in_flight,
            slowest = %slowest,
            slowest_secs = slowest_secs,
            elapsed_secs = elapsed.as_secs(),
            "preparing {what}"
        );
    })
    .await
}

/// Poll `work` to completion, calling `emit` with the tracker's state once per
/// `interval`. Split out so tests can use a recording sink instead of tracing.
async fn tick_while<F: Future>(
    tracker: &PrepareTracker,
    interval: Duration,
    work: F,
    mut emit: impl FnMut(PrepareProgress, Duration),
) -> F::Output {
    let mut work = std::pin::pin!(work);
    // tokio's clock: `std::time::Instant` does not advance under a paused runtime.
    let started = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` yields immediately; delay the first tick a full period.
    ticker.reset();
    loop {
        tokio::select! {
            // `work` first: a tick ready on the same poll as a finished future is dropped.
            biased;
            out = &mut work => return out,
            _ = ticker.tick() => emit(tracker.snapshot(), started.elapsed()),
        }
    }
}

/// Registry clients keyed by config alias.
///
/// A registry whose client could not be built is stored as its failure, so the
/// error surfaces only on the mappings that reference it.
pub(crate) type ClientMap = HashMap<String, Result<Arc<RegistryClient>, ClientInitError>>;

/// Why a registry's client could not be built.
///
/// Holds the error, not its text: the classification cannot be recovered from a
/// string, and losing it downgrades a wholly denied run from the auth exit code
/// to the generic failure code.
#[derive(Debug)]
pub(crate) struct ClientInitError(CliError);

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

/// Outcome of resolving a single mapping: ready for the engine, or no source
/// tag survived filtering and the caller decides whether to log a WARN (sync
/// mode: always; watch mode: only on transition).
///
/// The variant size disparity is intentional: `ResolvedMapping` flows straight
/// into `Vec<ResolvedMapping>`, so boxing it would add an allocation per success.
#[allow(clippy::large_enum_variant)]
pub(crate) enum MappingResolution {
    /// Ready for the engine.
    Resolved(ResolvedMapping),
    /// No source tag survived filtering.
    NoMatchingTags(NoTagsInfo),
}

/// A target a mapping could not use.
///
/// The mapping still syncs to its other targets, so this is not an
/// [`UnresolvedMapping`]; it is a hole in an otherwise successful run.
#[derive(Debug, Serialize)]
pub(crate) struct DroppedTarget {
    /// Source repository of the mapping that lost the target.
    pub from: String,
    /// Target registry alias that was dropped.
    pub registry: String,
    /// Why the target was unusable.
    pub error: String,
}

/// A mapping that never became transferable work. Resolution failures are
/// per-mapping: one must not stop the mappings behind it.
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
    /// it the `min_tags` tripwire.
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
        debug_assert!(false, "shared_failure_code called with no failures");
        return ExitCode::Failure;
    };
    if matches!(first, ExitCode::AuthError | ExitCode::ConfigError) && codes.all(|c| c == first) {
        first
    } else {
        ExitCode::Failure
    }
}

/// Diagnostic context for a mapping whose filter rejected every source tag:
/// source size and composition (image tags vs OCI 1.1 referrer fallbacks), the
/// active filter, and example tag names, in one log line.
pub(crate) struct NoTagsInfo {
    /// Source repository of the mapping.
    pub from: String,
    /// Source tags that are images.
    pub image_count: usize,
    /// Source tags that are referrer fallbacks.
    pub artifact_count: usize,
    /// Active filter clauses (e.g. `semver >=1.0.0, latest=5`). `None` only
    /// when no filter is configured.
    pub filter_desc: Option<String>,
    /// Up to [`NO_TAGS_SAMPLE_CAP`] image-tag names. Excludes referrer
    /// fallback tags, which would dominate the list on cosign-heavy repos
    /// (`sha256-<hex>(.sig|.sbom|.att)`).
    pub samples: Vec<String>,
}

impl NoTagsInfo {
    fn source_total(&self) -> usize {
        self.image_count + self.artifact_count
    }

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

/// How a mapping's outcome compares with the prior cycle's.
#[derive(Debug, PartialEq, Eq)]
enum OutcomeChange {
    /// Identical to the prior cycle: suppress the line.
    Unchanged,
    /// New or different: emit the line.
    Changed,
    /// `failed` went from nonzero to zero: emit with `[recovered]`.
    Recovered,
}

/// Cross-cycle state that lets watch mode log on transitions instead of every
/// cycle. Sync mode passes `None`; `watch::run` owns the state across loop
/// iterations.
///
/// The emit counter is bumped by every `observe_*` method that reports a
/// transition; the watch loop reads it to gate the idle heartbeat.
#[derive(Debug, Default)]
pub(crate) struct WatchLogState {
    warned_no_tags: HashSet<String>,
    /// Registries currently reported down, per mapping.
    dropped_targets: HashMap<String, HashSet<String>>,
    /// Mappings currently reported as unresolvable.
    unresolved: HashSet<String>,
    last_outcomes: HashMap<String, MappingOutcome>,
    cycle_emit_count: u32,
}

impl WatchLogState {
    /// Reset the per-cycle emit counter.
    pub(crate) fn begin_cycle(&mut self) {
        self.cycle_emit_count = 0;
    }

    /// Transitions reported since [`begin_cycle`](Self::begin_cycle).
    pub(crate) fn cycle_emit_count(&self) -> u32 {
        self.cycle_emit_count
    }

    /// Returns `true` on transition into the no-match state (caller emits a
    /// WARN).
    fn observe_no_match(&mut self, from: &str) -> bool {
        let changed = self.warned_no_tags.insert(from.to_string());
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Returns `true` on transition into the dropped state (caller emits a
    /// WARN).
    ///
    /// Keyed on mapping plus registry so two mappings losing the same mirror
    /// each report once, and a second mirror going down still reports.
    fn observe_dropped_target(&mut self, from: &str, registry: &str) -> bool {
        let down = match self.dropped_targets.get_mut(from) {
            Some(down) => down,
            None => self.dropped_targets.entry(from.to_owned()).or_default(),
        };
        let changed = down.insert(registry.to_owned());
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Forget any target of `from` absent from `still_down`, so a mirror that
    /// recovered warns again if it fails later.
    ///
    /// Reconciling rather than clearing only when nothing is down: otherwise a
    /// recovered mirror stays suppressed while any sibling is down.
    fn forget_recovered_targets(&mut self, from: &str, still_down: &[DroppedTarget]) {
        if let Some(down) = self.dropped_targets.get_mut(from) {
            down.retain(|registry| still_down.iter().any(|t| t.registry == *registry));
        }
    }

    /// Returns `true` on transition into the unresolved state (caller emits
    /// the ERROR).
    fn observe_mapping_unresolved(&mut self, from: &str) -> bool {
        // A stale outcome would suppress the recovery line when the mapping
        // returns with identical counts.
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

    /// Returns `true` when the mapping was previously in the no-match set
    /// (caller emits a recovery INFO).
    fn observe_resolved(&mut self, from: &str) -> bool {
        let changed = self.warned_no_tags.remove(from);
        if changed {
            self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
        }
        changed
    }

    /// Record `outcome` as the latest result for `from`.
    fn observe_mapping_outcome(&mut self, from: &str, outcome: &MappingOutcome) -> OutcomeChange {
        use std::collections::hash_map::Entry;
        match self.last_outcomes.entry(from.to_string()) {
            Entry::Occupied(mut slot) => {
                let prev = *slot.get();
                if &prev == outcome {
                    return OutcomeChange::Unchanged;
                }
                slot.insert(*outcome);
                self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
                if prev.failed > 0 && outcome.failed == 0 {
                    OutcomeChange::Recovered
                } else {
                    OutcomeChange::Changed
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(*outcome);
                self.cycle_emit_count = self.cycle_emit_count.saturating_add(1);
                OutcomeChange::Changed
            }
        }
    }

    /// Drop entries for mappings no longer in the active set, so the state does
    /// not grow across config edits.
    fn retain_active<'a>(&mut self, active: impl IntoIterator<Item = &'a str>) {
        let active_set: HashSet<&str> = active.into_iter().collect();
        self.warned_no_tags
            .retain(|k| active_set.contains(k.as_str()));
        self.last_outcomes
            .retain(|k, _| active_set.contains(k.as_str()));
        self.unresolved.retain(|k| active_set.contains(k.as_str()));
        self.dropped_targets
            .retain(|k, _| active_set.contains(k.as_str()));
    }
}

/// Resolve the cache directory and file path: `global.cache_dir`, else
/// `.ocync/cache/` beside the config file.
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

/// The cache TTL from config, defaulting to 12 hours.
pub(crate) fn resolve_cache_ttl(config: &Config) -> Duration {
    config
        .global
        .as_ref()
        .and_then(|g| g.cache_ttl)
        .map_or(DEFAULT_CACHE_TTL, CacheTtl::get)
}

/// Whether this run's mappings can reuse blobs staged on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagingNeed {
    /// Several targets or images may share a blob.
    Needed,
    /// Every blob goes to one place, so staging would only add disk I/O.
    NotNeeded,
}

/// Disk staging rooted at `dir`, with stale blobs evicted down to `limit`.
/// A disabled limit turns staging off, as the config documents, after
/// removing what earlier runs staged there, whether or not this run needs
/// staging. Any other limit leaves the directory alone when it is not needed.
fn open_staging(dir: PathBuf, limit: Option<StagingLimit>, need: StagingNeed) -> BlobStage {
    if need == StagingNeed::NotNeeded && limit != Some(StagingLimit::Disabled) {
        return BlobStage::disabled();
    }
    let stage = BlobStage::new(dir);
    if let Err(e) = stage.cleanup_tmp_files() {
        tracing::warn!(error = %e, "failed to clean staging tmp files");
    }
    let evict = |keep_bytes: u64| {
        if let Err(e) = stage.evict(keep_bytes) {
            tracing::warn!(error = %e, "failed to evict staged blobs");
        }
    };
    match limit {
        None => stage,
        Some(StagingLimit::Bytes(bytes)) => {
            evict(bytes.get());
            stage
        }
        Some(StagingLimit::Disabled) => {
            evict(0);
            BlobStage::disabled()
        }
    }
}

/// Run the sync command: load config, resolve mappings, and execute.
///
/// The `shutdown` signal is forwarded to the engine for graceful drain.
pub(crate) async fn run(
    args: &SyncArgs,
    progress: &dyn ocync_sync::progress::ProgressReporter,
    shutdown: Option<&ShutdownSignal>,
    external_cache: Option<Rc<RefCell<TransferStateCache>>>,
    verbose: bool,
    mut watch_log: Option<&mut WatchLogState>,
) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;

    // One ticker spans client construction, batch-checker setup and mapping
    // resolution, so the run is never silent for longer than the interval.
    let tracker = PrepareTracker::default();
    let resolution = with_prepare_progress(&tracker, "sync", async {
        let referenced = referenced_registries(&config);
        // One run's clients share credentials; the next watch cycle reloads them.
        let credentials = RunCredentials::default();
        let clients = build_clients(&config, &referenced, &tracker, &credentials).await;
        let batch_checkers =
            build_batch_checkers(&config, &clients, &referenced, &tracker, &credentials.aws);
        resolve_all(
            &config,
            &clients,
            &batch_checkers,
            if args.dry_run {
                Selection::DryRun
            } else {
                Selection::Sync
            },
            watch_log.as_deref_mut(),
            &tracker,
            PREPARE_MAPPING_CONCURRENCY,
        )
        .await
    })
    .await?;
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

    // Stage to disk when several targets or several images may share blobs
    // (pull once, push from disk). Conservative: disjoint mappings pay a local
    // disk round-trip per blob for nothing, since tighter detection needs
    // manifest data that is unavailable before discovery.
    let need = if mappings.iter().any(|m| m.targets.len() > 1) || mappings.len() > 1 {
        StagingNeed::Needed
    } else {
        StagingNeed::NotNeeded
    };
    let staging = open_staging(
        cache_dir.join("blobs"),
        config.global.as_ref().and_then(|g| g.staging_size_limit),
        need,
    );

    let max_concurrent = config
        .global
        .as_ref()
        .map_or(DEFAULT_MAX_CONCURRENT_TRANSFERS, |g| {
            g.max_concurrent_transfers
        });
    // Captured before the engine consumes `mappings`.
    let descriptors: Vec<MappingDescriptor> = mappings
        .iter()
        .map(|m| {
            let from = m.source_repo.as_str().to_string();
            MappingDescriptor {
                target_repo: m.target_repo.as_str().to_string(),
                target_names: m.targets.iter().map(|t| (*t.name).to_string()).collect(),
                // From the dropped list, since `m.targets` is already filtered;
                // otherwise the bracket vanishes when a registry is missing.
                dropped_names: dropped_targets
                    .iter()
                    .filter(|d| d.from == from)
                    .map(|d| d.registry.clone())
                    .collect(),
                from,
            }
        })
        .collect();

    // Anything absent from the mappings the engine sees looks deleted to the
    // prune, including a mapping whose tag list came back empty (a transient
    // short response looks the same).
    let complete = unresolved.is_empty() && dropped_targets.is_empty() && no_match == 0;
    let engine =
        SyncEngine::new(RetryConfig::default(), max_concurrent).with_cache_pruning(complete);
    let report = engine
        .run(mappings, cache.clone(), staging, progress, shutdown)
        .await;

    // Persist only when we own the cache (sync command); watch persists on shutdown.
    if should_persist {
        if let Err(e) = cache.borrow().persist(&cache_path) {
            tracing::error!(error = %e, "failed to persist transfer state cache");
        }
    }

    emit_mapping_outcomes(&descriptors, &report, watch_log.as_deref_mut());
    // Watch mode suppresses the tail on idle cycles; sync mode always emits it.
    // Unresolved or dropped targets force it: they produce no per-mapping line,
    // so a cycle where every mapping 403'd would otherwise go quiet.
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

/// Exit code for a dry run, which has no [`SyncReport`].
fn dry_run_exit_code(
    succeeded: usize,
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
    if succeeded > 0 {
        return ExitCode::PartialFailure;
    }
    total_failure_code(unresolved)
}

/// Exit code for a run where nothing succeeded.
///
/// Only unresolved mappings are consulted; a dropped target never reaches here
/// (the caller returns `PartialFailure` first). A run denied everywhere exits
/// with the auth code, so "fix the credentials" is distinguishable from the
/// generic failure.
fn total_failure_code(unresolved: &[UnresolvedMapping]) -> ExitCode {
    shared_failure_code(unresolved.iter().map(|u| u.code))
}

/// Fold mapping-level resolution failures into the run's exit code.
///
/// A mapping that never reached the engine produces no [`ocync_sync::ImageResult`],
/// so [`SyncReport::outcome`] cannot see it; counting each as a failure keeps a
/// run whose mappings all 403'd from exiting 0.
fn combined_exit_code(
    report: &SyncReport,
    unresolved: &[UnresolvedMapping],
    dropped: &[DroppedTarget],
    no_match: usize,
) -> ExitCode {
    if unresolved.is_empty() {
        // A dropped target means images never reached a configured registry.
        return match ExitCode::from_report(report.outcome()) {
            ExitCode::Success if !dropped.is_empty() => ExitCode::PartialFailure,
            other => other,
        };
    }
    // A mapping whose filter matched nothing is healthy with nothing to do, so
    // it counts as the run working, even though it contributes no image.
    if report.has_success() || no_match > 0 {
        return ExitCode::PartialFailure;
    }
    // Images that ran and failed make this a generic failure whatever the
    // unresolved mappings' cause.
    if !report.images.is_empty() {
        return ExitCode::Failure;
    }
    total_failure_code(unresolved)
}

/// Per-mapping metadata captured before the engine consumes `mappings`, joined
/// with the per-image report to emit one log line per mapping.
struct MappingDescriptor {
    from: String,
    target_repo: String,
    target_names: Vec<String>,
    /// Registries the config named that this mapping could not use.
    dropped_names: Vec<String>,
}

/// Per-mapping aggregated outcome derived from [`SyncReport::images`].
/// Used for log emission and watch-mode change detection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MappingOutcome {
    /// Images transferred.
    pub synced: u64,
    /// Images already present.
    pub skipped: u64,
    /// Images that failed.
    pub failed: u64,
    /// Bytes transferred by synced images.
    pub bytes: u64,
}

impl MappingOutcome {
    fn is_empty(&self) -> bool {
        self.synced == 0 && self.skipped == 0 && self.failed == 0
    }
}

/// Emit one INFO (WARN on failures) per mapping. In watch mode, mappings whose
/// outcome is unchanged from the prior cycle are suppressed.
fn emit_mapping_outcomes(
    descriptors: &[MappingDescriptor],
    report: &SyncReport,
    mut watch_log: Option<&mut WatchLogState>,
) {
    for d in descriptors {
        let outcome = aggregate_mapping_outcome(&d.from, &d.target_repo, report);
        // No images reached the engine; the no-tags WARN already covered it.
        if outcome.is_empty() {
            continue;
        }
        let recovered = match watch_log.as_deref_mut() {
            Some(state) => match state.observe_mapping_outcome(&d.from, &outcome) {
                OutcomeChange::Unchanged => continue,
                OutcomeChange::Changed => false,
                OutcomeChange::Recovered => true,
            },
            None => false,
        };
        let line = format_mapping_outcome(d, &outcome, recovered);
        // `from` / `to` are not structured fields: the message already names
        // them, and the text formatter would repeat them. The counts stay
        // because they carry zero values the terse message elides.
        // Two arms because `tracing::event!` needs a const level.
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
    // Shown whenever more than one registry was configured, including when
    // only one survived: that is when the operator needs to see which.
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

/// One-line cycle tail rolling up totals across all mappings. The caller gates
/// it in watch mode.
fn emit_cycle_tail(
    descriptors: &[MappingDescriptor],
    unresolved_count: usize,
    dropped_count: usize,
    report: &SyncReport,
) {
    let line = format_cycle_tail(descriptors.len(), unresolved_count, dropped_count, report);
    // Counts are already in the message; structured fields would repeat them.
    if report.stats.images_failed > 0 || unresolved_count > 0 || dropped_count > 0 {
        tracing::warn!("{line}");
    } else {
        tracing::info!("{line}");
    }
}

/// Render the cycle tail, split out so tests need no tracing subscriber.
fn format_cycle_tail(
    mapping_count: usize,
    unresolved_count: usize,
    dropped_count: usize,
    report: &SyncReport,
) -> String {
    let s = &report.stats;
    // Neither reached the engine, so they are absent from the counts above.
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

/// Build a `RegistryClient` for each registry at least one mapping references,
/// keyed by config alias.
///
/// Auth setup reaches the network, so any one registry can fail. The failure is
/// recorded against that alias instead of aborting: unrelated mappings still
/// sync, and the mappings that reference it fail with the original error text.
pub(crate) async fn build_clients(
    config: &Config,
    referenced: &Referenced,
    tracker: &PrepareTracker,
    credentials: &RunCredentials,
) -> ClientMap {
    tracker.begin(PreparePhase::Registries, referenced.len());
    let mut clients = ClientMap::with_capacity(referenced.len());
    for (name, reg) in &config.registries {
        if !referenced.contains(name) {
            continue;
        }
        // Token minting is a round trip; track it so a wedged provider is named.
        let _item = tracker.track(name);
        let entry = match build_registry_client(Registry::Configured(reg), credentials).await {
            Ok(client) => Ok(Arc::new(client)),
            Err(err) => {
                tracing::error!(
                    registry = %name,
                    error = %err,
                    "registry client setup failed; mappings using this registry will be skipped"
                );
                Err(ClientInitError(err))
            }
        };
        clients.insert(name.clone(), entry);
    }
    clients
}

/// Registry aliases at least one mapping actually references.
///
/// An unused registry would cost an auth round trip against the rate-limit
/// budget and, on failure, log an error nothing depends on. A target that does
/// not resolve is left out here; `resolve_mapping` reports it per mapping.
pub(crate) fn referenced_registries(config: &Config) -> Referenced {
    let known: HashSet<&str> = config.registries.keys().map(String::as_str).collect();
    let defaults = config.defaults.as_ref();
    let mut referenced = Referenced::default();
    for mapping in &config.mappings {
        if let Some(source) = mapping
            .source
            .as_deref()
            .or(defaults.and_then(|d| d.source.as_deref()))
            // Unknown aliases are reported per mapping, and counting them would
            // inflate the progress total past the clients built.
            .filter(|name| known.contains(name))
        {
            referenced.sources.insert(source.to_owned());
        }
        if let Ok(names) = mapping_target_names(mapping, config, &known) {
            referenced.targets.extend(names);
        }
    }
    referenced
}

/// Registry aliases the config uses, split by role. Both sets come from one
/// walk because expanding target groups is the costliest part of the prepare
/// phase and clients and batch checkers want different halves.
#[derive(Debug, Default)]
pub(crate) struct Referenced {
    /// Aliases named as a source.
    sources: HashSet<String>,
    /// Aliases named as a target, with groups expanded.
    targets: HashSet<String>,
}

impl Referenced {
    fn contains(&self, name: &str) -> bool {
        self.sources.contains(name) || self.targets.contains(name)
    }

    fn len(&self) -> usize {
        self.sources.union(&self.targets).count()
    }
}

/// Target registry aliases a mapping names, with groups expanded.
///
/// Shared with [`resolve_mapping`] so the sets cannot drift: a registry in one
/// and not the other would surface as a misleading "not found in clients".
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

/// Look up a usable client for `name`; a missing or failed registry becomes a
/// mapping-scoped error, not a run-wide one.
fn client_for(
    clients: &ClientMap,
    name: &str,
    mapping_from: &str,
    role: RegistryRole,
) -> Result<Arc<RegistryClient>, CliError> {
    match clients.get(name) {
        Some(Ok(client)) => Ok(Arc::clone(client)),
        Some(Err(ClientInitError(cause))) => {
            let msg = format!(
                "mapping '{mapping_from}': {role} registry '{name}' is unavailable: {cause}"
            );
            // Keeps the cause's classification so a credentials-only failure
            // still exits with the auth code.
            Err(match cause.exit_code() {
                ExitCode::AuthError => CliError::Auth(msg),
                _ => CliError::Input(msg),
            })
        }
        None => Err(CliError::Input(format!(
            "mapping '{mapping_from}': {role} registry '{name}' not found in clients"
        ))),
    }
}

/// Build a [`BatchChecker`] for every ECR registry (explicit `auth_type: ecr`
/// or hostname auto-detection) that a mapping names as a target and whose
/// client built.
fn build_batch_checkers(
    config: &Config,
    clients: &ClientMap,
    referenced: &Referenced,
    tracker: &PrepareTracker,
    aws_credentials: &AwsCredentialProviders,
) -> HashMap<String, Rc<dyn BatchBlobChecker>> {
    let mut checkers: HashMap<String, Rc<dyn BatchBlobChecker>> = HashMap::new();

    // The map is read by target alias, and a registry that already failed has
    // no use for a checker.
    let usable: Vec<(&String, &RegistryConfig)> = config
        .registries
        .iter()
        .filter(|(name, _)| {
            referenced.targets.contains(name.as_str()) && matches!(clients.get(*name), Some(Ok(_)))
        })
        .collect();
    tracker.begin(PreparePhase::BatchCheckers, usable.len());
    for (name, reg) in usable {
        // Held for the whole body so every `continue` still retires the item.
        let _item = tracker.track(name);
        let hostname = bare_hostname(&reg.url);
        if !is_ecr_registry(reg.auth_type.as_ref(), hostname) {
            continue;
        }

        // Losing the checker costs an optimization, not correctness
        // (`with_retry` still catches the `BLOB_UPLOAD_UNKNOWN` it would have
        // avoided), so a failure here degrades instead of aborting.
        match BatchChecker::from_hostname(hostname, reg.aws_profile.as_deref(), aws_credentials) {
            Ok(checker) => {
                checkers.insert(name.clone(), Rc::new(checker));
            }
            Err(e) => tracing::warn!(
                registry = %name,
                error = %e,
                "ECR batch checker unavailable; manifest commits fall back to retry-on-conflict"
            ),
        }
    }

    checkers
}

/// What resolving a whole config produced.
#[derive(Debug)]
pub(crate) struct Resolution {
    /// Mappings ready for the engine.
    pub resolved: Vec<ResolvedMapping>,
    /// Mappings that produced no work at all.
    pub unresolved: Vec<UnresolvedMapping>,
    /// Targets dropped from otherwise usable mappings.
    pub dropped_targets: Vec<DroppedTarget>,
    /// Mappings whose filter legitimately matched nothing.
    ///
    /// Healthy but imageless; uncounted, a half-fine, half-denied run looks
    /// wholly denied.
    pub no_match: usize,
}

/// Resolve every configured mapping, isolating per-mapping failures.
///
/// One unreachable registry, 403 or bad repository name must not cost the run
/// every other mapping. Each failure is logged and recorded as an
/// [`UnresolvedMapping`]; a config error is the exception and returns `Err`.
///
/// Up to `concurrency` mappings resolve at once, but the result is always in
/// config order: callers depend on that for deterministic logs, watch-mode
/// state, and which config error stops the run.
async fn resolve_all(
    config: &Config,
    clients: &ClientMap,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    selection: Selection,
    mut watch_log: Option<&mut WatchLogState>,
    tracker: &PrepareTracker,
    concurrency: usize,
) -> Result<Resolution, CliError> {
    tracker.begin(PreparePhase::Mappings, config.mappings.len());
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    let mut dropped_targets = Vec::new();
    let mut no_match = 0usize;

    // `buffer_unordered`, not `buffered`: an ordered queue frees a slot only
    // when the head yields, so one mapping listing tens of thousands of tags
    // pins its slot and every finished mapping behind it keeps its own.
    //
    // The bookkeeping below needs config order, so each outcome carries its
    // index and is re-sorted. That keeps `watch_log`, the counters and the
    // warning stream identical run to run.
    //
    // Mappings behind a config error may already have run and warned; the run
    // still stops at the first config error in config order.
    /// One mapping's resolution alongside the targets it lost on the way.
    type ResolutionAndDropped = (Result<MappingResolution, CliError>, Vec<DroppedTarget>);

    let mut indexed: Vec<(usize, ResolutionAndDropped)> =
        futures_util::stream::iter(config.mappings.iter().enumerate().map(|(idx, mapping)| {
            async move {
                // Counts the mapping finished however it ends.
                let _item = tracker.track(&mapping.from);
                (
                    idx,
                    resolve_mapping(mapping, config, clients, batch_checkers, selection).await,
                )
            }
        }))
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    indexed.sort_by_key(|(idx, _)| *idx);
    let outcomes: Vec<ResolutionAndDropped> =
        indexed.into_iter().map(|(_, outcome)| outcome).collect();

    for (mapping, (outcome, dropped)) in config.mappings.iter().zip(outcomes) {
        // Dropped targets come back whatever the outcome, so a mapping that
        // loses a mirror and then fails on its tag listing still reports it.
        // Reconciled first so a recovered mirror re-arms even if the mapping
        // then failed.
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
            // A broken config (bad glob, unparseable platform, unknown alias,
            // `min_tags` tripwire) means the operator asked for something the
            // run must not silently do less than. Isolation covers registries
            // that are down, not configs that are wrong.
            Err(err) if err.exit_code() == ExitCode::ConfigError => return Err(err),
            Err(err) => {
                // Gated like every watch surface: otherwise a permanently broken
                // mapping logs an ERROR every cycle while `cycle_emit_count`
                // stays zero and watch prints "no state changes" beside it.
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
    }

    Ok(Resolution {
        resolved,
        unresolved,
        dropped_targets,
        no_match,
    })
}

/// WARN once per target that goes down, not once per watch cycle.
///
/// Reconciles rather than accumulates: a mirror that came back is forgotten
/// even while a sibling stays down, so it warns again if it fails again.
/// Accumulating would suppress that until every target was healthy at once,
/// which a flapping fleet may never reach.
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
    state.forget_recovered_targets(from, dropped);
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

/// Report a mapping that could not be turned into work. Shared with `analyze`
/// so both say it the same way.
pub(crate) fn log_unresolved_mapping(from: &str, err: &CliError) {
    tracing::error!(
        from = %from,
        error = %err,
        "mapping could not be resolved; continuing with the remaining mappings"
    );
}

/// Build a mapping's target entries, keeping the ones whose registry works.
///
/// One unusable target must not take its siblings with it.
///
/// Returns the usable entries and the error to surface when nothing is left;
/// dropped targets are pushed to `dropped`. Reporting is the caller's job: only
/// it knows whether any target survived, and `sync` and `analyze` word it
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
    let mut codes = Vec::new();
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
                // Folded by the run-level rule, so the result does not depend
                // on config target order.
                codes.push(err.exit_code());
                if worst_err.is_none() {
                    worst_err = Some(err);
                }
            }
        }
    }
    // The surfaced error carries the folded code, not the first one seen.
    let worst_err = worst_err.map(|err| {
        let folded = shared_failure_code(codes);
        if folded == err.exit_code() {
            err
        } else {
            CliError::Input(err.to_string())
        }
    });
    (targets, worst_err)
}

/// Resolve a single mapping config, falling back to `defaults.source`,
/// `defaults.targets` and `defaults.tags`.
///
/// Returns the targets dropped along the way even when resolution fails.
pub(crate) async fn resolve_mapping(
    mapping: &MappingConfig,
    config: &Config,
    clients: &ClientMap,
    batch_checkers: &HashMap<String, Rc<dyn BatchBlobChecker>>,
    selection: Selection,
) -> (Result<MappingResolution, CliError>, Vec<DroppedTarget>) {
    // Returned unconditionally: a `?` in the rest of resolution would discard
    // them, and a mapping that loses a mirror and then hits a rate limit would
    // report the outage nowhere.
    let mut dropped = Vec::new();
    let result = resolve_mapping_inner(
        mapping,
        config,
        clients,
        batch_checkers,
        selection,
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
    selection: Selection,
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
    // Every named target failed: surface the cause. A config that names no
    // targets at all is left to the engine.
    if targets.is_empty()
        && let Some(err) = last_target_err
    {
        return Err(err);
    }
    if targets.is_empty() {
        // Reachable via `targets: []`; still costs a source manifest pull per
        // tag, so say so rather than letting it look like work.
        tracing::warn!(
            from = %mapping.from,
            "mapping names no target registries; nothing will be pushed"
        );
    }

    // --- Fetch and filter tags ---
    let source_repo_path = RepositoryName::new(&mapping.from)?;

    let mapping_tags = mapping.tags.as_ref();
    let defaults_tags = config.defaults.as_ref().and_then(|d| d.tags.as_ref());

    // Fast path: exact tag names only (no wildcards, semver, latest, exclude)
    // skip listing the source. Gated on no `defaults.tags` block, or inherited
    // filters (notably `defaults.exclude`) would be skipped silently.
    // The image/artifact partition runs in the same pass that feeds
    // `select_filtered_tags`, so the filter and the no-match WARN see the same
    // counts.
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
            select_filtered_tags(mapping_tags, defaults_tags, all_tags, selection)?;
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

        // Listings overlap so prepare cost does not scale with fan-out.
        // Unbounded across targets on purpose: each is a distinct registry with
        // its own AIMD window. Clients are cloned out so the listings do not
        // borrow `targets`, which is mutated below.
        let target_clients: Vec<Arc<RegistryClient>> =
            targets.iter().map(|e| Arc::clone(&e.client)).collect();
        let listings: Vec<Result<Vec<String>, ocync_distribution::Error>> =
            futures_util::stream::iter(
                target_clients
                    .iter()
                    .map(|client| client.list_tags(&target_repo_path)),
            )
            .buffered(target_clients.len().max(1))
            .collect()
            .await;

        // Target order, so warnings do not depend on which mirror answered first.
        for (entry, listing) in targets.iter_mut().zip(listings) {
            match listing {
                Ok(tags) => entry.existing_tags = tags.into_iter().collect(),
                Err(e) => {
                    tracing::warn!(
                        // Mappings resolve concurrently, so neighbouring log
                        // lines do not identify the mapping.
                        from = %mapping.from,
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

/// Emit a tracing WARN for a [`NoTagsInfo`]: the rendered message plus
/// structured fields for log aggregators.
fn emit_no_tags_warn(info: &NoTagsInfo) {
    // `from` is not a field: the message already leads with it.
    tracing::warn!(
        source_total = info.source_total(),
        image_count = info.image_count,
        artifact_count = info.artifact_count,
        filter = info.filter_desc.as_deref().unwrap_or(""),
        "{info}"
    );
}

/// Build a `FilterConfig` from a mapping `TagsConfig` plus an optional
/// `defaults.tags` block. Field-level merge: a field set on the mapping wins.
/// The exclude lists stay separate: mapping exclude is the hard tier (blocks
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

/// Resolve `immutable_tags`, mapping over defaults. Separate from
/// [`build_filter`] because the skip optimization consumes it, not the filter.
fn resolve_immutable_pattern<'a>(
    mapping: Option<&'a TagsConfig>,
    defaults: Option<&'a TagsConfig>,
) -> Option<&'a str> {
    mapping
        .and_then(|t| t.immutable_tags.as_deref())
        .or_else(|| defaults.and_then(|t| t.immutable_tags.as_deref()))
}

fn glob_or_list_to_vec_owned(g: &GlobOrList) -> Vec<String> {
    match g {
        GlobOrList::Single(s) => vec![s.clone()],
        GlobOrList::List(v) => v.clone(),
    }
}

/// Result of [`select_filtered_tags`]: kept tags, candidate count, and the
/// optional `--dry-run` filter trace.
type SelectionResult = (
    Vec<String>,
    Option<usize>,
    Option<ocync_sync::filter::FilterReport>,
);

/// How tag selection treats `min_tags`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    /// A real sync: [`FilterConfig::apply`] returns `BelowMinTags` when
    /// `min_tags` is violated, and no filter trace is built.
    Sync,
    /// `--dry-run`: [`FilterConfig::apply_with_report`] does NOT enforce
    /// `min_tags`; the report carries the configured value so the gap can be
    /// rendered.
    DryRun,
}

/// Apply the filter pipeline to `all_tags`.
fn select_filtered_tags(
    mapping_tags: Option<&TagsConfig>,
    defaults_tags: Option<&TagsConfig>,
    all_tags: Vec<String>,
    selection: Selection,
) -> Result<SelectionResult, CliError> {
    let n_candidates = all_tags.len();
    let filter = build_filter(mapping_tags, defaults_tags);
    let tag_refs: Vec<&str> = all_tags.iter().map(String::as_str).collect();
    match selection {
        Selection::DryRun => {
            let result = filter.apply_with_report(&tag_refs)?;
            Ok((result.kept, Some(n_candidates), Some(result.report)))
        }
        Selection::Sync => Ok((filter.apply(&tag_refs)?, Some(n_candidates), None)),
    }
}

/// One-line filter summary for logs, e.g. `semver >=1.0.0, latest=5`; `None`
/// when no filter applies. Delegates to [`FilterConfig::describe`] after the
/// same merge the engine uses, so dry-run labels and the no-tags WARN agree.
fn describe_filter(
    mapping_tags: Option<&TagsConfig>,
    defaults_tags: Option<&TagsConfig>,
) -> Option<String> {
    build_filter(mapping_tags, defaults_tags).describe()
}

/// The `--json` document: the engine's report plus the mappings that never
/// reached it.
///
/// Without the extra keys a consumer reading only the report would see a clean
/// run when mappings were dropped before the engine started.
#[derive(Serialize)]
struct SyncOutput<'a> {
    #[serde(flatten)]
    report: &'a SyncReport,
    /// Mappings dropped during resolution.
    ///
    /// Always present, empty on a clean run, matching `analyze`.
    unresolved_mappings: &'a [UnresolvedMapping],
    /// Targets a resolved mapping could not use.
    dropped_targets: &'a [DroppedTarget],
}

/// Print the `--json` document when `json` is set.
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

    /// A filter's semver range as written, for comparing against a literal.
    fn semver_text(filter: &FilterConfig) -> Option<String> {
        filter.semver.as_ref().map(ToString::to_string)
    }

    /// A parsed version range, for filters built in the tests below.
    fn range(text: &str) -> ocync_sync::Range {
        ocync_sync::Range::parse(text).unwrap()
    }

    /// A staging size limit of 0 disables staging and removes what earlier
    /// runs staged, whether or not this run needs staging. A positive limit
    /// or none keeps staging on when needed and leaves the directory alone
    /// when not. Breaks if `0` only evicts and leaves staging enabled, `0`
    /// skips the removal when staging is not needed, or a positive limit
    /// disables staging or evicts when it is not needed.
    #[test]
    fn a_zero_staging_limit_disables_staging() {
        let digest: ocync_distribution::Digest =
            format!("sha256:{}", "a".repeat(64)).parse().unwrap();
        let above = Some(StagingLimit::Bytes(
            std::num::NonZeroU64::new(1_000_000_000).unwrap(),
        ));
        let one_byte = Some(StagingLimit::Bytes(std::num::NonZeroU64::new(1).unwrap()));
        // want: (staging enabled, an earlier run's blob kept)
        let cases = [
            (
                "disabled, staging needed",
                Some(StagingLimit::Disabled),
                StagingNeed::Needed,
                (false, false),
            ),
            (
                "disabled, staging not needed",
                Some(StagingLimit::Disabled),
                StagingNeed::NotNeeded,
                (false, false),
            ),
            (
                "a limit above the staged size, needed",
                above,
                StagingNeed::Needed,
                (true, true),
            ),
            (
                "a limit of one byte, needed",
                one_byte,
                StagingNeed::Needed,
                (true, false),
            ),
            (
                "a limit of one byte, not needed",
                one_byte,
                StagingNeed::NotNeeded,
                (false, true),
            ),
            ("no limit, needed", None, StagingNeed::Needed, (true, true)),
            (
                "no limit, not needed",
                None,
                StagingNeed::NotNeeded,
                (false, true),
            ),
        ];
        for (name, limit, need, want) in cases {
            let dir = tempfile::tempdir().unwrap();
            let earlier = BlobStage::new(dir.path().to_path_buf());
            let mut writer = earlier.begin_write(&digest).unwrap();
            writer.write_chunk(b"staged by an earlier run").unwrap();
            writer.finish().unwrap();

            let stage = open_staging(dir.path().to_path_buf(), limit, need);
            let got = (stage.is_enabled(), earlier.exists(&digest));
            assert_eq!(got, want, "{name}");
        }
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
        assert_eq!(resolve_cache_ttl(&config), DEFAULT_CACHE_TTL);
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
    /// block, landing in the soft tier (`defaults_exclude`).
    #[test]
    fn build_filter_defaults_exclude_reaches_mapping_with_own_tags() {
        let mapping = TagsConfig {
            semver: Some(range(">=1.0")),
            ..Default::default()
        };
        let defaults = TagsConfig {
            exclude: Some(GlobOrList::List(vec!["*-dev".into(), "*-r[0-9]*".into()])),
            ..Default::default()
        };
        let filter = build_filter(Some(&mapping), Some(&defaults));
        assert_eq!(semver_text(&filter).as_deref(), Some(">=1.0"));
        assert_eq!(filter.defaults_exclude, vec!["*-dev", "*-r[0-9]*"]);
        assert!(filter.exclude.is_empty(), "mapping had no exclude");
    }

    /// `mapping.tags.exclude:` is the hard tier, `defaults.exclude` the soft
    /// tier; both apply.
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

    /// With no mapping `tags:` block, defaults flow through; the source
    /// decides the tier, so `defaults.exclude` stays soft.
    #[test]
    fn build_filter_inherits_defaults_when_mapping_unset() {
        let defaults = TagsConfig {
            semver: Some(range(">=2.0")),
            exclude: Some(GlobOrList::Single("*-dev".into())),
            ..Default::default()
        };
        let filter = build_filter(None, Some(&defaults));
        assert_eq!(semver_text(&filter).as_deref(), Some(">=2.0"));
        assert_eq!(filter.defaults_exclude, vec!["*-dev"]);
        assert!(filter.exclude.is_empty());
    }

    #[test]
    fn build_filter_glob_forms() {
        let cases = [
            ("single", GlobOrList::Single("v1.*".into()), vec!["v1.*"]),
            (
                "list",
                GlobOrList::List(vec!["v1.*".into(), "v2.*".into()]),
                vec!["v1.*", "v2.*"],
            ),
        ];
        for (name, glob, want) in cases {
            let tags = TagsConfig {
                glob: Some(glob),
                ..Default::default()
            };
            assert_eq!(build_filter(Some(&tags), None).glob, want, "{name}");
        }
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
            semver: Some(range(">=1.0.0")),
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
        assert_eq!(semver_text(&filter).as_deref(), Some(">=1.0.0"));
        assert_eq!(filter.exclude, vec!["*-alpine"]);
        assert_eq!(filter.sort, Some(SortOrder::Semver));
        assert_eq!(filter.latest, Some(5));
        assert_eq!(filter.min_tags, Some(1));
    }

    /// `include:` + `semver:` returns both the pinned literals and the
    /// version-range matches.
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

        assert_eq!(
            filter.include,
            vec!["latest".to_string(), "latest-dev".to_string()]
        );

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

        assert!(result.contains(&"latest".to_string()));
        assert!(result.contains(&"latest-dev".to_string()));
        assert!(result.contains(&"1.25.5-r0".to_string()));
        assert!(!result.contains(&"1.24.0".to_string()));
        assert!(!result.contains(&"1.25.5-rc1".to_string()));
        // Top 5 of the range plus 2 floats.
        assert_eq!(result.len(), 7);
    }

    /// Every fall-through field (`sort`, `latest`, `min_tags`, `include`,
    /// `glob`, `semver`, `immutable_tags`) comes from `defaults.tags` when the
    /// mapping leaves it unset, and the mapping wins when set.
    #[test]
    fn merge_inherits_all_fall_through_fields() {
        use ocync_sync::filter::SortOrder;

        let defaults = TagsConfig {
            include: Some(GlobOrList::Single("latest".into())),
            glob: Some(GlobOrList::Single("v*".into())),
            semver: Some(range(">=1.0")),
            sort: Some(SortOrder::Semver),
            latest: Some(10),
            min_tags: Some(2),
            immutable_tags: Some("v?[0-9]*.[0-9]*.[0-9]*".into()),
            ..Default::default()
        };

        let empty_mapping = TagsConfig::default();
        let inherited = build_filter(Some(&empty_mapping), Some(&defaults));
        assert_eq!(inherited.include, vec!["latest"]);
        assert_eq!(inherited.glob, vec!["v*"]);
        assert_eq!(semver_text(&inherited).as_deref(), Some(">=1.0"));
        assert_eq!(inherited.sort, Some(SortOrder::Semver));
        assert_eq!(inherited.latest, Some(10));
        assert_eq!(inherited.min_tags, Some(2));
        assert_eq!(
            resolve_immutable_pattern(Some(&empty_mapping), Some(&defaults)),
            Some("v?[0-9]*.[0-9]*.[0-9]*"),
        );

        let override_mapping = TagsConfig {
            include: Some(GlobOrList::Single("override".into())),
            glob: Some(GlobOrList::Single("override*".into())),
            semver: Some(range(">=2.0")),
            sort: Some(SortOrder::Alpha),
            latest: Some(3),
            min_tags: Some(1),
            immutable_tags: Some("override-pattern".into()),
            ..Default::default()
        };
        let overridden = build_filter(Some(&override_mapping), Some(&defaults));
        assert_eq!(overridden.include, vec!["override"]);
        assert_eq!(overridden.glob, vec!["override*"]);
        assert_eq!(semver_text(&overridden).as_deref(), Some(">=2.0"));
        assert_eq!(overridden.sort, Some(SortOrder::Alpha));
        assert_eq!(overridden.latest, Some(3));
        assert_eq!(overridden.min_tags, Some(1));
        assert_eq!(
            resolve_immutable_pattern(Some(&override_mapping), Some(&defaults)),
            Some("override-pattern"),
        );
    }

    /// Falls back when only one side carries a pattern; `None` when neither does.
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

    /// `defaults.exclude` filters dev and `-rN` revisions, one mapping rescues
    /// `latest-dev` with `include:`, another adds a hard-tier `-slim` exclude.
    /// Asserts the keep set and the dry-run drop attribution by tier.
    #[test]
    fn merge_chainguard_scenario_end_to_end() {
        let defaults_yaml = r#"
exclude: ["*-dev", "*-r[0-9]*"]
"#;
        let defaults: TagsConfig =
            serde_yaml::from_str(defaults_yaml).expect("defaults yaml parses");

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

        // Mapping A: pure inheritance must not lose `defaults.exclude`.
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

        // Mapping B: `include:` rescues from the soft tier.
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

        // Mapping C: hard-tier exclude stacks on defaults.
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

        // Mapping D: each drop carries the right `DropKind`.
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

    /// Through the production deserializer, `defaults.exclude` lands in the
    /// soft tier and mapping exclude in the hard tier. Catches serde aliasing
    /// or renaming regressions that bypass `build_filter`.
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

        assert_eq!(filter.defaults_exclude, vec!["*-dev", "*-r[0-9]*"]);
        assert_eq!(filter.exclude, vec!["*-slim"]);
        assert_eq!(semver_text(&filter).as_deref(), Some(">=1.0"));
        assert_eq!(filter.sort, Some(ocync_sync::filter::SortOrder::Semver));
        assert_eq!(filter.latest, Some(5));

        // -r0 dropped by the soft tier, -slim by the hard tier.
        let tags = vec!["1.27", "1.27-r0", "1.27-dev", "1.27-slim"];
        let kept = filter.apply(&tags).expect("filter applies");
        assert_eq!(kept, vec!["1.27".to_string()]);
    }

    /// A glob `include:` rescues `-dev` from `defaults.exclude`, bounded by a
    /// semver range.
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

        assert_eq!(filter.include, vec!["*-dev"]);
        assert_eq!(filter.defaults_exclude, vec!["*-dev", "*-r[0-9]*"]);
        assert_eq!(semver_text(&filter).as_deref(), Some(">=8.0.0, <9.0.0"));
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

    /// Per-mapping `include:` REPLACES `defaults.tags.include`, never concats.
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
    fn glob_or_list_to_vec_owned_forms() {
        let cases = [
            (
                "single",
                GlobOrList::Single("pattern".into()),
                vec!["pattern"],
            ),
            (
                "list",
                GlobOrList::List(vec!["a".into(), "b".into()]),
                vec!["a", "b"],
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(glob_or_list_to_vec_owned(&input), want, "{name}");
        }
    }

    #[test]
    fn describe_filter_combines_semver_and_latest() {
        let tags = TagsConfig {
            semver: Some(range(">=1.0.0")),
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
    fn no_tags_warn_renders() {
        let truncated = NoTagsInfo {
            from: "library/alpine".into(),
            // image_count > samples.len() drives the truncation marker.
            image_count: 100,
            artifact_count: 0,
            filter_desc: Some("semver >=99.0".into()),
            samples: (0..5).map(|i| format!("v{i}")).collect(),
        };
        assert!(truncated.samples_truncated());
        let cases = [
            (
                "simple repo, no truncation or artifact split",
                no_tags_info(
                    "library/nginx",
                    2,
                    0,
                    Some("semver >=2.0"),
                    &["v1.0", "v1.1"],
                ),
                "library/nginx: 0 of 2 source tags matched filter (semver >=2.0); skipping. Source: [v1.0, v1.1]",
            ),
            (
                "cosign-heavy repo splits image and referrer counts",
                no_tags_info(
                    "chainguard/nginx",
                    2,
                    14287,
                    Some("semver >=1.0.0, latest=5"),
                    &["latest", "latest-dev"],
                ),
                "chainguard/nginx: 0 of 14289 source tags (2 image tags, 14287 referrer artifacts) matched filter (semver >=1.0.0, latest=5); skipping. Source: [latest, latest-dev]",
            ),
            (
                "truncated samples end with an ellipsis",
                truncated,
                "library/alpine: 0 of 100 source tags matched filter (semver >=99.0); skipping. Source: [v0, v1, v2, v3, v4, ...]",
            ),
            (
                "empty samples and missing filter render markers",
                no_tags_info("x/y", 0, 0, None, &[]),
                "x/y: 0 of 0 source tags matched filter (no filter configured); skipping. Source: <empty>",
            ),
        ];
        for (name, info, want) in cases {
            assert_eq!(info.to_string(), want, "{name}");
        }
    }

    /// First observation warns; repeats are suppressed; recovery clears the
    /// entry so a relapse warns again.
    #[test]
    fn watch_log_state_emits_once_per_transition() {
        let mut state = WatchLogState::default();
        assert!(state.observe_no_match("repo-a"));
        assert!(!state.observe_no_match("repo-a"));
        assert!(state.observe_resolved("repo-a"));
        assert!(!state.observe_resolved("repo-a"));
        assert!(state.observe_no_match("repo-a"));
    }

    /// `retain_active` drops entries for mappings no longer in the config.
    #[test]
    fn watch_log_state_prunes_removed_mappings() {
        let mut state = WatchLogState::default();
        state.observe_no_match("repo-a");
        state.observe_no_match("repo-b");
        state.observe_no_match("repo-removed");

        state.retain_active(["repo-a", "repo-b"]);

        assert!(!state.observe_no_match("repo-a"));
        assert!(!state.observe_no_match("repo-b"));
        assert!(state.observe_no_match("repo-removed"));
    }

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

    /// Images from another mapping (same source, different target) must not
    /// bleed into this mapping's counts.
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

    /// A mapping with no images in the report is skipped silently.
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

    /// First observation emits; an identical follow-up is suppressed; a changed
    /// outcome emits again.
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

        assert_eq!(
            state.observe_mapping_outcome("repo-a", &steady),
            OutcomeChange::Changed
        );
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &steady),
            OutcomeChange::Unchanged
        );
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &steady),
            OutcomeChange::Unchanged
        );
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &active),
            OutcomeChange::Changed
        );
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &active),
            OutcomeChange::Unchanged
        );
        assert_eq!(
            state.observe_mapping_outcome("repo-a", &steady),
            OutcomeChange::Changed
        );
    }

    /// `failed > 0` followed by `failed == 0` returns `Some(true)` (the
    /// `[recovered]` marker) and advances the cycle counter on each transition.
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

        assert_eq!(
            state.observe_mapping_outcome("r", &failing),
            OutcomeChange::Changed
        );
        assert_eq!(
            state.observe_mapping_outcome("r", &healthy),
            OutcomeChange::Recovered
        );
        assert_eq!(state.cycle_emit_count(), 2);
    }

    /// `retain_active` also prunes cached outcomes; re-observation after
    /// pruning emits as a first observation.
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

        assert_eq!(
            state.observe_mapping_outcome("drop", &outcome),
            OutcomeChange::Changed
        );
        assert_eq!(
            state.observe_mapping_outcome("keep", &outcome),
            OutcomeChange::Unchanged
        );
    }

    /// Omits zero counts and the bytes clause when nothing transferred, tags
    /// recovery, keeps the `[targets]` bracket whenever more than one registry
    /// was configured (including when one is unreachable), and drops it for a
    /// single target (the destination is already in the `from -> to` arrow).
    #[test]
    fn format_mapping_outcome_table() {
        let outcome = |synced, skipped, failed, bytes| MappingOutcome {
            synced,
            skipped,
            failed,
            bytes,
        };
        let cases = [
            (
                "single target, synced only",
                vec!["ttl"],
                vec![],
                outcome(3, 0, 0, 1024),
                false,
                "library/alpine -> mirror/alpine: synced 3 (1.0 KB)",
            ),
            (
                "single target, skipped only",
                vec!["ttl"],
                vec![],
                outcome(0, 5, 0, 0),
                false,
                "library/alpine -> mirror/alpine: skipped 5",
            ),
            (
                "single target, mixed",
                vec!["ttl"],
                vec![],
                outcome(1, 2, 1, 2048),
                false,
                "library/alpine -> mirror/alpine: synced 1, skipped 2, failed 1 (2.0 KB)",
            ),
            (
                "single target, recovered",
                vec!["ttl"],
                vec![],
                outcome(3, 0, 0, 1024),
                true,
                "library/alpine -> mirror/alpine: synced 3 (1.0 KB) [recovered]",
            ),
            (
                "multi target keeps the bracket",
                vec!["ecr-prod", "ghcr-mirror"],
                vec![],
                outcome(1, 0, 0, 1024),
                false,
                "library/alpine -> mirror/alpine [ecr-prod, ghcr-mirror]: synced 1 (1.0 KB)",
            ),
            (
                "a lost target is still named",
                vec!["good"],
                vec!["broken"],
                outcome(4, 0, 0, 0),
                false,
                "library/alpine -> mirror/alpine [good, broken unreachable]: synced 4",
            ),
        ];
        for (name, targets, dropped, o, recovered, want) in cases {
            let d = MappingDescriptor {
                from: "library/alpine".into(),
                target_repo: "mirror/alpine".into(),
                target_names: targets.into_iter().map(String::from).collect(),
                dropped_names: dropped.into_iter().map(String::from).collect(),
            };
            assert_eq!(format_mapping_outcome(&d, &o, recovered), want, "{name}");
        }
    }

    /// A dry run yields a `FilterReport` carrying the configured `min_tags`
    /// and `include_kept`; selecting `Selection::Sync` instead fails this.
    #[test]
    fn select_filtered_tags_dry_run_populates_min_tags_and_include() {
        use ocync_sync::filter::SortOrder;

        let tags_config = TagsConfig {
            include: Some(GlobOrList::List(vec!["latest".into()])),
            semver: Some(range(">=1.0")),
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
            select_filtered_tags(Some(&tags_config), None, all_tags, Selection::DryRun).unwrap();

        assert_eq!(candidate_count, Some(5));
        let report = report.expect("report present for a dry run");
        assert_eq!(report.min_tags, Some(5));
        // `latest` fails semver but is rescued by `include:`.
        assert_eq!(report.include_kept, vec!["latest".to_string()]);
        // Union of include and the top 2 (1.2.0, 1.1.0); 3 < min_tags, so
        // real sync would error.
        assert_eq!(kept.len(), 3);
        assert!(kept.contains(&"latest".to_string()));
    }

    /// A real sync yields no report, avoiding drop-attribution cost on every
    /// watch cycle.
    #[test]
    fn select_filtered_tags_sync_returns_no_report() {
        let tags_config = TagsConfig {
            glob: Some(GlobOrList::Single("*".into())),
            ..Default::default()
        };
        let all_tags = vec!["1.0".into(), "2.0".into()];
        let (kept, candidate_count, report) =
            select_filtered_tags(Some(&tags_config), None, all_tags, Selection::Sync).unwrap();
        assert_eq!(candidate_count, Some(2));
        assert_eq!(kept.len(), 2);
        assert!(report.is_none());
    }

    /// A real sync rejects a tag set smaller than `min_tags`.
    #[test]
    fn select_filtered_tags_sync_enforces_min_tags() {
        let tags_config = TagsConfig {
            min_tags: Some(10),
            ..Default::default()
        };
        let all_tags = vec!["1.0".into(), "2.0".into()];
        let result = select_filtered_tags(Some(&tags_config), None, all_tags, Selection::Sync);
        assert!(result.is_err(), "expected BelowMinTags");
    }

    /// The dry run does not enforce `min_tags`, so its formatter can surface
    /// the gap instead of suppressing the report.
    #[test]
    fn select_filtered_tags_dry_run_reports_instead_of_enforcing_min_tags() {
        let tags_config = TagsConfig {
            min_tags: Some(10),
            ..Default::default()
        };
        let all_tags = vec!["1.0".into(), "2.0".into()];
        let (kept, candidate_count, report) =
            select_filtered_tags(Some(&tags_config), None, all_tags, Selection::DryRun)
                .expect("a dry run does not enforce min_tags");
        assert_eq!(kept.len(), 2);
        assert_eq!(candidate_count, Some(2));
        let report = report.expect("dry run returns a report despite min_tags");
        assert_eq!(report.min_tags, Some(10));
    }

    /// Three mappings; the middle one names a registry that does not exist.
    /// Literal globs keep resolution off the network.
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

    fn working_clients() -> ClientMap {
        ClientMap::from([
            ("src".to_string(), Ok(test_client("https://source.test"))),
            ("dst".to_string(), Ok(test_client("https://target.test"))),
        ])
    }

    /// A registry whose client could not be built, as `build_clients` records it.
    fn broken_client(message: &str, auth: bool) -> Result<Arc<RegistryClient>, ClientInitError> {
        Err(ClientInitError(if auth {
            CliError::Auth(message.to_string())
        } else {
            CliError::Input(message.to_string())
        }))
    }

    async fn resolve(config: &Config, clients: &ClientMap) -> Resolution {
        resolve_all(
            config,
            clients,
            &HashMap::new(),
            Selection::Sync,
            None,
            &PrepareTracker::default(),
            PREPARE_MAPPING_CONCURRENCY,
        )
        .await
        .expect("no config-class failure in these fixtures")
    }

    /// A mapping that cannot be resolved must not cost the mappings behind it.
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
        // The classification must survive, or a denied run drops to the generic code.
        assert!(
            matches!(unresolved[0].code, ExitCode::AuthError),
            "a denied registry must stay classified as a denial"
        );
    }

    /// The isolation path must not manufacture failures on a clean config.
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

    /// A broken mirror must not stop the other mirrors from receiving the image.
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

        // The unreached target must stay visible.
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].registry, "broken");
        assert_eq!(dropped[0].from, "repo/one");
        assert!(dropped[0].error.contains("403 Forbidden"));
    }

    /// A dropped target must move the exit code off success and appear in the
    /// cycle tail and the `--json` document.
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

    /// Losing every target leaves nowhere to sync: the mapping is unresolvable
    /// and keeps its cause.
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

    /// `build_clients` must not construct a client for a registry no mapping
    /// names. `static_token` resolves offline.
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

        let clients = build_clients(
            &config,
            &referenced_registries(&config),
            &PrepareTracker::default(),
            &RunCredentials::default(),
        )
        .await;

        let mut names: Vec<&str> = clients.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["dst", "src"], "'unused' must not be built");
    }

    /// A registry no mapping references must not be contacted at all.
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

        assert!(used.sources.contains("src"));
        assert!(
            used.targets.contains("dst"),
            "target groups must be expanded"
        );
        assert!(
            !used.contains("unused"),
            "a registry no mapping names must not be built"
        );
        assert_eq!(used.len(), 2);
    }

    /// The ticker fires on wall-clock, not on item boundaries, and work that
    /// finishes inside one interval stays silent.
    #[tokio::test(start_paused = true)]
    async fn prepare_ticker_cadence() {
        let cases = [
            ("17s of work at a 5s cadence", 17, 3),
            ("work inside one interval", 1, 0),
        ];
        for (name, work_secs, want) in cases {
            let tracker = PrepareTracker::default();
            tracker.begin(PreparePhase::Mappings, 1);
            let ticks = RefCell::new(0usize);

            tick_while(
                &tracker,
                Duration::from_secs(5),
                tokio::time::sleep(Duration::from_secs(work_secs)),
                |_, _| *ticks.borrow_mut() += 1,
            )
            .await;

            assert_eq!(*ticks.borrow(), want, "{name}");
        }
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
                let item = tracker.track("registry-a");
                tokio::time::sleep(Duration::from_secs(7)).await;
                drop(item);
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

    /// The progress counter must advance for non-ECR registries too, or it
    /// reports `done=0 total=N` forever.
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
        let referenced = referenced_registries(&config);
        let credentials = RunCredentials::default();
        let clients = build_clients(&config, &referenced, &tracker, &credentials).await;
        let checkers =
            build_batch_checkers(&config, &clients, &referenced, &tracker, &credentials.aws);

        assert!(checkers.is_empty(), "neither registry is ECR");
        let p = tracker.snapshot();
        assert_eq!(
            p.total, 1,
            "only the target registry is a checker candidate; the map is read by target alias"
        );
        assert_eq!(p.done, p.total, "every registry considered must be counted");
    }

    /// An ECR target's auth provider and batch checker, built for one run,
    /// share one set of AWS credentials. Breaks if either builder makes
    /// credentials of its own.
    #[tokio::test]
    async fn ecr_client_and_batch_checker_share_credentials() {
        let config: Config = serde_yaml::from_str(
            r#"
registries:
  src:
    url: source.test
    auth_type: static_token
    token: t
  dst:
    url: 123456789012.dkr.ecr.us-west-2.amazonaws.com
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
        let referenced = referenced_registries(&config);
        let credentials = RunCredentials::default();
        let clients = build_clients(&config, &referenced, &tracker, &credentials).await;
        let checkers =
            build_batch_checkers(&config, &clients, &referenced, &tracker, &credentials.aws);

        assert!(checkers.contains_key("dst"));
        assert_eq!(credentials.aws.users(), vec![2]);
    }

    /// Re-reporting the same dead mirror is noise; a different mirror going
    /// down is a new transition.
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

        // Partial recovery: mirror-a is back, mirror-b is still down.
        let still_down = [DroppedTarget {
            from: "repo/one".into(),
            registry: "mirror-b".into(),
            error: "still down".into(),
        }];
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

    /// A mapping dropped from config must not leave state behind, or re-adding
    /// it would suppress its next outage.
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
        assert_eq!(
            state.observe_mapping_outcome("repo/one", &outcome),
            OutcomeChange::Changed
        );
        assert_eq!(
            state.observe_mapping_outcome("repo/one", &outcome),
            OutcomeChange::Unchanged
        );

        state.observe_mapping_unresolved("repo/one");
        state.observe_mapping_resolved("repo/one");

        assert_eq!(
            state.observe_mapping_outcome("repo/one", &outcome),
            OutcomeChange::Changed,
            "recovery must report even when the counts are unchanged"
        );
    }

    /// A mapping naming no targets stays a zero-target mapping, not an error.
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

    /// A broken config must still stop the run: isolation is for registries
    /// that are down, not configs that are wrong.
    #[tokio::test]
    async fn a_config_error_is_fatal_rather_than_isolated() {
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
  - from: repo/two
    tags:
      glob: ["v1"]
      immutable_tags: "[unclosed"
"#,
        )
        .expect("config yaml parses");

        let err = resolve_all(
            &config,
            &working_clients(),
            &HashMap::new(),
            Selection::Sync,
            None,
            &PrepareTracker::default(),
            PREPARE_MAPPING_CONCURRENCY,
        )
        .await
        .expect_err("an invalid glob must not be isolated");

        assert_eq!(
            err.exit_code(),
            ExitCode::ConfigError,
            "and it keeps the config exit code"
        );
    }

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
    /// would score the run a clean success; a run denied everywhere keeps the
    /// auth code, and one non-auth failure makes the set generic.
    #[test]
    fn combined_exit_code_table() {
        use ocync_sync::ImageStatus;
        let skipped = || ImageStatus::Skipped {
            reason: ocync_sync::SkipReason::DigestMatch,
        };
        let cases = [
            (
                "clean report, nothing unresolved",
                vec![ImageStatus::Synced],
                vec![],
                ExitCode::Success,
            ),
            (
                "clean report downgraded by an unresolved mapping",
                vec![ImageStatus::Synced],
                vec![("repo/two", false)],
                ExitCode::PartialFailure,
            ),
            (
                "nothing succeeded is a total failure",
                vec![],
                vec![("repo/one", false), ("repo/two", true)],
                ExitCode::Failure,
            ),
            (
                "a skipped image counts as the run having worked",
                vec![skipped()],
                vec![("repo/two", false)],
                ExitCode::PartialFailure,
            ),
            (
                "denied everywhere keeps the auth code",
                vec![],
                vec![("repo/one", true), ("repo/two", true)],
                ExitCode::AuthError,
            ),
            (
                "a mixed failure set is not auth",
                vec![],
                vec![("repo/one", true), ("repo/two", false)],
                ExitCode::Failure,
            ),
        ];
        for (name, statuses, unresolved, want) in cases {
            let report = report_from(statuses);
            let unresolved: Vec<_> = unresolved
                .into_iter()
                .map(|(from, auth)| unresolved_mapping(from, auth))
                .collect();
            assert_eq!(
                combined_exit_code(&report, &unresolved, &[], 0),
                want,
                "{name}"
            );
        }
    }

    /// `--dry-run` returns before the engine, so it applies the auth rule itself.
    #[test]
    fn dry_run_exit_code_table() {
        let cases = [
            ("all resolved", 3, vec![], ExitCode::Success),
            (
                "some unresolved",
                2,
                vec![("repo/two", false)],
                ExitCode::PartialFailure,
            ),
            (
                "none resolved",
                0,
                vec![("repo/two", false)],
                ExitCode::Failure,
            ),
            (
                "none resolved, all denied",
                0,
                vec![("repo/one", true), ("repo/two", true)],
                ExitCode::AuthError,
            ),
            (
                "partly denied",
                1,
                vec![("repo/one", true), ("repo/two", true)],
                ExitCode::PartialFailure,
            ),
        ];
        for (name, succeeded, unresolved, want) in cases {
            let unresolved: Vec<_> = unresolved
                .into_iter()
                .map(|(from, auth)| unresolved_mapping(from, auth))
                .collect();
            assert_eq!(
                dry_run_exit_code(succeeded, &unresolved, &[]),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn cycle_tail_unresolved_clause() {
        let report = report_with(Vec::new());
        let line = format_cycle_tail(4, 2, 0, &report);
        assert!(line.contains("2 unresolved"), "names them: {line}");
        let line = format_cycle_tail(4, 0, 0, &report);
        assert!(!line.contains("unresolved"), "omitted when none: {line}");
    }

    /// `--json` consumers must see mappings that never reached the engine.
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
        // The classification drives the exit code only.
        assert!(
            value["unresolved_mappings"][0].get("auth").is_none(),
            "{value}"
        );
        // Flattened: the report's own keys stay at the top level.
        assert!(value.get("stats").is_some(), "{value}");
    }

    /// Both keys are always present, matching `analyze`.
    #[test]
    fn json_output_always_carries_the_failure_keys() {
        let report = report_with(Vec::new());
        let value = serde_json::to_value(SyncOutput {
            report: &report,
            unresolved_mappings: &[],
            dropped_targets: &[],
        })
        .expect("serializes");

        assert_eq!(value["unresolved_mappings"], serde_json::json!([]));
        assert_eq!(value["dropped_targets"], serde_json::json!([]));
    }

    /// The filter report must flow from `select_filtered_tags` through
    /// `ResolvedMapping` to the `dry_run::write_to` output.
    #[test]
    fn dry_run_full_wire_up_renders_min_tags_failure() {
        let tags_config = TagsConfig {
            semver: Some(range(">=2.0")),
            min_tags: Some(5),
            ..Default::default()
        };
        let all_tags: Vec<String> = (0..10).map(|i| format!("1.{i}.0")).collect();
        let (kept, candidate_count, filter_report) =
            select_filtered_tags(Some(&tags_config), None, all_tags, Selection::DryRun).unwrap();
        assert_eq!(kept.len(), 0); // all dropped by semver >=2.0
        assert!(filter_report.is_some());

        let mut mapping = mapping_with_tags_and_report(
            kept.into_iter().map(TagPair::same).collect(),
            filter_report,
        );
        mapping.candidate_count = candidate_count;

        let mut buf: Vec<u8> = Vec::new();
        crate::cli::commands::dry_run::write_to(&mut buf, &[mapping], &[], &[], false).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("min_tags: 5"), "{out}");
        assert!(out.contains("FAIL"), "{out}");
        assert!(out.contains("BelowMinTags"), "{out}");
        assert!(out.contains("source tags: 10"), "{out}");
        assert!(out.contains("dropped (10):"), "{out}");
    }

    use crate::cli::commands::test_support::{
        SlowRecorder, arrivals, config_yaml, mapping_with_tags_and_report, max_in_flight,
        repo_names, slow_registry, test_client,
    };

    /// Mapping resolution must overlap its network waits.
    #[tokio::test]
    async fn resolve_all_resolves_mappings_concurrently() {
        let delay = Duration::from_millis(250);
        let (_server, config, clients, arrivals, _) =
            slow_registry(&repo_names(8), &["v1"], delay).await;

        let resolution = resolve_all(
            &config,
            &clients,
            &HashMap::new(),
            Selection::Sync,
            None,
            &PrepareTracker::default(),
            PREPARE_MAPPING_CONCURRENCY,
        )
        .await
        .expect("no config-class failure in this fixture");

        assert_eq!(resolution.resolved.len(), 8, "every mapping must resolve");
        let observed = max_in_flight(&arrivals, delay);
        assert!(
            observed > 1,
            "tag listings must overlap; saw at most {observed} in flight"
        );
    }

    /// The limit is a ceiling, asserted exactly: reaching it proves overlap,
    /// never passing it proves the ceiling is real.
    #[tokio::test]
    async fn resolve_all_never_exceeds_its_concurrency_limit() {
        let delay = Duration::from_millis(250);
        let limit = 2;
        let (_server, config, clients, arrivals, _) =
            slow_registry(&repo_names(8), &["v1"], delay).await;

        resolve_all(
            &config,
            &clients,
            &HashMap::new(),
            Selection::Sync,
            None,
            &PrepareTracker::default(),
            limit,
        )
        .await
        .expect("no config-class failure in this fixture");

        let observed = max_in_flight(&arrivals, delay);
        assert_eq!(
            observed, limit,
            "resolution must run exactly {limit} mappings at a time"
        );
    }

    /// `buffered(0)` never completes, so a zero limit must floor at one or the
    /// run hangs before the engine starts.
    #[tokio::test]
    async fn resolve_all_treats_a_zero_limit_as_one() {
        let (_server, config, clients, ..) =
            slow_registry(&repo_names(2), &["v1"], Duration::from_millis(1)).await;

        let resolution = resolve_all(
            &config,
            &clients,
            &HashMap::new(),
            Selection::Sync,
            None,
            &PrepareTracker::default(),
            0,
        )
        .await
        .expect("no config-class failure in this fixture");

        assert_eq!(resolution.resolved.len(), 2);
    }

    /// A registry where the `slow` repositories answer after `slow_delay` and
    /// the rest after `fast_delay`.
    ///
    /// Under uniform latency the queue head is never the straggler, so an
    /// ordered queue looks identical to one that refills as work finishes;
    /// only a skewed fixture tells them apart.
    async fn skewed_tag_registry(
        total: usize,
        slow: &[usize],
        slow_delay: Duration,
        fast_delay: Duration,
    ) -> (wiremock::MockServer, Config, ClientMap) {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v2/repo/slow\d+/tags/list$",
            ))
            .respond_with(SlowRecorder::tag_list(&arrivals(), slow_delay, &["v1"]))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/v2/repo/img[\w-]+/tags/list$",
            ))
            .respond_with(SlowRecorder::tag_list(&arrivals(), fast_delay, &["v1"]))
            .mount(&server)
            .await;

        // The queue is built in config order, so slow positions matter.
        let repos: Vec<String> = (0..total)
            .map(|i| {
                if slow.contains(&i) {
                    format!("repo/slow{i}")
                } else {
                    format!("repo/img{i}")
                }
            })
            .collect();

        let yaml = config_yaml(&server, &repos, "      glob: [\"*\"]\n");
        let config: Config = serde_yaml::from_str(&yaml).expect("generated config parses");
        let src = test_client(&server.uri());

        // Warm the AIMD window: a cold client (window 1) serializes every
        // mapping and hides the queue's behaviour. The window grows by `1/w`
        // per success, reaching `sqrt(1 + 2n)` after n: 8 successes give ~4.3,
        // barely clearing a limit of 4; 16 give ~5.7, so a slack window can
        // only mean the queue.
        let warmup = RepositoryName::new("repo/img-warmup").expect("fixture repo name is valid");
        for _ in 0..16 {
            src.list_tags(&warmup)
                .await
                .expect("warmup listing succeeds");
        }

        let clients = ClientMap::from([
            ("src".to_string(), Ok(src)),
            ("dst".to_string(), Ok(test_client(&server.uri()))),
        ]);
        (server, config, clients)
    }

    /// A slow mapping must not hold the window closed behind it: while any
    /// mapping has not started, the window has to be full. This is the half
    /// `resolve_all_never_exceeds_its_concurrency_limit` cannot see.
    #[tokio::test]
    async fn resolve_all_keeps_its_window_full_while_mappings_wait() {
        let limit = 4;
        let (_server, config, clients) = skewed_tag_registry(
            12,
            &[0],
            Duration::from_millis(600),
            Duration::from_millis(20),
        )
        .await;

        let tracker = PrepareTracker::default();
        let starved: Rc<Cell<Option<(usize, usize)>>> = Rc::new(Cell::new(None));
        let checkers = HashMap::new();

        let work = resolve_all(
            &config,
            &clients,
            &checkers,
            Selection::Sync,
            None,
            &tracker,
            limit,
        );

        let observer = {
            let starved = Rc::clone(&starved);
            let tracker = &tracker;
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let p = tracker.snapshot();
                    let unstarted = p.total.saturating_sub(p.done + p.in_flight);
                    if unstarted > 0 && p.in_flight < limit {
                        starved.set(Some((p.in_flight, unstarted)));
                    }
                }
            }
        };

        let resolution = tokio::select! {
            r = work => r.expect("no config-class failure in this fixture"),
            _ = observer => unreachable!("the observer loops until resolution finishes"),
        };

        assert_eq!(resolution.resolved.len(), 12, "every mapping must resolve");
        assert_eq!(
            starved.get(),
            None,
            "the window went slack while mappings waited: (in_flight, unstarted)"
        );
    }

    /// A mapping's immutable-tag listings must run across its targets
    /// concurrently, or prepare cost scales with fan-out.
    #[tokio::test]
    async fn resolve_mapping_lists_target_tags_concurrently() {
        let delay = Duration::from_millis(250);
        let server = wiremock::MockServer::start().await;
        let arrivals = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Only the target listings land in the recorder.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/repo/img0/tags/list"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"name": "repo/img0", "tags": ["v1"]})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/mirror/img0/tags/list"))
            .respond_with(SlowRecorder::tag_list(&arrivals, delay, &["v1"]))
            .mount(&server)
            .await;

        let config: Config = serde_yaml::from_str(&format!(
            r#"
registries:
  src:
    url: {uri}
  dst1:
    url: {uri}
  dst2:
    url: {uri}
  dst3:
    url: {uri}
defaults:
  source: src
  targets: [dst1, dst2, dst3]
mappings:
  - from: repo/img0
    to: mirror/img0
    tags:
      glob: ["*"]
      immutable_tags: "v*"
"#,
            uri = server.uri()
        ))
        .expect("config yaml parses");

        let clients = ClientMap::from([
            ("src".to_string(), Ok(test_client(&server.uri()))),
            ("dst1".to_string(), Ok(test_client(&server.uri()))),
            ("dst2".to_string(), Ok(test_client(&server.uri()))),
            ("dst3".to_string(), Ok(test_client(&server.uri()))),
        ]);

        let (outcome, _dropped) = resolve_mapping(
            &config.mappings[0],
            &config,
            &clients,
            &HashMap::new(),
            Selection::Sync,
        )
        .await;
        assert!(matches!(
            outcome.expect("the mapping resolves"),
            MappingResolution::Resolved(_)
        ));

        let observed = max_in_flight(&arrivals, delay);
        assert_eq!(
            observed, 3,
            "all three target listings must be open at once"
        );
    }

    /// The progress line must name the item holding the step up.
    #[tokio::test(start_paused = true)]
    async fn prepare_ticker_names_the_item_holding_the_step_up() {
        let tracker = PrepareTracker::default();
        tracker.begin(PreparePhase::Mappings, 2);
        let seen = RefCell::new(Vec::new());

        tick_while(
            &tracker,
            Duration::from_secs(5),
            async {
                let slow = tracker.track("repo/slow");
                tokio::time::sleep(Duration::from_secs(4)).await;
                // Started later, so it must never be the one reported.
                let quick = tracker.track("repo/quick");
                tokio::time::sleep(Duration::from_secs(2)).await;
                drop(quick);
                tokio::time::sleep(Duration::from_secs(6)).await;
                drop(slow);
            },
            |p, _| seen.borrow_mut().push(p.slowest.clone()),
        )
        .await;

        let reported: Vec<(String, u64)> = seen
            .borrow()
            .iter()
            .map(|s| {
                let (name, age) = s.as_ref().expect("a mapping is in flight at every tick");
                (name.clone(), age.as_secs())
            })
            .collect();
        assert_eq!(
            reported,
            vec![("repo/slow".to_string(), 5), ("repo/slow".to_string(), 10),],
            "the oldest in-flight item must be reported, with how long it has been running"
        );
    }

    /// Dropping the guard both counts the item and retires it, so no early
    /// exit strands the progress line short of its total.
    #[tokio::test]
    async fn prepare_tracker_retires_an_item_when_its_guard_drops() {
        let tracker = PrepareTracker::default();
        tracker.begin(PreparePhase::Mappings, 1);

        let item = tracker.track("repo/one");
        let during = tracker.snapshot();
        assert_eq!((during.done, during.in_flight), (0, 1));

        drop(item);
        let after = tracker.snapshot();
        assert_eq!((after.done, after.in_flight), (1, 0));
        assert!(
            after.slowest.is_none(),
            "nothing is running, so nothing is reported as slow"
        );
    }

    /// A leaked item from the previous step must not be reported as holding up
    /// the next.
    #[tokio::test]
    async fn prepare_tracker_clears_in_flight_items_between_steps() {
        let tracker = PrepareTracker::default();
        tracker.begin(PreparePhase::Registries, 1);
        let leaked = tracker.track("registry-a");
        std::mem::forget(leaked);

        tracker.begin(PreparePhase::Mappings, 1);

        let p = tracker.snapshot();
        assert_eq!(p.in_flight, 0);
        assert!(p.slowest.is_none());
    }

    // Prepare-phase benchmarks, ignored by default (wall-clock against a mock
    // registry with injected latency). Run with:
    //   cargo test --bin ocync -- --ignored --nocapture prepare_phase

    /// Tag count the pagination benchmark serves.
    const BENCH_REPO_TAGS: usize = 3000;

    /// A `tags/list` that behaves like real registries: no `n` gets every tag in
    /// one response with no `Link` header, and only an explicit `n` pages.
    /// Matches Docker Hub and `distribution/distribution` (limit -1 when `n` is
    /// absent).
    struct PaginatedTagList {
        requests: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl wiremock::Respond for PaginatedTagList {
        fn respond(&self, req: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let all: Vec<String> = (0..BENCH_REPO_TAGS).map(|i| format!("tag{i:04}")).collect();
            let mut page = all.len();
            let mut last: Option<String> = None;
            for (k, v) in req.url.query_pairs() {
                match k.as_ref() {
                    "n" => page = v.parse().unwrap_or(all.len()),
                    "last" => last = Some(v.into_owned()),
                    _ => {}
                }
            }

            let start = match &last {
                Some(l) => all.iter().position(|t| t == l).map_or(0, |i| i + 1),
                None => 0,
            };
            let end = (start + page).min(all.len());
            let mut resp = wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"name": "repo/paged", "tags": &all[start..end]}));
            if end < all.len() {
                resp = resp.append_header(
                    "link",
                    format!(
                        r#"</v2/repo/paged/tags/list?n={page}&last={}>; rel="next""#,
                        all[end - 1]
                    )
                    .as_str(),
                );
            }
            resp
        }
    }

    /// Prepare-phase time as mapping resolution is widened; concurrency 1 is
    /// the baseline row.
    #[tokio::test]
    #[ignore = "wall-clock benchmark against a latency-injected mock registry"]
    async fn prepare_phase_benchmark_resolution_concurrency() {
        let mappings = 95;
        let delay = Duration::from_millis(60);

        println!(
            "\n{mappings} mappings, {}ms per tag listing\n",
            delay.as_millis()
        );
        println!(
            "{:>11}  {:>10}  {:>10}  {:>12}",
            "concurrency", "elapsed", "speedup", "max in flight"
        );

        let mut baseline: Option<Duration> = None;
        for concurrency in [1usize, 4, 8, 16, 32] {
            // Fresh server and clients per row: reused AIMD windows would give
            // later rows a head start.
            let (_server, config, clients, arrivals, _) =
                slow_registry(&repo_names(mappings), &["v1"], delay).await;

            let started = std::time::Instant::now();
            let resolution = resolve_all(
                &config,
                &clients,
                &HashMap::new(),
                Selection::Sync,
                None,
                &PrepareTracker::default(),
                concurrency,
            )
            .await
            .expect("no config-class failure in this fixture");
            let elapsed = started.elapsed();
            assert_eq!(resolution.resolved.len(), mappings);

            let observed = max_in_flight(&arrivals, delay);
            let base = *baseline.get_or_insert(elapsed);
            println!(
                "{concurrency:>11}  {:>9.2}s  {:>9.1}x  {observed:>12}",
                elapsed.as_secs_f64(),
                base.as_secs_f64() / elapsed.as_secs_f64()
            );
        }
        println!(
            "\nresolution runs at most {PREPARE_MAPPING_CONCURRENCY} mappings at once; \
             past that each registry's AIMD window is the binding constraint\n"
        );
    }

    /// Prepare-phase cost with skewed mapping latency, the only shape that
    /// tells an ordered queue from a window that refills as work finishes
    /// (the uniform benchmark reports the same speedup for both).
    ///
    /// The floor is the slowest single mapping; a run at a multiple of it is
    /// paying for slots it is not using.
    #[tokio::test]
    #[ignore = "wall-clock benchmark against a latency-injected mock registry"]
    async fn prepare_phase_benchmark_skewed_resolution() {
        let mappings = 95;
        let slow_delay = Duration::from_millis(800);
        let fast_delay = Duration::from_millis(20);
        // One slow mapping per window: the worst case for an ordered queue.
        let slow: Vec<usize> = (0..mappings).step_by(PREPARE_MAPPING_CONCURRENCY).collect();

        println!(
            "\n{mappings} mappings, {} slow at {}ms, rest at {}ms, one slow per window\n",
            slow.len(),
            slow_delay.as_millis(),
            fast_delay.as_millis()
        );
        println!(
            "{:>11}  {:>10}  {:>10}  {:>14}",
            "concurrency", "elapsed", "floor", "over floor"
        );

        for concurrency in [1usize, 8, 16, 32] {
            let (_server, config, clients) =
                skewed_tag_registry(mappings, &slow, slow_delay, fast_delay).await;

            let started = std::time::Instant::now();
            let resolution = resolve_all(
                &config,
                &clients,
                &HashMap::new(),
                Selection::Sync,
                None,
                &PrepareTracker::default(),
                concurrency,
            )
            .await
            .expect("no config-class failure in this fixture");
            let elapsed = started.elapsed();
            assert_eq!(resolution.resolved.len(), mappings);

            // Floor: the larger of the slowest mapping and total work spread
            // evenly across the window.
            let total_work = slow_delay.mul_f64(slow.len() as f64)
                + fast_delay.mul_f64((mappings - slow.len()) as f64);
            let floor = slow_delay.max(total_work.div_f64(concurrency as f64));
            println!(
                "{concurrency:>11}  {:>9.2}s  {:>9.2}s  {:>13.1}x",
                elapsed.as_secs_f64(),
                floor.as_secs_f64(),
                elapsed.as_secs_f64() / floor.as_secs_f64()
            );
        }
        println!();
    }

    /// Round trips for one repository's tag listing with and without a page
    /// size: the evidence for not sending `n`, reproducing Docker Hub's
    /// behaviour on `library/python`.
    #[tokio::test]
    #[ignore = "benchmark: counts round trips against a paginated mock registry"]
    async fn prepare_phase_benchmark_tag_list_pagination() {
        let server = wiremock::MockServer::start().await;
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/repo/paged/tags/list"))
            .respond_with(PaginatedTagList {
                requests: Arc::clone(&requests),
            })
            .mount(&server)
            .await;

        // Cost of an explicit page size: one round trip per page.
        let http = ocync_distribution::test_http_client();
        let mut url = format!("{}/v2/repo/paged/tags/list?n=1000", server.uri());
        let mut paged_tags = 0usize;
        loop {
            let resp = http.get(&url).send().await.expect("mock answers");
            // The mock emits one link-value: the path is between the angle
            // brackets. Deliberately not the client's parser, which this
            // baseline measures.
            let next = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(|h| {
                    let start = h.find('<')? + 1;
                    let end = h[start..].find('>')? + start;
                    Some(h[start..end].to_owned())
                });
            let body: serde_json::Value = resp.json().await.expect("mock returns json");
            paged_tags += body["tags"].as_array().map_or(0, |a| a.len());
            match next {
                Some(path) => url = format!("{}{path}", server.uri()),
                None => break,
            }
        }
        let paged_requests = requests.swap(0, std::sync::atomic::Ordering::Relaxed);

        let client = test_client(&server.uri());
        let tags = client
            .list_tags(&RepositoryName::new("repo/paged").unwrap())
            .await
            .expect("the listing succeeds");
        let ocync_requests = requests.load(std::sync::atomic::Ordering::Relaxed);

        assert_eq!(tags.len(), BENCH_REPO_TAGS, "the whole listing is walked");
        assert_eq!(paged_tags, BENCH_REPO_TAGS, "and so is the paged walk");

        println!("\n{BENCH_REPO_TAGS} tags in one repository\n");
        println!("{:>24}  {:>9}", "request", "round trips");
        println!("{:>24}  {ocync_requests:>9}", "no n (ocync)");
        println!("{:>24}  {paged_requests:>9}", "n=1000");
        println!(
            "\nasking for a page size costs {:.0}x the round trips\n",
            paged_requests as f64 / ocync_requests as f64
        );
    }
}
