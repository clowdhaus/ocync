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
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use ocync_distribution::Digest;
use ocync_distribution::RegistryClient;
use ocync_distribution::blob::MountResult;
use ocync_distribution::manifest::ManifestPull;
use ocync_distribution::spec::{Descriptor, ImageManifest, ManifestKind};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::cache::TransferStateCache;
use crate::retry::{self, RetryConfig};
use crate::shutdown::ShutdownSignal;
use crate::staging::BlobStage;
use crate::{BlobTransferStats, ImageResult, ImageStatus, SkipReason, SyncReport, SyncStats};

/// A fully resolved mapping ready for the sync engine.
///
/// All config resolution (registry lookup, tag filtering) is done
/// before constructing this type. The engine operates purely on
/// resolved values.
#[derive(Debug)]
pub struct ResolvedMapping {
    /// Client for the source registry.
    pub source_client: Arc<RegistryClient>,
    /// Repository path at the source (e.g. `library/nginx`).
    pub source_repo: String,
    /// Repository path at the target (e.g. `mirror/nginx`).
    pub target_repo: String,
    /// Target registries to sync to.
    pub targets: Vec<TargetEntry>,
    /// Tag pairs to sync (already filtered).
    pub tags: Vec<TagPair>,
}

/// A single target registry entry.
///
/// `name` must be unique across targets within a [`ResolvedMapping`] -- it is
/// used as the key in the blob deduplication map to track which blobs have
/// already been transferred to each target.
#[derive(Debug)]
pub struct TargetEntry {
    /// Registry identifier used as the blob dedup key.
    ///
    /// Must be unique per mapping. Typically the config-defined registry name
    /// (e.g. `us-ecr`) or the bare hostname for ad-hoc commands.
    pub name: String,
    /// Client for this target registry.
    pub client: Arc<RegistryClient>,
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
    target_name: String,
    /// Target client for pushing.
    target_client: Arc<RegistryClient>,
    /// Target repository path.
    target_repo: String,
    /// Source repository path.
    source_repo: String,
    /// Source tag (for display/logging).
    source_tag: String,
    /// Target tag (for pushing manifests).
    target_tag: String,
}

/// Discovery outcome for one (mapping, tag) pair.
enum DiscoveryOutcome {
    /// All targets match; nothing to do. Contains one `ImageResult` per target.
    Skip(Vec<ImageResult>),
    /// Some targets need sync. Contains active items and blob digests for
    /// frequency tracking.
    Active {
        /// One per target that needs sync.
        items: Vec<TransferTask>,
        /// All blob digests from this tag's source data (for frequency tracking).
        blobs: Vec<Digest>,
        /// Results for targets that were skipped (digest match).
        skipped: Vec<ImageResult>,
    },
    /// Source pull failed. Contains failure results for all targets.
    Failed(Vec<ImageResult>),
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

/// Sync engine -- orchestrates concurrent image transfers across registries.
///
/// Uses a pipelined architecture where discovery and execution overlap via
/// `tokio::select!`. The plan phase is eliminated entirely -- progressive
/// cache population replaces upfront batch HEAD checks.
#[derive(Debug)]
pub struct SyncEngine {
    retry: RetryConfig,
    max_concurrent: usize,
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
        }
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
    /// signal fires, drain in-flight transfers up to a 25-second deadline, then
    /// return. Pass `None` to run to completion without shutdown handling.
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

        // Seed discovery with all (mapping, tag) pairs.
        for mapping in &mappings {
            for tag_pair in &mapping.tags {
                let source_client = Arc::clone(&mapping.source_client);
                let source_repo = mapping.source_repo.clone();
                let target_repo = mapping.target_repo.clone();
                let source_tag = tag_pair.source.clone();
                let target_tag = tag_pair.target.clone();
                let retry = self.retry.clone();
                let targets: Vec<(String, Arc<RegistryClient>)> = mapping
                    .targets
                    .iter()
                    .map(|te| (te.name.clone(), Arc::clone(&te.client)))
                    .collect();

                discovery_futures.push(async move {
                    discover_tag(
                        source_client,
                        &source_repo,
                        &target_repo,
                        &source_tag,
                        &target_tag,
                        &targets,
                        &retry,
                    )
                    .await
                });
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
                    let source_display = format!(
                        "{}:{} -> {}",
                        item.source_repo, item.source_tag, item.target_name
                    );
                    let target_display = format!("{}:{}", item.target_repo, item.target_tag);

                    execution_futures.push(Box::pin(async move {
                        let _permit = sem.acquire().await.unwrap();
                        progress.image_started(&source_display, &target_display);
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
                    // SAFETY: guard ensures shutdown.is_some(), unwrap is safe.
                    shutdown.unwrap().notified().await
                }, if shutdown.is_some() && !shutting_down => {
                    shutting_down = true;
                    drain_deadline = Some(
                        tokio::time::Instant::now() + Duration::from_secs(25),
                    );
                    tracing::info!(
                        in_flight = execution_futures.len(),
                        "shutdown signal received, draining in-flight transfers"
                    );
                }
                _ = async {
                    // SAFETY: guard ensures drain_deadline.is_some(), unwrap is safe.
                    tokio::time::sleep_until(drain_deadline.unwrap()).await
                }, if drain_deadline.is_some() => {
                    tracing::warn!(
                        remaining = execution_futures.len(),
                        "drain deadline reached, abandoning in-flight transfers"
                    );
                    break;
                }
                Some(outcome) = discovery_futures.next(),
                    if !shutting_down && !discovery_futures.is_empty() =>
                {
                    match outcome {
                        DiscoveryOutcome::Skip(skip_results) => {
                            for r in &skip_results {
                                progress.image_completed(r);
                            }
                            results.extend(skip_results);
                        }
                        DiscoveryOutcome::Active { items, blobs, skipped } => {
                            for digest in &blobs {
                                freq_map.record(digest);
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

        let stats = compute_stats(&results);
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

/// Discover a single (mapping, tag) pair: pull source manifest, HEAD-check targets.
///
/// Returns a `DiscoveryOutcome` indicating whether all targets can be skipped,
/// some need sync, or the source pull failed.
async fn discover_tag(
    source_client: Arc<RegistryClient>,
    source_repo: &str,
    target_repo: &str,
    source_tag: &str,
    target_tag: &str,
    targets: &[(String, Arc<RegistryClient>)],
    retry: &RetryConfig,
) -> DiscoveryOutcome {
    // Pull source manifest (shared across all targets for this tag).
    let source_data =
        match pull_source_manifest(&source_client, source_repo, source_tag, retry).await {
            Ok(data) => Rc::new(data),
            Err(err) => {
                let error_str = err.to_string();
                warn!(
                    source_repo = %source_repo,
                    tag = %source_tag,
                    error = %error_str,
                    "source pull failed, skipping all targets"
                );
                let fail_results: Vec<ImageResult> = targets
                    .iter()
                    .map(|(target_name, _)| ImageResult {
                        image_id: Uuid::now_v7(),
                        source: format!("{source_repo}:{source_tag}"),
                        target: format!("{target_repo} ({target_name}):{target_tag}"),
                        status: ImageStatus::Failed {
                            error: error_str.clone(),
                            retries: retry.max_retries,
                        },
                        bytes_transferred: 0,
                        blob_stats: BlobTransferStats::default(),
                        duration: Duration::ZERO,
                    })
                    .collect();
                return DiscoveryOutcome::Failed(fail_results);
            }
        };

    let source_digest = &source_data.pull.digest;

    // HEAD-check all targets concurrently.
    let mut head_checks = FuturesUnordered::new();

    for (target_name, target_client) in targets {
        let client = Arc::clone(target_client);
        let repo = target_repo.to_owned();
        let tag = target_tag.to_owned();
        let name = target_name.clone();

        head_checks.push(async move {
            let result = client.manifest_head(&repo, &tag).await;
            (name, client, result)
        });
    }

    let mut active_items = Vec::new();
    let mut skipped_results = Vec::new();

    while let Some((target_name, target_client, result)) = head_checks.next().await {
        match result {
            Ok(Some(head)) if head.digest == *source_digest => {
                info!(
                    source_repo = %source_repo,
                    target_repo = %target_repo,
                    tag = %source_tag,
                    digest = %source_digest,
                    "skipping -- digest matches at target"
                );
                tracing::debug!(target: "ocync::metrics", "unchanged_skip");
                skipped_results.push(ImageResult {
                    image_id: Uuid::now_v7(),
                    source: format!("{source_repo}:{source_tag}"),
                    target: format!("{target_repo}:{target_tag}"),
                    status: ImageStatus::Skipped {
                        reason: SkipReason::DigestMatch,
                    },
                    bytes_transferred: 0,
                    blob_stats: BlobTransferStats::default(),
                    duration: Duration::ZERO,
                });
            }
            Ok(_) => {
                active_items.push(TransferTask {
                    source_data: Rc::clone(&source_data),
                    source_client: Arc::clone(&source_client),
                    target_name,
                    target_client,
                    target_repo: target_repo.to_owned(),
                    source_repo: source_repo.to_owned(),
                    source_tag: source_tag.to_owned(),
                    target_tag: target_tag.to_owned(),
                });
            }
            Err(e) => {
                warn!(
                    target_repo = %target_repo,
                    target_tag = %target_tag,
                    error = %e,
                    "target manifest HEAD failed, proceeding with sync"
                );
                active_items.push(TransferTask {
                    source_data: Rc::clone(&source_data),
                    source_client: Arc::clone(&source_client),
                    target_name,
                    target_client,
                    target_repo: target_repo.to_owned(),
                    source_repo: source_repo.to_owned(),
                    source_tag: source_tag.to_owned(),
                    target_tag: target_tag.to_owned(),
                });
            }
        }
    }

    if active_items.is_empty() {
        return DiscoveryOutcome::Skip(skipped_results);
    }

    // Collect blob digests for frequency tracking.
    let blobs: Vec<Digest> = blobs_from_manifest(&source_data)
        .iter()
        .map(|d| d.digest.clone())
        .collect();

    DiscoveryOutcome::Active {
        items: active_items,
        blobs,
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
        source_repo: &item.source_repo,
        target_client: &item.target_client,
        target_name: &item.target_name,
        target_repo: &item.target_repo,
    };
    let outcome = transfer_image_blobs(&ctx, &item.source_data, freq_counts).await;

    if let Some(err) = outcome.error {
        warn!(target_name = %item.target_name, error = %err, "blob transfer failed");
        return ImageResult {
            image_id: Uuid::now_v7(),
            source: format!("{}:{}", item.source_repo, item.source_tag),
            target: format!("{}:{}", item.target_repo, item.target_tag),
            status: ImageStatus::Failed {
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
        &item.target_repo,
        &item.target_tag,
        &item.source_data,
    )
    .await
    {
        Ok(()) => {
            info!(
                source_repo = %item.source_repo,
                target_repo = %item.target_repo,
                source_tag = %item.source_tag,
                target_tag = %item.target_tag,
                bytes = outcome.bytes_transferred,
                "image synced"
            );
            ImageResult {
                image_id: Uuid::now_v7(),
                source: format!("{}:{}", item.source_repo, item.source_tag),
                target: format!("{}:{}", item.target_repo, item.target_tag),
                status: ImageStatus::Synced,
                bytes_transferred: outcome.bytes_transferred,
                blob_stats: outcome.stats,
                duration: start.elapsed(),
            }
        }
        Err(err) => {
            warn!(target_name = %item.target_name, error = %err, "manifest push failed");
            ImageResult {
                image_id: Uuid::now_v7(),
                source: format!("{}:{}", item.source_repo, item.source_tag),
                target: format!("{}:{}", item.target_repo, item.target_tag),
                status: ImageStatus::Failed {
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

/// Pull all source manifest data for a single tag.
///
/// For image manifests, returns just the manifest. For index manifests,
/// also pulls all child manifests.
async fn pull_source_manifest(
    client: &RegistryClient,
    repo: &str,
    tag: &str,
    retry: &RetryConfig,
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
            let mut children = Vec::with_capacity(index.manifests.len());
            for child_desc in &index.manifests {
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
    source_repo: &'a str,
    target_client: &'a RegistryClient,
    target_name: &'a str,
    target_repo: &'a str,
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
    blobs.sort_by(|a, b| {
        let fa = freq_counts.get(&a.digest).copied().unwrap_or(0);
        let fb = freq_counts.get(&b.digest).copied().unwrap_or(0);
        fb.cmp(&fa)
    });

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
                .map(|s| s.to_owned())
        };

        if let Some(from_repo) = mount_source {
            debug!(%digest, %from_repo, target = ctx.target_name, "attempting mount");
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
                    debug!(%digest, target = ctx.target_name, "mount failed, falling back to HEAD+push");
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
                    target = ctx.target_name,
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
            if ctx.staging.exists(digest) {
                // Blob already staged from a previous target: push from disk.
                match ctx.staging.read(digest) {
                    Ok(data) => with_retry(ctx.retry, "blob push (staged)", || {
                        ctx.target_client.blob_push(ctx.target_repo, &data)
                    })
                    .await
                    .map(|_| ())
                    .map_err(|e| crate::Error::BlobTransfer {
                        digest: digest.clone(),
                        source: e,
                    }),
                    Err(e) => Err(crate::Error::BlobTransfer {
                        digest: digest.clone(),
                        source: ocync_distribution::Error::Other(format!(
                            "staging read failed: {e}"
                        )),
                    }),
                }
            } else {
                // Pull from source, write to staging, then push to target.
                let pull_result = with_retry(ctx.retry, "blob pull (to stage)", || async {
                    let stream = ctx.source_client.blob_pull(ctx.source_repo, digest).await?;
                    let mut buf = Vec::new();
                    futures_util::pin_mut!(stream);
                    use futures_util::StreamExt as _;
                    while let Some(chunk) = stream.next().await {
                        let chunk =
                            chunk.map_err(|e| ocync_distribution::Error::Other(e.to_string()))?;
                        buf.extend_from_slice(&chunk);
                    }
                    Ok::<Vec<u8>, ocync_distribution::Error>(buf)
                })
                .await;

                match pull_result {
                    Ok(data) => {
                        // Write to staging (best-effort: if it fails, skip caching).
                        if let Err(e) = ctx.staging.write(digest, &data) {
                            debug!(%digest, error = %e, "failed to write blob to staging, continuing");
                        }
                        with_retry(ctx.retry, "blob push (via stage)", || {
                            ctx.target_client.blob_push(ctx.target_repo, &data)
                        })
                        .await
                        .map(|_| ())
                        .map_err(|e| crate::Error::BlobTransfer {
                            digest: digest.clone(),
                            source: e,
                        })
                    }
                    Err(e) => Err(crate::Error::BlobTransfer {
                        digest: digest.clone(),
                        source: e,
                    }),
                }
            }
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
    target_repo: &str,
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
}
