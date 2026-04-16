//! Sync engine -- pipelined concurrent orchestrator for image transfers.
//!
//! The engine processes images through a pipelined architecture where discovery
//! and execution overlap via `tokio::select!`:
//!
//! - **Discovery** futures concurrently pull source manifests and HEAD-check
//!   target manifests. Source data is shared via `Rc<PulledManifest>` across targets
//!   for the same tag. Targets where the digest already matches produce
//!   `DiscoveryOutcome::Skip`; others become `TransferTask` entries for execution.
//!
//! - **Execution** futures transfer blobs and push manifests for each active
//!   (tag, target) pair, bounded by a global `Semaphore`. Blob transfers use
//!   progressive cache population: HEAD checks happen inline during execution
//!   instead of in a separate plan phase.
//!
//! All concurrency is cooperative (single-threaded tokio runtime). Shared mutable
//! state (`TransferStateCache`) is accessed via `Rc<RefCell<>>` with borrows
//! never held across `.await` points.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use ocync_distribution::BatchBlobChecker;
use ocync_distribution::Digest;
use ocync_distribution::RegistryClient;
use ocync_distribution::blob::MountResult;
use ocync_distribution::manifest::ManifestPull;
use ocync_distribution::sha256::Sha256;
use ocync_distribution::spec::{
    Descriptor, ImageIndex, ImageManifest, ManifestKind, PlatformFilter, RegistryAuthority,
    RepositoryName,
};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::cache::{PlatformFilterKey, SnapshotKey, SourceSnapshot, TransferStateCache};
use crate::retry::{self, RetryConfig};
use crate::shutdown::ShutdownSignal;
use crate::staging::BlobStage;
use crate::{
    BlobTransferStats, ErrorKind, ImageResult, ImageStatus, SkipReason, SyncReport, SyncStats,
};

/// An image reference within a single registry (repository name + tag).
///
/// Bundles the two fields that always travel together in discovery and
/// transfer operations. The [`Display`] impl formats as `repo:tag`.
///
/// This is **not** a full OCI reference (which includes the registry hostname).
/// The registry is tracked separately via the associated [`RegistryClient`].
#[derive(Debug, Clone)]
struct ImageRef {
    /// Repository path (e.g. `library/nginx`).
    repo: RepositoryName,
    /// Tag name (e.g. `latest`, `1.25`).
    tag: String,
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.repo, self.tag)
    }
}

/// A registry identifier used as the blob deduplication key.
///
/// Wraps the config-defined name (e.g. `us-ecr`) or bare hostname for
/// ad-hoc commands. Must be unique per mapping -- it is used as the outer
/// key in the blob dedup map.
#[derive(Debug, Clone)]
pub struct RegistryAlias(String);

impl RegistryAlias {
    /// Create a new registry alias.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl std::ops::Deref for RegistryAlias {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RegistryAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fully resolved mapping ready for the sync engine.
///
/// All config resolution (registry lookup, tag filtering) is done
/// before constructing this type. The engine operates purely on
/// resolved values.
#[derive(Debug)]
pub struct ResolvedMapping {
    /// Source registry authority for cache key construction (e.g. `cgr.dev:443`).
    pub source_authority: RegistryAuthority,
    /// Client for the source registry.
    pub source_client: Arc<RegistryClient>,
    /// Repository path at the source (e.g. `library/nginx`).
    pub source_repo: RepositoryName,
    /// Repository path at the target (e.g. `mirror/nginx`).
    pub target_repo: RepositoryName,
    /// Target registries to sync to.
    pub targets: Vec<TargetEntry>,
    /// Tag pairs to sync (already filtered).
    pub tags: Vec<TagPair>,
    /// Optional platform filter list (e.g., `["linux/amd64"]`).
    ///
    /// When `Some`, index manifests are filtered to only include descriptors
    /// whose platform matches one of the filters (using
    /// [`Platform::matches()`]). Child manifests and blobs for non-matching
    /// platforms are never pulled from the source.
    pub platforms: Option<Vec<PlatformFilter>>,
    /// When `true`, skip syncing when the target already has any manifest for
    /// the tag, regardless of digest comparison.
    ///
    /// This avoids the overhead of full digest comparison when the user only
    /// cares that *some* version exists at the target.
    pub skip_existing: bool,
}

/// A single target registry entry.
///
/// `name` must be unique across targets within a [`ResolvedMapping`] -- it is
/// used as the key in the blob deduplication map to track which blobs have
/// already been transferred to each target.
pub struct TargetEntry {
    /// Registry identifier used as the blob dedup key.
    ///
    /// Must be unique per mapping. Typically the config-defined registry name
    /// (e.g. `us-ecr`) or the bare hostname for ad-hoc commands.
    pub name: RegistryAlias,
    /// Client for this target registry.
    pub client: Arc<RegistryClient>,
    /// Optional batch blob checker for this target (ECR batch API).
    ///
    /// When present, [`transfer_image_blobs`] pre-populates the cache via
    /// a single batch call instead of per-blob HEAD checks.
    pub batch_checker: Option<Rc<dyn BatchBlobChecker>>,
}

impl Clone for TargetEntry {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            client: Arc::clone(&self.client),
            batch_checker: self.batch_checker.clone(),
        }
    }
}

impl fmt::Debug for TargetEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetEntry")
            .field("name", &self.name)
            .field("client", &self.client)
            .field("batch_checker", &self.batch_checker.as_ref().map(|_| ".."))
            .finish()
    }
}

/// A source/target tag pair for syncing.
///
/// For most sync operations the source and target tags are identical.
/// For `copy` with retagging they may differ.
#[derive(Debug, Clone)]
pub struct TagPair {
    /// Tag name at the source registry.
    pub source: String,
    /// Tag name at the target registry.
    pub target: String,
}

impl TagPair {
    /// Create a pair where source and target tags are the same.
    pub fn same(tag: impl Into<String>) -> Self {
        let t = tag.into();
        Self {
            source: t.clone(),
            target: t,
        }
    }

    /// Create a pair with different source and target tags.
    pub fn retag(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }
}

/// Collect all blob descriptors (config + layers) from an image manifest.
fn collect_image_blobs(manifest: &ImageManifest) -> Vec<&Descriptor> {
    let mut blobs = Vec::with_capacity(1 + manifest.layers.len());
    blobs.push(&manifest.config);
    for layer in &manifest.layers {
        blobs.push(layer);
    }
    blobs
}

/// Build an `ImageResult` for a skipped image (zero transfer, zero duration).
fn skip_image_result(source: &ImageRef, target: &ImageRef, reason: SkipReason) -> ImageResult {
    ImageResult {
        image_id: Uuid::now_v7(),
        source: source.to_string(),
        target: target.to_string(),
        status: ImageStatus::Skipped { reason },
        bytes_transferred: 0,
        blob_stats: BlobTransferStats::default(),
        duration: Duration::ZERO,
    }
}

/// Source manifest data pre-pulled once per tag.
///
/// Separating the source pull from the target push ensures manifests
/// are fetched once regardless of target count (1:N fan-out).
struct PulledManifest {
    /// The top-level manifest (image or index).
    pull: ManifestPull,
    /// For index manifests: pre-pulled child manifests.
    /// Empty for single-image manifests (blobs are derived from `pull.manifest`).
    children: Vec<ManifestPull>,
}

/// Per-target outcome of the blob transfer phase.
#[derive(Default)]
struct TargetBlobOutcome {
    bytes_transferred: u64,
    stats: BlobTransferStats,
    /// `Some` if a target-specific push failed. Other targets are unaffected.
    error: Option<crate::Error>,
}

/// An image ready for execution (one per target that needs sync).
struct TransferTask {
    /// Shared source data (pulled once per tag).
    source_data: Rc<PulledManifest>,
    /// Source client for pulling blobs.
    source_client: Arc<RegistryClient>,
    /// Target registry name (dedup key).
    target_name: RegistryAlias,
    /// Target client for pushing.
    target_client: Arc<RegistryClient>,
    /// Source image reference (repo + tag, for display/logging and blob pulls).
    source: ImageRef,
    /// Target image reference (repo + tag, for pushing manifests and blobs).
    target: ImageRef,
    /// Optional batch blob checker for pre-populating the cache.
    batch_checker: Option<Rc<dyn BatchBlobChecker>>,
}

/// Discovery outcome for one (mapping, tag) pair.
enum DiscoveryOutcome {
    /// All targets match; nothing to do. Contains one `ImageResult` per target.
    Skip(Vec<ImageResult>),
    /// Some targets need sync. Blob digests for frequency tracking are
    /// extracted from the shared source data in the first item.
    Active {
        /// One per target that needs sync.
        items: Vec<TransferTask>,
        /// Results for targets that were skipped (digest match).
        skipped: Vec<ImageResult>,
    },
    /// Source pull failed. Contains failure results for all targets.
    Failed(Vec<ImageResult>),
}

/// Signal from `discover_tag()` indicating which discovery route was taken.
///
/// Used by the pipeline loop to accumulate per-route counters for
/// `SyncStats` observability without polluting `DiscoveryOutcome`.
enum DiscoveryRoute {
    /// Source HEAD matched cache, no full source pull needed.
    CacheHit,
    /// Full source manifest pull was required (cold cache, source changed, config changed).
    CacheMiss,
    /// Source HEAD failed (network error, timeout, bad digest).
    HeadFailure,
    /// Source cache matched but target HEAD mismatch forced full pull.
    TargetStale,
}

/// Blob frequency tracker for ordering blobs by popularity.
///
/// Blobs shared across many images should be transferred first so the cache
/// benefits the most follow-on images.
struct BlobFrequencyMap {
    counts: HashMap<Digest, usize>,
}

impl BlobFrequencyMap {
    /// Create an empty frequency map.
    fn new() -> Self {
        Self {
            counts: HashMap::new(),
        }
    }

    /// Record a blob digest occurrence.
    fn record(&mut self, digest: &Digest) {
        *self.counts.entry(digest.clone()).or_insert(0) += 1;
    }

    /// Return the frequency count for a digest (0 if unknown).
    fn count(&self, digest: &Digest) -> usize {
        self.counts.get(digest).copied().unwrap_or(0)
    }
}

/// Default cap for concurrent image transfers (Level 1: global image semaphore).
pub const DEFAULT_MAX_CONCURRENT_TRANSFERS: usize = 50;

/// Default shutdown drain deadline in seconds.
const DEFAULT_DRAIN_DEADLINE_SECS: u64 = 25;

/// Sync engine -- orchestrates concurrent image transfers across registries.
///
/// Uses a pipelined architecture where discovery and execution overlap via
/// `tokio::select!`. The plan phase is eliminated entirely -- progressive
/// cache population replaces upfront batch HEAD checks.
#[derive(Debug)]
pub struct SyncEngine {
    retry: RetryConfig,
    max_concurrent: usize,
    drain_deadline: Duration,
    /// Timeout for the optimization source HEAD request. Default: 5 seconds.
    source_head_timeout: Duration,
}

impl SyncEngine {
    /// Create a new sync engine with the given retry configuration and concurrency cap.
    ///
    /// `max_concurrent` bounds the number of in-flight image sync futures in the
    /// execute phase (default recommendation: 50).
    pub fn new(retry: RetryConfig, max_concurrent: usize) -> Self {
        Self {
            retry,
            max_concurrent,
            drain_deadline: Duration::from_secs(DEFAULT_DRAIN_DEADLINE_SECS),
            source_head_timeout: Duration::from_secs(5),
        }
    }

    /// Set the maximum time to wait for in-flight transfers to complete after
    /// a shutdown signal is received. Defaults to 25 seconds.
    pub fn with_drain_deadline(mut self, deadline: Duration) -> Self {
        self.drain_deadline = deadline;
        self
    }

    /// Set the timeout for the optimization source HEAD request.
    ///
    /// Default: 5 seconds. If the HEAD doesn't complete within this duration,
    /// the engine falls through to the full source manifest pull.
    pub fn with_source_head_timeout(mut self, timeout: Duration) -> Self {
        self.source_head_timeout = timeout;
        self
    }

    /// Run the sync engine across all resolved mappings.
    ///
    /// Orchestrates the pipelined loop: discovery futures feed active items into
    /// a pending queue, execution futures drain them bounded by a semaphore.
    /// Progress reporting happens as results arrive from either phase.
    ///
    /// `staging` enables disk-based blob reuse for multi-target mappings. Pass
    /// [`BlobStage::disabled`] for single-target deployments to pay zero overhead.
    ///
    /// If `shutdown` is `Some`, the engine will stop accepting new work when the
    /// signal fires, drain in-flight transfers up to the configured drain deadline
    /// (default: 25s, configurable via [`with_drain_deadline`](Self::with_drain_deadline)),
    /// then return. Pass `None` to run to completion without shutdown handling.
    pub async fn run(
        &self,
        mappings: Vec<ResolvedMapping>,
        cache: Rc<RefCell<TransferStateCache>>,
        staging: BlobStage,
        progress: &dyn crate::progress::ProgressReporter,
        shutdown: Option<&ShutdownSignal>,
    ) -> SyncReport {
        let run_start = Instant::now();
        let run_id = Uuid::now_v7();

        let mut discovery_futures = FuturesUnordered::new();
        let mut execution_futures: FuturesUnordered<
            std::pin::Pin<Box<dyn Future<Output = ImageResult>>>,
        > = FuturesUnordered::new();
        let mut pending: VecDeque<TransferTask> = VecDeque::new();
        let mut freq_map = BlobFrequencyMap::new();
        let global_sem = Rc::new(Semaphore::new(self.max_concurrent));
        let staging = Rc::new(staging);
        let mut results: Vec<ImageResult> = Vec::new();
        let mut shutting_down = false;
        let mut drain_deadline: Option<tokio::time::Instant> = None;
        let mut discovery_hits: u64 = 0;
        let mut discovery_misses: u64 = 0;
        let mut discovery_head_failures: u64 = 0;
        let mut discovery_target_stale: u64 = 0;

        // Seed discovery with all (mapping, tag) pairs.
        for mapping in &mappings {
            for tag_pair in &mapping.tags {
                let params = DiscoveryParams {
                    source_client: Arc::clone(&mapping.source_client),
                    source_authority: mapping.source_authority.clone(),
                    source: ImageRef {
                        repo: mapping.source_repo.clone(),
                        tag: tag_pair.source.clone(),
                    },
                    target: ImageRef {
                        repo: mapping.target_repo.clone(),
                        tag: tag_pair.target.clone(),
                    },
                    targets: mapping.targets.clone(),
                    retry: self.retry.clone(),
                    platforms: mapping.platforms.clone(),
                    skip_existing: mapping.skip_existing,
                    cache: Rc::clone(&cache),
                    source_head_timeout: self.source_head_timeout,
                };

                discovery_futures.push(async move { discover_tag(params).await });
            }
        }

        loop {
            // Promote pending items to execution futures only when not shutting down.
            // Each future acquires a semaphore permit at the start (blocking if at
            // capacity), preserving the discovery-order benefit of the frequency map.
            if !shutting_down {
                while let Some(item) = pending.pop_front() {
                    let sem = Rc::clone(&global_sem);
                    let cache_ref = Rc::clone(&cache);
                    let retry = self.retry.clone();
                    let freq_counts: HashMap<Digest, usize> = {
                        let blobs = blobs_from_manifest(&item.source_data);
                        blobs
                            .iter()
                            .map(|d| (d.digest.clone(), freq_map.count(&d.digest)))
                            .collect()
                    };

                    let staging_ref = Rc::clone(&staging);
                    let source_str = item.source.to_string();
                    let target_str = item.target.to_string();

                    execution_futures.push(Box::pin(async move {
                        let _permit = sem.acquire().await.unwrap();
                        progress.image_started(&source_str, &target_str);
                        execute_item(item, &cache_ref, &staging_ref, &freq_counts, &retry).await
                    }));
                }
            }

            tokio::select! {
                biased;
                Some(result) = execution_futures.next(), if !execution_futures.is_empty() => {
                    progress.image_completed(&result);
                    results.push(result);
                }
                _ = async {
                    // Guard above ensures shutdown.is_some(); unwrap cannot panic.
                    shutdown.unwrap().notified().await
                }, if shutdown.is_some()
                    && !shutting_down
                    // Disable when all work is done — otherwise this branch
                    // blocks the `else` exit path indefinitely.
                    && !(discovery_futures.is_empty()
                        && execution_futures.is_empty()
                        && pending.is_empty()) =>
                {
                    shutting_down = true;
                    drain_deadline = Some(
                        tokio::time::Instant::now() + self.drain_deadline,
                    );
                    tracing::info!(
                        in_flight = execution_futures.len(),
                        "shutdown signal received, draining in-flight transfers"
                    );
                }
                _ = async {
                    // Guard above ensures drain_deadline.is_some(); unwrap cannot panic.
                    tokio::time::sleep_until(drain_deadline.unwrap()).await
                }, if drain_deadline.is_some() && !execution_futures.is_empty() => {
                    tracing::warn!(
                        remaining = execution_futures.len(),
                        "drain deadline reached, abandoning in-flight transfers"
                    );
                    break;
                }
                Some((outcome, route)) = discovery_futures.next(),
                    if !shutting_down && !discovery_futures.is_empty() =>
                {
                    // Accumulate discovery route counters.
                    match route {
                        DiscoveryRoute::CacheHit => discovery_hits += 1,
                        DiscoveryRoute::CacheMiss => discovery_misses += 1,
                        DiscoveryRoute::HeadFailure => {
                            discovery_misses += 1;
                            discovery_head_failures += 1;
                        }
                        DiscoveryRoute::TargetStale => {
                            discovery_misses += 1;
                            discovery_target_stale += 1;
                        }
                    }
                    match outcome {
                        DiscoveryOutcome::Skip(skip_results) => {
                            for r in &skip_results {
                                progress.image_completed(r);
                            }
                            results.extend(skip_results);
                        }
                        DiscoveryOutcome::Active { items, skipped } => {
                            // Record blob frequencies from the shared source data.
                            // All items share the same Rc<PulledManifest> (pulled once per tag).
                            if let Some(first) = items.first() {
                                for blob in blobs_from_manifest(&first.source_data) {
                                    freq_map.record(&blob.digest);
                                }
                            }
                            for r in &skipped {
                                progress.image_completed(r);
                            }
                            results.extend(skipped);
                            pending.extend(items);
                        }
                        DiscoveryOutcome::Failed(fail_results) => {
                            for r in &fail_results {
                                progress.image_completed(r);
                            }
                            results.extend(fail_results);
                        }
                    }
                }
                else => break,
            }
        }

        // Prune snapshot entries for tags no longer in the mapping set.
        // Prevents unbounded cache growth when source tags are deleted.
        {
            let live_keys: std::collections::HashSet<SnapshotKey> = mappings
                .iter()
                .flat_map(|m| {
                    m.tags
                        .iter()
                        .map(|t| SnapshotKey::new(&m.source_authority, &m.source_repo, &t.source))
                })
                .collect();
            cache.borrow_mut().prune_snapshots(&live_keys);
        }

        let mut stats = compute_stats(&results);
        stats.discovery_cache_hits = discovery_hits;
        stats.discovery_cache_misses = discovery_misses;
        stats.discovery_head_failures = discovery_head_failures;
        stats.discovery_target_stale = discovery_target_stale;
        let duration = run_start.elapsed();

        let report = SyncReport {
            run_id,
            images: results,
            stats,
            duration,
        };

        progress.run_completed(&report);
        report
    }
}

/// Parameters for [`discover_tag`], bundled to keep the argument count under
/// clippy's limit while preserving readability.
///
/// Owns all data so it can be moved directly into the async discovery future
/// without intermediate borrows.
struct DiscoveryParams {
    source_client: Arc<RegistryClient>,
    source_authority: RegistryAuthority,
    source: ImageRef,
    target: ImageRef,
    targets: Vec<TargetEntry>,
    retry: RetryConfig,
    /// Optional platform filter list (e.g., `["linux/amd64"]`).
    platforms: Option<Vec<PlatformFilter>>,
    /// When `true`, skip syncing when the target already has any manifest.
    skip_existing: bool,
    cache: Rc<RefCell<TransferStateCache>>,
    source_head_timeout: Duration,
}

/// Discover a single (mapping, tag) pair: HEAD source, check cache, pull if needed.
///
/// Returns `(DiscoveryOutcome, DiscoveryRoute)` — the transfer outcome and a
/// signal for which discovery path was taken (cache hit, miss, head failure, or
/// stale).
///
/// The optimization flow:
/// 1. HEAD source manifest with a short timeout
/// 2. If HEAD succeeds, look up the tag digest cache
/// 3. If cache hit (same source digest + platform key), HEAD-check all targets
///    against the cached filtered digest
/// 4. If all targets match, skip entirely (`CacheHit`)
/// 5. If any target mismatches, full-pull for mismatched targets only (`TargetStale`)
/// 6. If cache miss or HEAD failed, full-pull all targets (`CacheMiss` / `HeadFailure`)
///
/// When `platforms` is `Some`, index manifests are filtered to only include
/// descriptors matching the given platform filters before pulling children.
///
/// When `skip_existing` is `true`, targets that return any manifest HEAD response
/// (regardless of digest) are skipped without further comparison.
async fn discover_tag(params: DiscoveryParams) -> (DiscoveryOutcome, DiscoveryRoute) {
    let DiscoveryParams {
        source_client,
        source_authority,
        source,
        target,
        targets,
        retry,
        platforms,
        skip_existing,
        cache,
        source_head_timeout,
    } = params;

    let platform_key = PlatformFilterKey::from_filters(platforms.as_deref());

    // --- Step 1: HEAD source manifest with short timeout ---
    let source_head_digest = match tokio::time::timeout(
        source_head_timeout,
        source_client.manifest_head(&source.repo, &source.tag),
    )
    .await
    {
        Ok(Ok(Some(head))) => Some(head.digest),
        Ok(Ok(None)) => {
            debug!(
                repo = %source.repo, tag = %source.tag,
                "source HEAD 404, falling through to full pull"
            );
            None
        }
        Ok(Err(e)) => {
            debug!(
                repo = %source.repo, tag = %source.tag, error = %e,
                "source HEAD failed, falling through to full pull"
            );
            None
        }
        Err(_) => {
            debug!(
                repo = %source.repo, tag = %source.tag,
                "source HEAD timed out, falling through to full pull"
            );
            None
        }
    };

    // --- Steps 2-5: Cache lookup (only if HEAD succeeded) ---
    let snapshot_key = SnapshotKey::new(&source_authority, &source.repo, &source.tag);
    if let Some(ref head_digest) = source_head_digest {
        // Scoped borrow -- dropped before any .await
        let cached = {
            let c = cache.borrow();
            c.source_snapshot(&snapshot_key).cloned()
        };

        if let Some(snapshot) = cached {
            if *head_digest == snapshot.source_digest
                && platform_key == snapshot.platform_filter_key
            {
                // Source unchanged, config unchanged. Check targets against
                // the cached filtered_digest.
                let compare_digest = snapshot.filtered_digest;

                // HEAD-check all targets concurrently.
                let mut head_checks = FuturesUnordered::new();
                for entry in &targets {
                    let client = Arc::clone(&entry.client);
                    let repo = target.repo.clone();
                    let tag = target.tag.clone();
                    let name = entry.name.clone();
                    let checker = entry.batch_checker.clone();
                    head_checks.push(async move {
                        let result = client.manifest_head(&repo, &tag).await;
                        (name, client, checker, result)
                    });
                }

                let mut skipped_results = Vec::new();
                let mut mismatched_targets = Vec::new();

                while let Some((target_name, target_client, batch_checker, result)) =
                    head_checks.next().await
                {
                    match result {
                        Ok(Some(_)) if skip_existing => {
                            info!(
                                source_repo = %source.repo,
                                source_tag = %source.tag,
                                target_repo = %target.repo,
                                "skipping -- target manifest exists (skip_existing)"
                            );
                            tracing::debug!(target: "ocync::metrics", "skip_existing");
                            skipped_results.push(skip_image_result(
                                &source,
                                &target,
                                SkipReason::SkipExisting,
                            ));
                        }
                        Ok(Some(head)) if head.digest == compare_digest => {
                            info!(
                                source_repo = %source.repo,
                                source_tag = %source.tag,
                                target_repo = %target.repo,
                                digest = %compare_digest,
                                "skipping -- digest matches at target (cache hit)"
                            );
                            tracing::debug!(target: "ocync::metrics", "unchanged_skip");
                            skipped_results.push(skip_image_result(
                                &source,
                                &target,
                                SkipReason::DigestMatch,
                            ));
                        }
                        other => {
                            if let Err(e) = &other {
                                warn!(
                                    target_repo = %target.repo,
                                    target_tag = %target.tag,
                                    error = %e,
                                    "target manifest HEAD failed, proceeding with sync"
                                );
                            }
                            mismatched_targets.push(TargetEntry {
                                name: target_name,
                                client: target_client,
                                batch_checker,
                            });
                        }
                    }
                }

                if mismatched_targets.is_empty() {
                    // All targets match -- skip entirely (preserve existing cache entry).
                    return (
                        DiscoveryOutcome::Skip(skipped_results),
                        DiscoveryRoute::CacheHit,
                    );
                }

                // Some targets stale -- need full pull for those targets.
                let outcome = full_pull_and_build_tasks(FullPullParams {
                    source_client: &source_client,
                    source: &source,
                    target: &target,
                    targets: &mismatched_targets,
                    skipped_results,
                    retry: &retry,
                    platforms: platforms.as_deref(),
                    skip_existing,
                    cache: &cache,
                    snapshot_key: &snapshot_key,
                    head_digest: Some(head_digest),
                    platform_key: &platform_key,
                })
                .await;

                return (outcome, DiscoveryRoute::TargetStale);
            }
            // Cache entry exists but source changed or config changed -- fall through.
        }
        // No cache entry -- fall through.
    }

    // --- Full pull path (cache miss, HEAD failed, or source changed) ---
    let route = if source_head_digest.is_none() {
        DiscoveryRoute::HeadFailure
    } else {
        DiscoveryRoute::CacheMiss
    };

    let outcome = full_pull_and_build_tasks(FullPullParams {
        source_client: &source_client,
        source: &source,
        target: &target,
        targets: &targets,
        skipped_results: Vec::new(),
        retry: &retry,
        platforms: platforms.as_deref(),
        skip_existing,
        cache: &cache,
        snapshot_key: &snapshot_key,
        head_digest: source_head_digest.as_ref(),
        platform_key: &platform_key,
    })
    .await;

    (outcome, route)
}

/// Parameters for [`full_pull_and_build_tasks`], bundled to keep the argument
/// count under clippy's limit.
struct FullPullParams<'a> {
    source_client: &'a Arc<RegistryClient>,
    source: &'a ImageRef,
    target: &'a ImageRef,
    targets: &'a [TargetEntry],
    skipped_results: Vec<ImageResult>,
    retry: &'a RetryConfig,
    platforms: Option<&'a [PlatformFilter]>,
    skip_existing: bool,
    cache: &'a Rc<RefCell<TransferStateCache>>,
    snapshot_key: &'a SnapshotKey,
    head_digest: Option<&'a Digest>,
    platform_key: &'a PlatformFilterKey,
}

/// Full discovery: pull source manifest, HEAD-check targets, build transfer tasks.
///
/// Updates the tag digest cache on successful pull. Used by both the cache-miss
/// path and the target-stale path (where only mismatched targets need tasks).
async fn full_pull_and_build_tasks(params: FullPullParams<'_>) -> DiscoveryOutcome {
    let FullPullParams {
        source_client,
        source,
        target,
        targets,
        mut skipped_results,
        retry,
        platforms,
        skip_existing,
        cache,
        snapshot_key,
        head_digest,
        platform_key,
    } = params;
    // Pull source manifest (shared across all targets for this tag).
    let source_data = match pull_source_manifest(
        source_client,
        &source.repo,
        &source.tag,
        retry,
        platforms,
    )
    .await
    {
        Ok(data) => Rc::new(data),
        Err(err) => {
            let error_str = err.to_string();
            warn!(
                source_repo = %source.repo,
                source_tag = %source.tag,
                error = %error_str,
                "source pull failed, skipping all targets"
            );
            let fail_results: Vec<ImageResult> = targets
                .iter()
                .map(|t| ImageResult {
                    image_id: Uuid::now_v7(),
                    source: source.to_string(),
                    target: format!("{} ({}):{}", target.repo, t.name, target.tag),
                    status: ImageStatus::Failed {
                        kind: ErrorKind::ManifestPull,
                        error: error_str.clone(),
                        retries: retry.max_retries,
                    },
                    bytes_transferred: 0,
                    blob_stats: BlobTransferStats::default(),
                    duration: Duration::ZERO,
                })
                .collect();
            // Do NOT update cache on pull failure.
            return DiscoveryOutcome::Failed(fail_results);
        }
    };

    // Update cache with fresh source data (regardless of target outcomes).
    if let Some(hd) = head_digest {
        let snapshot = SourceSnapshot {
            source_digest: hd.clone(),
            filtered_digest: source_data.pull.digest.clone(),
            platform_filter_key: platform_key.clone(),
        };
        cache
            .borrow_mut()
            .set_source_snapshot(snapshot_key.clone(), snapshot);
    } // borrow dropped before any .await

    let source_digest = &source_data.pull.digest;

    // HEAD-check all targets concurrently.
    let mut head_checks = FuturesUnordered::new();
    for entry in targets {
        let client = Arc::clone(&entry.client);
        let repo = target.repo.clone();
        let tag = target.tag.clone();
        let name = entry.name.clone();
        let checker = entry.batch_checker.clone();
        head_checks.push(async move {
            let result = client.manifest_head(&repo, &tag).await;
            (name, client, checker, result)
        });
    }

    let mut active_items = Vec::new();

    while let Some((target_name, target_client, batch_checker, result)) = head_checks.next().await {
        match result {
            Ok(Some(_)) if skip_existing => {
                info!(
                    source_repo = %source.repo,
                    source_tag = %source.tag,
                    target_repo = %target.repo,
                    "skipping -- target manifest exists (skip_existing)"
                );
                tracing::debug!(target: "ocync::metrics", "skip_existing");
                skipped_results.push(skip_image_result(source, target, SkipReason::SkipExisting));
            }
            Ok(Some(head)) if head.digest == *source_digest => {
                info!(
                    source_repo = %source.repo,
                    source_tag = %source.tag,
                    target_repo = %target.repo,
                    digest = %source_digest,
                    "skipping -- digest matches at target"
                );
                tracing::debug!(target: "ocync::metrics", "unchanged_skip");
                skipped_results.push(skip_image_result(source, target, SkipReason::DigestMatch));
            }
            other => {
                if let Err(e) = &other {
                    warn!(
                        target_repo = %target.repo,
                        target_tag = %target.tag,
                        error = %e,
                        "target manifest HEAD failed, proceeding with sync"
                    );
                }
                active_items.push(TransferTask {
                    source_data: Rc::clone(&source_data),
                    source_client: Arc::clone(source_client),
                    target_name,
                    target_client,
                    source: source.clone(),
                    target: target.clone(),
                    batch_checker,
                });
            }
        }
    }

    if active_items.is_empty() {
        return DiscoveryOutcome::Skip(skipped_results);
    }

    DiscoveryOutcome::Active {
        items: active_items,
        skipped: skipped_results,
    }
}

/// Execute a single active item: transfer blobs, push manifests.
async fn execute_item(
    item: TransferTask,
    cache: &Rc<RefCell<TransferStateCache>>,
    staging: &Rc<BlobStage>,
    freq_counts: &HashMap<Digest, usize>,
    retry: &RetryConfig,
) -> ImageResult {
    let start = Instant::now();

    let ctx = TransferContext {
        cache,
        staging,
        retry,
        source_client: &item.source_client,
        source_repo: &item.source.repo,
        target_client: &item.target_client,
        target_name: &item.target_name,
        target_repo: &item.target.repo,
        batch_checker: item.batch_checker.as_ref(),
    };
    let outcome = transfer_image_blobs(&ctx, &item.source_data, freq_counts).await;

    if let Some(err) = outcome.error {
        warn!(target_name = %item.target_name, error = %err, "blob transfer failed");
        return ImageResult {
            image_id: Uuid::now_v7(),
            source: item.source.to_string(),
            target: item.target.to_string(),
            status: ImageStatus::Failed {
                kind: ErrorKind::BlobTransfer,
                error: err.to_string(),
                retries: retry.max_retries,
            },
            bytes_transferred: outcome.bytes_transferred,
            blob_stats: outcome.stats,
            duration: start.elapsed(),
        };
    }

    // Push manifests (children first, then top-level by tag).
    match push_manifests(
        retry,
        &item.target_client,
        &item.target.repo,
        &item.target.tag,
        &item.source_data,
    )
    .await
    {
        Ok(()) => {
            info!(
                source_repo = %item.source.repo,
                source_tag = %item.source.tag,
                target_repo = %item.target.repo,
                target_tag = %item.target.tag,
                bytes = outcome.bytes_transferred,
                "image synced"
            );
            ImageResult {
                image_id: Uuid::now_v7(),
                source: item.source.to_string(),
                target: item.target.to_string(),
                status: ImageStatus::Synced,
                bytes_transferred: outcome.bytes_transferred,
                blob_stats: outcome.stats,
                duration: start.elapsed(),
            }
        }
        Err(err) => {
            if is_immutable_tag_error(&err) {
                info!(
                    source_repo = %item.source.repo,
                    target_repo = %item.target.repo,
                    target_tag = %item.target.tag,
                    "target tag is immutable, skipping"
                );
                ImageResult {
                    image_id: Uuid::now_v7(),
                    source: item.source.to_string(),
                    target: item.target.to_string(),
                    status: ImageStatus::Skipped {
                        reason: SkipReason::ImmutableTag,
                    },
                    bytes_transferred: outcome.bytes_transferred,
                    blob_stats: outcome.stats,
                    duration: start.elapsed(),
                }
            } else {
                warn!(target_name = %item.target_name, error = %err, "manifest push failed");
                ImageResult {
                    image_id: Uuid::now_v7(),
                    source: item.source.to_string(),
                    target: item.target.to_string(),
                    status: ImageStatus::Failed {
                        kind: ErrorKind::ManifestPush,
                        error: err.to_string(),
                        retries: retry.max_retries,
                    },
                    bytes_transferred: outcome.bytes_transferred,
                    blob_stats: outcome.stats,
                    duration: start.elapsed(),
                }
            }
        }
    }
}

/// Pull all source manifest data for a single tag.
///
/// For image manifests, returns just the manifest. For index manifests,
/// also pulls all child manifests.
///
/// When `platforms` is `Some`, index manifests are filtered to only include
/// descriptors whose platform matches one of the filter strings. Only
/// matching child manifests are pulled, and the index's `raw_bytes` are
/// re-serialized to reflect only the kept descriptors.
async fn pull_source_manifest(
    client: &RegistryClient,
    repo: &RepositoryName,
    tag: &str,
    retry: &RetryConfig,
    platforms: Option<&[PlatformFilter]>,
) -> Result<PulledManifest, crate::Error> {
    let pull = with_retry(retry, "manifest pull", || client.manifest_pull(repo, tag))
        .await
        .map_err(|e| crate::Error::Manifest {
            reference: tag.to_owned(),
            source: e,
        })?;

    let children = match &pull.manifest {
        ManifestKind::Image(_) => Vec::new(),
        ManifestKind::Index(index) => {
            // When platform filters are active, only pull children for matching platforms.
            let descriptors: Vec<&Descriptor> = if let Some(filters) = platforms {
                let total = index.manifests.len();
                let kept: Vec<&Descriptor> = index
                    .manifests
                    .iter()
                    .filter(|desc| {
                        desc.platform
                            .as_ref()
                            .is_some_and(|p| filters.iter().any(|f| p.matches(f)))
                    })
                    .collect();

                if !index.manifests.is_empty() && kept.is_empty() {
                    let available: Vec<String> = index
                        .manifests
                        .iter()
                        .filter_map(|d| d.platform.as_ref())
                        .map(|p| {
                            if let Some(ref v) = p.variant {
                                format!("{}/{}/{v}", p.os, p.architecture)
                            } else {
                                format!("{}/{}", p.os, p.architecture)
                            }
                        })
                        .collect();
                    let filter_strs: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
                    return Err(crate::Error::Manifest {
                        reference: tag.to_owned(),
                        source: ocync_distribution::Error::Other(format!(
                            "platform filter [{}] matched no manifests in index (source has: [{}])",
                            filter_strs.join(", "),
                            available.join(", "),
                        )),
                    });
                }

                info!(
                    repo = %repo,
                    tag = %tag,
                    kept = kept.len(),
                    total = total,
                    "filtered index: {}/{} platforms",
                    kept.len(),
                    total,
                );
                kept
            } else {
                index.manifests.iter().collect()
            };

            let mut children = Vec::with_capacity(descriptors.len());
            for child_desc in &descriptors {
                let child_digest_str = child_desc.digest.to_string();
                let child_pull = with_retry(retry, "manifest pull", || {
                    client.manifest_pull(repo, &child_digest_str)
                })
                .await
                .map_err(|e| crate::Error::Manifest {
                    reference: child_digest_str.clone(),
                    source: e,
                })?;

                match &child_pull.manifest {
                    ManifestKind::Image(_) => {
                        children.push(child_pull);
                    }
                    ManifestKind::Index(_) => {
                        return Err(crate::Error::Manifest {
                            reference: child_digest_str,
                            source: ocync_distribution::Error::Other(
                                "nested index manifests are not supported".into(),
                            ),
                        });
                    }
                }
            }

            // When platform filtering is active, rebuild the index manifest with
            // only the matching descriptors. The raw_bytes and digest must be
            // recomputed so targets receive the filtered index.
            if platforms.is_some() && descriptors.len() != index.manifests.len() {
                let filtered_index = ImageIndex {
                    schema_version: index.schema_version,
                    media_type: index.media_type.clone(),
                    manifests: descriptors.into_iter().cloned().collect(),
                    subject: index.subject.clone(),
                    artifact_type: index.artifact_type.clone(),
                    annotations: index.annotations.clone(),
                };
                let new_bytes =
                    serde_json::to_vec(&filtered_index).map_err(|e| crate::Error::Manifest {
                        reference: tag.to_owned(),
                        source: ocync_distribution::Error::Other(format!(
                            "failed to serialize filtered index: {e}"
                        )),
                    })?;
                let new_digest = Digest::from_sha256(Sha256::digest(&new_bytes));
                let filtered_pull = ManifestPull {
                    manifest: ManifestKind::Index(Box::new(filtered_index)),
                    raw_bytes: new_bytes,
                    media_type: pull.media_type,
                    digest: new_digest,
                };
                return Ok(PulledManifest {
                    pull: filtered_pull,
                    children,
                });
            }

            children
        }
    };

    Ok(PulledManifest { pull, children })
}

/// Extract all blob descriptors from source data (handles both image and index).
fn blobs_from_manifest(source_data: &PulledManifest) -> Vec<&Descriptor> {
    match &source_data.pull.manifest {
        ManifestKind::Image(m) => collect_image_blobs(m),
        ManifestKind::Index(_) => source_data
            .children
            .iter()
            .flat_map(|c| match &c.manifest {
                ManifestKind::Image(m) => collect_image_blobs(m),
                ManifestKind::Index(_) => Vec::new(),
            })
            .collect(),
    }
}

/// Context for transferring blobs from source to a single target.
///
/// Bundles the parameters needed by [`transfer_image_blobs`] to keep
/// the function signature under the clippy argument limit.
struct TransferContext<'a> {
    cache: &'a Rc<RefCell<TransferStateCache>>,
    /// Shared staging area for pull-once, fan-out multi-target transfers.
    staging: &'a Rc<BlobStage>,
    retry: &'a RetryConfig,
    source_client: &'a RegistryClient,
    source_repo: &'a RepositoryName,
    target_client: &'a RegistryClient,
    target_name: &'a RegistryAlias,
    target_repo: &'a RepositoryName,
    /// Optional batch blob checker for pre-populating the cache.
    batch_checker: Option<&'a Rc<dyn BatchBlobChecker>>,
}

/// Transfer all blobs for a single image to one target.
///
/// Uses progressive cache population: checks cache first, then tries cross-repo
/// mount, then HEAD check at target, and finally falls back to pull+push.
/// Blobs are sorted by descending frequency so the most-shared blobs populate
/// the cache first.
async fn transfer_image_blobs(
    ctx: &TransferContext<'_>,
    source_data: &PulledManifest,
    freq_counts: &HashMap<Digest, usize>,
) -> TargetBlobOutcome {
    let mut blobs: Vec<&Descriptor> = blobs_from_manifest(source_data);

    // Sort by descending frequency (most-shared first for maximum cache benefit).
    // Rust's sort_by is stable, so blobs with equal frequency preserve their
    // original manifest order (config first, then layers).
    blobs.sort_by(|a, b| {
        let fa = freq_counts.get(&a.digest).copied().unwrap_or(0);
        let fb = freq_counts.get(&b.digest).copied().unwrap_or(0);
        fb.cmp(&fa)
    });

    // Batch existence check: when a batch checker is available, check all
    // blobs upfront in a single API call and pre-populate the cache. The
    // per-blob loop then hits cache at Step 1 for existing blobs (skipping
    // the per-blob HEAD at Step 3 entirely).
    if let Some(checker) = ctx.batch_checker {
        let all_digests: Vec<Digest> = blobs.iter().map(|b| b.digest.clone()).collect();
        match checker
            .check_blob_existence(ctx.target_repo, &all_digests)
            .await
        {
            Ok(existing) => {
                let existing_count = existing.len();
                for digest in &existing {
                    ctx.cache.borrow_mut().set_blob_exists(
                        ctx.target_name,
                        digest.clone(),
                        ctx.target_repo.to_owned(),
                    );
                }
                debug!(
                    target_name = %ctx.target_name,
                    repo = %ctx.target_repo,
                    total = all_digests.len(),
                    existing = existing_count,
                    "batch check pre-populated cache"
                );
            }
            Err(e) => {
                warn!(
                    target_name = %ctx.target_name,
                    error = %e,
                    "batch check failed, falling back to per-blob HEAD"
                );
                // Graceful degradation: continue with per-blob HEAD checks.
            }
        }
    }

    let mut outcome = TargetBlobOutcome::default();

    for blob in &blobs {
        let digest = &blob.digest;
        let size = blob.size;

        // Step 1: Check cache -- known at this repo -> skip (0 API calls).
        // OCI blobs are repo-scoped: a blob at repo-a is NOT accessible from
        // repo-b without a mount. So we must check the specific repo.
        let skip = {
            let c = ctx.cache.borrow();
            c.blob_known_at_repo(ctx.target_name, digest, ctx.target_repo)
        };

        if skip {
            tracing::debug!(target: "ocync::metrics", tier = "hot", "cache_hit");
            outcome.stats.skipped += 1;
            continue;
        }

        // Step 2: Check cache for cross-repo mount source.
        let mount_source = {
            let c = ctx.cache.borrow();
            c.blob_mount_source(ctx.target_name, digest, ctx.target_repo)
                .cloned()
        };

        if let Some(from_repo) = mount_source {
            debug!(%digest, %from_repo, target = %ctx.target_name, "attempting mount");
            match ctx
                .target_client
                .blob_mount(ctx.target_repo, digest, &from_repo)
                .await
            {
                Ok(MountResult::Mounted) => {
                    ctx.cache.borrow_mut().set_blob_completed(
                        ctx.target_name,
                        digest.clone(),
                        ctx.target_repo.to_owned(),
                    );
                    outcome.stats.mounted += 1;
                    tracing::debug!(target: "ocync::metrics", result = "success", "mount");
                    continue;
                }
                Ok(MountResult::FallbackUpload { .. }) | Err(_) => {
                    debug!(%digest, target = %ctx.target_name, "mount failed, falling back to HEAD+push");
                    // Invalidate the stale mount source entry.
                    ctx.cache
                        .borrow_mut()
                        .invalidate_blob(ctx.target_name, digest);
                    tracing::debug!(target: "ocync::metrics", result = "fallback", "mount");
                    tracing::debug!(target: "ocync::metrics", "cache_invalidation");
                }
            }
        }

        // Step 3: HEAD check at target (1 API call), record in cache if exists.
        let head_result = ctx.target_client.blob_exists(ctx.target_repo, digest).await;
        match head_result {
            Ok(Some(_)) => {
                ctx.cache.borrow_mut().set_blob_exists(
                    ctx.target_name,
                    digest.clone(),
                    ctx.target_repo.to_owned(),
                );
                outcome.stats.skipped += 1;
                continue;
            }
            Ok(None) => {
                // Blob doesn't exist, proceed to pull+push.
            }
            Err(e) => {
                debug!(
                    %digest,
                    target = %ctx.target_name,
                    error = %e,
                    "blob HEAD failed, proceeding with push"
                );
            }
        }

        // Step 4: Pull from source + push to target, record in cache.
        // When staging is enabled, pull-once semantics apply: if the blob is
        // already staged from a previous target, push from the local file
        // instead of re-pulling from source. Otherwise pull from source and
        // write to staging so subsequent targets can reuse it.
        ctx.cache
            .borrow_mut()
            .set_blob_in_progress(ctx.target_name, digest.clone());

        let transfer_result: Result<(), crate::Error> = if ctx.staging.is_enabled() {
            if !ctx.staging.exists(digest) {
                // Pull from source, stream to staging file chunk-by-chunk.
                // This avoids buffering the entire blob in memory during pull.
                if let Err(e) = with_retry(ctx.retry, "blob pull (to stage)", || async {
                    let mut writer = ctx.staging.begin_write(digest).map_err(|e| {
                        ocync_distribution::Error::Other(format!("staging create: {e}"))
                    })?;
                    let stream = ctx.source_client.blob_pull(ctx.source_repo, digest).await?;
                    futures_util::pin_mut!(stream);
                    while let Some(chunk) = stream.next().await {
                        let chunk =
                            chunk.map_err(|e| ocync_distribution::Error::Other(e.to_string()))?;
                        writer.write_chunk(&chunk).map_err(|e| {
                            ocync_distribution::Error::Other(format!("staging write: {e}"))
                        })?;
                    }
                    writer.finish().map_err(|e| {
                        ocync_distribution::Error::Other(format!("staging finalize: {e}"))
                    })?;
                    Ok::<(), ocync_distribution::Error>(())
                })
                .await
                {
                    let err = crate::Error::BlobTransfer {
                        digest: digest.clone(),
                        source: e,
                    };
                    ctx.cache.borrow_mut().set_blob_failed(
                        ctx.target_name,
                        digest.clone(),
                        err.to_string(),
                    );
                    outcome.error = Some(err);
                    return outcome;
                }
            }

            // Push from staged file, streaming from disk.
            with_retry(ctx.retry, "blob push (staged)", || async {
                let file = ctx
                    .staging
                    .open_read(digest)
                    .map_err(|e| ocync_distribution::Error::Other(format!("staging read: {e}")))?;
                let file_size = file.metadata().map(|m| m.len()).ok();
                let stream = file_read_stream(file).map(|r| {
                    r.map_err(|e| ocync_distribution::Error::Other(format!("staging read: {e}")))
                });
                ctx.target_client
                    .blob_push_stream(ctx.target_repo, digest, file_size, stream)
                    .await
            })
            .await
            .map(|_| ())
            .map_err(|e| crate::Error::BlobTransfer {
                digest: digest.clone(),
                source: e,
            })
        } else {
            with_retry(ctx.retry, "blob transfer", || async {
                let stream = ctx.source_client.blob_pull(ctx.source_repo, digest).await?;
                ctx.target_client
                    .blob_push_stream(ctx.target_repo, digest, Some(size), stream)
                    .await
            })
            .await
            .map(|_| ())
            .map_err(|e| crate::Error::BlobTransfer {
                digest: digest.clone(),
                source: e,
            })
        };

        match transfer_result {
            Ok(()) => {
                outcome.bytes_transferred += size;
                outcome.stats.transferred += 1;
                ctx.cache.borrow_mut().set_blob_completed(
                    ctx.target_name,
                    digest.clone(),
                    ctx.target_repo.to_owned(),
                );
            }
            Err(err) => {
                ctx.cache.borrow_mut().set_blob_failed(
                    ctx.target_name,
                    digest.clone(),
                    err.to_string(),
                );
                outcome.error = Some(err);
                return outcome; // Stop transferring blobs for this target on first failure.
            }
        }
    }

    outcome
}

/// Push all manifests (children for indexes, then top-level) to one target.
async fn push_manifests(
    retry: &RetryConfig,
    target_client: &RegistryClient,
    target_repo: &RepositoryName,
    target_tag: &str,
    source_data: &PulledManifest,
) -> Result<(), crate::Error> {
    // For index manifests, push each child by digest first.
    for child in &source_data.children {
        let child_digest_str = child.digest.to_string();
        with_retry(retry, "manifest push", || {
            target_client.manifest_push(
                target_repo,
                &child_digest_str,
                &child.media_type,
                &child.raw_bytes,
            )
        })
        .await
        .map_err(|e| crate::Error::Manifest {
            reference: child_digest_str.clone(),
            source: e,
        })?;
    }

    // Push top-level manifest by tag.
    with_retry(retry, "manifest push", || {
        target_client.manifest_push(
            target_repo,
            target_tag,
            &source_data.pull.media_type,
            &source_data.pull.raw_bytes,
        )
    })
    .await
    .map_err(|e| crate::Error::Manifest {
        reference: target_tag.to_owned(),
        source: e,
    })?;

    Ok(())
}

/// Read a file in 256 KB chunks, yielding a stream of `Bytes`.
///
/// Uses synchronous `std::fs::Read` internally. On a single-threaded tokio
/// runtime, each individual read call blocks for microseconds (local disk),
/// which is negligible compared to network RTT.
fn file_read_stream(
    file: std::fs::File,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    const CHUNK_SIZE: usize = 256 * 1024;
    let buf = vec![0u8; CHUNK_SIZE];
    futures_util::stream::unfold((file, buf), |(mut file, mut buf)| async move {
        use std::io::Read;
        match file.read(&mut buf) {
            Ok(0) => None,
            Ok(n) => Some((Ok(Bytes::copy_from_slice(&buf[..n])), (file, buf))),
            Err(e) => Some((Err(e), (file, buf))),
        }
    })
}

/// Retry an async operation with exponential backoff on transient HTTP errors.
///
/// Calls `f()` in a loop. If the result is `Err` with a retryable HTTP status
/// (408, 429, 5xx), waits with exponential backoff and tries again up to
/// `config.max_retries` times. Returns the first `Ok` or the final `Err`.
async fn with_retry<T, F, Fut>(
    config: &RetryConfig,
    operation: &str,
    f: F,
) -> Result<T, ocync_distribution::Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, ocync_distribution::Error>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if let Some(status) = e.status_code() {
                    if retry::should_retry(status, attempt, config.max_retries) {
                        let backoff = config.backoff_for(attempt);
                        warn!(
                            operation,
                            attempt,
                            status = %status,
                            backoff_ms = backoff.as_millis(),
                            "retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }
                }
                return Err(e);
            }
        }
    }
}

/// Compute aggregate statistics from a list of image results.
fn compute_stats(images: &[ImageResult]) -> SyncStats {
    let mut stats = SyncStats::default();
    for image in images {
        match &image.status {
            ImageStatus::Synced => {
                stats.images_synced += 1;
                stats.bytes_transferred += image.bytes_transferred;
            }
            ImageStatus::Skipped { .. } => {
                stats.images_skipped += 1;
            }
            ImageStatus::Failed { .. } => {
                stats.images_failed += 1;
            }
        }
        stats.blobs_transferred += image.blob_stats.transferred;
        stats.blobs_skipped += image.blob_stats.skipped;
        stats.blobs_mounted += image.blob_stats.mounted;
    }
    stats
}

/// Check if a manifest push error is an ECR immutable tag rejection.
///
/// ECR returns HTTP 400 with `ImageTagAlreadyExistsException` when a
/// manifest push targets a tag that already exists on a repository with
/// immutable tag settings enabled.
fn is_immutable_tag_error(err: &crate::Error) -> bool {
    matches!(
        err,
        crate::Error::Manifest {
            source: ocync_distribution::Error::RegistryError { status, message },
            ..
        } if *status == http::StatusCode::BAD_REQUEST
            && message.contains("ImageTagAlreadyExistsException")
    )
}

#[cfg(test)]
mod tests {
    use ocync_distribution::Digest;
    use ocync_distribution::spec::{Descriptor, MediaType};

    use super::*;

    /// Build a valid sha256 digest from a short suffix, zero-padded to 64 hex chars.
    fn test_digest(suffix: &str) -> Digest {
        format!("sha256:{suffix:0>64}").parse().unwrap()
    }

    /// Build a minimal descriptor with the given digest.
    fn test_descriptor(digest: Digest, media_type: MediaType) -> Descriptor {
        Descriptor {
            media_type,
            digest,
            size: 0,
            platform: None,
            artifact_type: None,
            annotations: None,
        }
    }

    #[test]
    fn collect_blobs_config_and_layers() {
        let config_digest = test_digest("c0");
        let layer1_digest = test_digest("a1");
        let layer2_digest = test_digest("b2");

        let config_desc = Descriptor {
            size: 50,
            ..test_descriptor(config_digest.clone(), MediaType::OciConfig)
        };
        let layer1_desc = Descriptor {
            size: 200,
            ..test_descriptor(layer1_digest.clone(), MediaType::OciLayerGzip)
        };
        let layer2_desc = Descriptor {
            size: 300,
            ..test_descriptor(layer2_digest.clone(), MediaType::OciLayerGzip)
        };

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: config_desc,
            layers: vec![layer1_desc, layer2_desc],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0].digest, config_digest);
        assert_eq!(blobs[0].size, 50);
        assert_eq!(blobs[1].digest, layer1_digest);
        assert_eq!(blobs[1].size, 200);
        assert_eq!(blobs[2].digest, layer2_digest);
        assert_eq!(blobs[2].size, 300);
    }

    #[test]
    fn collect_blobs_no_layers() {
        let config_digest = test_digest("cc");

        let config_desc = Descriptor {
            size: 42,
            ..test_descriptor(config_digest.clone(), MediaType::OciConfig)
        };

        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: config_desc,
            layers: vec![],
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let blobs = collect_image_blobs(&manifest);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].digest, config_digest);
        assert_eq!(blobs[0].size, 42);
    }

    fn make_image_result(status: ImageStatus, bytes: u64) -> ImageResult {
        ImageResult {
            image_id: Uuid::now_v7(),
            source: "source/repo:tag".into(),
            target: "target/repo:tag".into(),
            status,
            bytes_transferred: bytes,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::from_millis(100),
        }
    }

    #[test]
    fn compute_stats_mixed_results() {
        let images = vec![
            make_image_result(ImageStatus::Synced, 1024),
            make_image_result(
                ImageStatus::Skipped {
                    reason: SkipReason::DigestMatch,
                },
                0,
            ),
            make_image_result(
                ImageStatus::Failed {
                    kind: ErrorKind::BlobTransfer,
                    error: "timeout".into(),
                    retries: 3,
                },
                0,
            ),
        ];

        let stats = compute_stats(&images);
        assert_eq!(stats.images_synced, 1);
        assert_eq!(stats.images_skipped, 1);
        assert_eq!(stats.images_failed, 1);
        assert_eq!(stats.bytes_transferred, 1024);
    }

    #[test]
    fn compute_stats_empty() {
        let stats = compute_stats(&[]);
        assert_eq!(stats.images_synced, 0);
        assert_eq!(stats.images_skipped, 0);
        assert_eq!(stats.images_failed, 0);
        assert_eq!(stats.bytes_transferred, 0);
    }

    #[test]
    fn blob_frequency_map_tracks_counts() {
        let mut freq = BlobFrequencyMap::new();
        let d1 = test_digest("01");
        let d2 = test_digest("02");

        assert_eq!(freq.count(&d1), 0);

        freq.record(&d1);
        freq.record(&d1);
        freq.record(&d2);

        assert_eq!(freq.count(&d1), 2);
        assert_eq!(freq.count(&d2), 1);
    }

    #[test]
    fn immutable_tag_error_detected() {
        let err = crate::Error::Manifest {
            reference: "v1.0".into(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::BAD_REQUEST,
                message: "ImageTagAlreadyExistsException: tag already exists".into(),
            },
        };
        assert!(is_immutable_tag_error(&err));
    }

    #[test]
    fn non_immutable_400_not_detected() {
        let err = crate::Error::Manifest {
            reference: "v1.0".into(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::BAD_REQUEST,
                message: "some other 400 error".into(),
            },
        };
        assert!(!is_immutable_tag_error(&err));
    }

    #[test]
    fn non_400_not_detected_as_immutable() {
        let err = crate::Error::Manifest {
            reference: "v1.0".into(),
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                message: "ImageTagAlreadyExistsException".into(),
            },
        };
        assert!(!is_immutable_tag_error(&err));
    }

    #[test]
    fn blob_error_not_detected_as_immutable() {
        let digest: ocync_distribution::Digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();
        let err = crate::Error::BlobTransfer {
            digest,
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::BAD_REQUEST,
                message: "ImageTagAlreadyExistsException".into(),
            },
        };
        assert!(!is_immutable_tag_error(&err));
    }
}
