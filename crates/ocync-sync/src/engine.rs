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
//!   (tag, target) pair, bounded by a global `Semaphore`. Blob existence is
//!   checked inline during execution, progressively populating the cache.
//!
//! All concurrency is cooperative (single-threaded tokio runtime). Shared mutable
//! state (`TransferStateCache`) is accessed via `Rc<RefCell<>>` with borrows
//! never held across `.await` points.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
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
use crate::error::ManifestRefusal;
pub use crate::plan::{RegistryAlias, TargetId};
use crate::progress::RunProgress;
use crate::retry::{RetryConfig, count_retries, retries_made, with_retry};
use crate::shared::{
    Claim, ClaimGuard, CopyKey, PerRun, Referrers, claim, copy_key, run_once, source_key,
    target_key,
};
use crate::shutdown::ShutdownSignal;
use crate::staging::BlobStage;
use crate::{
    BlobTransferStats, ErrorKind, ImageResult, ImageStatus, SkipReason, SyncReport, SyncStats,
};

/// Writes a JSONL timing event (`{"phase":"<name>","elapsed_ms":<N>}`) to the
/// optional `OCYNC_TIMING_FILE` benchmark file. No-ops when `file` is `None`.
///
/// The phase name is written unescaped, which spares this crate a
/// `serde_json` dependency for a two-field line.
fn write_timing(file: &mut Option<std::fs::File>, phase: TimingPhase, start: &Instant) {
    if let Some(f) = file {
        use std::io::Write;
        let _ = writeln!(
            f,
            r#"{{"phase":"{}","elapsed_ms":{}}}"#,
            phase.as_str(),
            start.elapsed().as_millis()
        );
    }
}

/// A phase boundary recorded in the `OCYNC_TIMING_FILE` benchmark file.
#[derive(Clone, Copy)]
enum TimingPhase {
    DiscoveryStart,
    DiscoveryComplete,
    ExecutionComplete,
}

impl TimingPhase {
    /// The name written to the timing file.
    fn as_str(self) -> &'static str {
        match self {
            Self::DiscoveryStart => "discovery_start",
            Self::DiscoveryComplete => "discovery_complete",
            Self::ExecutionComplete => "execution_complete",
        }
    }
}

/// A repository and tag within one registry, displayed as `repo:tag`.
///
/// Not a full OCI reference: the registry is tracked separately via the
/// associated [`RegistryClient`].
#[derive(Debug, Clone)]
struct ImageRef {
    repo: RepositoryName,
    tag: String,
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.repo, self.tag)
    }
}

/// Resolved artifact sync configuration: whether the engine transfers OCI
/// referrers (signatures, SBOMs, attestations) after each image manifest.
#[derive(Debug, Clone)]
pub struct ResolvedArtifacts {
    /// Whether artifact discovery and transfer is enabled.
    pub enabled: bool,
    /// Only artifact types in this list pass; empty means all pass.
    pub include: Vec<String>,
    /// Artifact types in this list never pass, even if also in `include`.
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

/// A fully resolved mapping ready for the sync engine: registry lookup and tag
/// filtering are already done.
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
    /// When `Some`, index manifests keep only descriptors whose platform
    /// matches a filter; child manifests and blobs for other platforms are
    /// never pulled from the source.
    pub platforms: Option<Vec<PlatformFilter>>,
    /// When true, HEAD-check all targets against the source HEAD digest
    /// before the full source manifest GET on a cache miss, so a target that
    /// is already in sync costs no source GET (saves rate-limit tokens on
    /// registries such as Docker Hub). Reuses the discovery source HEAD.
    pub head_first: bool,
    /// Compiled glob pattern identifying immutable tags.
    ///
    /// A tag matching it that exists in every target's
    /// [`existing_tags`](TargetEntry::existing_tags) is skipped with zero API
    /// calls.
    pub immutable_glob: Option<globset::GlobSet>,
    /// Artifact sync configuration (shared across all tasks for this mapping).
    pub artifacts_config: Rc<ResolvedArtifacts>,
    /// Number of source tags considered before filtering. `None` on the
    /// exact-tag fast path where source tags are not enumerated.
    pub candidate_count: Option<usize>,
    /// Filter pipeline trace. `Some` only when `--dry-run` requested a report.
    pub filter_report: Option<crate::filter::FilterReport>,
}

impl ResolvedMapping {
    /// Returns `true` when `tag` matches the configured immutable glob AND
    /// already exists at every target.
    ///
    /// Returns `false` when there is no glob, the tag does not match, any
    /// target lacks the tag, or `targets` is empty (where `.all()` would be
    /// vacuously `true`).
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
pub struct TargetEntry {
    /// Registry identifier (unique per [`ResolvedMapping`]) and the address
    /// `client` serves. Private so the two cannot be set apart from the client.
    id: TargetId,
    /// Client for this target registry. Private with `id`, whose address it
    /// serves.
    client: Arc<RegistryClient>,
    /// Optional batch blob checker for this target (ECR batch API). When
    /// present, one batch call pre-populates the cache instead of per-blob HEADs.
    pub batch_checker: Option<Rc<dyn BatchBlobChecker>>,
    /// Tags already present at this target at resolution time.
    ///
    /// Populated only when `immutable_glob` is configured on the parent
    /// [`ResolvedMapping`]. Empty when it is absent or `list_tags` failed
    /// (degraded: the immutable skip is disabled for this target and
    /// discovery falls through to HEAD checks).
    pub existing_tags: HashSet<String>,
}

impl TargetEntry {
    /// An entry for the registry `name` reached through `client`, with no
    /// existing tags. The cache key pairs `name` with the client's address.
    pub fn new(
        name: RegistryAlias,
        client: Arc<RegistryClient>,
        batch_checker: Option<Rc<dyn BatchBlobChecker>>,
    ) -> Self {
        Self {
            id: TargetId::new(name, client.registry_authority().clone()),
            client,
            batch_checker,
            existing_tags: HashSet::new(),
        }
    }

    /// The registry identifier.
    pub fn name(&self) -> &RegistryAlias {
        self.id.alias()
    }

    /// The transfer state cache key.
    pub fn id(&self) -> &TargetId {
        &self.id
    }

    /// The client for this target registry.
    pub fn client(&self) -> &Arc<RegistryClient> {
        &self.client
    }
}

impl Clone for TargetEntry {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            client: Arc::clone(&self.client),
            batch_checker: self.batch_checker.clone(),
            existing_tags: self.existing_tags.clone(),
        }
    }
}

impl fmt::Debug for TargetEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetEntry")
            .field("id", &self.id)
            .field("client", &self.client)
            .field("batch_checker", &self.batch_checker.as_ref().map(|_| ".."))
            .field("existing_tags", &self.existing_tags.len())
            .finish()
    }
}

/// A source/target tag pair; the tags differ only for `copy` with retagging.
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

impl fmt::Display for TagPair {
    /// Renders as the bare tag when source and target match, otherwise
    /// `source -> target`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.source == self.target {
            f.write_str(&self.source)
        } else {
            write!(f, "{} -> {}", self.source, self.target)
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

/// Build an `ImageResult` with zero transfer and zero duration; callers that
/// moved bytes override those fields with struct update syntax.
fn image_result(source: &ImageRef, target: &ImageRef, status: ImageStatus) -> ImageResult {
    ImageResult {
        image_id: Uuid::now_v7(),
        source: source.to_string(),
        target: target.to_string(),
        status,
        bytes_transferred: 0,
        blob_stats: BlobTransferStats::default(),
        duration: Duration::ZERO,
        artifacts_skipped: false,
    }
}

/// The failed status for `err`, with the retries this task made.
fn failed_status(kind: ErrorKind, err: &crate::Error) -> ImageStatus {
    ImageStatus::Failed {
        kind,
        error: err.to_string(),
        retries: retries_made(),
        status_code: err.status_code().map(|s| s.as_u16()),
    }
}

/// Source manifest data pulled once per tag, however many targets it fans out to.
struct PulledManifest {
    pull: ManifestPull,
    /// For index manifests: the pulled child manifests, shared with every
    /// other tag whose index lists the same child. Empty for single images.
    children: Vec<Rc<ManifestPull>>,
}

/// Per-target outcome of the blob transfer phase.
#[derive(Default)]
struct TargetBlobOutcome {
    bytes_transferred: u64,
    stats: BlobTransferStats,
    /// `Some` if a target-specific push failed. Other targets are unaffected.
    error: Option<crate::Error>,
}

impl TargetBlobOutcome {
    /// The image's result, carrying what its transfer moved and how long it took.
    fn into_result(
        self,
        source: &ImageRef,
        target: &ImageRef,
        status: ImageStatus,
        start: Instant,
    ) -> ImageResult {
        ImageResult {
            bytes_transferred: self.bytes_transferred,
            blob_stats: self.stats,
            duration: start.elapsed(),
            ..image_result(source, target, status)
        }
    }

    /// Fold in a refused manifest's repair. A blob the repair sent was
    /// skipped earlier on an earlier run's word, so it moves from skipped to
    /// sent; one the repair skipped is already counted.
    fn absorb_repair(&mut self, repair: &TargetBlobOutcome) {
        let sent = repair.stats.transferred + repair.stats.mounted;
        self.bytes_transferred += repair.bytes_transferred;
        self.stats.skipped = self.stats.skipped.saturating_sub(sent);
        self.stats.transferred += repair.stats.transferred;
        self.stats.mounted += repair.stats.mounted;
    }
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
    /// The target registry: its alias keys in-run state and labels logs, its
    /// id keys the transfer state cache, and it carries the client to push
    /// with and the optional batch checker.
    target_entry: Rc<TargetEntry>,
    /// Source image reference (repo + tag, for display/logging and blob pulls).
    source: ImageRef,
    /// Target image reference (repo + tag, for pushing manifests and blobs).
    target: ImageRef,
    /// Artifact sync configuration (Rc-shared with mapping).
    artifacts_config: Rc<ResolvedArtifacts>,
    /// Retries made discovering this image, counted into its total.
    discovery_retries: u32,
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

/// Which route `discover_tag` took, counted into `SyncStats`.
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
/// Tasks sharing a [`PulledManifest`] `Rc` are one source image bound to
/// different targets. Repeatedly picks the image covering the most shared blob
/// digests not already covered by earlier leaders, stopping when no candidate
/// adds coverage. Leader tasks move to the front of the deque; two leaders
/// sharing a blob are deduplicated by `ClaimAction::Wait` in
/// `transfer_image_blobs` (the second waits, then mounts).
///
/// Returns the number of tasks promoted as leaders (0 if election has no benefit).
fn elect_leaders(pending: &mut VecDeque<TransferTask>) -> usize {
    if pending.len() <= 1 {
        return 0;
    }

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

    // Each round picks the image with the most marginal shared-blob coverage.
    // The inverted index `blob_remaining_count` maps each blob to the number
    // of remaining groups containing it, so a candidate's marginal gain from a
    // blob outside `leader_blob_union` is `count - 1`. That costs O(n * b_avg)
    // per round instead of O(n^2 * b) for a pairwise scan. Must stay
    // sub-cubic; `elect_leaders_scales_subcubically` pins it.
    let mut leader_set: Vec<usize> = Vec::new();
    let mut leader_blob_union: HashSet<Digest> = HashSet::new();
    let mut remaining: Vec<usize> = (0..num_groups).collect();

    let mut blob_remaining_count: HashMap<Digest, usize> = HashMap::new();
    for &i in &remaining {
        for d in &group_blobs[i] {
            *blob_remaining_count.entry(d.clone()).or_insert(0) += 1;
        }
    }

    loop {
        if remaining.len() <= 1 {
            break;
        }

        let best = remaining
            .iter()
            .map(|&i| {
                let marginal: usize = group_blobs[i]
                    .iter()
                    .filter(|d| !leader_blob_union.contains(*d))
                    .map(|d| blob_remaining_count.get(d).copied().unwrap_or(0) - 1)
                    .sum();
                (i, marginal)
            })
            .max_by_key(|&(_, m)| m);

        match best {
            Some((idx, marginal)) if marginal > 0 => {
                for d in &group_blobs[idx] {
                    if let Some(c) = blob_remaining_count.get_mut(d) {
                        *c -= 1;
                    }
                }
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

/// Blob frequency tracker: blobs shared across many images are transferred
/// first so the cache benefits the most follow-on images.
struct BlobFrequencyMap {
    counts: HashMap<Digest, usize>,
}

impl BlobFrequencyMap {
    fn new() -> Self {
        Self {
            counts: HashMap::new(),
        }
    }

    fn record(&mut self, digest: &Digest) {
        *self.counts.entry(digest.clone()).or_insert(0) += 1;
    }

    /// Frequency count for a digest (0 if unknown).
    fn count(&self, digest: &Digest) -> usize {
        self.counts.get(digest).copied().unwrap_or(0)
    }
}

/// Accumulates discovery route counters during the pipeline loop.
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
    /// `HeadFailure` and `TargetStale` also count as `cache_misses`: they are
    /// cache misses classified further by cause.
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

/// Pipeline phase, advancing monotonically `Discovering -> Done`.
///
/// Leaders are ordered first by `elect_leaders` so they take semaphore
/// permits and claim blob uploads before followers. Correctness then rests on
/// two waits: a follower waits on a leader's per-blob claim
/// (`ClaimAction::Wait`) before claiming the same blob, and on the leader's
/// manifest commit before a cross-repo mount (ECR requires a committed
/// manifest in the source repo for a mount to succeed).
enum PromotionPhase {
    /// Discovery futures still in flight; tasks accumulate in `pending`.
    Discovering,
    /// All tasks promoted, leaders and followers together.
    Done,
}

/// Engine state every promoted task needs, borrowed by the `promote` closure.
/// Created after discovery completes, once `freq_map` is frozen.
struct PromoteContext<'a> {
    sem: &'a Rc<Semaphore>,
    cache: &'a Rc<RefCell<TransferStateCache>>,
    staging: &'a Rc<BlobStage>,
    freq_map: &'a BlobFrequencyMap,
    retry: &'a RetryConfig,
    shared: &'a Rc<PerRun>,
}

/// Default cap for concurrent image transfers (the global image semaphore).
///
/// Per-target `RegistryClient::blob_upload_sem` caps long-lived HTTP/2 blob
/// streams at 64 (registries advertise `SETTINGS_MAX_CONCURRENT_STREAMS` of
/// 100-128, less metadata headroom). With `BLOB_CONCURRENCY` = 6 per image,
/// 10 images gives 10 * 6 = 60 streams at saturation, inside that budget.
/// A cap of 50 (300 streams) deadlocks HTTP/2 against every registry probed
/// (see `DEFAULT_BLOB_UPLOAD_CONCURRENCY` in `ocync-distribution`). A higher
/// value only helps workloads that mostly skip or mount, via config.
pub const DEFAULT_MAX_CONCURRENT_TRANSFERS: NonZeroUsize = NonZeroUsize::new(10).unwrap();

/// Maximum concurrent blob transfers within a single image (skopeo's default).
/// Each `RegistryClient` separately caps uploads and downloads across images.
const BLOB_CONCURRENCY: usize = 6;

/// Default shutdown drain deadline in seconds.
const DEFAULT_DRAIN_DEADLINE_SECS: u64 = 25;

/// Default per-blob wait for a source repo's manifest commit before a
/// cross-repo mount.
///
/// Measured on the 55 GB benchmark corpus to ECR at caps 20 and 50: an image
/// needed up to 149 s for the repo it mounts from to commit, because that repo
/// also uploads its own large layers. A shorter wait uploads the blob again.
const DEFAULT_MOUNT_SOURCE_WAIT: Duration = Duration::from_secs(180);

/// Default cadence for [`crate::progress::ProgressReporter::run_heartbeat`] callbacks.
///
/// Discovery and execution can each run for minutes without an image reaching
/// a terminal state, so without a callback a slow run looks like a wedged one.
const DEFAULT_HEARTBEAT_SECS: u64 = 30;

/// Sync engine: orchestrates concurrent image transfers across registries.
///
/// Discovery futures drain first via `tokio::select!`, then leader-follower
/// election reorders pending tasks so images sharing the most blobs execute
/// first (enabling cross-repo mounts for followers).
#[derive(Debug)]
pub struct SyncEngine {
    retry: RetryConfig,
    max_concurrent: NonZeroUsize,
    drain_deadline: Duration,
    source_head_timeout: Duration,
    mount_source_wait_deadline: Duration,
    heartbeat_interval: Duration,
    /// Whether the end-of-run prune may treat absent entries as deleted.
    prune_cache: bool,
}

impl SyncEngine {
    /// Create a new sync engine with the given retry configuration and concurrency cap.
    ///
    /// `max_concurrent` bounds the in-flight image sync futures in the execute
    /// phase (see [`DEFAULT_MAX_CONCURRENT_TRANSFERS`]). It is non-zero because
    /// a zero-permit semaphore would never admit an image.
    pub fn new(retry: RetryConfig, max_concurrent: NonZeroUsize) -> Self {
        Self {
            retry,
            max_concurrent,
            drain_deadline: Duration::from_secs(DEFAULT_DRAIN_DEADLINE_SECS),
            source_head_timeout: Duration::from_secs(5),
            mount_source_wait_deadline: DEFAULT_MOUNT_SOURCE_WAIT,
            heartbeat_interval: Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
            prune_cache: true,
        }
    }

    /// Set the maximum time to wait for in-flight transfers to complete after
    /// a shutdown signal is received. Defaults to 25 seconds.
    pub fn with_drain_deadline(mut self, deadline: Duration) -> Self {
        self.drain_deadline = deadline;
        self
    }

    /// Set the timeout for the optimization source HEAD request (default 5
    /// seconds); on expiry the engine falls through to the full source pull.
    pub fn with_source_head_timeout(mut self, timeout: Duration) -> Self {
        self.source_head_timeout = timeout;
        self
    }

    /// Configure the per-blob mount-source wait deadline (default 180 seconds).
    ///
    /// A follower waiting on a leader's manifest-commit watch falls back to
    /// push on expiry instead of hanging.
    pub fn with_mount_source_wait_deadline(mut self, deadline: Duration) -> Self {
        self.mount_source_wait_deadline = deadline;
        self
    }

    /// Configure how often [`crate::progress::ProgressReporter::run_heartbeat`] fires while the
    /// run still has work in flight. Default 30 seconds.
    ///
    /// # Panics
    ///
    /// Panics on [`Duration::ZERO`] - a zero-period interval would spin the
    /// engine's `select!` loop.
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "heartbeat interval must be greater than zero"
        );
        self.heartbeat_interval = interval;
        self
    }

    /// Whether the end-of-run cache prune may run. Default `true`.
    ///
    /// The prune derives "still live" from the mappings it was handed, so it
    /// cannot tell a mapping deleted from config from one that failed to
    /// resolve this run. Callers that dropped work before the engine started
    /// must pass `false`, or a single transient denial discards the cached
    /// state for everything it could not reach and the next run re-fetches it.
    pub fn with_cache_pruning(mut self, prune: bool) -> Self {
        self.prune_cache = prune;
        self
    }

    /// Run the sync engine across all resolved mappings.
    ///
    /// Discovery futures feed active items into a pending queue; execution
    /// futures drain it, bounded by a semaphore.
    ///
    /// `staging` enables disk-based blob reuse for multi-target mappings; pass
    /// `BlobStage::disabled` for single-target runs to pay zero overhead.
    ///
    /// If `shutdown` is `Some`, the engine stops accepting new work when the
    /// signal fires, drains in-flight transfers up to the drain deadline
    /// (default 25s, see [`with_drain_deadline`](Self::with_drain_deadline)),
    /// then returns. `None` runs to completion.
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
        cache.borrow_mut().start_run();

        // Synchronous writes are fine: current_thread runtime, file is tiny.
        let mut timing_file = std::env::var("OCYNC_TIMING_FILE")
            .ok()
            .and_then(|path| std::fs::File::create(&path).ok());
        write_timing(&mut timing_file, TimingPhase::DiscoveryStart, &run_start);

        let mut discovery_futures = FuturesUnordered::new();
        let mut execution_futures: FuturesUnordered<
            std::pin::Pin<Box<dyn Future<Output = ImageResult>>>,
        > = FuturesUnordered::new();
        let mut pending: VecDeque<TransferTask> = VecDeque::new();
        let mut freq_map = BlobFrequencyMap::new();
        let global_sem = Rc::new(Semaphore::new(self.max_concurrent.get()));
        let staging = Rc::new(staging);
        let shared = Rc::new(PerRun::default());
        let mut results: Vec<ImageResult> = Vec::new();
        let mut shutting_down = false;
        let mut drain_deadline: Option<tokio::time::Instant> = None;
        let mut discovery_counters = DiscoveryCounters::default();

        let mut promotion_phase = PromotionPhase::Discovering;

        // Budget circuit breaker: pause discovery when a source registry's
        // rate-limit remaining drops below 10% of the remaining discovery count.
        // Execution continues for already-discovered images, and discovery
        // resumes once a response shows the budget has refilled.
        //
        // Pause is all-or-nothing: ANY source below threshold pauses ALL
        // discovery, because `FuturesUnordered` cannot poll selectively. The
        // anti-stall resume (nothing executing) prevents deadlock.
        let mut discovery_paused = false;
        // Unique source clients, deduped by Arc pointer identity.
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

        // `interval` yields its first tick immediately; `reset` pushes it out a
        // full period so the first callback lands after the run has been quiet
        // that long.
        let mut heartbeat = tokio::time::interval(self.heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.reset();
        // The `select!` branch only fires on a poll where no work was ready,
        // so a steady completion stream would starve it. This deadline is
        // also checked from the work arms, which guarantees a cadence.
        let mut next_heartbeat = tokio::time::Instant::now() + self.heartbeat_interval;

        for mapping in &mappings {
            let targets: Vec<Rc<TargetEntry>> =
                mapping.targets.iter().cloned().map(Rc::new).collect();
            for tag_pair in &mapping.tags {
                // Tier 1: immutable tag skip (0 API calls).
                if mapping.should_skip_immutable(&tag_pair.target) {
                    debug!(
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
                        let r = image_result(
                            &source_ref,
                            &target_ref,
                            ImageStatus::Skipped {
                                reason: SkipReason::ImmutableTag,
                            },
                        );
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
                    targets: targets.clone(),
                    retry: self.retry.clone(),
                    platforms: mapping.platforms.clone(),
                    cache: Rc::clone(&cache),
                    source_head_timeout: self.source_head_timeout,
                    head_first: mapping.head_first,
                    artifacts_config: mapping.artifacts_config.clone(),
                    shared: Rc::clone(&shared),
                };

                discovery_futures.push(async move { count_retries(0, discover_tag(params)).await });
            }
        }

        let mount_source_wait_deadline = self.mount_source_wait_deadline;
        let promote = |item: TransferTask,
                       ctx: &PromoteContext<'_>|
         -> std::pin::Pin<Box<dyn Future<Output = ImageResult>>> {
            let sem = Rc::clone(ctx.sem);
            let cache_ref = Rc::clone(ctx.cache);
            let retry = ctx.retry.clone();
            let discovery_retries = item.discovery_retries;
            let freq_counts: HashMap<Digest, usize> = {
                let blobs = blobs_from_manifest(&item.source_data);
                blobs
                    .iter()
                    .map(|d| (d.digest.clone(), ctx.freq_map.count(&d.digest)))
                    .collect()
            };
            let staging_ref = Rc::clone(ctx.staging);
            let shared_ref = Rc::clone(ctx.shared);
            let source_str = item.source.to_string();
            let target_str = item.target.to_string();
            Box::pin(async move {
                let _permit = sem.acquire().await.unwrap();
                progress.image_started(&source_str, &target_str);
                count_retries(
                    discovery_retries,
                    execute_item(
                        item,
                        &cache_ref,
                        &staging_ref,
                        &freq_counts,
                        &retry,
                        mount_source_wait_deadline,
                        &shared_ref,
                    ),
                )
                .await
            })
        };

        loop {
            // Resume discovery when (a) the budget has refilled above the
            // threshold, or (b) nothing is executing: pending items are not
            // promoted until discovery completes, so no progress is possible
            // without resuming.
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

            // Promote all pending tasks at once after discovery; `freq_map` is
            // frozen from here, so `PromoteContext` can borrow it.
            if !shutting_down
                && matches!(promotion_phase, PromotionPhase::Discovering)
                && discovery_futures.is_empty()
            {
                elect_leaders(&mut pending);
                promotion_phase = PromotionPhase::Done;
                write_timing(&mut timing_file, TimingPhase::DiscoveryComplete, &run_start);
                let ctx = PromoteContext {
                    sem: &global_sem,
                    cache: &cache,
                    staging: &staging,
                    freq_map: &freq_map,
                    retry: &self.retry,
                    shared: &shared,
                };
                let to_promote = pending.len();
                for _ in 0..to_promote {
                    if let Some(item) = pending.pop_front() {
                        execution_futures.push(promote(item, &ctx));
                    }
                }
            }

            tokio::select! {
                biased;
                Some(result) = execution_futures.next(), if !execution_futures.is_empty() => {
                    progress.image_completed(&result);
                    results.push(result);
                    if tokio::time::Instant::now() >= next_heartbeat {
                        progress.run_heartbeat(RunProgress {
                            in_discovery: discovery_futures.len(),
                            pending: pending.len(),
                            in_flight: execution_futures.len(),
                            completed: results.len(),
                            elapsed: run_start.elapsed(),
                        });
                        next_heartbeat = tokio::time::Instant::now() + self.heartbeat_interval;
                        heartbeat.reset();
                    }
                }
                _ = async {
                    // The guard ensures shutdown.is_some().
                    shutdown.unwrap().notified().await
                }, if shutdown.is_some()
                    && !shutting_down
                    // Disabled when all work is done, or this branch would
                    // block the `else` exit forever.
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
                    // The guard ensures drain_deadline.is_some().
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
                            // All items share one `Rc<PulledManifest>`, so the first suffices.
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

                    if !discovery_paused && !discovery_futures.is_empty() {
                        let remaining_discovery = discovery_futures.len() as u64;
                        let threshold = (remaining_discovery / 10).max(1);
                        for client in &source_clients {
                            if let Some(remaining) = client.rate_limit_remaining() {
                                if remaining < threshold {
                                    discovery_paused = true;
                                    warn!(
                                        rate_limit_remaining = remaining,
                                        remaining_discovery,
                                        threshold,
                                        registry = client.registry_authority().as_str(),
                                        "rate-limit budget low, pausing discovery"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
                // Last in `biased` order, so real work pre-empts the heartbeat.
                // The guard must stay the disjunction of the two work branches'
                // own preconditions: a looser one (say, "discovery futures
                // remain") stays enabled after shutdown freezes discovery,
                // starving the `else` exit and hanging the engine for a whole
                // interval.
                _ = heartbeat.tick(), if !execution_futures.is_empty()
                    || (!shutting_down && !discovery_paused && !discovery_futures.is_empty()) =>
                {
                    progress.run_heartbeat(RunProgress {
                        in_discovery: discovery_futures.len(),
                        pending: pending.len(),
                        in_flight: execution_futures.len(),
                        completed: results.len(),
                        elapsed: run_start.elapsed(),
                    });
                    next_heartbeat = tokio::time::Instant::now() + self.heartbeat_interval;
                }
                else => break,
            }
        }

        write_timing(&mut timing_file, TimingPhase::ExecutionComplete, &run_start);

        // Prune cache entries for tags/targets no longer in the mapping set.
        // Skipped when the caller could not resolve everything: absence then
        // means "failed this run", not "deleted from config".
        if self.prune_cache {
            let live_keys: HashSet<SnapshotKey> = mappings
                .iter()
                .flat_map(|m| {
                    m.tags
                        .iter()
                        .map(|t| SnapshotKey::new(&m.source_authority, &m.source_repo, &t.source))
                })
                .collect();
            let live_targets: HashSet<TargetId> = mappings
                .iter()
                .flat_map(|m| m.targets.iter().map(|t| t.id().clone()))
                .collect();
            let mut cache_mut = cache.borrow_mut();
            cache_mut.prune_snapshots(&live_keys);
            cache_mut.prune_dedup(&live_targets);
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

/// Parameters for [`discover_tag`]; owns all data so it moves into the async
/// discovery future without borrows.
struct DiscoveryParams {
    source_client: Arc<RegistryClient>,
    source_authority: RegistryAuthority,
    source: ImageRef,
    target: ImageRef,
    targets: Vec<Rc<TargetEntry>>,
    retry: RetryConfig,
    platforms: Option<Vec<PlatformFilter>>,
    cache: Rc<RefCell<TransferStateCache>>,
    source_head_timeout: Duration,
    head_first: bool,
    artifacts_config: Rc<ResolvedArtifacts>,
    /// Work shared between tasks in this run.
    shared: Rc<PerRun>,
}

/// Discover a single (mapping, tag) pair: HEAD source, check cache, pull if needed.
///
/// Returns the outcome and which route was taken. The flow:
/// 1. HEAD the source manifest with a short timeout.
/// 2. If that succeeds, look up the tag digest cache.
/// 3. On a hit (same source digest and platform key), HEAD-check all targets
///    against the cached filtered digest.
/// 4. All match: skip entirely (`CacheHit`). Some mismatch: full-pull for the
///    mismatched targets only (`TargetStale`).
/// 5. Cache miss or failed HEAD: full-pull for all targets (`CacheMiss` /
///    `HeadFailure`).
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
        shared,
    } = params;

    let platform_key = PlatformFilterKey::from_filters(platforms.as_deref());

    // Step 1: HEAD the source manifest with a short timeout.
    let source_head_digest = match tokio::time::timeout(
        source_head_timeout,
        source_client.manifest_head(&source.repo, &source.tag),
    )
    .await
    {
        Ok(Ok(Some(digest))) => Some(digest),
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

    let snapshot_key = SnapshotKey::new(&source_authority, &source.repo, &source.tag);

    // HEAD-check every target against `compare_digest`: all matching skips
    // the image with `skip_route`; otherwise only the mismatched targets are
    // pulled and transferred, reported as `stale_route`.
    let skip_or_pull_stale = async |head_digest: &Digest,
                                    compare_digest: &Digest,
                                    label: &'static str,
                                    skip_route: DiscoveryRoute,
                                    stale_route: DiscoveryRoute| {
        let check = check_targets_against_digest(TargetCheckParams {
            targets: &targets,
            source: &source,
            target: &target,
            compare_digest,
            label,
        })
        .await;

        if check.mismatched.is_empty() {
            return (DiscoveryOutcome::Skip(check.skipped), skip_route);
        }

        let outcome = full_pull_and_build_tasks(FullPullParams {
            source_client: &source_client,
            shared: &shared,
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

        (outcome, stale_route)
    };

    // Steps 2-4: cache lookup, only if the HEAD succeeded.
    if let Some(ref head_digest) = source_head_digest {
        // Borrow dropped before any .await.
        let cached = {
            let c = cache.borrow();
            c.source_snapshot(&snapshot_key).cloned()
        };

        if let Some(snapshot) = cached {
            if *head_digest == snapshot.source_digest
                && platform_key == snapshot.platform_filter_key
            {
                return skip_or_pull_stale(
                    head_digest,
                    &snapshot.filtered_digest,
                    "cache hit",
                    DiscoveryRoute::CacheHit,
                    DiscoveryRoute::TargetStale,
                )
                .await;
            }
        }
    }

    // head_first: on a cache miss, HEAD-check all targets against the source
    // HEAD digest (reused from step 1) before the full manifest GET, saving
    // source rate-limit tokens when targets are already in sync.
    //
    // Skipped when platform filtering is active: the target then holds a
    // filtered index whose digest differs from the source HEAD digest, so the
    // comparison would always miss. The full pull compares filtered digests.
    if head_first && platforms.is_none() {
        if let Some(ref head_digest) = source_head_digest {
            // A match writes no cache entry: the full manifest was never pulled,
            // so there is no filtered_digest. In watch mode head_first
            // therefore fires every cycle for tags that stay in sync (1
            // source HEAD + N target HEADs per tag) until a change forces
            // a full pull.
            return skip_or_pull_stale(
                head_digest,
                head_digest,
                "head_first",
                DiscoveryRoute::HeadFirstSkip,
                DiscoveryRoute::CacheMiss,
            )
            .await;
        }
    }

    let route = if source_head_digest.is_none() {
        DiscoveryRoute::HeadFailure
    } else {
        DiscoveryRoute::CacheMiss
    };

    let outcome = full_pull_and_build_tasks(FullPullParams {
        source_client: &source_client,
        shared: &shared,
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

/// Parameters for [`check_targets_against_digest`].
struct TargetCheckParams<'a> {
    targets: &'a [Rc<TargetEntry>],
    source: &'a ImageRef,
    target: &'a ImageRef,
    compare_digest: &'a Digest,
    /// Label for log messages (e.g., `"cache hit"`, `"head_first"`).
    label: &'static str,
}

/// Result of [`check_targets_against_digest`].
struct TargetCheckResult {
    /// Targets whose HEAD matched the compare digest.
    skipped: Vec<ImageResult>,
    /// Targets whose HEAD did not match or failed (need sync).
    mismatched: Vec<Rc<TargetEntry>>,
}

/// HEAD-check all targets concurrently against a known digest: the cached
/// filtered digest on the cache-hit path, the source HEAD digest on `head_first`.
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
        head_checks.push(async move {
            let result = entry
                .client()
                .manifest_head(&target.repo, &target.tag)
                .await;
            (entry, result)
        });
    }

    let mut skipped = Vec::new();
    let mut mismatched = Vec::new();

    while let Some((entry, result)) = head_checks.next().await {
        match result {
            Ok(Some(digest)) if digest == *compare_digest => {
                debug!(
                    source_repo = %source.repo,
                    source_tag = %source.tag,
                    target_repo = %target.repo,
                    digest = %compare_digest,
                    "skipping -- digest matches at target ({label})"
                );
                skipped.push(image_result(
                    source,
                    target,
                    ImageStatus::Skipped {
                        reason: SkipReason::DigestMatch,
                    },
                ));
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
                mismatched.push(Rc::clone(entry));
            }
        }
    }

    TargetCheckResult {
        skipped,
        mismatched,
    }
}

/// Parameters for [`full_pull_and_build_tasks`].
struct FullPullParams<'a> {
    source_client: &'a Arc<RegistryClient>,
    shared: &'a PerRun,
    source: &'a ImageRef,
    target: &'a ImageRef,
    targets: &'a [Rc<TargetEntry>],
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
/// Updates the tag digest cache on a successful pull.
async fn full_pull_and_build_tasks(params: FullPullParams<'_>) -> DiscoveryOutcome {
    let FullPullParams {
        source_client,
        shared,
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
    let source_data = match pull_source_manifest(
        source_client,
        &source.repo,
        &source.tag,
        retry,
        platforms,
        shared,
    )
    .await
    {
        Ok(data) => Rc::new(data),
        Err(err) => {
            warn!(
                source_repo = %source.repo,
                source_tag = %source.tag,
                error = %err,
                "source pull failed, skipping all targets"
            );
            let fail_results: Vec<ImageResult> = targets
                .iter()
                .map(|t| ImageResult {
                    target: format!("{} ({}):{}", target.repo, t.name(), target.tag),
                    ..image_result(source, target, failed_status(ErrorKind::ManifestPull, &err))
                })
                .collect();
            // The cache is not updated on pull failure.
            return DiscoveryOutcome::Failed(fail_results);
        }
    };

    // Record the fresh source data regardless of target outcomes.
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

    let mut head_checks = FuturesUnordered::new();
    for entry in targets {
        head_checks.push(async move {
            let result = entry
                .client()
                .manifest_head(&target.repo, &target.tag)
                .await;
            (entry, result)
        });
    }

    let mut active_items = Vec::new();

    while let Some((entry, result)) = head_checks.next().await {
        match result {
            Ok(Some(digest)) if digest == *source_digest => {
                debug!(
                    source_repo = %source.repo,
                    source_tag = %source.tag,
                    target_repo = %target.repo,
                    digest = %source_digest,
                    "skipping -- digest matches at target"
                );
                skipped_results.push(image_result(
                    source,
                    target,
                    ImageStatus::Skipped {
                        reason: SkipReason::DigestMatch,
                    },
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
                active_items.push(TransferTask {
                    source_data: Rc::clone(&source_data),
                    source_client: Arc::clone(source_client),
                    target_entry: Rc::clone(entry),
                    source: source.clone(),
                    target: target.clone(),
                    artifacts_config: Rc::clone(artifacts_config),
                    discovery_retries: retries_made(),
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
    mount_source_wait_deadline: Duration,
    shared: &PerRun,
) -> ImageResult {
    let start = Instant::now();

    let ctx = TransferContext {
        cache,
        staging,
        retry,
        shared,
        source_client: &item.source_client,
        source_repo: &item.source.repo,
        target_client: item.target_entry.client(),
        target_id: item.target_entry.id(),
        target_repo: &item.target.repo,
        batch_checker: item.target_entry.batch_checker.as_ref(),
        mount_source_wait_deadline,
    };
    let mut outcome =
        transfer_image_blobs(&ctx, blobs_from_manifest(&item.source_data), freq_counts).await;

    if let Some(err) = outcome.error.take() {
        warn!(target_name = %item.target_entry.id().alias(), error = %err, "blob transfer failed");
        // Unblock followers waiting on this repo's manifest commit; they
        // fall through to HEAD+push.
        cache
            .borrow_mut()
            .notify_repo_failed(item.target_entry.id(), &item.target.repo);
        let status = failed_status(ErrorKind::BlobTransfer, &err);
        return outcome.into_result(&item.source, &item.target, status, start);
    }

    let outcome = RefCell::new(outcome);
    let pushed = push_manifests(&ctx, &item, &outcome).await;
    let outcome = outcome.into_inner();
    match pushed {
        Ok(()) => {
            // The repo now has a committed manifest, so it is a valid
            // cross-repo mount source.
            cache
                .borrow_mut()
                .mark_repo_committed(item.target_entry.id(), &item.target.repo);

            let artifacts_skipped = match discover_and_sync_artifacts(&ctx, &item).await {
                Ok(skipped) => skipped,
                Err(err) => {
                    let (kind, retries) = if err.is_required_artifacts_missing() {
                        (ErrorKind::RequiredArtifactsMissing, 0)
                    } else {
                        (ErrorKind::ArtifactSync, retries_made())
                    };
                    warn!(
                        source_repo = %item.source.repo,
                        target_repo = %item.target.repo,
                        error = %err,
                        "artifact sync failed"
                    );
                    let status = ImageStatus::Failed {
                        kind,
                        error: err.to_string(),
                        retries,
                        status_code: None,
                    };
                    return outcome.into_result(&item.source, &item.target, status, start);
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
                artifacts_skipped,
                ..outcome.into_result(&item.source, &item.target, ImageStatus::Synced, start)
            }
        }
        Err(err) => {
            // Unblock followers waiting on this repo's manifest commit.
            cache
                .borrow_mut()
                .notify_repo_failed(item.target_entry.id(), &item.target.repo);

            if err.manifest_refusal() == Some(ManifestRefusal::ImmutableTag) {
                debug!(
                    source_repo = %item.source.repo,
                    target_repo = %item.target.repo,
                    target_tag = %item.target.tag,
                    "target tag is immutable, skipping"
                );
                let status = ImageStatus::Skipped {
                    reason: SkipReason::ImmutableTag,
                };
                outcome.into_result(&item.source, &item.target, status, start)
            } else {
                warn!(target_name = %item.target_entry.id().alias(), error = %err, "manifest push failed");
                // A refused manifest's repair can fail copying a blob.
                let kind = if err.is_blob_transfer() {
                    ErrorKind::BlobTransfer
                } else {
                    ErrorKind::ManifestPush
                };
                let status = failed_status(kind, &err);
                outcome.into_result(&item.source, &item.target, status, start)
            }
        }
    }
}

/// Pull all source manifest data for a single tag: the manifest, plus the
/// child manifests for an index.
///
/// When `platforms` is `Some`, only matching children are pulled and the
/// index's `raw_bytes` and digest are re-serialized to list only the kept
/// descriptors, so targets receive the filtered index.
async fn pull_source_manifest(
    client: &RegistryClient,
    repo: &RepositoryName,
    tag: &str,
    retry: &RetryConfig,
    platforms: Option<&[PlatformFilter]>,
    shared: &PerRun,
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

            // Children are pulled concurrently; the registry's AIMD controller
            // limits the real concurrency. A child shared with another tag's
            // index is pulled once per run.
            let child_futures: Vec<_> = descriptors
                .iter()
                .map(|child_desc| {
                    let child_digest_str = child_desc.digest.to_string();
                    let key = source_key(client, repo, &child_desc.digest);
                    async move {
                        let child_pull = run_once(&shared.child_pulls, key, || async {
                            with_retry(retry, "manifest pull", || {
                                client.manifest_pull(repo, &child_digest_str)
                            })
                            .await
                            .map(Rc::new)
                            .map_err(|e| crate::Error::Manifest {
                                reference: child_digest_str.clone(),
                                source: e,
                            })
                        })
                        .await?;
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
            .flat_map(|c| child_blobs(c))
            .collect(),
    }
}

/// Blobs an image manifest references. An index has none of its own:
/// discovery rejects nested index children, and an index artifact is pushed
/// as a manifest only.
fn child_blobs(child: &ManifestPull) -> Vec<&Descriptor> {
    match &child.manifest {
        ManifestKind::Image(m) => collect_image_blobs(m),
        ManifestKind::Index(_) => Vec::new(),
    }
}

/// Write-only view of the transfer state cache for use under the per-image
/// blob semaphore.
///
/// Exposes completion/failure signals and repo management but NOT wait
/// primitives (`unresolved_watches_for`, `claim_blob_upload`). External waits
/// (`Notify::notified()`, `watch::wait_for()`) must never hold a permit: if all
/// `BLOB_CONCURRENCY` permits sat in external waits, no other blob could make
/// progress. Code holding a permit therefore cannot block on external signals,
/// because the type has no method to do so. All such waits run before the
/// permit is taken (`wait_for_blob_claim`, `resolve_mount_source`,
/// `resolve_staging`).
struct BlobSink<'a> {
    cache: &'a Rc<RefCell<TransferStateCache>>,
    target_id: &'a TargetId,
}

impl<'a> BlobSink<'a> {
    fn complete(&self, digest: Digest, repo: RepositoryName) {
        self.cache
            .borrow_mut()
            .set_blob_completed(self.target_id, digest, repo);
    }

    fn fail(&self, digest: Digest, repo: RepositoryName, error: String) {
        self.cache
            .borrow_mut()
            .set_blob_failed(self.target_id, digest, repo, error);
    }

    fn exists(&self, digest: Digest, repo: RepositoryName) {
        self.cache
            .borrow_mut()
            .set_blob_verified(self.target_id, digest, repo);
    }

    fn release_claim(&self, digest: &Digest) {
        self.cache
            .borrow_mut()
            .release_blob_claim(self.target_id, digest);
    }

    fn mark_repo_stale(&self, digest: &Digest, repo: &RepositoryName) {
        self.cache
            .borrow_mut()
            .mark_blob_repo_stale(self.target_id, digest, repo);
    }
}

/// I/O context for blob transfers under the semaphore: registry clients plus
/// the write-only [`BlobSink`], with no cache read/wait access.
struct BlobIoContext<'a> {
    sink: BlobSink<'a>,
    /// Failed copies in this run, written when this task's copy fails.
    blob_failures: &'a RefCell<HashMap<CopyKey, Arc<crate::Error>>>,
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
                target_id: ctx.target_id,
            },
            blob_failures: &ctx.shared.blob_failures,
            source_client: ctx.source_client,
            source_repo: ctx.source_repo,
            target_client: ctx.target_client,
            target_repo: ctx.target_repo,
            retry: ctx.retry,
            staging: ctx.staging,
        }
    }

    /// The failure recorded in this run for this task's copy of `digest`.
    fn recorded_failure(&self, digest: &Digest) -> Option<Arc<crate::Error>> {
        let failures = self.blob_failures.borrow();
        if failures.is_empty() {
            return None;
        }
        failures
            .get(&copy_key(
                self.source_client,
                self.source_repo,
                self.sink.target_id.alias(),
                self.target_repo,
                digest,
            ))
            .cloned()
    }

    /// Record a failed copy of `digest` for the rest of the run, release the
    /// claim, and return the error every task copying it reports. A local
    /// staging I/O error is not recorded: it says nothing about the
    /// registries, so the next task copying the blob tries again.
    fn fail_copy(&self, digest: &Digest, err: crate::Error) -> crate::Error {
        let err = Arc::new(err);
        if !is_local_io(&err) {
            let key = copy_key(
                self.source_client,
                self.source_repo,
                self.sink.target_id.alias(),
                self.target_repo,
                digest,
            );
            self.blob_failures
                .borrow_mut()
                .insert(key, Arc::clone(&err));
        }
        self.release_failed(digest, err)
    }

    /// Mark this task's copy of `digest` failed with `err` and release the
    /// claim, waking its waiters.
    fn release_failed(&self, digest: &Digest, err: Arc<crate::Error>) -> crate::Error {
        self.sink
            .fail(digest.clone(), self.target_repo.to_owned(), err.to_string());
        self.sink.release_claim(digest);
        crate::Error::Shared(err)
    }
}

/// Result of the pre-semaphore staging resolution ([`resolve_staging`]).
enum StagingClaim<'a> {
    /// Staging disabled: direct source pull + target push.
    Disabled,
    /// Blob already staged by another task: read from disk.
    Staged,
    /// This task claimed the source pull: pull and stage, then push.
    /// Dropping the claim on any path wakes tasks waiting on this digest.
    Pull(ClaimGuard<'a, Digest, ()>),
}

/// Context for transferring blobs from source to a single target, with full
/// cache access for the pre-semaphore phase (claim, mount-source resolution,
/// staging claim). Under the semaphore, [`BlobIoContext`] replaces it.
struct TransferContext<'a> {
    cache: &'a Rc<RefCell<TransferStateCache>>,
    /// Work shared between tasks in this run.
    shared: &'a PerRun,
    /// Shared staging area for pull-once, fan-out multi-target transfers.
    staging: &'a Rc<BlobStage>,
    retry: &'a RetryConfig,
    source_client: &'a RegistryClient,
    source_repo: &'a RepositoryName,
    target_client: &'a RegistryClient,
    target_id: &'a TargetId,
    target_repo: &'a RepositoryName,
    batch_checker: Option<&'a Rc<dyn BatchBlobChecker>>,
    /// Per-blob deadline for mount-source resolution.
    mount_source_wait_deadline: Duration,
}

/// Transfer all blobs for a single image to one target.
///
/// Per blob: cache check, then cross-repo mount, then HEAD at the target,
/// then pull+push. Blobs are sorted by descending frequency so the
/// most-shared blobs populate the cache first.
async fn transfer_image_blobs(
    ctx: &TransferContext<'_>,
    mut blobs: Vec<&Descriptor>,
    freq_counts: &HashMap<Digest, usize>,
) -> TargetBlobOutcome {
    // The sort is stable: equal frequencies keep manifest order (config first).
    blobs.sort_by(|a, b| {
        let fa = freq_counts.get(&a.digest).copied().unwrap_or(0);
        let fb = freq_counts.get(&b.digest).copied().unwrap_or(0);
        fb.cmp(&fa)
    });

    // With a batch checker, check all blobs in one call and pre-populate the
    // cache: the per-blob loop then hits cache for existing blobs and skips
    // the HEAD for absent ones. A blob appearing after the check is harmless:
    // the redundant push succeeds because the registry deduplicates by digest.
    let batch_checked: HashSet<Digest> = if let Some(checker) = ctx.batch_checker {
        let all_digests: Vec<Digest> = blobs.iter().map(|b| b.digest.clone()).collect();
        match checker
            .check_blob_existence(ctx.target_repo, &all_digests)
            .await
        {
            Ok(existence) => {
                // A digest the check left unknown still gets its HEAD.
                let total = all_digests.len();
                let checked: HashSet<Digest> = all_digests
                    .iter()
                    .filter(|d| existence.is_known(d))
                    .cloned()
                    .collect();
                let available = existence.into_available();
                debug!(
                    target_name = %ctx.target_id.alias(),
                    repo = %ctx.target_repo,
                    checked = checked.len(),
                    existing = available.len(),
                    unknown = total - checked.len(),
                    "batch check pre-populated cache"
                );
                let mut c = ctx.cache.borrow_mut();
                // Only a blob the check confirms present is trusted: an earlier
                // run's entry for any other is dropped, since the repository
                // may have lost it.
                c.forget_remembered_blobs(
                    ctx.target_id,
                    ctx.target_repo,
                    all_digests.iter().filter(|d| !available.contains(*d)),
                );
                for digest in available {
                    c.set_blob_verified(ctx.target_id, digest, ctx.target_repo.to_owned());
                }
                checked
            }
            Err(e) => {
                warn!(
                    target_name = %ctx.target_id.alias(),
                    error = %e,
                    "batch check failed, falling back to per-blob HEAD"
                );
                // Degrade to a HEAD per blob, earlier runs' entries included:
                // trusting them would wait out the manifest commit gate on the
                // failed API for a blob that may be gone.
                ctx.cache.borrow_mut().forget_remembered_blobs(
                    ctx.target_id,
                    ctx.target_repo,
                    &all_digests,
                );
                HashSet::new()
            }
        }
    } else {
        HashSet::new()
    };

    let blob_sem = Semaphore::new(BLOB_CONCURRENCY);
    let cancel = Cancel::default();
    let mut blob_futures = FuturesUnordered::new();

    for blob in &blobs {
        let digest = blob.digest.clone();
        let size = blob.size;
        // Reborrow shared state as Copy references for `async move`.
        let sem = &blob_sem;
        let cancel = &cancel;
        let batch = &batch_checked;
        blob_futures.push(async move {
            // Pre-semaphore phase: all external waits run here, with no permit held.
            let claim = wait_for_blob_claim(ctx, &digest, cancel).await;
            match claim {
                BlobClaim::Skipped => return BlobResult::Skipped,
                BlobClaim::Cancelled => return BlobResult::Cancelled,
                BlobClaim::Failed(err) => return BlobResult::Failed(crate::Error::Shared(err)),
                BlobClaim::Claimed => {}
            }
            let mount_source = resolve_mount_source(ctx, &digest, cancel).await;
            let staging_claim = resolve_staging(ctx.staging, &digest).await;

            // `run_under_semaphore` takes `BlobIoContext`, not `TransferContext`,
            // so `ctx` is unreachable inside the permit scope.
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
                cancel.set();
                outcome.error = Some(err);
                // No break: drain the remaining futures so permits are
                // released and the cancel flag takes effect.
            }
        }
    }

    outcome
}

/// Per-image cancel signal: set when one blob fails so the image's other
/// blobs stop waiting, including those parked on a claim or mount-source wait.
#[derive(Default)]
struct Cancel {
    flag: Cell<bool>,
    notify: tokio::sync::Notify,
}

impl Cancel {
    fn set(&self) {
        self.flag.set(true);
        self.notify.notify_waiters();
    }

    fn is_set(&self) -> bool {
        self.flag.get()
    }

    /// Returns once [`set`](Self::set) has run. Interest is registered before
    /// the flag is read, so a `set` between the two is not lost.
    async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_set() {
            return;
        }
        notified.await;
    }
}

/// Result of the pre-semaphore blob claim phase.
enum BlobClaim {
    /// Blob already exists at target: skip.
    Skipped,
    /// Cancel flag was set: abort.
    Cancelled,
    /// Blob claimed for upload by this task.
    Claimed,
    /// Another task's copy of this blob between the same repos failed in
    /// this run, and no mount source exists; report its error.
    Failed(Arc<crate::Error>),
}

/// Whether a blob copy failed on local staging I/O (disk full, a missing
/// staged file) rather than on either registry. A read error while
/// streaming a staged file reaches us wrapped in the HTTP client's error,
/// so the whole source chain is searched.
fn is_local_io(err: &crate::Error) -> bool {
    let crate::Error::BlobTransfer { source, .. } = err else {
        return false;
    };
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(source);
    while let Some(e) = cause {
        if let Some(ocync_distribution::Error::Io { .. }) = e.downcast_ref() {
            return true;
        }
        cause = e.source();
    }
    false
}

/// Cache check + atomic claim, run OUTSIDE the per-image blob semaphore:
/// skip if the blob is already at our repo, else loop on an atomic
/// check-and-claim, awaiting the holder's notify between iterations.
async fn wait_for_blob_claim(
    ctx: &TransferContext<'_>,
    digest: &Digest,
    cancel: &Cancel,
) -> BlobClaim {
    let skip = {
        let c = ctx.cache.borrow();
        c.blob_present_at(ctx.target_id, digest, ctx.target_repo)
    };
    if skip {
        return BlobClaim::Skipped;
    }

    // A copy that already failed between the same repos fails this one too,
    // unless a mount can avoid the copy.
    loop {
        if cancel.is_set() {
            return BlobClaim::Cancelled;
        }
        if let Some(err) = BlobIoContext::from_ctx(ctx).recorded_failure(digest) {
            let mountable = ctx
                .cache
                .borrow()
                .mount_sources(ctx.target_id, digest, ctx.target_repo)
                .next()
                .is_some();
            if !mountable {
                return BlobClaim::Failed(err);
            }
        }
        let action = {
            let mut c = ctx.cache.borrow_mut();
            c.claim_blob_upload(ctx.target_id, digest, ctx.target_repo)
        };
        match action {
            crate::plan::ClaimAction::Wait { holder, notify } => {
                debug!(
                    %digest,
                    holder = %holder,
                    target = %ctx.target_id.alias(),
                    "waiting for in-flight upload of this blob to finish",
                );
                tokio::select! {
                    _ = notify.notified() => {}
                    () = cancel.cancelled() => return BlobClaim::Cancelled,
                }
                // A batch check may have populated our repo's state during the wait.
                let skip = {
                    let c = ctx.cache.borrow();
                    c.blob_present_at(ctx.target_id, digest, ctx.target_repo)
                };
                if skip {
                    return BlobClaim::Skipped;
                }
                continue;
            }
            crate::plan::ClaimAction::Claimed => return BlobClaim::Claimed,
        }
    }
}

/// Resolve the staging dedup claim, run OUTSIDE the per-image blob semaphore:
/// the wait lasts as long as another task's source pull (see [`BlobSink`]).
async fn resolve_staging<'a>(staging: &'a Rc<BlobStage>, digest: &Digest) -> StagingClaim<'a> {
    if !staging.is_enabled() {
        return StagingClaim::Disabled;
    }
    loop {
        match staging.claim_or_check(digest) {
            crate::staging::StagePullAction::Exists => return StagingClaim::Staged,
            crate::staging::StagePullAction::Pull(guard) => return StagingClaim::Pull(guard),
            crate::staging::StagePullAction::Wait(notify) => {
                notify.notified().await;
                continue;
            }
        }
    }
}

/// Cross-repo mount-source resolver with bounded deadline.
///
/// Loops until (a) a mount source emerges, (b) all candidate repos resolve
/// without committing, or (c) the deadline expires. Returns the mount source
/// to attempt, or `None` if the caller should fall back to push.
///
/// Runs OUTSIDE the per-image blob semaphore (see [`BlobSink`]). `select_all`
/// is cancel-safe: dropping the per-blob future releases all watches.
async fn resolve_mount_source(
    ctx: &TransferContext<'_>,
    digest: &Digest,
    cancel: &Cancel,
) -> Option<RepositoryName> {
    let deadline = tokio::time::Instant::now() + ctx.mount_source_wait_deadline;
    loop {
        let receivers: Vec<tokio::sync::watch::Receiver<bool>> = {
            let mut c = ctx.cache.borrow_mut();
            if let Some(r) = c
                .mount_sources(ctx.target_id, digest, ctx.target_repo)
                .next()
                .cloned()
            {
                return Some(r);
            }
            c.unresolved_watches_for(ctx.target_id, digest, ctx.target_repo)
        };
        if receivers.is_empty() {
            return None;
        }

        let wait = wait_any_fired(receivers);
        tokio::pin!(wait);
        tokio::select! {
            _ = &mut wait => continue,
            _ = tokio::time::sleep_until(deadline) => {
                debug!(
                    %digest,
                    target = %ctx.target_id.alias(),
                    "mount-source wait deadline reached, falling back to push"
                );
                return None;
            }
            // The caller reaches `run_under_semaphore`, which sees the flag.
            () = cancel.cancelled() => return None,
        }
    }
}

/// Wait until ANY of the given watch receivers fires (transitions to true).
async fn wait_any_fired(receivers: Vec<tokio::sync::watch::Receiver<bool>>) {
    if receivers.is_empty() {
        return;
    }
    let futures: Vec<_> = receivers
        .into_iter()
        .map(|mut rx| {
            Box::pin(async move {
                let _ = rx.wait_for(|&v| v).await;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>
        })
        .collect();
    let _ = futures_util::future::select_all(futures).await;
}

/// Entry point for the semaphore-bounded phase of blob transfer.
///
/// Takes [`BlobIoContext`], not [`TransferContext`]: this signature is the
/// compile-time boundary that keeps cache wait methods out of the semaphore scope.
async fn run_under_semaphore(
    io: &BlobIoContext<'_>,
    digest: &Digest,
    size: u64,
    batch_checked: &HashSet<Digest>,
    cancel: &Cancel,
    mount_source: Option<RepositoryName>,
    staging: StagingClaim<'_>,
) -> BlobResult {
    if cancel.is_set() {
        io.sink.fail(
            digest.clone(),
            io.target_repo.to_owned(),
            "cancelled".into(),
        );
        io.sink.release_claim(digest);
        return BlobResult::Cancelled;
    }
    transfer_claimed_blob(io, digest, size, batch_checked, mount_source, staging).await
}

/// Transfer a claimed blob to one target: mount, HEAD, pull+push.
///
/// The blob MUST already be claimed via [`wait_for_blob_claim`]. Runs under
/// the per-image blob semaphore, reached through [`run_under_semaphore`].
async fn transfer_claimed_blob(
    io: &BlobIoContext<'_>,
    digest: &Digest,
    size: u64,
    batch_checked: &HashSet<Digest>,
    mount_source: Option<RepositoryName>,
    staging: StagingClaim<'_>,
) -> BlobResult {
    if let Some(from_repo) = mount_source {
        debug!(%digest, %from_repo, target = %io.sink.target_id.alias(), "attempting mount");
        match io
            .target_client
            .blob_mount(io.target_repo, digest, &from_repo)
            .await
        {
            Ok(MountResult::Mounted) => {
                io.sink.complete(digest.clone(), io.target_repo.to_owned());
                io.sink.release_claim(digest);
                return BlobResult::Mounted;
            }
            Ok(MountResult::NotMounted) | Err(_) => {
                debug!(%digest, %from_repo, target = %io.sink.target_id.alias(), "mount not fulfilled, falling back to HEAD+push");
                // Mark the source stale so later tasks do not retry it, but
                // keep the upload claim so concurrent waiters do not re-claim
                // and start duplicate pushes.
                io.sink.mark_repo_stale(digest, &from_repo);
            }
        }
    }

    if !batch_checked.contains(digest) {
        let head_result = io.target_client.blob_exists(io.target_repo, digest).await;
        match head_result {
            Ok(true) => {
                io.sink.exists(digest.clone(), io.target_repo.to_owned());
                io.sink.release_claim(digest);
                return BlobResult::Skipped;
            }
            Ok(false) => {}
            Err(e) => {
                debug!(
                    %digest,
                    target = %io.sink.target_id.alias(),
                    error = %e,
                    "blob HEAD failed, proceeding with push"
                );
            }
        }
    }

    // A mount source let this task past a recorded copy failure, and the
    // mount did not avoid the copy: copying would repeat that failure.
    if let Some(err) = io.recorded_failure(digest) {
        return BlobResult::Failed(io.release_failed(digest, err));
    }

    let transfer_result: Result<(), crate::Error> = match staging {
        StagingClaim::Pull(guard) => {
            if let Err(e) = with_retry(io.retry, "blob pull (to stage)", || async {
                let mut writer =
                    io.staging
                        .begin_write(digest)
                        .map_err(|e| ocync_distribution::Error::Io {
                            context: "staging create",
                            source: e.into(),
                        })?;
                let stream = io.source_client.blob_pull(io.source_repo, digest).await?;
                futures_util::pin_mut!(stream);
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    writer
                        .write_chunk(&chunk)
                        .map_err(|e| ocync_distribution::Error::Io {
                            context: "staging write",
                            source: e.into(),
                        })?;
                }
                writer.finish().map_err(|e| ocync_distribution::Error::Io {
                    context: "staging finalize",
                    source: e.into(),
                })?;
                Ok::<(), ocync_distribution::Error>(())
            })
            .await
            {
                let err = crate::Error::BlobTransfer {
                    digest: digest.clone(),
                    source: e,
                };
                return BlobResult::Failed(io.fail_copy(digest, err));
            }
            // Staged: waiters read it from disk while this task pushes.
            drop(guard);

            push_staged_blob(io, digest).await
        }
        StagingClaim::Staged => push_staged_blob(io, digest).await,
        StagingClaim::Disabled => with_retry(io.retry, "blob transfer", || async {
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
        }),
    };

    match transfer_result {
        Ok(()) => {
            io.sink.complete(digest.clone(), io.target_repo.to_owned());
            io.sink.release_claim(digest);
            BlobResult::Transferred { bytes: size }
        }
        Err(err) => BlobResult::Failed(io.fail_copy(digest, err)),
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
                source: e.into(),
            })?;
        let file_size = file.metadata().map(|m| m.len()).ok();
        let stream = file_read_stream(file).map(|r| {
            r.map_err(|e| ocync_distribution::Error::Io {
                context: "staging read",
                source: e.into(),
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
///
/// Each child is pushed once per target repo per run: this task first claims
/// every child it can, pushes those, then awaits the ones other tasks hold.
/// A child whose push failed in this run fails this task with the same error
/// and is not pushed again, so the retry budget is spent once. The top-level
/// manifest is always pushed, by tag.
///
/// When `batch_checker` is `Some` AND `retry.manifest_commit_wait` is
/// non-zero, calls [`BatchBlobChecker::wait_for_blobs_available`] on every
/// blob referenced by the manifests this task pushes (the image, or the
/// children it claimed) BEFORE any manifest `PUT`. That gates the commit on
/// the authoritative blob-visibility API, closing the `BLOB_UPLOAD_UNKNOWN`
/// race on targets whose consistency view lags blob upload (ECR). An index
/// whose children were all pushed by other tasks needs no wait.
async fn push_manifests(
    ctx: &TransferContext<'_>,
    item: &TransferTask,
    outcome: &RefCell<TargetBlobOutcome>,
) -> Result<(), crate::Error> {
    let (retry, shared) = (ctx.retry, ctx.shared);
    let target_repo = &item.target.repo;
    let source_data = &item.source_data;
    let child_key = |child: &ManifestPull| {
        target_key(item.target_entry.id().alias(), target_repo, &child.digest)
    };

    // Claim before waiting on blob availability, so tasks sharing an index
    // split its children instead of each waiting on all of them.
    let mut claimed = Vec::new();
    let mut held_elsewhere: Vec<&ManifestPull> = Vec::new();
    let mut failed: Option<crate::Error> = None;
    for child in &source_data.children {
        match claim(&shared.child_pushes, child_key(child)) {
            Claim::Claimed(guard) => claimed.push((child, guard)),
            Claim::Wait(_) => held_elsewhere.push(child),
            Claim::Done(Ok(())) => {}
            // The index cannot be pushed without this child. Returning drops
            // this task's claims, so tasks needing those children do them.
            Claim::Done(Err(err)) => return Err(crate::Error::Shared(err)),
        }
    }

    let wait_for: Vec<&Descriptor> = match &source_data.pull.manifest {
        ManifestKind::Image(m) => collect_image_blobs(m),
        ManifestKind::Index(_) => claimed.iter().flat_map(|(c, _)| child_blobs(c)).collect(),
    };
    // Deduplicated: children of one index often share a config blob.
    let digests: Vec<Digest> = wait_for
        .iter()
        .map(|d| d.digest.clone())
        .collect::<HashSet<Digest>>()
        .into_iter()
        .collect();
    wait_for_commit(item, retry, &digests).await;

    // join_all, not try_join_all: a transient failure on one child must not
    // cancel the others.
    let pushes = claimed.into_iter().map(|(child, guard)| async move {
        let outcome = push_manifest(ctx, &child.digest.to_string(), child, Some(outcome))
            .await
            .map_err(Arc::new);
        guard.finish(outcome.clone());
        outcome.map_err(crate::Error::Shared)
    });
    for outcome in futures_util::future::join_all(pushes).await {
        if let Err(err) = outcome {
            failed.get_or_insert(err);
        }
    }
    // The index cannot be pushed now; the other children are not needed.
    if let Some(err) = failed {
        return Err(err);
    }
    // Then the children other tasks are pushing. Should one of them have
    // been dropped before finishing, this task pushes the child itself.
    let awaited = held_elsewhere.iter().map(|child| {
        run_once(&shared.child_pushes, child_key(child), move || {
            let reference = child.digest.to_string();
            async move { push_manifest(ctx, &reference, child, Some(outcome)).await }
        })
    });
    for result in futures_util::future::join_all(awaited).await {
        result?;
    }

    push_manifest(ctx, &item.target.tag, &source_data.pull, Some(outcome)).await
}

/// Wait until the target reports every blob in `digests` visible, when it
/// has a batch checker (production: ECR) and `manifest_commit_wait` is
/// non-zero. ECR's manifest validator can lag blob PUT-201s; on timeout the
/// push goes ahead and `with_retry` catches any residual `BLOB_UPLOAD_UNKNOWN`.
async fn wait_for_commit(item: &TransferTask, retry: &RetryConfig, digests: &[Digest]) {
    let Some(checker) = item.target_entry.batch_checker.as_deref() else {
        return;
    };
    if retry.manifest_commit_wait.is_zero() || digests.is_empty() {
        return;
    }
    let target_repo = &item.target.repo;
    match checker
        .wait_for_blobs_available(target_repo, digests, retry.manifest_commit_wait)
        .await
    {
        Ok(()) => debug!(
            target_repo = %target_repo,
            blob_count = digests.len(),
            "BatchCheckLayerAvailability confirms all blobs visible; proceeding with manifest push"
        ),
        Err(e) => warn!(
            target_repo = %target_repo,
            blob_count = digests.len(),
            error = %e,
            "BatchCheckLayerAvailability wait did not converge before deadline; proceeding (retry path will catch any BLOB_UPLOAD_UNKNOWN)"
        ),
    }
}

/// Push `manifest` to the task's target repo as `reference` (a tag, or a
/// child's digest).
///
/// A manifest refused as invalid, the answer to a blob the registry has
/// lost, drops an earlier run's entries for its blobs. When any of its blobs
/// was dropped this run, by this refusal or another task's, they are copied
/// again and the manifest is pushed once more. The repair's blob work is
/// folded into `outcome`, when the caller reports it.
async fn push_manifest(
    ctx: &TransferContext<'_>,
    reference: &str,
    manifest: &ManifestPull,
    outcome: Option<&RefCell<TargetBlobOutcome>>,
) -> Result<(), crate::Error> {
    let blobs = child_blobs(manifest);
    let pushed = put_manifest(ctx, reference, manifest).await;
    if !settle_blobs(ctx, &blobs, &pushed) {
        return pushed;
    }
    warn!(
        target_name = %ctx.target_id.alias(),
        target_repo = %ctx.target_repo,
        reference,
        "manifest refused for blobs an earlier run cached, copying them again"
    );
    // A blob still recorded present is skipped from the cache, so only the
    // lost ones cost a request.
    let repair = transfer_image_blobs(ctx, blobs, &HashMap::new()).await;
    if let Some(outcome) = outcome {
        outcome.borrow_mut().absorb_repair(&repair);
    }
    if let Some(err) = repair.error {
        return Err(err);
    }
    // No commit wait here: a batch check settles every earlier run's entry
    // before the push, so only a target without one reaches the repair. The
    // blobs copied again are recorded present, so this push has nothing left
    // to settle, and a second refusal is returned as is.
    put_manifest(ctx, reference, manifest).await
}

/// Push `manifest` to the task's target repo as `reference`, retrying
/// transient failures.
async fn put_manifest(
    ctx: &TransferContext<'_>,
    reference: &str,
    manifest: &ManifestPull,
) -> Result<(), crate::Error> {
    with_retry(ctx.retry, "manifest push", || {
        ctx.target_client.manifest_push(
            ctx.target_repo,
            reference,
            &manifest.media_type,
            &manifest.raw_bytes,
        )
    })
    .await
    .map(|_| ())
    .map_err(|e| crate::Error::Manifest {
        reference: reference.to_owned(),
        source: e,
    })
}

/// Record what a manifest push proves about the blobs it references, and
/// return whether it calls for a repair: a refusal as invalid of a manifest
/// referencing a blob a refusal dropped from the cache this run. A
/// committed manifest proves its blobs are in the repo.
fn settle_blobs(
    ctx: &TransferContext<'_>,
    blobs: &[&Descriptor],
    pushed: &Result<(), crate::Error>,
) -> bool {
    let digests = blobs.iter().map(|b| &b.digest);
    let key = |digest: &Digest| target_key(ctx.target_id.alias(), ctx.target_repo, digest);
    match pushed {
        Ok(()) => {
            ctx.cache
                .borrow_mut()
                .confirm_blobs(ctx.target_id, ctx.target_repo, digests);
            false
        }
        Err(e) if e.manifest_refusal() == Some(ManifestRefusal::Invalid) => {
            let dropped = ctx.cache.borrow_mut().forget_remembered_blobs(
                ctx.target_id,
                ctx.target_repo,
                digests,
            );
            let mut lost = ctx.shared.lost_blobs.borrow_mut();
            lost.extend(dropped.into_iter().map(key));
            blobs.iter().any(|b| lost.contains(&key(&b.digest)))
        }
        Err(_) => false,
    }
}

/// Discover referrers for a synced manifest and transfer matching artifacts.
///
/// Discovery tries the referrers API, then the tag fallback on 404; if
/// neither works, no artifacts are available. Each matching artifact's blobs
/// are pushed, then its manifest (preserving the `subject` reference to the
/// parent). With a `batch_checker`, an artifact follows the same batch-check
/// protocol as an image (see `transfer_image_blobs` and `push_manifests`).
///
/// Returns `Ok(true)` when artifact discovery was skipped due to a transient
/// error (the image synced but artifacts may be missing at the target).
async fn discover_and_sync_artifacts(
    ctx: &TransferContext<'_>,
    item: &TransferTask,
) -> Result<bool, crate::Error> {
    let shared = ctx.shared;
    let artifacts_config = &item.artifacts_config;
    let source_client = &item.source_client;
    let source_repo = &item.source.repo;
    let parent_digest = &item.source_data.pull.digest;
    if !artifacts_config.enabled {
        return Ok(false);
    }

    // Discover once per source manifest per run, however many tags and
    // targets sync it.
    let referrers_key = source_key(source_client, source_repo, parent_digest);
    let referrers = run_once(&shared.referrers, referrers_key, || async {
        Ok(discover_referrers(source_client, source_repo, parent_digest).await)
    })
    .await?;
    let discovery_succeeded = !matches!(referrers, Referrers::Failed);

    // A descriptor without `artifact_type` (e.g. a tag-fallback single
    // manifest) is matched on its media_type, per OCI 1.1 artifact guidance.
    let matching: Vec<&Descriptor> = match referrers {
        Referrers::Found(ref index) => index
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
        Referrers::NotFound | Referrers::Failed => Vec::new(),
    };

    // Fires only on confirmed zero referrers, not on a failed discovery.
    if artifacts_config.require_artifacts && matching.is_empty() && discovery_succeeded {
        return Err(crate::Error::RequiredArtifactsMissing {
            reference: parent_digest.to_string(),
        });
    }

    if matching.is_empty() {
        // A failed discovery reports artifacts_skipped; zero referrers does not.
        return Ok(!discovery_succeeded);
    }

    info!(
        repo = %source_repo,
        digest = %parent_digest,
        count = matching.len(),
        "syncing artifacts"
    );

    // Copy each matching artifact once per (source repo, target repo) per
    // run. Keyed by artifact rather than parent, so two mappings into one
    // repo with different artifact filters each get the artifacts they
    // select, and by source, so one source's failure never fails another.
    for desc in &matching {
        let push_key = copy_key(
            &item.source_client,
            &item.source.repo,
            item.target_entry.id().alias(),
            &item.target.repo,
            &desc.digest,
        );
        run_once(&shared.artifact_pushes, push_key, || {
            sync_artifact(ctx, item, desc)
        })
        .await?;
    }

    Ok(false)
}

/// Copy one artifact from the source to the target repo: pull manifest,
/// push blobs, push manifest.
async fn sync_artifact(
    ctx: &TransferContext<'_>,
    item: &TransferTask,
    desc: &Descriptor,
) -> Result<(), crate::Error> {
    let (retry, shared) = (ctx.retry, ctx.shared);
    let source_client = &item.source_client;
    let source_repo = &item.source.repo;
    let artifact_digest_str = desc.digest.to_string();

    // Pulled once per run across targets.
    let pull_key = source_key(source_client, source_repo, &desc.digest);
    let artifact_pull = run_once(&shared.artifact_pulls, pull_key, || async {
        with_retry(retry, "artifact manifest pull", || {
            source_client.manifest_pull(source_repo, &artifact_digest_str)
        })
        .await
        .map(Rc::new)
        .map_err(|e| crate::Error::ArtifactSync {
            reference: artifact_digest_str.clone(),
            reason: format!("manifest pull failed: {e}"),
        })
    })
    .await?;

    // Artifact blobs take the image blob path: claimed, mounted, staged
    // and recorded like any other blob copy. Byte and blob counts stay out
    // of the image's report, as an artifact can be shared by many images.
    let blobs = child_blobs(&artifact_pull);
    let blob_digests: Vec<Digest> = blobs.iter().map(|b| b.digest.clone()).collect();
    if let Some(err) = transfer_image_blobs(ctx, blobs, &HashMap::new())
        .await
        .error
    {
        return Err(crate::Error::ArtifactSync {
            reference: artifact_digest_str.clone(),
            reason: format!("blob copy failed: {err}"),
        });
    }

    wait_for_commit(item, retry, &blob_digests).await;

    // Pushed by digest, not tag; its blob work stays out of the image's report.
    push_manifest(ctx, &artifact_digest_str, &artifact_pull, None)
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

    Ok(())
}

/// Discover referrers for a manifest from the source registry: the referrers
/// API first, then the tag-based fallback on 404.
async fn discover_referrers(
    source_client: &RegistryClient,
    source_repo: &RepositoryName,
    parent_digest: &Digest,
) -> Referrers {
    match source_client
        .referrers(source_repo, parent_digest, None)
        .await
    {
        Ok(Some(index)) => Referrers::Found(index),
        Ok(None) => {
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
                    ManifestKind::Index(index) => Referrers::Found(*index),
                    ManifestKind::Image(_) => {
                        // A single image manifest at the fallback tag is itself
                        // a referrer; wrap it in a synthetic one-entry index so
                        // the filter and transfer loop treat it uniformly.
                        let desc = Descriptor {
                            media_type: pull.media_type.clone(),
                            digest: pull.digest.clone(),
                            size: pull.raw_bytes.len() as u64,
                            platform: None,
                            artifact_type: None,
                            annotations: None,
                        };
                        Referrers::Found(ImageIndex {
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
                    debug!(
                        repo = %source_repo,
                        digest = %parent_digest,
                        "no referrers found (API 404, tag fallback 404)"
                    );
                    Referrers::NotFound
                }
                Err(e) => {
                    debug!(
                        repo = %source_repo,
                        digest = %parent_digest,
                        error = %e,
                        "tag fallback failed, skipping artifact sync"
                    );
                    Referrers::Failed
                }
            }
        }
        Err(e) => {
            // A failed artifact query must not fail the image sync.
            debug!(
                repo = %source_repo,
                digest = %parent_digest,
                error = %e,
                "referrers API failed, skipping artifact sync"
            );
            Referrers::Failed
        }
    }
}

/// Read a file in 256 KB chunks, yielding a stream of `Bytes`.
///
/// # Assumption: local filesystem
///
/// Reads are blocking `std::fs::Read` calls on the `current_thread` runtime:
/// microseconds on local disk, negligible next to network RTT. The staging
/// directory MUST therefore be on local disk (tmpfs, ext4, APFS); a network
/// filesystem (NFS, EFS, CIFS) can block for milliseconds to seconds per read
/// and stall the whole event loop. See [`BlobStage`] for staging path requirements.
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

#[cfg(test)]
mod tests {
    use ocync_distribution::Digest;
    use ocync_distribution::spec::{Descriptor, MediaType};

    use super::*;

    /// The default mount-source wait outlasts the longest commit wait measured
    /// on the benchmark corpus (149 s); a shorter one uploads blobs a mount
    /// would have saved.
    #[test]
    fn default_mount_source_wait_outlasts_the_longest_measured_commit_wait() {
        let engine = SyncEngine::new(RetryConfig::default(), DEFAULT_MAX_CONCURRENT_TRANSFERS);
        assert!(engine.mount_source_wait_deadline > Duration::from_secs(149));
    }

    /// A transferred image reports its own byte count, blob stats and elapsed
    /// time, not the zeroed defaults of a skip.
    #[test]
    fn into_result_carries_transfer_counts_and_elapsed_time() {
        let image = |repo: &str| ImageRef {
            repo: repo.parse().unwrap(),
            tag: "v1".into(),
        };
        let outcome = TargetBlobOutcome {
            bytes_transferred: 42,
            stats: BlobTransferStats {
                transferred: 2,
                skipped: 1,
                mounted: 3,
            },
            error: None,
        };
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(5));

        let result = outcome.into_result(&image("src"), &image("dst"), ImageStatus::Synced, start);

        assert_eq!(result.bytes_transferred, 42);
        assert_eq!(
            (
                result.blob_stats.transferred,
                result.blob_stats.skipped,
                result.blob_stats.mounted
            ),
            (2, 1, 3)
        );
        assert!(
            result.duration >= Duration::from_millis(5),
            "{:?}",
            result.duration
        );
    }

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
    fn collect_image_blobs_orders_config_first_then_layers() {
        // (name, config (suffix, size), layers, want (suffix, size) in order)
        type Blob = (&'static str, u64);
        let cases: [(&str, Blob, Vec<Blob>, Vec<Blob>); 2] = [
            (
                "config then two layers keep manifest order",
                ("c0", 50),
                vec![("a1", 200), ("b2", 300)],
                vec![("c0", 50), ("a1", 200), ("b2", 300)],
            ),
            (
                "config alone when there are no layers",
                ("cc", 42),
                vec![],
                vec![("cc", 42)],
            ),
        ];
        for (name, (cfg, cfg_size), layers, want) in cases {
            let manifest = ImageManifest {
                schema_version: 2,
                media_type: None,
                config: Descriptor {
                    size: cfg_size,
                    ..test_descriptor(test_digest(cfg), MediaType::OciConfig)
                },
                layers: layers
                    .iter()
                    .map(|&(suffix, size)| Descriptor {
                        size,
                        ..test_descriptor(test_digest(suffix), MediaType::OciLayerGzip)
                    })
                    .collect(),
                subject: None,
                artifact_type: None,
                annotations: None,
            };

            let got: Vec<(Digest, u64)> = collect_image_blobs(&manifest)
                .iter()
                .map(|d| (d.digest.clone(), d.size))
                .collect();
            let want: Vec<(Digest, u64)> = want
                .iter()
                .map(|&(suffix, size)| (test_digest(suffix), size))
                .collect();
            assert_eq!(got, want, "{name}");
        }
    }

    fn make_image_result(status: ImageStatus, bytes: u64) -> ImageResult {
        let image = |repo: &str| ImageRef {
            repo: repo.parse().unwrap(),
            tag: "tag".into(),
        };
        ImageResult {
            bytes_transferred: bytes,
            ..image_result(&image("source/repo"), &image("target/repo"), status)
        }
    }

    #[test]
    fn compute_stats_counts_each_status_and_sums_synced_bytes() {
        // want: (synced, skipped, failed, bytes_transferred)
        let cases = [
            (
                "one of each status; only the synced image's bytes count",
                vec![
                    make_image_result(ImageStatus::Synced, 1024),
                    make_image_result(
                        ImageStatus::Skipped {
                            reason: SkipReason::ImmutableTag,
                        },
                        512,
                    ),
                    make_image_result(
                        ImageStatus::Failed {
                            kind: ErrorKind::BlobTransfer,
                            error: "timeout".into(),
                            retries: 3,
                            status_code: None,
                        },
                        256,
                    ),
                ],
                (1, 1, 1, 1024),
            ),
            ("no images gives all zeros", vec![], (0, 0, 0, 0)),
        ];
        for (name, images, want) in cases {
            let stats = compute_stats(&images);
            let got = (
                stats.images_synced,
                stats.images_skipped,
                stats.images_failed,
                stats.bytes_transferred,
            );
            assert_eq!(got, want, "{name}");
        }
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

    /// A staged file that fails to read mid-upload surfaces wrapped in the
    /// HTTP client's error on the streaming path; it is still local I/O.
    #[tokio::test]
    async fn staging_read_error_during_streaming_push_is_local_io() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(202).insert_header("location", "/v2/repo/blobs/uploads/id"),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        let client = RegistryClient::builder(server.uri().parse().unwrap())
            .build()
            .unwrap();
        let digest = Digest::from_sha256(Sha256::digest(b"blob"));
        let unreadable = futures_util::stream::iter([Err(ocync_distribution::Error::Io {
            context: "staging read",
            source: std::io::Error::other("disk read failed").into(),
        })]);

        let source = client
            .blob_push_stream(
                &RepositoryName::new("repo").unwrap(),
                &digest,
                Some(4),
                unreadable,
            )
            .await
            .expect_err("an unreadable body cannot upload");
        assert!(
            is_local_io(&crate::Error::BlobTransfer { digest, source }),
            "a wrapped staging read error must not be recorded as a registry failure"
        );
    }

    #[test]
    fn only_registry_failures_are_recorded_as_copy_failures() {
        let digest = Digest::from_sha256(Sha256::digest(b"blob"));
        let local = crate::Error::BlobTransfer {
            digest: digest.clone(),
            source: ocync_distribution::Error::Io {
                context: "staging write",
                source: std::io::Error::other("disk full").into(),
            },
        };
        let registry = crate::Error::BlobTransfer {
            digest,
            source: ocync_distribution::Error::RegistryError {
                status: http::StatusCode::NOT_FOUND,
                message: "blob unknown".into(),
            },
        };
        assert!(
            is_local_io(&local),
            "a disk error must not fail other tasks"
        );
        assert!(!is_local_io(&registry));
    }

    // elect_leaders

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
            target_entry: Rc::new(TargetEntry::new(
                RegistryAlias::new("test-target"),
                client,
                None,
            )),
            source: ImageRef {
                repo: RepositoryName::new(format!("source/{tag}")).unwrap(),
                tag: tag.to_owned(),
            },
            target: ImageRef {
                repo: RepositoryName::new(format!("target/{tag}")).unwrap(),
                tag: tag.to_owned(),
            },
            artifacts_config: Rc::new(ResolvedArtifacts::default()),
            discovery_retries: 0,
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
    fn elect_leaders_returns_zero_without_election_benefit() {
        let cases: [(&str, Vec<Vec<&str>>); 3] = [
            ("empty deque", vec![]),
            ("a single task", vec![vec!["c1", "a1"]]),
            (
                "three images with disjoint blob sets",
                vec![vec!["c1", "aa"], vec!["c2", "bb"], vec!["c3", "cc"]],
            ),
        ];
        for (name, images) in cases {
            let mut pending: VecDeque<TransferTask> = images
                .iter()
                .enumerate()
                .map(|(i, blobs)| {
                    test_task(Rc::new(test_pulled_manifest(blobs)), &format!("img{i}"))
                })
                .collect();
            assert_eq!(elect_leaders(&mut pending), 0, "{name}");
        }
    }

    #[test]
    fn elect_leaders_picks_most_shared() {
        // base:    {c0, a1, a2, a3}, shares 3 with each child
        // child_a: {c0, a1, a2, d1}, shares 2 with child_b
        // child_b: {c0, a1, a3, e1}
        //
        // Sum of intersections: base 6, children 5 each, so base wins.
        let data_base = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "a3"]));
        let data_ca = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "d1"]));
        let data_cb = Rc::new(test_pulled_manifest(&["c0", "a1", "a3", "e1"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&data_base), "base"));
        pending.push_back(test_task(Rc::clone(&data_ca), "child_a"));
        pending.push_back(test_task(Rc::clone(&data_cb), "child_b"));

        let n = elect_leaders(&mut pending);

        assert_eq!(n, 1);
        assert_eq!(pending[0].source.tag, "base");
        let leader_blobs = leader_blob_digests(&pending, n);
        assert_eq!(leader_blobs.len(), 4);
        assert!(leader_blobs.contains(&test_digest("c0")));
        assert!(leader_blobs.contains(&test_digest("a3")));
        // Followers keep their original order.
        assert_eq!(pending[1].source.tag, "child_a");
        assert_eq!(pending[2].source.tag, "child_b");
    }

    #[test]
    fn elect_leaders_multi_target_groups_by_rc() {
        // Same blob sets as `elect_leaders_picks_most_shared`, but base targets
        // two registries (two tasks sharing one Rc), so electing it yields n = 2.
        let base = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "a3"]));
        let child_a = Rc::new(test_pulled_manifest(&["c0", "a1", "a2", "d1"]));
        let child_b = Rc::new(test_pulled_manifest(&["c0", "a1", "a3", "e1"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&base), "v1"));
        pending.push_back(test_task(Rc::clone(&base), "v1"));
        pending.push_back(test_task(child_a, "child_a"));
        pending.push_back(test_task(child_b, "child_b"));

        let n = elect_leaders(&mut pending);

        assert_eq!(n, 2);
        let leader_blobs = leader_blob_digests(&pending, n);
        assert_eq!(leader_blobs.len(), 4); // {c0, a1, a2, a3}
        assert_eq!(pending[0].source.tag, "v1");
        assert_eq!(pending[1].source.tag, "v1");
        assert_eq!(pending[2].source.tag, "child_a");
        assert_eq!(pending[3].source.tag, "child_b");
    }

    #[test]
    fn elect_leaders_marginal_coverage_across_rounds() {
        // A spans both clusters via blob c0, so round 1 elects A (score 4 vs
        // 2, 3, 3). Round 2 must deduct A's blobs: C and D share {c0, d0}, but
        // c0 is covered, leaving d0 as their only marginal blob (1 each, B 0).
        //
        // A: {a0, b0, c0}  B: {a0, b0, e0}  C: {c0, d0, f0}  D: {c0, d0, fa}
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

        // A (round 1) + one of C/D (round 2, marginal on d0).
        assert_eq!(n, 2);
        let leader_blobs = leader_blob_digests(&pending, n);
        assert!(leader_blobs.contains(&test_digest("a0")));
        assert!(leader_blobs.contains(&test_digest("d0")));

        let leader_tags: Vec<&str> = pending
            .iter()
            .take(n)
            .map(|t| t.source.tag.as_str())
            .collect();

        assert_eq!(leader_tags[0], "imgA");
        // Not B: it shares no marginal blobs once A's {a0,b0,c0} are deducted.
        assert!(
            leader_tags[1] == "imgC" || leader_tags[1] == "imgD",
            "second leader should be from cluster 2, got: {leader_tags:?}"
        );
    }

    #[test]
    fn elect_leaders_tie_breaks_deterministically() {
        // A: {a0, b0}  B: {a0, b0}  C: {a0, c0}. A and B tie, and
        // max_by_key returns the last maximum, so B (higher index) wins.
        let data_a = Rc::new(test_pulled_manifest(&["a0", "b0"]));
        let data_b = Rc::new(test_pulled_manifest(&["a0", "b0"]));
        let data_c = Rc::new(test_pulled_manifest(&["a0", "c0"]));

        let mut pending = VecDeque::new();
        pending.push_back(test_task(Rc::clone(&data_a), "imgA"));
        pending.push_back(test_task(Rc::clone(&data_b), "imgB"));
        pending.push_back(test_task(Rc::clone(&data_c), "imgC"));

        let n = elect_leaders(&mut pending);
        assert_eq!(n, 1);
        assert_eq!(
            pending[0].source.tag, "imgB",
            "tie should break deterministically in favor of later candidate"
        );
    }

    #[test]
    fn elect_leaders_minimal_set_covers_all_follower_shared_blobs() {
        // The greedy election picks the minimal leader set such that every
        // shared blob of every follower is in the leader union. Two clusters
        // ({img1, img2, img3} sharing a1..a3, {img4, img5} sharing b4, b5)
        // need exactly two leaders.
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

        assert_eq!(
            n, 2,
            "should elect exactly 2 leaders for 2 disjoint clusters"
        );

        let leader_blobs = leader_blob_digests(&pending, n);

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
            // Shared means present in at least one other image.
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

    #[test]
    fn elect_leaders_scales_subcubically() {
        // With 75 disjoint pairs (n=150 distinct source manifests) the greedy
        // loop runs one round per pair: an O(n^3) version takes hundreds of
        // milliseconds, a sub-cubic one under 500ms.
        let n_pairs: usize = 75;
        let mut counter: usize = 0;
        let mut next = || {
            let s = format!("{counter:x}");
            counter += 1;
            s
        };

        let mut pending = VecDeque::new();
        for pair_idx in 0..n_pairs {
            let shared = next();
            let cfg_a = next();
            let lay_a1 = next();
            let lay_a2 = next();
            let data_a = Rc::new(test_pulled_manifest(&[&cfg_a, &shared, &lay_a1, &lay_a2]));
            pending.push_back(test_task(data_a, &format!("a{pair_idx}")));

            let cfg_b = next();
            let lay_b1 = next();
            let lay_b2 = next();
            let data_b = Rc::new(test_pulled_manifest(&[&cfg_b, &shared, &lay_b1, &lay_b2]));
            pending.push_back(test_task(data_b, &format!("b{pair_idx}")));
        }

        let start = Instant::now();
        let leader_count = elect_leaders(&mut pending);
        let elapsed = start.elapsed();

        // Electing one member of a pair drops the other's marginal coverage to 0.
        assert_eq!(leader_count, n_pairs, "expected one leader per pair");
        assert!(
            elapsed < Duration::from_millis(500),
            "elect_leaders took {elapsed:?} for n=150 (75 pairs); expected sub-500ms on a non-cubic implementation",
        );
    }

    #[test]
    fn type_matches_applies_include_then_exclude() {
        const SPDX: &str = "application/spdx+json";
        const COSIGN: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";
        // (name, include, exclude, artifact type, want)
        let cases = [
            ("no filters pass cosign", vec![], vec![], COSIGN, true),
            ("no filters pass spdx", vec![], vec![], SPDX, true),
            ("no filters pass anything", vec![], vec![], "anything", true),
            (
                "include admits a listed type",
                vec![SPDX],
                vec![],
                SPDX,
                true,
            ),
            (
                "include rejects an unlisted type",
                vec![SPDX],
                vec![],
                COSIGN,
                false,
            ),
            (
                "exclude rejects a listed type",
                vec![],
                vec![SPDX],
                SPDX,
                false,
            ),
            (
                "exclude admits other types",
                vec![],
                vec![SPDX],
                COSIGN,
                true,
            ),
            (
                "exclude beats include",
                vec![SPDX, COSIGN],
                vec![SPDX],
                SPDX,
                false,
            ),
            (
                "include still admits the non-excluded type",
                vec![SPDX, COSIGN],
                vec![SPDX],
                COSIGN,
                true,
            ),
        ];
        for (name, include, exclude, artifact_type, want) in cases {
            let config = ResolvedArtifacts {
                enabled: true,
                include: include.into_iter().map(String::from).collect(),
                exclude: exclude.into_iter().map(String::from).collect(),
                require_artifacts: false,
            };
            assert_eq!(config.type_matches(artifact_type), want, "{name}");
        }
    }

    #[test]
    fn discovery_counters_count_each_route() {
        // want: [cache_hits, cache_misses, head_failures, target_stale, head_first_skips]
        let cases = [
            (
                "HeadFailure counts as a cache miss and a head failure",
                vec![DiscoveryRoute::HeadFailure],
                [0, 1, 1, 0, 0],
            ),
            (
                "TargetStale counts as a cache miss and a stale target",
                vec![DiscoveryRoute::TargetStale],
                [0, 1, 0, 1, 0],
            ),
            (
                "every route once: CacheMiss, HeadFailure and TargetStale make 3 misses",
                vec![
                    DiscoveryRoute::CacheHit,
                    DiscoveryRoute::CacheMiss,
                    DiscoveryRoute::HeadFailure,
                    DiscoveryRoute::TargetStale,
                    DiscoveryRoute::HeadFirstSkip,
                ],
                [1, 3, 1, 1, 1],
            ),
        ];
        for (name, routes, want) in cases {
            let mut c = DiscoveryCounters::default();
            for route in &routes {
                c.record(route);
            }
            let got = [
                c.cache_hits,
                c.cache_misses,
                c.head_failures,
                c.target_stale,
                c.head_first_skips,
            ];
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn write_timing_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timing.jsonl");
        let mut file = Some(std::fs::File::create(&path).unwrap());
        let start = Instant::now();

        let phases = [
            (TimingPhase::DiscoveryStart, "discovery_start"),
            (TimingPhase::DiscoveryComplete, "discovery_complete"),
            (TimingPhase::ExecutionComplete, "execution_complete"),
        ];
        for (phase, _) in phases {
            write_timing(&mut file, phase, &start);
        }

        drop(file);
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), phases.len());
        for (i, (_, phase)) in phases.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(lines[i]).unwrap();
            assert_eq!(v["phase"].as_str().unwrap(), *phase);
            assert!(v["elapsed_ms"].as_u64().is_some());
        }
    }
}
