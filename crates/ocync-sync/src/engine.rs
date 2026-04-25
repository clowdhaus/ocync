//! Sync engine - pipelined concurrent orchestrator for image transfers.
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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
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
    Descriptor, ImageIndex, ImageManifest, ManifestKind, MediaType, PlatformFilter,
    RegistryAuthority, RepositoryName,
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

/// Writes a JSONL timing event to the optional timing file.
///
/// Used for benchmark instrumentation via `OCYNC_TIMING_FILE`. Each line is
/// `{"phase":"<name>","elapsed_ms":<N>}`. No-ops when `file` is `None`.
///
/// `phase` must be an ASCII identifier (no quotes or backslashes).
/// All call sites pass string literals, so this invariant is enforced at
/// compile time. We avoid adding a `serde_json` dependency to this crate
/// for a two-field diagnostic line.
fn write_timing(file: &mut Option<std::fs::File>, phase: &str, start: &Instant) {
    if let Some(f) = file {
        use std::io::Write;
        debug_assert!(
            phase
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "timing phase name must be ASCII alphanumeric: {phase}"
        );
        let _ = writeln!(
            f,
            r#"{{"phase":"{phase}","elapsed_ms":{}}}"#,
            start.elapsed().as_millis()
        );
    }
}

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
/// ad-hoc commands. Must be unique per mapping - it is used as the outer
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

/// Resolved artifact sync configuration.
///
/// Controls whether the engine discovers and transfers OCI referrers
/// (signatures, SBOMs, attestations) after syncing each image manifest.
#[derive(Debug, Clone)]
pub struct ResolvedArtifacts {
    /// Whether artifact discovery and transfer is enabled.
    pub enabled: bool,
    /// Only sync artifacts whose artifact type matches one of these MIME types.
    /// Empty means all types pass.
    pub include: Vec<String>,
    /// Exclude artifacts whose artifact type matches one of these MIME types.
    pub exclude: Vec<String>,
    /// When true, images with no referrers cause a sync failure.
    pub require_artifacts: bool,
}

impl Default for ResolvedArtifacts {
    fn default() -> Self {
        Self {
            enabled: true,
            include: Vec::new(),
            exclude: Vec::new(),
            require_artifacts: false,
        }
    }
}

impl ResolvedArtifacts {
    /// Check whether a given artifact type passes the include/exclude filters.
    pub fn type_matches(&self, artifact_type: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|t| t == artifact_type) {
            return false;
        }
        if self.exclude.iter().any(|t| t == artifact_type) {
            return false;
        }
        true
    }
}

/// Cached result of referrer discovery for a (`source_repo`, `parent_digest`) pair.
///
/// Avoids redundant referrers API calls when the same source manifest is synced
/// to multiple targets.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum CachedReferrers {
    /// Successfully discovered referrers (index may have zero entries).
    Found(ImageIndex),
    /// No referrers exist (API 404 + tag fallback 404).
    NotFound,
    /// Discovery failed due to a transient error.
    Failed,
}

/// Per-run cache of referrer discovery results, keyed by (`source_repo`, `parent_digest`).
type ReferrersCache = Rc<RefCell<HashMap<(RepositoryName, Digest), CachedReferrers>>>;

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
    /// When true, HEAD-check all targets against the source HEAD digest
    /// before performing a full source manifest GET on cache miss.
    ///
    /// Conserves rate-limit tokens on source registries (e.g., Docker Hub)
    /// by avoiding the expensive GET when all targets already have the
    /// correct manifest. On cache hit the discovery optimization already
    /// skips the GET; `head_first` extends that to the cold-cache path.
    ///
    /// The source HEAD from the discovery optimization is reused, so
    /// enabling both features does not issue a redundant HEAD.
    pub head_first: bool,
    /// Compiled glob pattern identifying immutable tags.
    ///
    /// When a tag matches this pattern AND exists in every target's
    /// [`existing_tags`](TargetEntry::existing_tags), the tag is skipped with
    /// zero API calls. See the skip optimization hierarchy in the design docs.
    pub immutable_glob: Option<globset::GlobSet>,
    /// Artifact sync configuration (shared across all tasks for this mapping).
    pub artifacts_config: Rc<ResolvedArtifacts>,
}

impl ResolvedMapping {
    /// Returns `true` when `tag` should be skipped via the immutable-tag
    /// optimization: the tag matches the configured glob AND already exists
    /// at every target.
    ///
    /// Returns `false` (no skip) when:
    /// - No `immutable_glob` is configured.
    /// - The tag does not match the pattern.
    /// - `targets` is empty (degenerate case; `.all()` on empty is `true`).
    /// - Any target's `existing_tags` does not contain the tag.
    pub fn should_skip_immutable(&self, tag: &str) -> bool {
        let Some(ref pattern) = self.immutable_glob else {
            return false;
        };
        if self.targets.is_empty() {
            return false;
        }
        pattern.is_match(tag) && self.targets.iter().all(|t| t.existing_tags.contains(tag))
    }
}

/// A single target registry entry.
///
/// `name` must be unique across targets within a [`ResolvedMapping`] - it is
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
    /// Tags already present at this target registry at resolution time.
    ///
    /// Populated when `immutable_glob` is configured on the parent
    /// [`ResolvedMapping`]. Empty when the pattern is absent OR when
    /// `list_tags` failed (degraded: the immutable skip is disabled
    /// for this target, falling through to HEAD-based discovery).
    pub existing_tags: HashSet<String>,
}

impl Clone for TargetEntry {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            client: Arc::clone(&self.client),
            batch_checker: self.batch_checker.clone(),
            existing_tags: self.existing_tags.clone(),
        }
    }
}

impl fmt::Debug for TargetEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetEntry")
            .field("name", &self.name)
            .field("client", &self.client)
            .field("batch_checker", &self.batch_checker.as_ref().map(|_| ".."))
            .field("existing_tags", &self.existing_tags.len())
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
        artifacts_skipped: false,
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

/// Outcome of transferring a single blob to one target.
enum BlobResult {
    /// Cache hit - blob already exists at target repo.
    Skipped,
    /// Cross-repo mount succeeded.
    Mounted,
    /// Pulled from source and pushed to target.
    Transferred { bytes: u64 },
    /// Another blob failed; this one was cancelled before starting I/O.
    Cancelled,
    /// Transfer failed.
    Failed(crate::Error),
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
    /// Artifact sync configuration (Rc-shared with mapping).
    artifacts_config: Rc<ResolvedArtifacts>,
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
    /// `head_first` enabled: all targets matched the source HEAD digest (no
    /// cache entry existed), avoiding the full source GET entirely.
    HeadFirstSkip,
}

/// Elect leader images for mount optimization via greedy set-cover.
///
/// Groups pending tasks by source image (tasks sharing the same
/// [`PulledManifest`] via `Rc` target different registries for the same
/// tag). Repeatedly picks the image that covers the most shared blob
/// digests not already covered by previously elected leaders. Stops when
/// no candidate provides marginal coverage.
///
/// All leader tasks are promoted to the front of the deque. The existing
/// `ClaimAction::Wait` mechanism in [`transfer_image_blobs`] handles
/// inter-leader blob deduplication: if two leaders share a blob, the
/// second waits for the first's upload then mounts rather than
/// re-uploading.
///
/// Returns the number of tasks promoted to the front of the deque as leaders
/// (0 if no election benefit exists).
fn elect_leaders(pending: &mut VecDeque<TransferTask>) -> usize {
    if pending.len() <= 1 {
        return 0;
    }

    // Group tasks by source image. Tasks sharing an Rc<PulledManifest>
    // represent the same source image bound to different target registries.
    let mut group_ptrs: Vec<usize> = Vec::new();
    let mut group_blobs: Vec<HashSet<Digest>> = Vec::new();
    let mut task_group: Vec<usize> = Vec::with_capacity(pending.len());

    for task in pending.iter() {
        let ptr = Rc::as_ptr(&task.source_data) as usize;
        let idx = if let Some(pos) = group_ptrs.iter().position(|&p| p == ptr) {
            pos
        } else {
            let blobs: HashSet<Digest> = blobs_from_manifest(&task.source_data)
                .into_iter()
                .map(|d| d.digest.clone())
                .collect();
            let idx = group_ptrs.len();
            group_ptrs.push(ptr);
            group_blobs.push(blobs);
            idx
        };
        task_group.push(idx);
    }

    let num_groups = group_ptrs.len();
    if num_groups <= 1 {
        // All tasks are the same source image; no election needed.
        return 0;
    }

    // Greedy multi-leader election. Each round picks the image that
    // maximizes marginal shared-blob coverage: for each remaining
    // (non-leader) image, count blobs shared with the candidate that are
    // not already in a previous leader's blob set.
    let mut leader_set: Vec<usize> = Vec::new();
    let mut leader_blob_union: HashSet<Digest> = HashSet::new();
    let mut remaining: Vec<usize> = (0..num_groups).collect();

    loop {
        if remaining.len() <= 1 {
            break;
        }

        let best = remaining
            .iter()
            .map(|&i| {
                let marginal: usize = remaining
                    .iter()
                    .filter(|&&j| j != i)
                    .map(|&j| {
                        group_blobs[i]
                            .intersection(&group_blobs[j])
                            .filter(|b| !leader_blob_union.contains(*b))
                            .count()
                    })
                    .sum();
                (i, marginal)
            })
            .max_by_key(|&(_, m)| m);

        match best {
            Some((idx, marginal)) if marginal > 0 => {
                leader_blob_union.extend(group_blobs[idx].iter().cloned());
                leader_set.push(idx);
                remaining.retain(|&i| i != idx);
            }
            _ => break,
        }
    }

    if leader_set.is_empty() {
        // No shared blobs across any images; concurrent is optimal.
        return 0;
    }

    // Log each leader's source reference.
    for &leader_idx in &leader_set {
        if let Some((pos, _)) = task_group
            .iter()
            .enumerate()
            .find(|&(_, &g)| g == leader_idx)
        {
            info!(
                leader = %pending[pos].source,
                leader_idx = leader_set.iter().position(|&l| l == leader_idx).unwrap() + 1,
                total_leaders = leader_set.len(),
                images = num_groups,
                "elected leader for mount optimization"
            );
        }
    }

    // Stable partition: leader tasks first, then followers.
    let mut leaders: VecDeque<TransferTask> = VecDeque::new();
    let mut followers: VecDeque<TransferTask> = VecDeque::new();
    for (task, &group) in pending.drain(..).zip(task_group.iter()) {
        if leader_set.contains(&group) {
            leaders.push_back(task);
        } else {
            followers.push_back(task);
        }
    }

    let leader_count = leaders.len();
    pending.extend(leaders);
    pending.extend(followers);
    leader_count
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

/// Accumulates discovery route counters during the pipeline loop.
///
/// Groups the per-route counters that were previously separate `let mut`
/// locals in `run()`, reducing variable count and centralizing the match.
#[derive(Default)]
struct DiscoveryCounters {
    /// Tags where source HEAD matched the tag-digest cache.
    cache_hits: u64,
    /// Tags where a full source manifest pull was required.
    cache_misses: u64,
    /// Tags where the source HEAD request itself failed.
    head_failures: u64,
    /// Tags where the cache matched but a target was stale.
    target_stale: u64,
    /// Tags skipped via `head_first` (all targets matched source HEAD).
    head_first_skips: u64,
}

impl DiscoveryCounters {
    /// Record a discovery route, incrementing the appropriate counters.
    ///
    /// `HeadFailure` and `TargetStale` both increment `cache_misses` in
    /// addition to their own counter because they represent cache misses
    /// that were further classified by failure reason.
    fn record(&mut self, route: &DiscoveryRoute) {
        match route {
            DiscoveryRoute::CacheHit => self.cache_hits += 1,
            DiscoveryRoute::CacheMiss => self.cache_misses += 1,
            DiscoveryRoute::HeadFailure => {
                self.cache_misses += 1;
                self.head_failures += 1;
            }
            DiscoveryRoute::TargetStale => {
                self.cache_misses += 1;
                self.target_stale += 1;
            }
            DiscoveryRoute::HeadFirstSkip => {
                self.head_first_skips += 1;
            }
        }
    }
}

/// Pipeline phase for leader-follower mount optimization.
///
/// Tracks which group of tasks is currently being promoted into execution
/// futures. The phase advances monotonically: `Discovering -> Done`.
///
/// Leaders are ordered first by `elect_leaders` so they acquire semaphore
/// permits and claim blob uploads before followers. Two-level Notify
/// synchronization ensures correctness:
/// 1. Per-blob `Notify` via `ClaimAction::Wait`: followers wait for a
///    leader's blob upload to complete before claiming the same blob.
/// 2. Per-repo committed `Notify`: followers wait for the leader's
///    manifest commit before attempting cross-repo mounts. ECR requires
///    the source repo to have a committed manifest for mount to succeed.
enum PromotionPhase {
    /// Discovery futures still in flight; tasks accumulate in `pending`.
    Discovering,
    /// All tasks promoted (leaders and followers together); no further
    /// phase transitions. Per-blob and per-repo-committed `Notify`
    /// synchronization in `wait_for_blob_claim` ensures followers wait
    /// for their specific leader blobs and manifest commits.
    Done,
}

/// Borrowed references shared by the `promote` closure at each call site.
///
/// Groups the invariant engine state that every promoted task needs, keeping
/// the closure signature to `(TransferTask, &PromoteContext, &[RepositoryName])`.
///
/// Created after discovery completes (once `freq_map` is frozen).
struct PromoteContext<'a> {
    sem: &'a Rc<Semaphore>,
    cache: &'a Rc<RefCell<TransferStateCache>>,
    staging: &'a Rc<BlobStage>,
    freq_map: &'a BlobFrequencyMap,
    retry: &'a RetryConfig,
    referrers_cache: &'a ReferrersCache,
}

/// Default cap for concurrent image transfers (Level 1: global image semaphore).
pub const DEFAULT_MAX_CONCURRENT_TRANSFERS: usize = 50;

/// Maximum concurrent blob transfers within a single image.
///
/// Within a single image task, blobs are transferred via `FuturesUnordered`
/// gated by a local `Semaphore(BLOB_CONCURRENCY)`. This bounds the number of
/// simultaneous blob uploads/downloads per image while allowing the global
/// semaphore to independently control total in-flight image tasks.
///
/// Matches skopeo's default (6). Higher values risk approaching ECR's
/// `InitiateLayerUpload` limit (100 TPS shared across all images).
/// Candidate for `SyncEngine` builder configuration if workloads need tuning.
const BLOB_CONCURRENCY: usize = 6;

/// Default shutdown drain deadline in seconds.
const DEFAULT_DRAIN_DEADLINE_SECS: u64 = 25;

/// Sync engine - orchestrates concurrent image transfers across registries.
///
/// Discovery futures drain first via `tokio::select!`, then leader-follower
/// election reorders pending tasks so images sharing the most blobs execute
/// first (enabling cross-repo mounts for followers). The plan phase is
/// eliminated entirely - progressive cache population replaces upfront
/// batch HEAD checks.
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

        // Synchronous writes are fine: current_thread runtime, file is tiny.
        let mut timing_file = std::env::var("OCYNC_TIMING_FILE")
            .ok()
            .and_then(|path| std::fs::File::create(&path).ok());
        write_timing(&mut timing_file, "discovery_start", &run_start);

        let mut discovery_futures = FuturesUnordered::new();
        let mut execution_futures: FuturesUnordered<
            std::pin::Pin<Box<dyn Future<Output = ImageResult>>>,
        > = FuturesUnordered::new();
        let mut pending: VecDeque<TransferTask> = VecDeque::new();
        let mut freq_map = BlobFrequencyMap::new();
        let global_sem = Rc::new(Semaphore::new(self.max_concurrent));
        let staging = Rc::new(staging);
        let referrers_cache: ReferrersCache = Rc::new(RefCell::new(HashMap::new()));
        let mut results: Vec<ImageResult> = Vec::new();
        let mut shutting_down = false;
        let mut drain_deadline: Option<tokio::time::Instant> = None;
        let mut discovery_counters = DiscoveryCounters::default();

        // Leader-follower mount optimization state.
        //
        // `promotion_phase` tracks which stage of the pipeline we are in.
        // After discovery completes, all tasks are promoted at once (leaders
        // ordered first). Per-blob `Notify` handles synchronization.
        let mut promotion_phase = PromotionPhase::Discovering;
        // Repos whose manifests are already committed at the target. Followers
        // prefer these as cross-repo mount sources because ECR requires a
        // committed manifest for mount to succeed.
        let mut preferred_mount_sources: Vec<RepositoryName> = Vec::new();

        // Budget circuit breaker: pause discovery when a source registry's
        // rate-limit remaining drops below 10% of the remaining discovery count.
        // Execution continues for already-discovered images. Discovery resumes
        // when a subsequent response shows the budget has refilled.
        //
        // Pause is all-or-nothing: ANY source below threshold pauses ALL
        // discovery (not just that source's mappings). This is a structural
        // constraint -- `FuturesUnordered` doesn't support selective polling,
        // so per-source pausing would require a different architecture.
        // In practice, multi-source syncs with mixed rate-limited / unlimited
        // registries are uncommon, and the anti-stall path prevents deadlock.
        let mut discovery_paused = false;
        // Collect unique source clients for rate-limit checking. Deduped by
        // Arc pointer identity (same source registry = same client).
        let source_clients: Vec<Arc<RegistryClient>> = {
            let mut seen = HashSet::new();
            let mut clients = Vec::new();
            for m in &mappings {
                if seen.insert(Arc::as_ptr(&m.source_client)) {
                    clients.push(Arc::clone(&m.source_client));
                }
            }
            clients
        };

        let mut immutable_tag_skips: u64 = 0;

        // Seed discovery with all (mapping, tag) pairs.
        for mapping in &mappings {
            for tag_pair in &mapping.tags {
                // Tier 1: immutable tag skip (0 API calls).
                if mapping.should_skip_immutable(&tag_pair.target) {
                    info!(
                        source_repo = %mapping.source_repo,
                        tag = %tag_pair.target,
                        "skipping -- immutable tag exists at all targets"
                    );
                    let source_ref = ImageRef {
                        repo: mapping.source_repo.clone(),
                        tag: tag_pair.source.clone(),
                    };
                    let target_ref = ImageRef {
                        repo: mapping.target_repo.clone(),
                        tag: tag_pair.target.clone(),
                    };
                    for _entry in &mapping.targets {
                        let r =
                            skip_image_result(&source_ref, &target_ref, SkipReason::ImmutableTag);
                        progress.image_completed(&r);
                        results.push(r);
                    }
                    immutable_tag_skips += mapping.targets.len() as u64;
                    continue;
                }

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
                    cache: Rc::clone(&cache),
                    source_head_timeout: self.source_head_timeout,
                    head_first: mapping.head_first,
                    artifacts_config: mapping.artifacts_config.clone(),
                };

                discovery_futures.push(async move { discover_tag(params).await });
            }
        }

        // Helper closure: promote a task from `pending` into an execution future.
        let promote = |item: TransferTask,
                       ctx: &PromoteContext<'_>,
                       preferred_mount_sources: &[RepositoryName]|
         -> std::pin::Pin<Box<dyn Future<Output = ImageResult>>> {
            let sem = Rc::clone(ctx.sem);
            let cache_ref = Rc::clone(ctx.cache);
            let retry = ctx.retry.clone();
            let freq_counts: HashMap<Digest, usize> = {
                let blobs = blobs_from_manifest(&item.source_data);
                blobs
                    .iter()
                    .map(|d| (d.digest.clone(), ctx.freq_map.count(&d.digest)))
                    .collect()
            };
            let staging_ref = Rc::clone(ctx.staging);
            let referrers_ref = Rc::clone(ctx.referrers_cache);
            let source_str = item.source.to_string();
            let target_str = item.target.to_string();
            let preferred_mount_sources = preferred_mount_sources.to_vec();
            Box::pin(async move {
                let _permit = sem.acquire().await.unwrap();
                progress.image_started(&source_str, &target_str);
                execute_item(
                    item,
                    &cache_ref,
                    &staging_ref,
                    &freq_counts,
                    &retry,
                    &preferred_mount_sources,
                    &referrers_ref,
                )
                .await
            })
        };

        loop {
            // Budget circuit breaker: check if discovery should resume.
            // Resume when (a) budget has refilled above the threshold, or
            // (b) nothing is executing - pending items cannot be promoted until
            // all discovery completes, so execution draining means no progress
            // is possible without resuming discovery.
            if discovery_paused && !discovery_futures.is_empty() {
                let remaining_discovery = discovery_futures.len() as u64;
                let threshold = (remaining_discovery / 10).max(1);
                let budget_ok = source_clients
                    .iter()
                    .all(|c| c.rate_limit_remaining().is_none_or(|r| r >= threshold));
                let must_resume = execution_futures.is_empty();
                if budget_ok || must_resume {
                    discovery_paused = false;
                    if must_resume && !budget_ok {
                        info!(
                            remaining_discovery,
                            pending = pending.len(),
                            "no active execution, resuming discovery despite low budget"
                        );
                    } else {
                        info!(
                            remaining_discovery,
                            "rate-limit budget refilled, resuming discovery"
                        );
                    }
                }
            }

            // Promotion: accumulate during discovery, then promote all at once.
            // Leaders are ordered first by `elect_leaders` so they acquire
            // semaphore permits and claim blob uploads before followers.
            // Followers that need a blob still in-flight will wait on the
            // per-blob `Notify` via `ClaimAction::Wait` in `wait_for_blob_claim`,
            // providing fine-grained synchronization without a coarse gate.
            if !shutting_down
                && matches!(promotion_phase, PromotionPhase::Discovering)
                && discovery_futures.is_empty()
            {
                // All discovery complete - elect leaders and promote.
                // `freq_map` is frozen from this point (no more discovery
                // mutations), so `PromoteContext` can borrow it.
                let n = elect_leaders(&mut pending);
                // Collect leader target repos for follower mount preference.
                if n > 0 {
                    preferred_mount_sources = pending
                        .iter()
                        .take(n)
                        .map(|t| t.target.repo.clone())
                        .collect();
                }
                promotion_phase = PromotionPhase::Done;
                write_timing(&mut timing_file, "discovery_complete", &run_start);
                let ctx = PromoteContext {
                    sem: &global_sem,
                    cache: &cache,
                    staging: &staging,
                    freq_map: &freq_map,
                    retry: &self.retry,
                    referrers_cache: &referrers_cache,
                };
                let to_promote = pending.len();
                for _ in 0..to_promote {
                    if let Some(item) = pending.pop_front() {
                        execution_futures.push(promote(item, &ctx, &preferred_mount_sources));
                    }
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
                    // Disable when all work is done - otherwise this branch
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
                    if !shutting_down && !discovery_paused && !discovery_futures.is_empty() =>
                {
                    discovery_counters.record(&route);
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

                    // Budget circuit breaker: check if we should pause discovery.
                    if !discovery_paused && !discovery_futures.is_empty() {
                        let remaining_discovery = discovery_futures.len() as u64;
                        let threshold = (remaining_discovery / 10).max(1);
                        for client in &source_clients {
                            if let Some(remaining) = client.rate_limit_remaining() {
                                if remaining < threshold {
                                    discovery_paused = true;
                                    let registry = client
                                        .registry_authority()
                                        .map(|a| a.to_string())
                                        .unwrap_or_else(|_| "unknown".into());
                                    warn!(
                                        rate_limit_remaining = remaining,
                                        remaining_discovery,
                                        threshold,
                                        %registry,
                                        "rate-limit budget low, pausing discovery"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
                else => break,
            }
        }

        write_timing(&mut timing_file, "execution_complete", &run_start);

        // Prune stale cache entries for tags/targets no longer in the mapping set.
        // Prevents unbounded cache growth when source tags or targets are deleted.
        {
            let live_keys: HashSet<SnapshotKey> = mappings
                .iter()
                .flat_map(|m| {
                    m.tags
                        .iter()
                        .map(|t| SnapshotKey::new(&m.source_authority, &m.source_repo, &t.source))
                })
                .collect();
            let live_targets: HashSet<String> = mappings
                .iter()
                .flat_map(|m| m.targets.iter().map(|t| t.name.to_string()))
                .collect();
            let mut cache_mut = cache.borrow_mut();
            cache_mut.prune_snapshots(&live_keys);
            cache_mut.prune_dedup(&live_targets);
            cache_mut.clear_notifies();
        }

        let mut stats = compute_stats(&results);
        stats.discovery_cache_hits = discovery_counters.cache_hits;
        stats.discovery_cache_misses = discovery_counters.cache_misses;
        stats.discovery_head_failures = discovery_counters.head_failures;
        stats.discovery_target_stale = discovery_counters.target_stale;
        stats.discovery_head_first_skips = discovery_counters.head_first_skips;
        stats.immutable_tag_skips = immutable_tag_skips;
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
    cache: Rc<RefCell<TransferStateCache>>,
    source_head_timeout: Duration,
    /// When true, HEAD-check targets on cache miss before the full source GET.
    head_first: bool,
    /// Artifact sync configuration (Rc-shared with mapping).
    artifacts_config: Rc<ResolvedArtifacts>,
}

/// Discover a single (mapping, tag) pair: HEAD source, check cache, pull if needed.
///
/// Returns `(DiscoveryOutcome, DiscoveryRoute)` - the transfer outcome and a
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
async fn discover_tag(params: DiscoveryParams) -> (DiscoveryOutcome, DiscoveryRoute) {
    let DiscoveryParams {
        source_client,
        source_authority,
        source,
        target,
        targets,
        artifacts_config,
        retry,
        platforms,
        cache,
        source_head_timeout,
        head_first,
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
        // Scoped borrow - dropped before any .await
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
                let check = check_targets_against_digest(TargetCheckParams {
                    targets: &targets,
                    source: &source,
                    target: &target,
                    compare_digest: &snapshot.filtered_digest,
                    label: "cache hit",
                })
                .await;

                if check.mismatched.is_empty() {
                    return (
                        DiscoveryOutcome::Skip(check.skipped),
                        DiscoveryRoute::CacheHit,
                    );
                }

                // Some targets stale - need full pull for those targets.
                let outcome = full_pull_and_build_tasks(FullPullParams {
                    source_client: &source_client,
                    source: &source,
                    target: &target,
                    targets: &check.mismatched,
                    skipped_results: check.skipped,
                    retry: &retry,
                    platforms: platforms.as_deref(),
                    cache: &cache,
                    snapshot_key: &snapshot_key,
                    head_digest: Some(head_digest),
                    platform_key: &platform_key,
                    artifacts_config: &artifacts_config,
                })
                .await;

                return (outcome, DiscoveryRoute::TargetStale);
            }
        }
        // Cache miss or source/config changed - fall through.
    }

    // --- head_first: HEAD targets on cache miss to avoid full source GET ---
    //
    // When head_first is enabled and the source HEAD succeeded, HEAD-check
    // all targets against the source HEAD digest before performing the
    // expensive full manifest GET. This conserves source rate-limit tokens
    // on cold cache when targets are already in sync.
    //
    // The source HEAD digest from the discovery optimization (step 1) is
    // reused here, avoiding a redundant second HEAD.
    //
    // Limitation: platform filtering can change the pushed digest relative
    // to the source HEAD digest. When platform filtering is active, the
    // target holds a filtered index whose digest differs from the source
    // HEAD digest, so the comparison would always miss. We skip the
    // head_first check in that case (fall through to full pull, which
    // handles filtered comparisons correctly).
    if head_first && platforms.is_none() {
        if let Some(ref head_digest) = source_head_digest {
            let check = check_targets_against_digest(TargetCheckParams {
                targets: &targets,
                source: &source,
                target: &target,
                compare_digest: head_digest,
                label: "head_first",
            })
            .await;

            if check.mismatched.is_empty() {
                // All targets match the source HEAD digest - skip entirely.
                // No cache entry is written since we never pulled the full
                // manifest (no filtered_digest available). In watch mode this
                // means head_first fires every cycle for tags that remain in
                // sync (1 source HEAD + N target HEADs per tag). The cache
                // only warms when a manifest change forces a full pull.
                return (
                    DiscoveryOutcome::Skip(check.skipped),
                    DiscoveryRoute::HeadFirstSkip,
                );
            }

            // Some targets stale - fall through to full pull for those targets.
            let outcome = full_pull_and_build_tasks(FullPullParams {
                source_client: &source_client,
                source: &source,
                target: &target,
                targets: &check.mismatched,
                skipped_results: check.skipped,
                retry: &retry,
                platforms: platforms.as_deref(),
                cache: &cache,
                snapshot_key: &snapshot_key,
                head_digest: Some(head_digest),
                platform_key: &platform_key,
                artifacts_config: &artifacts_config,
            })
            .await;

            return (outcome, DiscoveryRoute::CacheMiss);
        }
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
        cache: &cache,
        snapshot_key: &snapshot_key,
        head_digest: source_head_digest.as_ref(),
        platform_key: &platform_key,
        artifacts_config: &artifacts_config,
    })
    .await;

    (outcome, route)
}

/// Parameters for [`full_pull_and_build_tasks`], bundled to keep the argument
/// count under clippy's limit.
/// Parameters for [`check_targets_against_digest`].
struct TargetCheckParams<'a> {
    targets: &'a [TargetEntry],
    source: &'a ImageRef,
    target: &'a ImageRef,
    compare_digest: &'a Digest,
    /// Label for log messages (e.g., `"cache hit"`, `"head_first"`).
    label: &'static str,
}

/// Result of [`check_targets_against_digest`].
struct TargetCheckResult {
    /// Targets whose HEAD matched the compare digest (skipped).
    skipped: Vec<ImageResult>,
    /// Targets whose HEAD did not match or failed (need sync).
    mismatched: Vec<TargetEntry>,
}

/// HEAD-check all targets concurrently against a known digest, partitioning
/// them into matched (skipped) and mismatched (need sync).
///
/// Used by both the cache-hit path (comparing against the cached filtered
/// digest) and the `head_first` path (comparing against the source HEAD digest).
async fn check_targets_against_digest(params: TargetCheckParams<'_>) -> TargetCheckResult {
    let TargetCheckParams {
        targets,
        source,
        target,
        compare_digest,
        label,
    } = params;

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

    let mut skipped = Vec::new();
    let mut mismatched = Vec::new();

    while let Some((target_name, target_client, batch_checker, result)) = head_checks.next().await {
        match result {
            Ok(Some(head)) if head.digest == *compare_digest => {
                info!(
                    source_repo = %source.repo,
                    source_tag = %source.tag,
                    target_repo = %target.repo,
                    digest = %compare_digest,
                    "skipping -- digest matches at target ({label})"
                );
                skipped.push(skip_image_result(source, target, SkipReason::DigestMatch));
            }
            other => {
                if let Err(e) = &other {
                    warn!(
                        target_repo = %target.repo,
                        target_tag = %target.tag,
                        error = %e,
                        "target manifest HEAD failed during {label}, proceeding with sync"
                    );
                }
                mismatched.push(TargetEntry {
                    name: target_name,
                    client: target_client,
                    batch_checker,
                    existing_tags: HashSet::new(),
                });
            }
        }
    }

    TargetCheckResult {
        skipped,
        mismatched,
    }
}

struct FullPullParams<'a> {
    source_client: &'a Arc<RegistryClient>,
    source: &'a ImageRef,
    target: &'a ImageRef,
    targets: &'a [TargetEntry],
    skipped_results: Vec<ImageResult>,
    retry: &'a RetryConfig,
    platforms: Option<&'a [PlatformFilter]>,
    cache: &'a Rc<RefCell<TransferStateCache>>,
    snapshot_key: &'a SnapshotKey,
    head_digest: Option<&'a Digest>,
    platform_key: &'a PlatformFilterKey,
    artifacts_config: &'a Rc<ResolvedArtifacts>,
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
        cache,
        snapshot_key,
        head_digest,
        platform_key,
        artifacts_config,
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
            let status_code = err.status_code().map(|s| s.as_u16());
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
                        status_code,
                    },
                    bytes_transferred: 0,
                    blob_stats: BlobTransferStats::default(),
                    duration: Duration::ZERO,
                    artifacts_skipped: false,
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
            Ok(Some(head)) if head.digest == *source_digest => {
                info!(
                    source_repo = %source.repo,
                    source_tag = %source.tag,
                    target_repo = %target.repo,
                    digest = %source_digest,
                    "skipping -- digest matches at target"
                );
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
                    artifacts_config: Rc::clone(artifacts_config),
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
    preferred_mount_sources: &[RepositoryName],
    referrers_cache: &ReferrersCache,
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
        preferred_mount_sources,
    };
    let outcome = transfer_image_blobs(&ctx, &item.source_data, freq_counts).await;

    if let Some(err) = outcome.error {
        warn!(target_name = %item.target_name, error = %err, "blob transfer failed");
        // Unblock any followers waiting for this repo's manifest commit so
        // they fall through to HEAD+push.
        cache
            .borrow_mut()
            .notify_repo_failed(&item.target_name, &item.target.repo);
        return ImageResult {
            image_id: Uuid::now_v7(),
            source: item.source.to_string(),
            target: item.target.to_string(),
            status: ImageStatus::Failed {
                kind: ErrorKind::BlobTransfer,
                error: err.to_string(),
                retries: retry.max_retries,
                status_code: err.status_code().map(|s| s.as_u16()),
            },
            bytes_transferred: outcome.bytes_transferred,
            blob_stats: outcome.stats,
            duration: start.elapsed(),
            artifacts_skipped: false,
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
            // Mark this repo as having a committed manifest so it becomes
            // a valid cross-repo mount source. ECR requires a committed
            // manifest in the source repo for mount to succeed.
            cache
                .borrow_mut()
                .mark_repo_committed(&item.target_name, &item.target.repo);

            // Discover and sync artifacts (signatures, SBOMs, attestations).
            let artifacts_skipped = match discover_and_sync_artifacts(
                &item.source_client,
                &item.target_client,
                &item.source.repo,
                &item.target.repo,
                &item.source_data.pull.digest,
                &item.artifacts_config,
                retry,
                referrers_cache,
            )
            .await
            {
                Ok(skipped) => skipped,
                Err(err) => {
                    let (kind, retries) = if err.is_required_artifacts_missing() {
                        (ErrorKind::RequiredArtifactsMissing, 0)
                    } else {
                        (ErrorKind::ArtifactSync, retry.max_retries)
                    };
                    warn!(
                        source_repo = %item.source.repo,
                        target_repo = %item.target.repo,
                        error = %err,
                        "artifact sync failed"
                    );
                    return ImageResult {
                        image_id: Uuid::now_v7(),
                        source: item.source.to_string(),
                        target: item.target.to_string(),
                        status: ImageStatus::Failed {
                            kind,
                            error: err.to_string(),
                            retries,
                            status_code: None,
                        },
                        bytes_transferred: outcome.bytes_transferred,
                        blob_stats: outcome.stats,
                        duration: start.elapsed(),
                        artifacts_skipped: false,
                    };
                }
            };

            if artifacts_skipped {
                warn!(
                    source_repo = %item.source.repo,
                    target_repo = %item.target.repo,
                    "artifact discovery skipped due to transient error"
                );
            }

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
                artifacts_skipped,
            }
        }
        Err(err) => {
            // Unblock any followers waiting for this repo's manifest commit
            // so they fall through to HEAD+push.
            cache
                .borrow_mut()
                .notify_repo_failed(&item.target_name, &item.target.repo);

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
                    artifacts_skipped: false,
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
                        status_code: err.status_code().map(|s| s.as_u16()),
                    },
                    bytes_transferred: outcome.bytes_transferred,
                    blob_stats: outcome.stats,
                    duration: start.elapsed(),
                    artifacts_skipped: false,
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
                    return Err(crate::Error::ManifestLogic {
                        reference: tag.to_owned(),
                        reason: format!(
                            "platform filter [{}] matched no manifests in index (source has: [{}])",
                            filter_strs.join(", "),
                            available.join(", "),
                        ),
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

            // Pull all child manifests concurrently. On single-threaded
            // tokio these share the same event loop, so the registry's AIMD
            // controller naturally limits concurrency.
            let child_futures: Vec<_> = descriptors
                .iter()
                .map(|child_desc| {
                    let child_digest_str = child_desc.digest.to_string();
                    async move {
                        let child_pull = with_retry(retry, "manifest pull", || {
                            client.manifest_pull(repo, &child_digest_str)
                        })
                        .await
                        .map_err(|e| crate::Error::Manifest {
                            reference: child_digest_str.clone(),
                            source: e,
                        })?;
                        match &child_pull.manifest {
                            ManifestKind::Image(_) => Ok(child_pull),
                            ManifestKind::Index(_) => Err(crate::Error::ManifestLogic {
                                reference: child_digest_str,
                                reason: "nested index manifests are not supported".into(),
                            }),
                        }
                    }
                })
                .collect();
            let children = futures_util::future::try_join_all(child_futures).await?;

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
                let new_bytes = serde_json::to_vec(&filtered_index).map_err(|e| {
                    crate::Error::ManifestLogic {
                        reference: tag.to_owned(),
                        reason: format!("failed to serialize filtered index: {e}"),
                    }
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

/// Outcome of the atomic check-and-claim on a blob's in-progress state.
///
/// Used by [`execute_transfer_blobs`] to decide whether to wait for another
/// repository's in-flight upload or to claim the upload itself. See the
/// Step 2a comment for the reasoning.
enum ClaimAction {
    /// Another repository is currently uploading this blob; wait on `notify`
    /// then re-check. `uploader` is included for diagnostic logs.
    Wait {
        uploader: RepositoryName,
        notify: Rc<tokio::sync::Notify>,
    },
    /// This repository has claimed the upload (set in-progress state). Proceed
    /// with HEAD + push; on completion, call `notify_blob` to wake waiters.
    Claimed,
}

/// Write-only view of the transfer state cache for use inside the
/// per-image blob semaphore.
///
/// Exposes completion/failure signals and repo management but NOT wait
/// primitives (`repo_committed_watch`, `blob_in_progress_uploader`,
/// `blob_notify`). This boundary is the structural guarantee against
/// semaphore-exhaustion deadlocks: code holding a permit literally
/// cannot block on external signals because the type does not expose
/// the methods needed to do so.
struct BlobSink<'a> {
    cache: &'a Rc<RefCell<TransferStateCache>>,
    target_name: &'a RegistryAlias,
}

impl<'a> BlobSink<'a> {
    fn complete(&self, digest: Digest, repo: RepositoryName) {
        self.cache
            .borrow_mut()
            .set_blob_completed(self.target_name, digest, repo);
    }

    fn fail(&self, digest: Digest, error: String) {
        self.cache
            .borrow_mut()
            .set_blob_failed(self.target_name, digest, error);
    }

    fn exists(&self, digest: Digest, repo: RepositoryName) {
        self.cache
            .borrow_mut()
            .set_blob_exists(self.target_name, digest, repo);
    }

    fn notify(&self, digest: &Digest) {
        self.cache.borrow().notify_blob(self.target_name, digest);
    }

    fn remove_repo(&self, digest: &Digest, repo: &RepositoryName) {
        self.cache
            .borrow_mut()
            .remove_blob_repo(self.target_name, digest, repo);
    }
}

/// I/O context for blob transfer operations under the semaphore.
///
/// Provides access to registry clients and the write-only [`BlobSink`]
/// for cache updates. Does NOT provide access to cache read/wait methods,
/// making it structurally impossible to block on external signals while
/// holding a semaphore permit.
struct BlobIoContext<'a> {
    sink: BlobSink<'a>,
    source_client: &'a RegistryClient,
    source_repo: &'a RepositoryName,
    target_client: &'a RegistryClient,
    target_repo: &'a RepositoryName,
    retry: &'a RetryConfig,
    staging: &'a Rc<BlobStage>,
}

impl<'a> BlobIoContext<'a> {
    fn from_ctx(ctx: &'a TransferContext<'a>) -> Self {
        Self {
            sink: BlobSink {
                cache: ctx.cache,
                target_name: ctx.target_name,
            },
            source_client: ctx.source_client,
            source_repo: ctx.source_repo,
            target_client: ctx.target_client,
            target_repo: ctx.target_repo,
            retry: ctx.retry,
            staging: ctx.staging,
        }
    }
}

/// Pre-semaphore staging resolution. The staging dedup wait
/// (`Notify::notified()`) runs outside the semaphore via
/// [`resolve_staging`], preventing permit exhaustion.
#[derive(Clone, Copy)]
enum StagingClaim {
    /// Staging disabled -- use direct source-pull + target-push.
    Disabled,
    /// Blob already staged by another task -- read from disk.
    Staged,
    /// This task claimed the source pull -- pull and stage, then push.
    /// If the blob is resolved via mount or HEAD before the pull happens,
    /// the caller MUST call `staging.notify_failed(digest)` to unblock
    /// other tasks waiting on this digest's staging notification.
    Pull,
}

/// Context for transferring blobs from source to a single target.
///
/// Bundles the parameters needed by [`transfer_image_blobs`] to keep
/// the function signature under the clippy argument limit.
///
/// Used in the pre-semaphore phase (claim, mount-source resolution,
/// staging claim) where full cache access is needed. After the semaphore
/// is acquired, [`BlobIoContext`] provides a restricted view that
/// prevents external waits.
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
    /// Leader repos whose manifests are committed. Preferred as mount sources.
    preferred_mount_sources: &'a [RepositoryName],
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
    // per-blob loop then hits cache at Step 1 for existing blobs and skips
    // the per-blob HEAD at Step 3 for absent blobs (both were already
    // checked by the batch API, so per-blob HEAD is redundant).
    //
    // TOCTOU: a blob could appear between the batch-check and the per-blob
    // push loop. This is harmless - the redundant push succeeds because
    // the registry deduplicates on content-addressable digest.
    let batch_checked: HashSet<Digest> = if let Some(checker) = ctx.batch_checker {
        let all_digests: Vec<Digest> = blobs.iter().map(|b| b.digest.clone()).collect();
        match checker
            .check_blob_existence(ctx.target_repo, &all_digests)
            .await
        {
            Ok(existing) => {
                let existing_count = existing.len();
                {
                    let mut c = ctx.cache.borrow_mut();
                    for digest in &existing {
                        c.set_blob_exists(
                            ctx.target_name,
                            digest.clone(),
                            ctx.target_repo.to_owned(),
                        );
                        c.notify_blob(ctx.target_name, digest);
                    }
                }
                // Record all checked digests so the per-blob loop can skip
                // HEAD for absent blobs too (batch already confirmed absent).
                let checked: HashSet<Digest> = all_digests.into_iter().collect();
                debug!(
                    target_name = %ctx.target_name,
                    repo = %ctx.target_repo,
                    total = checked.len(),
                    existing = existing_count,
                    "batch check pre-populated cache"
                );
                checked
            }
            Err(e) => {
                warn!(
                    target_name = %ctx.target_name,
                    error = %e,
                    "batch check failed, falling back to per-blob HEAD"
                );
                // Graceful degradation: continue with per-blob HEAD checks.
                HashSet::new()
            }
        }
    } else {
        HashSet::new()
    };

    // Transfer blobs concurrently, capped by BLOB_CONCURRENCY.
    let blob_sem = Semaphore::new(BLOB_CONCURRENCY);
    let cancel = Cell::new(false);
    let mut blob_futures = FuturesUnordered::new();

    for blob in &blobs {
        let digest = blob.digest.clone();
        let size = blob.size;
        // Reborrow shared state as Copy references for `async move`.
        let sem = &blob_sem;
        let cancel = &cancel;
        let batch = &batch_checked;
        blob_futures.push(async move {
            // === PRE-SEMAPHORE PHASE ===
            //
            // All external waits (Notify, watch) run here where no
            // semaphore permit is held. `ctx` (full TransferContext with
            // cache read/wait access) is only used in this phase.
            let claim = wait_for_blob_claim(ctx, &digest, cancel).await;
            match claim {
                BlobClaim::Skipped => return BlobResult::Skipped,
                BlobClaim::Cancelled => return BlobResult::Cancelled,
                BlobClaim::Claimed => {}
            }
            let mount_source = resolve_mount_source(ctx, &digest).await;
            let staging_claim = resolve_staging(ctx.staging, &digest).await;

            // === SEMAPHORE BOUNDARY ===
            //
            // `run_under_semaphore` takes `BlobIoContext` (not
            // `TransferContext`), so `ctx` is unreachable inside the
            // permit scope. `BlobSink` has no repo_committed_watch,
            // no blob_in_progress_uploader, no blob_notify -- adding
            // an external wait inside the semaphore is a compile error.
            let _permit = sem.acquire().await.unwrap();
            run_under_semaphore(
                &BlobIoContext::from_ctx(ctx),
                &digest,
                size,
                batch,
                cancel,
                mount_source,
                staging_claim,
            )
            .await
        });
    }

    let mut outcome = TargetBlobOutcome::default();
    while let Some(result) = blob_futures.next().await {
        match result {
            BlobResult::Skipped => outcome.stats.skipped += 1,
            BlobResult::Mounted => outcome.stats.mounted += 1,
            BlobResult::Transferred { bytes } => {
                outcome.bytes_transferred += bytes;
                outcome.stats.transferred += 1;
            }
            BlobResult::Cancelled => {}
            BlobResult::Failed(err) => {
                cancel.set(true);
                outcome.error = Some(err);
                // Don't break - drain remaining futures so permits are
                // released and cancel flag takes effect.
            }
        }
    }

    outcome
}

/// Result of the pre-semaphore blob claim phase.
enum BlobClaim {
    /// Blob already exists at target -- skip.
    Skipped,
    /// Cancel flag was set -- abort.
    Cancelled,
    /// Blob claimed for upload by this task.
    Claimed,
}

/// Cache check + claim loop, run OUTSIDE the per-image blob semaphore.
///
/// This must not hold a semaphore permit because the claim-wait
/// (`Notify::notified().await`) can block indefinitely waiting for another
/// image to finish uploading a shared blob. If all semaphore permits were
/// consumed by claim-waits, the image could not make progress on its own
/// claimed blobs, creating a cross-image deadlock.
async fn wait_for_blob_claim(
    ctx: &TransferContext<'_>,
    digest: &Digest,
    cancel: &Cell<bool>,
) -> BlobClaim {
    // Step 1: Check cache -- known at this repo -> skip (0 API calls).
    let skip = {
        let c = ctx.cache.borrow();
        c.blob_known_at_repo(ctx.target_name, digest, ctx.target_repo)
    };
    if skip {
        return BlobClaim::Skipped;
    }

    // Step 2a: Atomic check-and-claim.
    loop {
        if cancel.get() {
            return BlobClaim::Cancelled;
        }
        let action: ClaimAction = {
            let mut c = ctx.cache.borrow_mut();
            match c
                .blob_in_progress_uploader(ctx.target_name, digest, ctx.target_repo)
                .cloned()
            {
                Some(uploader) => ClaimAction::Wait {
                    uploader,
                    notify: c.blob_notify(ctx.target_name, digest),
                },
                None => {
                    c.set_blob_in_progress(
                        ctx.target_name,
                        digest.clone(),
                        ctx.target_repo.to_owned(),
                    );
                    ClaimAction::Claimed
                }
            }
        };
        match action {
            ClaimAction::Wait { uploader, notify } => {
                debug!(
                    %digest,
                    %uploader,
                    target = %ctx.target_name,
                    "waiting for in-flight upload to complete before mounting",
                );
                notify.notified().await;
                continue;
            }
            ClaimAction::Claimed => return BlobClaim::Claimed,
        }
    }
}

/// Resolve staging dedup claim, run OUTSIDE the per-image blob semaphore.
///
/// The wait (`Notify::notified()`) blocks until another task finishes
/// pulling the same blob from source. Running it under the semaphore
/// would consume a permit for the entire duration of another task's
/// source pull, contributing to the same semaphore-exhaustion deadlock
/// class as blob claim waits and repo-committed waits.
async fn resolve_staging(staging: &Rc<BlobStage>, digest: &Digest) -> StagingClaim {
    if !staging.is_enabled() {
        return StagingClaim::Disabled;
    }
    loop {
        match staging.claim_or_check(digest) {
            crate::staging::StagePullAction::Exists => return StagingClaim::Staged,
            crate::staging::StagePullAction::Pull => return StagingClaim::Pull,
            crate::staging::StagePullAction::Wait(notify) => {
                notify.notified().await;
                continue;
            }
        }
    }
}

/// Resolve the cross-repo mount source for a claimed blob, run OUTSIDE the
/// per-image blob semaphore.
///
/// This wait can block for the entire duration of a leader's blob transfers
/// and manifest push. Running it under the semaphore would consume all
/// `BLOB_CONCURRENCY` permits when enough blobs wait on the same leader,
/// preventing the image from uploading other blobs that could break the
/// dependency chain. This is the same class of deadlock as the blob claim
/// wait (see [`wait_for_blob_claim`] and `sync_cross_image_blob_claim_no_deadlock`).
///
/// Returns the repository to attempt a cross-repo mount from, or `None` if
/// no mount source is available and the caller should proceed to HEAD+push.
async fn resolve_mount_source(
    ctx: &TransferContext<'_>,
    digest: &Digest,
) -> Option<RepositoryName> {
    // Cross-repo mount. Prefer leader repos (committed manifests)
    // over other followers (whose manifests may not be committed yet).
    //
    // ECR requires the source repo to have a committed manifest for a mount
    // to return 201. When `blob_mount_source` returns an uncommitted leader
    // repo (Tier 3), followers wait for the leader's manifest to commit
    // rather than sending a mount request that will be rejected with 202.
    // This avoids wasted round-trips and the `remove_blob_repo` cascade
    // that would permanently discard a valid mount source.
    //
    // Leaders must NOT wait on other leaders' `repo_committed_watch` --
    // this creates a circular dependency when two leaders share blobs and
    // each waits for the other's manifest commit before its own blobs can
    // finish. Leaders instead fall through to the direct mount attempt
    // (which may get 202 on ECR, triggering HEAD+push fallback).
    let is_leader = ctx.preferred_mount_sources.contains(ctx.target_repo);
    let (source, is_committed) = {
        let c = ctx.cache.borrow();
        let source = c
            .blob_mount_source(
                ctx.target_name,
                digest,
                ctx.target_repo,
                ctx.preferred_mount_sources,
            )
            .cloned();
        let committed = source
            .as_ref()
            .is_some_and(|r| c.is_repo_committed(ctx.target_name, r));
        (source, committed)
    };

    match source {
        // No mount source available.
        None => None,
        // Source has a committed manifest (Tier 1/2) -- mount immediately.
        Some(repo) if is_committed => Some(repo),
        // Source is an uncommitted leader and we are a follower -- wait
        // for the leader's manifest commit. Leaders skip this branch to
        // avoid circular waits (leader A waiting on leader B while B
        // waits on A).
        Some(repo) if !is_leader && ctx.preferred_mount_sources.contains(&repo) => {
            debug!(
                %digest,
                %repo,
                target = %ctx.target_name,
                "mount source uncommitted, waiting for leader manifest commit"
            );
            let mut rx = {
                let mut c = ctx.cache.borrow_mut();
                c.repo_committed_watch(ctx.target_name, &repo)
            };
            // watch::Receiver::wait_for returns immediately if the leader
            // already committed -- no signal can ever be lost, regardless
            // of polling order in FuturesUnordered.
            if rx.wait_for(|&v| v).await.is_err() {
                debug!(%digest, %repo, "repo-committed watch sender dropped");
            }

            // Leader committed or failed -- re-query mount source.
            let c = ctx.cache.borrow();
            if c.is_repo_committed(ctx.target_name, &repo) {
                c.blob_mount_source(
                    ctx.target_name,
                    digest,
                    ctx.target_repo,
                    ctx.preferred_mount_sources,
                )
                .cloned()
            } else {
                // Leader failed; fall through to HEAD+push.
                debug!(
                    %digest,
                    %repo,
                    target = %ctx.target_name,
                    "leader manifest push failed, skipping mount"
                );
                None
            }
        }
        // Uncommitted non-leader source, or we are a leader ourselves
        // (Tier 3 fallback) -- attempt mount directly. Non-ECR registries
        // may fulfill mounts without a committed manifest; on ECR the 202
        // triggers HEAD+push fallback.
        Some(repo) => Some(repo),
    }
}

/// Entry point for the semaphore-bounded phase of blob transfer.
///
/// Takes [`BlobIoContext`] (not [`TransferContext`]), making it impossible
/// to call cache wait methods (`repo_committed_watch`,
/// `blob_in_progress_uploader`, `blob_notify`) inside the semaphore scope.
/// This is the compile-time enforcement boundary against semaphore-exhaustion
/// deadlocks.
async fn run_under_semaphore(
    io: &BlobIoContext<'_>,
    digest: &Digest,
    size: u64,
    batch_checked: &HashSet<Digest>,
    cancel: &Cell<bool>,
    mount_source: Option<RepositoryName>,
    staging: StagingClaim,
) -> BlobResult {
    if cancel.get() {
        if matches!(staging, StagingClaim::Pull) {
            io.staging.notify_failed(digest);
        }
        io.sink.fail(digest.clone(), "cancelled".into());
        io.sink.notify(digest);
        return BlobResult::Cancelled;
    }
    transfer_claimed_blob(io, digest, size, batch_checked, mount_source, staging).await
}

/// Transfer a claimed blob to one target: mount, HEAD, pull+push.
///
/// The blob MUST already be claimed via [`wait_for_blob_claim`]. This
/// function runs under the per-image blob semaphore to bound concurrent
/// I/O. All `RefCell` borrows are dropped before `await` points.
///
/// Called via [`run_under_semaphore`], which is the type boundary that
/// prevents cache wait methods from being accessible inside the semaphore.
async fn transfer_claimed_blob(
    io: &BlobIoContext<'_>,
    digest: &Digest,
    size: u64,
    batch_checked: &HashSet<Digest>,
    mount_source: Option<RepositoryName>,
    staging: StagingClaim,
) -> BlobResult {
    let mut mount_attempted = false;

    if let Some(from_repo) = mount_source {
        debug!(%digest, %from_repo, target = %io.sink.target_name, "attempting mount");
        match io
            .target_client
            .blob_mount(io.target_repo, digest, &from_repo)
            .await
        {
            Ok(MountResult::Mounted) => {
                if matches!(staging, StagingClaim::Pull) {
                    io.staging.notify_failed(digest);
                }
                io.sink.complete(digest.clone(), io.target_repo.to_owned());
                io.sink.notify(digest);
                return BlobResult::Mounted;
            }
            Ok(MountResult::NotMounted) | Err(_) => {
                debug!(%digest, %from_repo, target = %io.sink.target_name, "mount not fulfilled, falling back to HEAD+push");
                // Remove the stale mount source from repos, but keep the
                // blob entry as InProgress. This task already owns the claim
                // and will proceed with HEAD+push. Removing only the stale
                // repo prevents future mount attempts from retrying it,
                // while keeping the entry prevents concurrent waiters from
                // re-claiming and starting duplicate pushes.
                io.sink.remove_repo(digest, &from_repo);
                mount_attempted = true;
            }
        }
    }

    if !batch_checked.contains(digest) && !mount_attempted {
        let head_result = io.target_client.blob_exists(io.target_repo, digest).await;
        match head_result {
            Ok(Some(_)) => {
                if matches!(staging, StagingClaim::Pull) {
                    io.staging.notify_failed(digest);
                }
                io.sink.exists(digest.clone(), io.target_repo.to_owned());
                io.sink.notify(digest);
                return BlobResult::Skipped;
            }
            Ok(None) => {}
            Err(e) => {
                debug!(
                    %digest,
                    target = %io.sink.target_name,
                    error = %e,
                    "blob HEAD failed, proceeding with push"
                );
            }
        }
    }

    let transfer_result: Result<(), crate::Error> = match staging {
        StagingClaim::Pull => {
            // This task claimed the source pull. Pull to disk, then push.
            if let Err(e) = with_retry(io.retry, "blob pull (to stage)", || async {
                let mut writer =
                    io.staging
                        .begin_write(digest)
                        .map_err(|e| ocync_distribution::Error::Io {
                            context: "staging create",
                            source: e,
                        })?;
                let stream = io.source_client.blob_pull(io.source_repo, digest).await?;
                futures_util::pin_mut!(stream);
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    writer
                        .write_chunk(&chunk)
                        .map_err(|e| ocync_distribution::Error::Io {
                            context: "staging write",
                            source: e,
                        })?;
                }
                writer.finish().map_err(|e| ocync_distribution::Error::Io {
                    context: "staging finalize",
                    source: e,
                })?;
                Ok::<(), ocync_distribution::Error>(())
            })
            .await
            {
                io.staging.notify_failed(digest);
                let err = crate::Error::BlobTransfer {
                    digest: digest.clone(),
                    source: e,
                };
                io.sink.fail(digest.clone(), err.to_string());
                io.sink.notify(digest);
                return BlobResult::Failed(err);
            }
            io.staging.notify_staged(digest);

            push_staged_blob(io, digest).await
        }
        StagingClaim::Staged => {
            // Another task already staged this blob. Read from disk, push.
            push_staged_blob(io, digest).await
        }
        StagingClaim::Disabled => {
            // No staging -- direct pull from source + push to target.
            with_retry(io.retry, "blob transfer", || async {
                let stream = io.source_client.blob_pull(io.source_repo, digest).await?;
                io.target_client
                    .blob_push_stream(io.target_repo, digest, Some(size), stream)
                    .await
            })
            .await
            .map(|_| ())
            .map_err(|e| crate::Error::BlobTransfer {
                digest: digest.clone(),
                source: e,
            })
        }
    };

    match transfer_result {
        Ok(()) => {
            io.sink.complete(digest.clone(), io.target_repo.to_owned());
            io.sink.notify(digest);
            BlobResult::Transferred { bytes: size }
        }
        Err(err) => {
            io.sink.fail(digest.clone(), err.to_string());
            io.sink.notify(digest);
            BlobResult::Failed(err)
        }
    }
}

/// Push a staged blob from disk to the target registry.
async fn push_staged_blob(io: &BlobIoContext<'_>, digest: &Digest) -> Result<(), crate::Error> {
    with_retry(io.retry, "blob push (staged)", || async {
        let file = io
            .staging
            .open_read(digest)
            .map_err(|e| ocync_distribution::Error::Io {
                context: "staging open",
                source: e,
            })?;
        let file_size = file.metadata().map(|m| m.len()).ok();
        let stream = file_read_stream(file).map(|r| {
            r.map_err(|e| ocync_distribution::Error::Io {
                context: "staging read",
                source: e,
            })
        });
        io.target_client
            .blob_push_stream(io.target_repo, digest, file_size, stream)
            .await
    })
    .await
    .map(|_| ())
    .map_err(|e| crate::Error::BlobTransfer {
        digest: digest.clone(),
        source: e,
    })
}

/// Push all manifests (children for indexes, then top-level) to one target.
async fn push_manifests(
    retry: &RetryConfig,
    target_client: &RegistryClient,
    target_repo: &RepositoryName,
    target_tag: &str,
    source_data: &PulledManifest,
) -> Result<(), crate::Error> {
    // For index manifests, push all children concurrently by digest.
    // The target registry's AIMD controller gates concurrency naturally.
    // Uses join_all (not try_join_all) so that a transient failure on one
    // child does not cancel in-flight pushes for the other children.
    let child_push_futures: Vec<_> = source_data
        .children
        .iter()
        .map(|child| {
            let child_digest_str = child.digest.to_string();
            async move {
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
                })
            }
        })
        .collect();
    let push_results = futures_util::future::join_all(child_push_futures).await;
    // Return the first error after all futures have completed.
    for result in push_results {
        result?;
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

/// Discover referrers for a synced manifest and transfer matching artifacts.
///
/// Implements the two-step fallback described in the design doc:
/// 1. Try the referrers API (`GET /v2/{repo}/referrers/{digest}`)
/// 2. If 404, try tag fallback (`GET /v2/{repo}/manifests/{algo}-{hex}`)
/// 3. If neither works, log INFO and continue (no artifacts available)
///
/// For each matching artifact: push blobs, then push manifest (preserving
/// the `subject` reference to the parent).
///
/// Returns `Ok(true)` when artifact discovery was skipped due to a transient
/// error (the image synced but artifacts may be missing at the target).
#[allow(clippy::too_many_arguments)]
async fn discover_and_sync_artifacts(
    source_client: &RegistryClient,
    target_client: &RegistryClient,
    source_repo: &RepositoryName,
    target_repo: &RepositoryName,
    parent_digest: &Digest,
    artifacts_config: &ResolvedArtifacts,
    retry: &RetryConfig,
    referrers_cache: &ReferrersCache,
) -> Result<bool, crate::Error> {
    if !artifacts_config.enabled {
        return Ok(false);
    }

    // Check the per-run referrers cache to avoid redundant discovery when
    // the same source manifest is synced to multiple targets.
    let cache_key = (source_repo.clone(), parent_digest.clone());
    let cached = referrers_cache.borrow().get(&cache_key).cloned();

    let (referrers_index, discovery_succeeded) = if let Some(cached_entry) = cached {
        debug!(
            repo = %source_repo,
            digest = %parent_digest,
            "using cached referrers discovery result"
        );
        match cached_entry {
            CachedReferrers::Found(index) => (Some(index), true),
            CachedReferrers::NotFound => (None, true),
            CachedReferrers::Failed => (None, false),
        }
    } else {
        let (index, succeeded) =
            discover_referrers(source_client, source_repo, parent_digest).await;
        // Cache the result for subsequent targets.
        let to_cache = match &index {
            Some(idx) => CachedReferrers::Found(idx.clone()),
            None if succeeded => CachedReferrers::NotFound,
            None => CachedReferrers::Failed,
        };
        referrers_cache.borrow_mut().insert(cache_key, to_cache);
        (index, succeeded)
    };

    // Filter matching descriptors. When `artifact_type` is None (e.g. for
    // tag-fallback single-manifest referrers), falls back to media_type per
    // OCI 1.1 artifact guidance.
    let matching: Vec<&Descriptor> = match referrers_index {
        Some(ref index) => index
            .manifests
            .iter()
            .filter(|desc| {
                let artifact_type = desc
                    .artifact_type
                    .as_deref()
                    .unwrap_or(desc.media_type.as_str());
                artifacts_config.type_matches(artifact_type)
            })
            .collect(),
        None => Vec::new(),
    };

    // require_artifacts enforcement: only fire when we successfully confirmed
    // zero referrers, not when discovery itself failed (transient error).
    if artifacts_config.require_artifacts && matching.is_empty() && discovery_succeeded {
        return Err(crate::Error::RequiredArtifactsMissing {
            reference: parent_digest.to_string(),
        });
    }

    if matching.is_empty() {
        // Transient discovery failure -> artifacts_skipped = true.
        // Successful discovery with zero referrers -> false.
        return Ok(!discovery_succeeded);
    }

    info!(
        repo = %source_repo,
        digest = %parent_digest,
        count = matching.len(),
        "syncing artifacts"
    );

    // Transfer each matching artifact: pull manifest, push blobs, push manifest.
    for desc in &matching {
        let artifact_digest_str = desc.digest.to_string();

        // Pull the artifact manifest from source.
        let artifact_pull = with_retry(retry, "artifact manifest pull", || {
            source_client.manifest_pull(source_repo, &artifact_digest_str)
        })
        .await
        .map_err(|e| crate::Error::ArtifactSync {
            reference: artifact_digest_str.clone(),
            reason: format!("manifest pull failed: {e}"),
        })?;

        // Push artifact blobs.
        if let ManifestKind::Image(ref manifest) = artifact_pull.manifest {
            let blobs = collect_image_blobs(manifest);
            for blob in blobs {
                // HEAD check target first.
                let exists = target_client
                    .blob_exists(target_repo, &blob.digest)
                    .await
                    .unwrap_or(None);

                if exists.is_some() {
                    continue;
                }

                // Pull from source, push to target.
                let blob_digest = blob.digest.clone();
                let blob_size = blob.size;
                let stream = with_retry(retry, "artifact blob pull", || {
                    source_client.blob_pull(source_repo, &blob_digest)
                })
                .await
                .map_err(|e| crate::Error::ArtifactSync {
                    reference: artifact_digest_str.clone(),
                    reason: format!("blob pull failed for {blob_digest}: {e}"),
                })?;

                target_client
                    .blob_push_stream(target_repo, &blob_digest, Some(blob_size), stream)
                    .await
                    .map_err(|e| crate::Error::ArtifactSync {
                        reference: artifact_digest_str.clone(),
                        reason: format!("blob push failed for {blob_digest}: {e}"),
                    })?;
            }
        }

        // Push artifact manifest by digest (not by tag).
        with_retry(retry, "artifact manifest push", || {
            target_client.manifest_push(
                target_repo,
                &artifact_digest_str,
                &artifact_pull.media_type,
                &artifact_pull.raw_bytes,
            )
        })
        .await
        .map_err(|e| crate::Error::ArtifactSync {
            reference: artifact_digest_str.clone(),
            reason: format!("manifest push failed: {e}"),
        })?;

        debug!(
            repo = %source_repo,
            artifact_digest = %artifact_digest_str,
            "artifact synced"
        );
    }

    Ok(false)
}

/// Discover referrers for a manifest from the source registry.
///
/// Tries the referrers API first, falls back to the tag-based lookup.
/// Returns `(Option<ImageIndex>, discovery_succeeded)`.
async fn discover_referrers(
    source_client: &RegistryClient,
    source_repo: &RepositoryName,
    parent_digest: &Digest,
) -> (Option<ImageIndex>, bool) {
    let mut discovery_succeeded = true;

    // Step 1: Try referrers API.
    let index = match source_client
        .referrers(source_repo, parent_digest, None)
        .await
    {
        Ok(Some(index)) => Some(index),
        Ok(None) => {
            // 404 - try tag fallback (step 2).
            let fallback_tag = parent_digest.tag_fallback();
            debug!(
                repo = %source_repo,
                digest = %parent_digest,
                fallback_tag = %fallback_tag,
                "referrers API returned 404, trying tag fallback"
            );
            match source_client
                .manifest_pull(source_repo, &fallback_tag)
                .await
            {
                Ok(pull) => match pull.manifest {
                    ManifestKind::Index(index) => Some(*index),
                    ManifestKind::Image(_) => {
                        // A single image manifest at the fallback tag is
                        // itself an artifact referrer. Wrap in a synthetic
                        // single-entry index so the filter + transfer loop
                        // handles it uniformly.
                        let desc = Descriptor {
                            media_type: pull.media_type.clone(),
                            digest: pull.digest.clone(),
                            size: pull.raw_bytes.len() as u64,
                            platform: None,
                            artifact_type: None,
                            annotations: None,
                        };
                        Some(ImageIndex {
                            schema_version: 2,
                            media_type: Some(MediaType::OciIndex),
                            manifests: vec![desc],
                            subject: None,
                            artifact_type: None,
                            annotations: None,
                        })
                    }
                },
                Err(e) if e.is_not_found() => {
                    info!(
                        repo = %source_repo,
                        digest = %parent_digest,
                        "no referrers found (API 404, tag fallback 404)"
                    );
                    None
                }
                Err(e) => {
                    // Non-404 error on fallback is not fatal; log and continue.
                    info!(
                        repo = %source_repo,
                        digest = %parent_digest,
                        error = %e,
                        "tag fallback failed, skipping artifact sync"
                    );
                    discovery_succeeded = false;
                    None
                }
            }
        }
        Err(e) => {
            // Non-404 error from referrers API. Log and continue rather
            // than failing the entire image sync for an artifact query.
            info!(
                repo = %source_repo,
                digest = %parent_digest,
                error = %e,
                "referrers API failed, skipping artifact sync"
            );
            discovery_succeeded = false;
            None
        }
    };

    (index, discovery_succeeded)
}

/// Read a file in 256 KB chunks, yielding a stream of `Bytes`.
///
/// Uses synchronous `std::fs::Read` internally. On a single-threaded tokio
/// runtime, each individual read call blocks for microseconds (local disk),
/// which is negligible compared to network RTT.
///
/// # Assumption: local filesystem
///
/// This function performs blocking I/O on the tokio `current_thread` runtime.
/// The staging directory MUST reside on local disk (tmpfs, ext4, APFS, etc.).
/// Network filesystems (NFS, EFS, CIFS) can block for milliseconds to seconds
/// per read, stalling the entire event loop. See [`BlobStage`] documentation
/// for staging path requirements.
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

/// Retry an async operation with exponential backoff on transient errors.
///
/// Calls `f()` in a loop. Retries on HTTP 408/429/5xx status codes and on
/// transport-level errors (connection refused, DNS failure, request timeout).
/// Waits with jittered exponential backoff up to `config.max_retries` times.
/// Returns the first `Ok` or the final `Err`.
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
                let retryable = if let Some(status) = e.status_code() {
                    retry::should_retry(status, attempt, config.max_retries)
                } else {
                    // Transport-level errors (connection refused, DNS failure,
                    // request timeout) are retryable when attempts remain.
                    attempt < config.max_retries && retry::should_retry_transport(&e)
                };

                if retryable {
                    let backoff = config.backoff_for(attempt);
                    warn!(
                        operation,
                        attempt,
                        error = %e,
                        backoff_ms = backoff.as_millis(),
                        "retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                    continue;
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
        if image.artifacts_skipped {
            stats.artifacts_skipped += 1;
        }
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
            artifacts_skipped: false,
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
                    status_code: None,
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

    // --- elect_leaders tests ---

    /// Build a `PulledManifest` with the given blob digests (first = config, rest = layers).
    fn test_pulled_manifest(blob_suffixes: &[&str]) -> PulledManifest {
        assert!(!blob_suffixes.is_empty(), "need at least a config blob");
        let config = Descriptor {
            size: 100,
            ..test_descriptor(test_digest(blob_suffixes[0]), MediaType::OciConfig)
        };
        let layers: Vec<Descriptor> = blob_suffixes[1..]
            .iter()
            .map(|s| Descriptor {
                size: 1000,
                ..test_descriptor(test_digest(s), MediaType::OciLayerGzip)
            })
            .collect();
        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config,
            layers,
            subject: None,
            artifact_type: None,
            annotations: None,
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let digest = Digest::from_sha256(Sha256::digest(&raw));
        PulledManifest {
            pull: ManifestPull {
                manifest: ManifestKind::Image(Box::new(manifest)),
                raw_bytes: raw,
                media_type: MediaType::OciManifest,
                digest,
            },
            children: Vec::new(),
        }
    }

    fn test_client() -> Arc<RegistryClient> {
        Arc::new(
            RegistryClient::builder("https://test.example.com".parse().unwrap())
                .build()
                .unwrap(),
        )
    }

    fn test_task(source_data: Rc<PulledManifest>, tag: &str) -> TransferTask {
        let client = test_client();
        TransferTask {
            source_data,
            source_client: Arc::clone(&client),
            target_name: RegistryAlias::new("test-target"),
            target_client: client,
            source: ImageRef {
                repo: RepositoryName::new(format!("source/{tag}")).unwrap(),
                tag: tag.to_owned(),
            },
            target: ImageRef {
                repo: RepositoryName::new(format!("target/{tag}")).unwrap(),
                tag: tag.to_owned(),
            },
            batch_checker: None,
            artifacts_config: Rc::new(ResolvedArtifacts::default()),
        }
    }

    /// Collect blob digests from leader tasks at the front of the deque.
    fn leader_blob_digests(pending: &VecDeque<TransferTask>, n: usize) -> HashSet<Digest> {
        pending
            .iter()
            .take(n)
            .flat_map(|t| {
                blobs_from_manifest(&t.source_data)
                    .into_iter()
                    .map(|d| d.digest.clone())
            })
            .collect()
    }

    #[test]
    fn elect_leaders_empty_deque() {
        let mut pending = VecDeque::new();
        let n = elect_leaders(&mut pending);
        assert_eq!(n, 0);
    }

    #[test]
    fn elect_leaders_single_task() {
        let data = Rc::new(test_pulled_manifest(&["c1", "a1"]));
        let mut pending = VecDeque::new();
        pending.push_back(test_task(data, "img1"));
        let n = elect_leaders(&mut pending);
        assert_eq!(n, 0);
    }

    #[test]
    fn elect_leaders_picks_most_shared() {
        // base:    blobs {c0, a1, a2, a3} - 4 blobs, base layer image
        // child_a: blobs {c0, a1, a2, d1} - shares 3 with base, 2 with child_b
        // child_b: blobs {c0, a1, a3, e1} - shares 3 with base, 2 with child_a
        //
        // Scoring (sum of |intersection| with each other image):
        //   base:    |base^child_a|=3 + |base^child_b|=3 = 6
        //   child_a: |child_a^base|=3 + |child_a^child_b|=2 = 5
        //   child_b: |child_b^base|=3 + |child_b^child_a|=2 = 5
        // base wins.
        let data_base = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "a3"]));
        let data_ca = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "d1"]));
        let data_cb = Rc::new(test_pulled_manifest(&["c0", "a1", "a3", "e1"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&data_base), "base"));
        pending.push_back(test_task(Rc::clone(&data_ca), "child_a"));
        pending.push_back(test_task(Rc::clone(&data_cb), "child_b"));

        let n = elect_leaders(&mut pending);

        // base is the sole leader (score 6 > 5).
        assert_eq!(n, 1);
        assert_eq!(pending[0].source.tag, "base");
        // Leader blobs are base's: {c0, a1, a2, a3}.
        let leader_blobs = leader_blob_digests(&pending, n);
        assert_eq!(leader_blobs.len(), 4);
        assert!(leader_blobs.contains(&test_digest("c0")));
        assert!(leader_blobs.contains(&test_digest("a3")));
        // Followers preserve original order.
        assert_eq!(pending[1].source.tag, "child_a");
        assert_eq!(pending[2].source.tag, "child_b");
    }

    #[test]
    fn elect_leaders_no_shared_blobs() {
        // Three images with completely disjoint blob sets.
        let data_a = Rc::new(test_pulled_manifest(&["c1", "aa"]));
        let data_b = Rc::new(test_pulled_manifest(&["c2", "bb"]));
        let data_c = Rc::new(test_pulled_manifest(&["c3", "cc"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(data_a, "imgA"));
        pending.push_back(test_task(data_b, "imgB"));
        pending.push_back(test_task(data_c, "imgC"));

        // No shared blobs -> 0 leaders.
        let n = elect_leaders(&mut pending);
        assert_eq!(n, 0);
    }

    #[test]
    fn elect_leaders_multi_target_groups_by_rc() {
        // "base" image targets 2 registries (2 tasks, same Rc).
        // "child_a" and "child_b" derive from base.
        //
        // base:    blobs {c0, a1, a2, a3} - shares 3 with each child
        // child_a: blobs {c0, a1, a2, d1} - shares 3 with base, 2 with child_b
        // child_b: blobs {c0, a1, a3, e1} - shares 3 with base, 2 with child_a
        //
        // base scores 6, children score 5 each. base elected.
        // base has 2 tasks -> n = 2.
        let base = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "a3"]));
        let child_a = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "d1"]));
        let child_b = Rc::new(test_pulled_manifest(&["c0", "a1", "a3", "e1"]));

        let client_a = test_client();
        let client_b = test_client();

        let mut pending = VecDeque::new();
        // Two tasks sharing the same Rc (same source image, two targets).
        pending.push_back(TransferTask {
            source_data: Rc::clone(&base),
            source_client: Arc::clone(&client_a),
            target_name: RegistryAlias::new("target-1"),
            target_client: Arc::clone(&client_a),
            source: ImageRef {
                repo: RepositoryName::new("src/base").unwrap(),
                tag: "v1".into(),
            },
            target: ImageRef {
                repo: RepositoryName::new("tgt/base").unwrap(),
                tag: "v1".into(),
            },
            batch_checker: None,
            artifacts_config: Rc::new(ResolvedArtifacts::default()),
        });
        pending.push_back(TransferTask {
            source_data: Rc::clone(&base),
            source_client: Arc::clone(&client_b),
            target_name: RegistryAlias::new("target-2"),
            target_client: Arc::clone(&client_b),
            source: ImageRef {
                repo: RepositoryName::new("src/base").unwrap(),
                tag: "v1".into(),
            },
            target: ImageRef {
                repo: RepositoryName::new("tgt/base").unwrap(),
                tag: "v1".into(),
            },
            batch_checker: None,
            artifacts_config: Rc::new(ResolvedArtifacts::default()),
        });
        pending.push_back(test_task(child_a, "child_a"));
        pending.push_back(test_task(child_b, "child_b"));

        let n = elect_leaders(&mut pending);

        // base wins (score 6 vs 5). Both base tasks are leaders.
        assert_eq!(n, 2);
        let leader_blobs = leader_blob_digests(&pending, n);
        assert_eq!(leader_blobs.len(), 4); // {c0, a1, a2, a3}
        assert_eq!(pending[0].source.tag, "v1");
        assert_eq!(pending[1].source.tag, "v1");
        assert_eq!(pending[2].source.tag, "child_a");
        assert_eq!(pending[3].source.tag, "child_b");
    }

    #[test]
    fn elect_leaders_two_clusters() {
        // Cluster 1: images A and B share blobs {c0, f1}
        // Cluster 2: images C and D share blobs {c9, f2}
        // No overlap between clusters.
        let data_a = Rc::new(test_pulled_manifest(&["c0", "f1", "a1"]));
        let data_b = Rc::new(test_pulled_manifest(&["c0", "f1", "b1"]));
        let data_c = Rc::new(test_pulled_manifest(&["c9", "f2", "d1"]));
        let data_d = Rc::new(test_pulled_manifest(&["c9", "f2", "e1"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&data_a), "imgA"));
        pending.push_back(test_task(Rc::clone(&data_b), "imgB"));
        pending.push_back(test_task(Rc::clone(&data_c), "imgC"));
        pending.push_back(test_task(Rc::clone(&data_d), "imgD"));

        let n = elect_leaders(&mut pending);

        // Two leaders (one per cluster), two followers.
        assert_eq!(n, 2);
        // Leader blobs span both clusters.
        let leader_blobs = leader_blob_digests(&pending, n);
        assert!(
            leader_blobs.len() >= 4,
            "leader blobs should span both clusters"
        );

        // Leaders are at the front.
        let leader_tags: Vec<&str> = pending
            .iter()
            .take(n)
            .map(|t| t.source.tag.as_str())
            .collect();
        let follower_tags: Vec<&str> = pending
            .iter()
            .skip(n)
            .map(|t| t.source.tag.as_str())
            .collect();

        // Each cluster contributes one leader: A or B from cluster 1, C or D from cluster 2.
        // The follower from each cluster is the other member.
        assert_eq!(leader_tags.len(), 2);
        assert_eq!(follower_tags.len(), 2);

        // Verify leaders come from different clusters.
        let cluster1 = ["imgA", "imgB"];
        let cluster2 = ["imgC", "imgD"];
        assert!(
            leader_tags.iter().any(|t| cluster1.contains(t))
                && leader_tags.iter().any(|t| cluster2.contains(t)),
            "leaders should cover both clusters: {leader_tags:?}"
        );
    }

    #[test]
    fn elect_leaders_marginal_coverage_across_rounds() {
        // A spans both clusters via blob c0. Round 1 elects A. Round 2
        // must deduct A's blobs from marginal scores: C and D share
        // {c0, d0}, but c0 is already covered - only d0 is marginal.
        //
        // A: {a0, b0, c0}  B: {a0, b0, e0}  C: {c0, d0, f0}  D: {c0, d0, fa}
        //
        // Round 1 scores:
        //   A: |A^B|=2 + |A^C|=1 + |A^D|=1 = 4  (wins)
        //   B: 2, C: 3, D: 3
        //
        // Round 2 (covered = {a0,b0,c0}):
        //   B: |B^C\covered|=0 + |B^D\covered|=0 = 0
        //   C: |C^B\covered|=0 + |C^D\covered|=|{d0}|=1 = 1
        //   D: |D^B\covered|=0 + |D^C\covered|=|{d0}|=1 = 1
        //   C or D elected (tie on d0). B stays follower.
        let data_a = Rc::new(test_pulled_manifest(&["a0", "b0", "c0"]));
        let data_b = Rc::new(test_pulled_manifest(&["a0", "b0", "e0"]));
        let data_c = Rc::new(test_pulled_manifest(&["c0", "d0", "f0"]));
        let data_d = Rc::new(test_pulled_manifest(&["c0", "d0", "fa"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&data_a), "imgA"));
        pending.push_back(test_task(Rc::clone(&data_b), "imgB"));
        pending.push_back(test_task(Rc::clone(&data_c), "imgC"));
        pending.push_back(test_task(Rc::clone(&data_d), "imgD"));

        let n = elect_leaders(&mut pending);

        // Two leaders: A (round 1) + one of C/D (round 2, marginal on d0).
        assert_eq!(n, 2);
        // Leader blobs include A's {a0,b0,c0} + winner's {c0,d0,f0 or fa}.
        let leader_blobs = leader_blob_digests(&pending, n);
        assert!(leader_blobs.contains(&test_digest("a0")));
        assert!(leader_blobs.contains(&test_digest("d0")));

        let leader_tags: Vec<&str> = pending
            .iter()
            .take(n)
            .map(|t| t.source.tag.as_str())
            .collect();

        // A is always first leader.
        assert_eq!(leader_tags[0], "imgA");
        // Second leader is from cluster 2 (C or D), not B - because B
        // shares no marginal blobs after A's {a0,b0,c0} are deducted.
        assert!(
            leader_tags[1] == "imgC" || leader_tags[1] == "imgD",
            "second leader should be from cluster 2, got: {leader_tags:?}"
        );
    }

    #[test]
    fn elect_leaders_tie_breaks_deterministically() {
        // A and B have identical sharing with C. max_by_key returns the
        // last maximum in iteration order, so B (higher index) wins.
        //
        // A: {a0, b0}  B: {a0, b0}  C: {a0, c0}
        // A: |A^B|=2 + |A^C|=1 = 3
        // B: |B^A|=2 + |B^C|=1 = 3   (tie with A)
        // C: |C^A|=1 + |C^B|=1 = 2
        let data_a = Rc::new(test_pulled_manifest(&["a0", "b0"]));
        let data_b = Rc::new(test_pulled_manifest(&["a0", "b0"]));
        let data_c = Rc::new(test_pulled_manifest(&["a0", "c0"]));

        for _ in 0..5 {
            let mut pending = VecDeque::new();
            pending.push_back(test_task(Rc::clone(&data_a), "imgA"));
            pending.push_back(test_task(Rc::clone(&data_b), "imgB"));
            pending.push_back(test_task(Rc::clone(&data_c), "imgC"));

            let n = elect_leaders(&mut pending);
            assert_eq!(n, 1);
            // max_by_key picks last maximum: B (index 1) over A (index 0).
            assert_eq!(
                pending[0].source.tag, "imgB",
                "tie should break deterministically in favor of later candidate"
            );
        }
    }

    #[test]
    fn elect_leaders_minimal_set_covers_all_follower_shared_blobs() {
        // Verify the greedy election picks the minimal leader set such that
        // every shared blob of every follower is present in the leader union.
        //
        // Setup: 5 images with overlapping blob sets designed so that 2
        // leaders suffice to cover all shared blobs.
        //
        // img1: {10, a1, a2, a3, f1}  -- shares a1,a2,a3 broadly
        // img2: {20, a1, a2, f2}       -- shares a1,a2 with img1
        // img3: {30, a2, a3, f3}       -- shares a2,a3 with img1
        // img4: {40, b4, b5, f4}       -- disjoint cluster
        // img5: {50, b4, b5, f5}       -- shares b4,b5 with img4
        //
        // Optimal: img1 covers {a1,a2,a3} for cluster 1, img4 or img5 covers
        // {b4,b5} for cluster 2. Two leaders total.
        let data1 = Rc::new(test_pulled_manifest(&["10", "a1", "a2", "a3", "f1"]));
        let data2 = Rc::new(test_pulled_manifest(&["20", "a1", "a2", "f2"]));
        let data3 = Rc::new(test_pulled_manifest(&["30", "a2", "a3", "f3"]));
        let data4 = Rc::new(test_pulled_manifest(&["40", "b4", "b5", "f4"]));
        let data5 = Rc::new(test_pulled_manifest(&["50", "b4", "b5", "f5"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&data1), "img1"));
        pending.push_back(test_task(Rc::clone(&data2), "img2"));
        pending.push_back(test_task(Rc::clone(&data3), "img3"));
        pending.push_back(test_task(Rc::clone(&data4), "img4"));
        pending.push_back(test_task(Rc::clone(&data5), "img5"));

        let n = elect_leaders(&mut pending);

        // Exactly 2 leaders: one per cluster.
        assert_eq!(
            n, 2,
            "should elect exactly 2 leaders for 2 disjoint clusters"
        );

        // Collect the leader blob union.
        let leader_blobs = leader_blob_digests(&pending, n);

        // Verify every shared blob of every follower is in the leader union.
        // Shared blobs are those appearing in more than one image.
        let all_blob_sets: Vec<HashSet<Digest>> = pending
            .iter()
            .map(|t| {
                blobs_from_manifest(&t.source_data)
                    .into_iter()
                    .map(|d| d.digest.clone())
                    .collect()
            })
            .collect();

        for (i, task) in pending.iter().skip(n).enumerate() {
            let follower_blobs: HashSet<Digest> = blobs_from_manifest(&task.source_data)
                .into_iter()
                .map(|d| d.digest.clone())
                .collect();
            // A blob is "shared" if it appears in at least one other image.
            for blob in &follower_blobs {
                let shared_with_others = all_blob_sets
                    .iter()
                    .enumerate()
                    .any(|(j, set)| j != (i + n) && set.contains(blob));
                if shared_with_others {
                    assert!(
                        leader_blobs.contains(blob),
                        "follower {} has shared blob {} not covered by leaders",
                        task.source.tag,
                        blob
                    );
                }
            }
        }
    }

    // --- ResolvedArtifacts::type_matches tests ---

    #[test]
    fn type_matches_no_filters_passes_everything() {
        let config = ResolvedArtifacts::default();
        assert!(config.type_matches("application/vnd.dev.cosign.artifact.sig.v1+json"));
        assert!(config.type_matches("application/spdx+json"));
        assert!(config.type_matches("anything"));
    }

    #[test]
    fn type_matches_include_filters() {
        let config = ResolvedArtifacts {
            enabled: true,
            include: vec!["application/spdx+json".to_string()],
            exclude: Vec::new(),
            require_artifacts: false,
        };
        assert!(config.type_matches("application/spdx+json"));
        assert!(
            !config.type_matches("application/vnd.dev.cosign.artifact.sig.v1+json"),
            "type not in include list should be rejected"
        );
    }

    #[test]
    fn type_matches_exclude_filters() {
        let config = ResolvedArtifacts {
            enabled: true,
            include: Vec::new(),
            exclude: vec!["application/spdx+json".to_string()],
            require_artifacts: false,
        };
        assert!(
            !config.type_matches("application/spdx+json"),
            "excluded type should be rejected"
        );
        assert!(config.type_matches("application/vnd.dev.cosign.artifact.sig.v1+json"));
    }

    #[test]
    fn type_matches_include_and_exclude() {
        let config = ResolvedArtifacts {
            enabled: true,
            include: vec![
                "application/spdx+json".to_string(),
                "application/vnd.dev.cosign.artifact.sig.v1+json".to_string(),
            ],
            exclude: vec!["application/spdx+json".to_string()],
            require_artifacts: false,
        };
        // spdx is in both include and exclude - exclude wins.
        assert!(
            !config.type_matches("application/spdx+json"),
            "exclude should take priority over include"
        );
        assert!(config.type_matches("application/vnd.dev.cosign.artifact.sig.v1+json"));
    }

    // -- DiscoveryCounters --

    #[test]
    fn discovery_counters_head_failure_increments_both_misses_and_failures() {
        let mut c = DiscoveryCounters::default();
        c.record(&DiscoveryRoute::HeadFailure);
        assert_eq!(c.cache_misses, 1, "HeadFailure must count as a cache miss");
        assert_eq!(c.head_failures, 1);
        assert_eq!(c.cache_hits, 0);
        assert_eq!(c.target_stale, 0);
        assert_eq!(c.head_first_skips, 0);
    }

    #[test]
    fn discovery_counters_target_stale_increments_both_misses_and_stale() {
        let mut c = DiscoveryCounters::default();
        c.record(&DiscoveryRoute::TargetStale);
        assert_eq!(c.cache_misses, 1, "TargetStale must count as a cache miss");
        assert_eq!(c.target_stale, 1);
        assert_eq!(c.cache_hits, 0);
        assert_eq!(c.head_failures, 0);
        assert_eq!(c.head_first_skips, 0);
    }

    #[test]
    fn write_timing_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timing.jsonl");
        let mut file = Some(std::fs::File::create(&path).unwrap());
        let start = Instant::now();

        write_timing(&mut file, "discovery_start", &start);
        write_timing(&mut file, "execution_complete", &start);

        drop(file);
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // Verify each line is valid JSON with expected fields.
        for (i, phase) in ["discovery_start", "execution_complete"].iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(lines[i]).unwrap();
            assert_eq!(v["phase"].as_str().unwrap(), *phase);
            assert!(v["elapsed_ms"].as_u64().is_some());
        }
    }

    #[test]
    fn write_timing_noop_when_none() {
        let mut file: Option<std::fs::File> = None;
        let start = Instant::now();
        // Should not panic or error.
        write_timing(&mut file, "test_phase", &start);
    }

    #[test]
    fn discovery_counters_all_routes() {
        let mut c = DiscoveryCounters::default();
        c.record(&DiscoveryRoute::CacheHit);
        c.record(&DiscoveryRoute::CacheMiss);
        c.record(&DiscoveryRoute::HeadFailure);
        c.record(&DiscoveryRoute::TargetStale);
        c.record(&DiscoveryRoute::HeadFirstSkip);
        assert_eq!(c.cache_hits, 1);
        // CacheMiss + HeadFailure + TargetStale = 3 total cache_misses
        assert_eq!(c.cache_misses, 3);
        assert_eq!(c.head_failures, 1);
        assert_eq!(c.target_stale, 1);
        assert_eq!(c.head_first_skips, 1);
    }
}
