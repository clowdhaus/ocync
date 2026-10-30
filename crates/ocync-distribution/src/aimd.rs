//! AIMD concurrency controller - adaptive rate limiting via additive increase, multiplicative decrease.
//!
//! Each registry action gets its own `AimdWindow`, which grows the allowed concurrency
//! by one unit per successful response and halves on a 429 (Too Many Requests). A
//! congestion epoch prevents multiple halvings from the same burst of 429s.
//!
//! `AimdController` manages one set of windows per registry host under an
//! aggregate concurrency cap. Windows of registries with documented TPS
//! ceilings are also paced by a `TokenBucket`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::detect::{ProviderKind, detect_provider_kind};

/// Default congestion epoch - prevents multiple halvings from the same burst.
const DEFAULT_EPOCH: Duration = Duration::from_millis(100);

/// Default initial concurrency window size.
///
/// Starts at 1.0 so the first request to any registry always succeeds (no
/// blind burst). AIMD additive increase (`w += 1/w`) reaches window=5 after
/// ~12 successes -- under 1 second at typical cloud RTTs. This eliminates
/// the class of startup-burst 429s observed on registries with low burst
/// tolerance (Docker Hub cold sync: 21 429s at window=5, 0 at window=1).
const DEFAULT_INITIAL_WINDOW: f64 = 1.0;

/// One AIMD window tracking concurrency for a specific registry action.
///
/// The window value is a floating-point "virtual slot count". [`limit`](AimdWindow::limit)
/// converts it to a usable `usize` by taking the ceiling, capped at `cap`.
#[derive(Debug)]
pub(crate) struct AimdWindow {
    window: f64,
    cap: usize,
    last_decrease: Instant,
    epoch: Duration,
}

impl AimdWindow {
    /// A window with the given initial value and cap and the default epoch.
    pub(crate) fn new(initial: f64, cap: usize) -> Self {
        Self::with_epoch(initial, cap, DEFAULT_EPOCH)
    }

    /// A window with a custom congestion epoch: how long after a halving
    /// before another can occur.
    pub(crate) fn with_epoch(initial: f64, cap: usize, epoch: Duration) -> Self {
        Self {
            window: initial,
            cap,
            last_decrease: Instant::now() - epoch - Duration::from_millis(1),
            epoch,
        }
    }

    /// Current concurrency limit - ceiling of the window, capped at `cap`.
    pub(crate) fn limit(&self) -> usize {
        (self.window.ceil() as usize).min(self.cap)
    }

    /// Record a successful response: additive increase by `1.0 / window`,
    /// TCP-style.
    pub(crate) fn on_success(&mut self) {
        self.window += 1.0 / self.window;
    }

    /// The raw floating-point window value.
    #[cfg(test)]
    pub(crate) fn window_value(&self) -> f64 {
        self.window
    }

    /// Record a 429: halve the window (minimum 1.0), at most once per epoch.
    pub(crate) fn on_throttle(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_decrease) > self.epoch {
            self.window = (self.window / 2.0).max(1.0);
            self.last_decrease = now;
        }
    }
}

/// Token-bucket rate limiter. Tokens refill at `rate_per_sec` up to `burst`;
/// [`acquire`](Self::acquire) blocks until a token is available.
///
/// Enforces a documented registry rate ceiling (see
/// `bucket_config_for_window`) that AIMD's concurrency window cannot.
///
/// Refill and sleep must share tokio's clock: with `std::time::Instant` the
/// bucket livelocks under paused test time, since the std clock never
/// advances while `sleep` returns at once.
#[derive(Debug)]
pub(crate) struct TokenBucket {
    rate_per_sec: f64,
    burst: f64,
    state: Mutex<TokenBucketState>,
}

#[derive(Debug)]
struct TokenBucketState {
    tokens: f64,
    last_refill: tokio::time::Instant,
}

impl TokenBucket {
    pub(crate) fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            rate_per_sec,
            burst,
            state: Mutex::new(TokenBucketState {
                tokens: burst,
                last_refill: tokio::time::Instant::now(),
            }),
        }
    }

    /// Acquire a single token, sleeping until one is available.
    pub(crate) async fn acquire(&self) {
        loop {
            let sleep_for = {
                let mut s = self.state.lock().expect("token-bucket lock poisoned");
                let now = tokio::time::Instant::now();
                let elapsed = now.duration_since(s.last_refill).as_secs_f64();
                s.tokens = (s.tokens + elapsed * self.rate_per_sec).min(self.burst);
                s.last_refill = now;
                if s.tokens >= 1.0 {
                    s.tokens -= 1.0;
                    return;
                }
                let need = 1.0 - s.tokens;
                Duration::from_secs_f64(need / self.rate_per_sec)
            };
            // The lock is released before sleeping, or every consumer
            // would serialize on it.
            tokio::time::sleep(sleep_for).await;
        }
    }
}

/// An OCI registry API action, used to map operations to independent AIMD windows.
///
/// ECR enforces rate limits at the individual action level (e.g., `InitiateLayerUpload`
/// at 100 TPS vs `UploadLayerPart` at 500 TPS). Using a single window for all uploads
/// would cause the tighter limit to throttle the faster action unnecessarily.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryAction {
    /// `HEAD /v2/{name}/manifests/{reference}`
    ManifestHead,
    /// `GET /v2/{name}/manifests/{reference}`
    ManifestRead,
    /// `PUT /v2/{name}/manifests/{reference}` - ECR enforces ~10 TPS
    ManifestWrite,
    /// `HEAD /v2/{name}/blobs/{digest}`
    BlobHead,
    /// `GET /v2/{name}/blobs/{digest}`
    BlobRead,
    /// `POST /v2/{name}/blobs/uploads/` - ECR enforces ~100 TPS
    BlobUploadInit,
    /// `PATCH /v2/{name}/blobs/uploads/{uuid}` - ECR enforces ~500 TPS
    BlobUploadChunk,
    /// `PUT /v2/{name}/blobs/uploads/{uuid}?digest=` - ECR enforces ~100 TPS
    BlobUploadComplete,
    /// `GET /v2/{name}/tags/list` - separate from manifest reads
    TagList,
}

/// Identifies a per-action AIMD window within an [`AimdController`], at
/// each provider's own rate-limit granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WindowKey {
    /// ECR private: every API action has an independent per-region TPS cap.
    Ecr(RegistryAction),
    /// ECR Public: same per-action enforcement model but caps are 10x lower
    /// and read paths are aggregated.
    EcrPublic(EcrPublicGroup),
    /// Docker Hub: HEADs unmetered, manifest reads quota'd, others shared.
    DockerHub(DockerHubGroup),
    /// GAR / GCR: a single per-project, per-region quota shared across all
    /// actions.
    GarShared,
    /// GHCR: a single 2000 RPM aggregate cap per authenticated principal,
    /// shared across reads and writes.
    GhcrShared,
    /// ACR: separate `ReadOps` and `WriteOps` quotas per registry.
    Acr(AcrGroup),
    /// Coarse fallback for registries with no documented per-action cap
    /// (Chainguard, Quay, generic). AIMD discovers capacity from 429s.
    Unknown(UnknownGroup),
}

/// Action grouping for ECR Public: all read paths share one window, each
/// write action gets its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // Variants are self-describing.
pub(crate) enum EcrPublicGroup {
    /// Manifest reads/heads, blob reads/heads, and tag list.
    ///
    /// Paced from the 10 TPS authenticated pull quota. AWS publishes no TPS
    /// quota for tag listing, and tag listing has been measured throttling
    /// below this pacing; see `bucket_config_for_window`.
    Read,
    ManifestWrite,
    BlobUploadInit,
    BlobUploadChunk,
    BlobUploadComplete,
}

/// Action grouping for Docker Hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DockerHubGroup {
    /// Manifest and blob HEADs -- unmetered.
    Heads,
    /// Manifest GETs -- counted against the 100/6h pull quota.
    ManifestRead,
    /// All other actions share one window.
    Other,
}

/// Action grouping for ACR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AcrGroup {
    /// Reads + heads + tag list (`ReadOps`).
    Read,
    /// Manifest writes + blob upload phases (`WriteOps`).
    Write,
}

/// Coarse action grouping for registries without a documented per-action cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // Variants are self-describing.
pub(crate) enum UnknownGroup {
    Heads,
    Reads,
    Uploads,
    ManifestWrite,
    TagList,
}

/// Map a registry's detected provider and an action to its AIMD window key.
fn window_key_for_provider(provider: Option<ProviderKind>, action: RegistryAction) -> WindowKey {
    use RegistryAction::*;
    match provider {
        Some(ProviderKind::Ecr) => WindowKey::Ecr(action),
        Some(ProviderKind::EcrPublic) => WindowKey::EcrPublic(match action {
            ManifestHead | ManifestRead | BlobHead | BlobRead | TagList => EcrPublicGroup::Read,
            ManifestWrite => EcrPublicGroup::ManifestWrite,
            BlobUploadInit => EcrPublicGroup::BlobUploadInit,
            BlobUploadChunk => EcrPublicGroup::BlobUploadChunk,
            BlobUploadComplete => EcrPublicGroup::BlobUploadComplete,
        }),
        Some(ProviderKind::DockerHub) => WindowKey::DockerHub(match action {
            ManifestHead | BlobHead => DockerHubGroup::Heads,
            ManifestRead => DockerHubGroup::ManifestRead,
            _ => DockerHubGroup::Other,
        }),
        // GCR is an alias for GAR.
        Some(ProviderKind::Gar | ProviderKind::Gcr) => WindowKey::GarShared,
        Some(ProviderKind::Ghcr) => WindowKey::GhcrShared,
        Some(ProviderKind::Acr) => WindowKey::Acr(match action {
            ManifestHead | ManifestRead | BlobHead | BlobRead | TagList => AcrGroup::Read,
            ManifestWrite | BlobUploadInit | BlobUploadChunk | BlobUploadComplete => {
                AcrGroup::Write
            }
        }),
        _ => WindowKey::Unknown(match action {
            ManifestHead | BlobHead => UnknownGroup::Heads,
            ManifestRead | BlobRead => UnknownGroup::Reads,
            BlobUploadInit | BlobUploadChunk | BlobUploadComplete => UnknownGroup::Uploads,
            ManifestWrite => UnknownGroup::ManifestWrite,
            TagList => UnknownGroup::TagList,
        }),
    }
}

/// Token-bucket configuration for a [`WindowKey`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct BucketConfig {
    pub(crate) rate_per_sec: f64,
    pub(crate) burst: f64,
}

/// Token-bucket configuration for the given window key, or `None` if rate
/// gating is not configured for this window (AIMD alone governs concurrency).
///
/// Values are at least 20% under documented caps to absorb tail bursts
/// without artificial throttling at steady state. Caps verified against
/// official registry documentation (AWS service quotas, Google quotas
/// page, Microsoft historical SKU table) on 2026-04-26.
fn bucket_config_for_window(key: WindowKey) -> Option<BucketConfig> {
    use RegistryAction::*;
    let cfg = |rate_per_sec: f64, burst: f64| BucketConfig {
        rate_per_sec,
        burst,
    };
    Some(match key {
        // ECR private (per-account, per-region; AWS docs).
        // PutImage 10 TPS, InitiateLayerUpload/CompleteLayerUpload 100 TPS,
        // UploadLayerPart 500 TPS. Reads (HEAD/GET/TagList) have headroom
        // far above any sync workload, so AIMD alone governs them.
        WindowKey::Ecr(ManifestWrite) => cfg(8.0, 10.0),
        WindowKey::Ecr(BlobUploadInit | BlobUploadComplete) => cfg(80.0, 20.0),
        WindowKey::Ecr(BlobUploadChunk) => cfg(400.0, 50.0),
        // ECR Public (per-account, per-region; AWS docs).
        // PutImage / InitiateLayerUpload / CompleteLayerUpload / authenticated
        // pulls 10 TPS each, UploadLayerPart 260 TPS. Unauthenticated pulls
        // are 1 TPS and not adjustable, which this table does not model
        // because the window key is derived from host and action only.
        //
        // The read value is measured-insufficient, not documented: a
        // 95-mapping run on 2026-08-28 with credentials loaded drew repeated
        // 429s on TagList at this pacing, halving the window 11 -> 6 -> 3.
        // AWS documents no TPS quota for tag listing at all, so grouping it
        // with pulls is an assumption. `list_tags` retries the 429 rather
        // than failing the mapping; lowering this value is the other half and
        // wants its own measurement rather than a guess.
        WindowKey::EcrPublic(EcrPublicGroup::BlobUploadChunk) => cfg(200.0, 30.0),
        WindowKey::EcrPublic(_) => cfg(8.0, 10.0),
        // GHCR: single 2000 RPM (~33 RPS) aggregate cap per authenticated
        // principal. 20 TPS leaves 40% headroom for tail bursts.
        WindowKey::GhcrShared => cfg(20.0, 10.0),
        // GAR/GCR: 60,000 RPM total (~1000 RPS) and 18,000 RPM writes
        // (~300 RPS) per project per region. 240 TPS shared keeps 20%
        // margin under the write ceiling, the tighter of the two.
        WindowKey::GarShared => cfg(240.0, 30.0),
        // ACR Premium SKU historical defaults: ~10,000 ReadOps/min (167 RPS)
        // and ~2,000 WriteOps/min (33 RPS). Microsoft removed the explicit
        // table in 2026-03; values preserved from the prior published
        // numbers. Basic / Standard SKUs hit AIMD halve on first 429.
        WindowKey::Acr(AcrGroup::Read) => cfg(130.0, 30.0),
        WindowKey::Acr(AcrGroup::Write) => cfg(26.0, 10.0),
        // All other windows: AIMD only.
        WindowKey::Ecr(_) | WindowKey::DockerHub(_) | WindowKey::Unknown(_) => return None,
    })
}

/// How a request admitted by an [`AimdPermit`] ended.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Success,
    Throttled,
}

/// State stored per AIMD window inside [`AimdController`].
struct WindowState {
    window: AimdWindow,
    /// Semaphore enforcing the current window limit. Never replaced: a
    /// request queued on it must stay reachable by every later release.
    semaphore: Arc<Semaphore>,
    /// Permits still to retire after a shrink, taken as their holders
    /// release them. Total permits minus `owed` equals the window limit.
    owed: usize,
    /// Optional token bucket for hard TPS ceilings (None = AIMD only).
    bucket: Option<Arc<TokenBucket>>,
}

impl WindowState {
    /// Build a fresh window for `key` capped at `max_concurrent`.
    fn new_for(key: WindowKey, max_concurrent: usize) -> Self {
        let initial = DEFAULT_INITIAL_WINDOW.min(max_concurrent as f64);
        let window = AimdWindow::new(initial, max_concurrent);
        let limit = window.limit();
        let bucket = bucket_config_for_window(key)
            .map(|c| Arc::new(TokenBucket::new(c.rate_per_sec, c.burst)));
        Self {
            window,
            semaphore: Arc::new(Semaphore::new(limit)),
            owed: 0,
            bucket,
        }
    }

    /// Apply `outcome` to the window, then resize the semaphore to the new
    /// limit and return or retire `permit`. A shrink retires idle permits at
    /// once and the rest as they are released, so requests in flight above
    /// the new limit are not joined by new ones. Returns the old and new
    /// limits.
    fn settle(&mut self, outcome: Outcome, permit: OwnedSemaphorePermit) -> (usize, usize) {
        let old_limit = self.window.limit();
        match outcome {
            Outcome::Success => self.window.on_success(),
            Outcome::Throttled => self.window.on_throttle(),
        }
        let new_limit = self.window.limit();
        if new_limit > old_limit {
            let repaid = self.owed.min(new_limit - old_limit);
            self.owed -= repaid;
            self.semaphore.add_permits(new_limit - old_limit - repaid);
        } else {
            self.owed += old_limit - new_limit;
        }
        if self.owed > 0 {
            permit.forget();
            self.owed -= 1;
        }
        self.owed -= self.semaphore.forget_permits(self.owed);
        (old_limit, new_limit)
    }
}

impl std::fmt::Debug for WindowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowState")
            .field("limit", &self.window.limit())
            .finish_non_exhaustive()
    }
}

/// Adaptive concurrency controller for a single registry host.
///
/// An aggregate cap bounds all requests to the host; per-action
/// `AimdWindow`s find each endpoint's own limit from 429 feedback.
///
/// A [`Mutex`] keeps the controller `Sync` for `Arc<RegistryClient>`. It is
/// never held across an await.
#[derive(Debug)]
pub struct AimdController {
    host: String,
    /// Detected once from `host`, so a request does not re-parse it.
    provider: Option<ProviderKind>,
    aggregate: Arc<Semaphore>,
    windows: Mutex<HashMap<WindowKey, WindowState>>,
    max_concurrent: usize,
}

impl AimdController {
    /// A controller for `host` with `max_concurrent` as the aggregate limit.
    pub fn new(host: &str, max_concurrent: usize) -> Self {
        Self {
            host: host.to_owned(),
            provider: detect_provider_kind(host),
            aggregate: Arc::new(Semaphore::new(max_concurrent)),
            windows: Mutex::new(HashMap::new()),
            max_concurrent,
        }
    }

    /// The provider detected from the controller's host.
    pub(crate) fn provider(&self) -> Option<ProviderKind> {
        self.provider
    }

    /// Acquire the window's rate token, if paced, then its window and
    /// aggregate permits.
    pub async fn acquire(&self, op: RegistryAction) -> AimdPermit<'_> {
        let key = window_key_for_provider(self.provider, op);

        let (action_semaphore, bucket) = {
            let mut map = self.windows.lock().expect("aimd lock poisoned");
            let state = map
                .entry(key)
                .or_insert_with(|| WindowState::new_for(key, self.max_concurrent));
            (Arc::clone(&state.semaphore), state.bucket.clone())
        };

        // Paced before any permit, so a paced action holds no slot another
        // window could use.
        if let Some(b) = &bucket {
            b.acquire().await;
        }

        // Per-action first, then aggregate: a request queued on a throttled
        // window must not hold aggregate capacity other windows need.
        let action_permit = action_semaphore
            .acquire_owned()
            .await
            .expect("action semaphore closed");
        let aggregate_permit = Arc::clone(&self.aggregate)
            .acquire_owned()
            .await
            .expect("aggregate semaphore closed");

        AimdPermit {
            key,
            windows: &self.windows,
            aggregate_permit,
            action_permit: Some(action_permit),
            outcome: Outcome::Success,
            host: self.host.clone(),
            action: op,
        }
    }

    /// Current window limit for `op`, the initial value before its first use.
    pub fn window_limit(&self, op: RegistryAction) -> usize {
        let key = window_key_for_provider(self.provider, op);
        let map = self.windows.lock().expect("aimd lock poisoned");
        map.get(&key).map(|s| s.window.limit()).unwrap_or_else(|| {
            let initial = DEFAULT_INITIAL_WINDOW.min(self.max_concurrent as f64);
            AimdWindow::new(initial, self.max_concurrent).limit()
        })
    }
}

/// RAII permit returned by [`AimdController::acquire`].
///
/// Resolved by [`success`](AimdPermit::success) or
/// [`throttled`](AimdPermit::throttled); the window is updated when the
/// permit drops. Dropped unresolved, the outcome is a success:
/// non-rate-limit errors (network timeouts, auth failures) must not penalise
/// the window.
pub struct AimdPermit<'a> {
    key: WindowKey,
    windows: &'a Mutex<HashMap<WindowKey, WindowState>>,
    /// Held for RAII: returned to the aggregate semaphore on drop.
    #[allow(dead_code)]
    aggregate_permit: OwnedSemaphorePermit,
    /// `Option` so `drop` can hand the permit to [`WindowState::settle`].
    action_permit: Option<OwnedSemaphorePermit>,
    outcome: Outcome,
    /// Registry hostname for diagnostic logging on a halving.
    host: String,
    /// The operation type for diagnostic logging on a halving.
    action: RegistryAction,
}

impl std::fmt::Debug for AimdPermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AimdPermit")
            .field("key", &self.key)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

impl AimdPermit<'_> {
    /// Report a successful response and release both permits.
    pub fn success(self) {}

    /// Report a 429 throttle response and release both permits, halving
    /// the window at most once per congestion epoch.
    pub fn throttled(mut self) {
        self.outcome = Outcome::Throttled;
    }
}

impl Drop for AimdPermit<'_> {
    fn drop(&mut self) {
        let Some(permit) = self.action_permit.take() else {
            return;
        };
        // A poisoned lock must not panic here: a permit can drop while a
        // panic unwinds, and a second panic aborts the process.
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = windows.get_mut(&self.key) else {
            return;
        };
        let (old_window, new_window) = state.settle(self.outcome, permit);
        if new_window < old_window {
            tracing::warn!(
                registry = %self.host,
                action = ?self.action,
                old_window,
                new_window,
                "AIMD halved on 429"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // ---------------------------------------------------------------------------
    // AimdWindow arithmetic - these tests use window_value() which is cfg(test) only
    // ---------------------------------------------------------------------------

    #[test]
    fn window_limit_is_ceiling_of_value() {
        // Initial value 1.5 → ceiling = 2
        let mut w = AimdWindow::with_epoch(1.5, 16, Duration::from_millis(1));
        assert_eq!(w.limit(), 2);

        // After one success: 1.5 + 1/1.5 ≈ 2.167 → ceiling = 3
        w.on_success();
        assert_eq!(w.limit(), 3);
    }

    #[test]
    fn window_limit_capped_at_cap() {
        let w = AimdWindow::with_epoch(100.0, 8, Duration::from_millis(1));
        assert_eq!(w.limit(), 8, "window above cap should be clamped to cap");
    }

    #[test]
    fn window_success_increases_by_inverse_of_window() {
        let mut w = AimdWindow::with_epoch(4.0, 64, Duration::from_millis(1));
        let before = w.window_value();
        w.on_success();
        let after = w.window_value();
        let expected = before + 1.0 / before;
        assert!((after - expected).abs() < 1e-10);
    }

    #[test]
    fn window_throttle_halves_window() {
        let mut w = AimdWindow::with_epoch(8.0, 64, Duration::from_millis(1));
        // Sleep past the epoch so the decrease is allowed.
        std::thread::sleep(Duration::from_millis(5));
        w.on_throttle();
        assert!((w.window_value() - 4.0).abs() < 1e-10);
    }

    #[test]
    fn window_throttle_minimum_one() {
        let mut w = AimdWindow::with_epoch(1.0, 64, Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        w.on_throttle();
        assert!(
            (w.window_value() - 1.0).abs() < 1e-10,
            "window should not go below 1.0"
        );
    }

    // ---------------------------------------------------------------------------
    // Epoch prevents multiple decreases from the same burst
    // ---------------------------------------------------------------------------

    #[test]
    fn epoch_prevents_second_halving_within_epoch() {
        // Use a long epoch so the second call is still within it.
        let mut w = AimdWindow::with_epoch(8.0, 64, Duration::from_secs(60));
        // The initial last_decrease is set to (now - epoch - 1ms), so the
        // first throttle is always allowed.
        w.on_throttle(); // allowed → window = 4.0
        let after_first = w.window_value();
        assert!((after_first - 4.0).abs() < 1e-10);

        w.on_throttle(); // within epoch → ignored
        assert!(
            (w.window_value() - after_first).abs() < 1e-10,
            "second throttle within epoch should be ignored"
        );
    }

    #[test]
    fn epoch_expiry_allows_new_decrease() {
        // Use a very short epoch so we can sleep past it.
        let mut w = AimdWindow::with_epoch(8.0, 64, Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        w.on_throttle(); // first decrease allowed → 4.0
        assert!((w.window_value() - 4.0).abs() < 1e-10);

        // Sleep past the epoch again.
        std::thread::sleep(Duration::from_millis(5));
        w.on_throttle(); // second decrease now allowed → 2.0
        assert!(
            (w.window_value() - 2.0).abs() < 1e-10,
            "after epoch expiry a new decrease should be allowed"
        );
    }

    // ---------------------------------------------------------------------------
    // Convergence
    // ---------------------------------------------------------------------------

    #[test]
    fn window_converges_from_1_to_50() {
        let cap = 50;
        let mut w = AimdWindow::with_epoch(1.0, cap, Duration::from_millis(1));
        // AIMD additive increase (w += 1/w) takes ~1249 steps to go from 1 to 50.
        // Use 1500 to have a safe margin.
        for _ in 0..1500 {
            w.on_success();
        }
        assert_eq!(
            w.limit(),
            cap,
            "window should grow to cap after enough successes"
        );
    }

    // ---------------------------------------------------------------------------
    // TokenBucket -- pacing under paused virtual time
    // ---------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn token_bucket_burst_completes_without_pacing() {
        // Burst capacity covers the first N acquires with zero wait.
        let tb = TokenBucket::new(80.0, 20.0);
        let start = tokio::time::Instant::now();
        for _ in 0..20 {
            tb.acquire().await;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(1),
            "burst of 20 should complete near-instantly, got {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn token_bucket_paces_after_burst() {
        // After draining the burst, the next acquire must wait >= 1/rate seconds.
        let tb = TokenBucket::new(80.0, 20.0);
        for _ in 0..20 {
            tb.acquire().await;
        }
        let before = tokio::time::Instant::now();
        tb.acquire().await;
        let waited = before.elapsed();
        let expected = std::time::Duration::from_secs_f64(1.0 / 80.0);
        // Lower bound: pacing must engage past the burst.
        // Upper bound: pacing must not over-sleep -- a "refill never advances"
        // regression (e.g., reverting `last_refill` to `std::time::Instant`
        // under paused virtual time) would surface here as the loop sleeps
        // forever, and a 4x ceiling catches a refill miscalculation that
        // would otherwise pass the lower bound vacuously.
        let ceiling = expected * 4;
        assert!(
            waited >= expected && waited <= ceiling,
            "expected wait in [{expected:?}, {ceiling:?}], got {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn token_bucket_refills_at_configured_rate() {
        // Drain the bucket, advance by exactly enough for one refill, then
        // verify the next acquire is immediate (no further sleep).
        let tb = TokenBucket::new(10.0, 1.0);
        tb.acquire().await; // consume the only token
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        let before = tokio::time::Instant::now();
        tb.acquire().await;
        assert!(
            before.elapsed() < std::time::Duration::from_millis(1),
            "token should be available after 100ms refill at 10/s, got {:?}",
            before.elapsed()
        );
    }

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn token_bucket_serves_concurrent_consumers_without_deadlock() {
        // Lock-correctness check. If TokenBucket::acquire holds the std::sync
        // Mutex across tokio::time::sleep, the second consumer's lock() call
        // blocks the only thread on a current_thread runtime, deadlocking the
        // runtime entirely (the awaiting task holding the lock can never be
        // polled to release it). cargo test's per-test timeout catches this
        // as a hang. We do NOT assert on elapsed wall-clock -- timing assertions
        // are flaky in CI; "all 20 acquires complete" is the property we want.
        use std::sync::Arc;
        let tb = Arc::new(TokenBucket::new(100.0, 10.0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let tb = Arc::clone(&tb);
            handles.push(tokio::spawn(async move {
                for _ in 0..5 {
                    tb.acquire().await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Reaching here means all 20 acquires completed; the lock is released
        // across the await inside acquire().
    }

    // - Window-key routing tests --

    /// Each registry meters actions by its own published quotas: ECR per
    /// action, ECR Public by its five groups, Docker Hub with HEADs and
    /// manifest reads apart, GAR and GHCR as one shared cap, ACR as reads
    /// and writes, and an unknown registry by five coarse groups. Breaks if
    /// an action lands in another window, so its throttling slows actions
    /// the registry meters apart, or escapes a shared cap.
    #[test]
    fn window_keys_follow_each_registry_quota() {
        use AcrGroup as Acr;
        use DockerHubGroup as Hub;
        use EcrPublicGroup as Public;
        use RegistryAction::*;
        use UnknownGroup as Other;
        use WindowKey as K;
        let hosts = [
            "123456789012.dkr.ecr.us-east-1.amazonaws.com",
            "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com",
            "public.ecr.aws",
            "registry-1.docker.io",
            "us-central1-docker.pkg.dev",
            "ghcr.io",
            "myreg.azurecr.io",
            "quay.io",
        ];
        #[rustfmt::skip]
        let rows: [(RegistryAction, [WindowKey; 8]); 9] = [
            (ManifestHead, [K::Ecr(ManifestHead), K::Ecr(ManifestHead), K::EcrPublic(Public::Read), K::DockerHub(Hub::Heads), K::GarShared, K::GhcrShared, K::Acr(Acr::Read), K::Unknown(Other::Heads)]),
            (BlobHead, [K::Ecr(BlobHead), K::Ecr(BlobHead), K::EcrPublic(Public::Read), K::DockerHub(Hub::Heads), K::GarShared, K::GhcrShared, K::Acr(Acr::Read), K::Unknown(Other::Heads)]),
            (ManifestRead, [K::Ecr(ManifestRead), K::Ecr(ManifestRead), K::EcrPublic(Public::Read), K::DockerHub(Hub::ManifestRead), K::GarShared, K::GhcrShared, K::Acr(Acr::Read), K::Unknown(Other::Reads)]),
            (BlobRead, [K::Ecr(BlobRead), K::Ecr(BlobRead), K::EcrPublic(Public::Read), K::DockerHub(Hub::Other), K::GarShared, K::GhcrShared, K::Acr(Acr::Read), K::Unknown(Other::Reads)]),
            (TagList, [K::Ecr(TagList), K::Ecr(TagList), K::EcrPublic(Public::Read), K::DockerHub(Hub::Other), K::GarShared, K::GhcrShared, K::Acr(Acr::Read), K::Unknown(Other::TagList)]),
            (ManifestWrite, [K::Ecr(ManifestWrite), K::Ecr(ManifestWrite), K::EcrPublic(Public::ManifestWrite), K::DockerHub(Hub::Other), K::GarShared, K::GhcrShared, K::Acr(Acr::Write), K::Unknown(Other::ManifestWrite)]),
            (BlobUploadInit, [K::Ecr(BlobUploadInit), K::Ecr(BlobUploadInit), K::EcrPublic(Public::BlobUploadInit), K::DockerHub(Hub::Other), K::GarShared, K::GhcrShared, K::Acr(Acr::Write), K::Unknown(Other::Uploads)]),
            (BlobUploadChunk, [K::Ecr(BlobUploadChunk), K::Ecr(BlobUploadChunk), K::EcrPublic(Public::BlobUploadChunk), K::DockerHub(Hub::Other), K::GarShared, K::GhcrShared, K::Acr(Acr::Write), K::Unknown(Other::Uploads)]),
            (BlobUploadComplete, [K::Ecr(BlobUploadComplete), K::Ecr(BlobUploadComplete), K::EcrPublic(Public::BlobUploadComplete), K::DockerHub(Hub::Other), K::GarShared, K::GhcrShared, K::Acr(Acr::Write), K::Unknown(Other::Uploads)]),
        ];
        for (action, keys) in rows {
            for (host, key) in hosts.into_iter().zip(keys) {
                assert_eq!(
                    window_key_for_provider(detect_provider_kind(host), action),
                    key,
                    "{host} {action:?}"
                );
            }
        }
    }

    // - bucket_config_for_window: structural invariants --

    #[test]
    fn bucket_table_has_all_required_ecr_upload_windows() {
        // ECR private upload paths must have a bucket; the bench failure that
        // motivated this PR was 5x429s on InitiateLayerUpload under cross-repo
        // aggregation. Removing any of these silently re-opens that hazard.
        use RegistryAction::*;
        for key in [
            WindowKey::Ecr(ManifestWrite),
            WindowKey::Ecr(BlobUploadInit),
            WindowKey::Ecr(BlobUploadComplete),
            WindowKey::Ecr(BlobUploadChunk),
        ] {
            assert!(
                bucket_config_for_window(key).is_some(),
                "ECR upload key {key:?} must have a bucket"
            );
        }
    }

    #[test]
    fn bucket_table_excludes_unmetered_windows() {
        // ECR HEAD is documented as unmetered; Docker Hub HEAD is unmetered;
        // Unknown* registries have no documented cap and must fall back to
        // AIMD-only.
        use RegistryAction::*;
        for key in [
            WindowKey::Ecr(ManifestHead),
            WindowKey::Ecr(BlobHead),
            WindowKey::DockerHub(DockerHubGroup::Heads),
            WindowKey::DockerHub(DockerHubGroup::ManifestRead),
            WindowKey::DockerHub(DockerHubGroup::Other),
            WindowKey::Unknown(UnknownGroup::Heads),
            WindowKey::Unknown(UnknownGroup::Reads),
            WindowKey::Unknown(UnknownGroup::Uploads),
            WindowKey::Unknown(UnknownGroup::ManifestWrite),
            WindowKey::Unknown(UnknownGroup::TagList),
        ] {
            assert!(
                bucket_config_for_window(key).is_none(),
                "key {key:?} must NOT have a bucket (no documented cap)"
            );
        }
    }

    #[test]
    fn ecr_chunk_rate_exceeds_init_rate() {
        // AWS documents InitiateLayerUpload at 100 TPS and UploadLayerPart at
        // 500 TPS. Chunk MUST be the more permissive of the two; reversing
        // this is a documented misconfiguration that would slow uploads.
        let init =
            bucket_config_for_window(WindowKey::Ecr(RegistryAction::BlobUploadInit)).unwrap();
        let chunk =
            bucket_config_for_window(WindowKey::Ecr(RegistryAction::BlobUploadChunk)).unwrap();
        assert!(
            chunk.rate_per_sec > init.rate_per_sec,
            "ECR chunk rate ({}) must exceed init rate ({})",
            chunk.rate_per_sec,
            init.rate_per_sec
        );
    }

    #[test]
    fn bucket_values_finite_and_bounded() {
        // Walk every configured key and assert: rate > 0, burst > 0, both
        // finite, and burst is bounded relative to rate. A bucket only
        // emits sustained traffic at `rate_per_sec`; the burst absorbs a
        // brief over-rate prefix before depletion. We therefore cannot
        // assert `burst <= rate` (low-rate windows like ECR ManifestWrite
        // deliberately set burst at ~1 documented-cap-second), but a
        // burst more than 10x the per-second rate is structurally unsafe
        // because it lets a cold path emit a multi-second over-cap spike
        // before the bucket reins it in.
        use EcrPublicGroup as Pub;
        use RegistryAction::*;
        let configured = [
            WindowKey::Ecr(ManifestWrite),
            WindowKey::Ecr(BlobUploadInit),
            WindowKey::Ecr(BlobUploadComplete),
            WindowKey::Ecr(BlobUploadChunk),
            WindowKey::EcrPublic(Pub::Read),
            WindowKey::EcrPublic(Pub::ManifestWrite),
            WindowKey::EcrPublic(Pub::BlobUploadInit),
            WindowKey::EcrPublic(Pub::BlobUploadComplete),
            WindowKey::EcrPublic(Pub::BlobUploadChunk),
            WindowKey::GhcrShared,
            WindowKey::GarShared,
            WindowKey::Acr(AcrGroup::Read),
            WindowKey::Acr(AcrGroup::Write),
        ];
        for key in configured {
            let cfg = bucket_config_for_window(key).unwrap_or_else(|| {
                panic!(
                    "key {key:?} listed as configured but bucket_config_for_window returned None"
                )
            });
            let BucketConfig {
                rate_per_sec,
                burst,
            } = cfg;
            assert!(
                rate_per_sec.is_finite() && rate_per_sec > 0.0,
                "key {key:?}: rate {rate_per_sec} not positive-finite"
            );
            assert!(
                burst.is_finite() && burst > 0.0,
                "key {key:?}: burst {burst} not positive-finite"
            );
            assert!(
                burst <= 10.0 * rate_per_sec,
                "key {key:?}: burst {burst} exceeds 10x rate {rate_per_sec}; permits multi-second over-cap spike"
            );
        }
    }

    // - AimdController bucket integration --

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn controller_paces_ecr_blob_upload_init() {
        // ECR_BLOB_UPLOAD_INIT is configured at 80/sec burst 20.
        // Acquire 100 permits in series; total elapsed should be at least
        // (100 - 20) / 80 = 1.0 sec because of bucket pacing past burst.
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        let ctrl = AimdController::new(host, 100);
        let start = tokio::time::Instant::now();
        for _ in 0..100 {
            let permit = ctrl.acquire(RegistryAction::BlobUploadInit).await;
            permit.success();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "100 acquires at 80/sec burst 20 should take >= 1s, got {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn controller_does_not_pace_unconfigured_window() {
        // ECR_MANIFEST_HEAD has no bucket -- acquires should be near-instant.
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        let ctrl = AimdController::new(host, 100);
        let start = tokio::time::Instant::now();
        for _ in 0..100 {
            let permit = ctrl.acquire(RegistryAction::ManifestHead).await;
            permit.success();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(10),
            "100 acquires on unconfigured window should be near-instant, got {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn aimd_halving_preserves_bucket_state() {
        // AIMD halving shrinks the per-action Semaphore. The bucket must NOT
        // be rebuilt -- it represents an independent rate-cap concern that
        // survives concurrency adjustment.
        // Regression: a future refactor that rebuilds bucket on halving would
        // restore burst tokens and re-enable over-cap traffic exactly when the
        // registry has signalled it cannot handle current load.
        let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
        let ctrl = AimdController::new(host, 100);

        // Drain the bucket: 20 burst + a small post-burst sample.
        for _ in 0..21 {
            let permit = ctrl.acquire(RegistryAction::BlobUploadInit).await;
            permit.success();
        }
        // Trigger a 429 halving.
        let permit = ctrl.acquire(RegistryAction::BlobUploadInit).await;
        permit.throttled();
        // Next acquire must still observe bucket pacing (>= 1/80s) -- bucket
        // was not reset by the halving.
        let before = tokio::time::Instant::now();
        let permit = ctrl.acquire(RegistryAction::BlobUploadInit).await;
        permit.success();
        let waited = before.elapsed();
        assert!(
            waited >= std::time::Duration::from_secs_f64(1.0 / 80.0),
            "bucket pacing must survive AIMD halving, got {waited:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiters_queued_before_a_halving_are_still_admitted() {
        // ECR manifest HEADs have no token bucket, so only the window gates.
        let ctrl = AimdController::new("123456789012.dkr.ecr.us-east-1.amazonaws.com", 100);
        let op = RegistryAction::ManifestHead;
        let admitted = Duration::from_secs(1);

        ctrl.acquire(op).await.success();
        assert_eq!(ctrl.window_limit(op), 2);
        let first = ctrl.acquire(op).await;
        let second = ctrl.acquire(op).await;
        let mut queued = [
            Box::pin(ctrl.acquire(op)),
            Box::pin(ctrl.acquire(op)),
            Box::pin(ctrl.acquire(op)),
        ];
        for waiter in &mut queued {
            assert!(tokio::time::timeout(Duration::ZERO, waiter).await.is_err());
        }
        let [third, fourth, mut fifth] = queued;

        // Halve, grow back, halve again. Each halving is reported by a request
        // admitted before it, and `fifth` stays queued across both.
        first.throttled();
        second.success();
        let third = tokio::time::timeout(admitted, third)
            .await
            .expect("third admitted after the window grew back");
        let fourth = tokio::time::timeout(admitted, fourth)
            .await
            .expect("fourth admitted by the permit second released");
        tokio::time::sleep(DEFAULT_EPOCH + Duration::from_millis(5)).await;
        third.throttled();
        assert_eq!(ctrl.window_limit(op), 1);
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut fifth)
                .await
                .is_err(),
            "fourth still fills a window of one"
        );

        fourth.success();
        tokio::time::timeout(admitted, fifth)
            .await
            .expect("a waiter queued before the halvings must not be stranded")
            .success();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_on_a_full_window_holds_no_aggregate_permit() {
        // Aggregate cap of 2; manifest and blob HEADs are separate ECR windows.
        let ctrl = AimdController::new("123456789012.dkr.ecr.us-east-1.amazonaws.com", 2);
        let holder = ctrl.acquire(RegistryAction::ManifestHead).await;
        let mut waiter = Box::pin(ctrl.acquire(RegistryAction::ManifestHead));
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut waiter)
                .await
                .is_err()
        );

        tokio::time::timeout(Duration::ZERO, ctrl.acquire(RegistryAction::BlobHead))
            .await
            .expect("another window must still get the second aggregate permit")
            .success();
        holder.success();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn halving_holds_new_requests_until_in_flight_drops_below_limit() {
        let ctrl = AimdController::new("123456789012.dkr.ecr.us-east-1.amazonaws.com", 100);
        let op = RegistryAction::ManifestHead;
        for _ in 0..4 {
            ctrl.acquire(op).await.success();
        }
        assert_eq!(ctrl.window_limit(op), 4);
        let in_flight = [
            ctrl.acquire(op).await,
            ctrl.acquire(op).await,
            ctrl.acquire(op).await,
            ctrl.acquire(op).await,
        ];

        let [first, second, _third, _fourth] = in_flight;
        first.throttled();
        assert_eq!(ctrl.window_limit(op), 2);
        let mut next = Box::pin(ctrl.acquire(op));
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut next)
                .await
                .is_err(),
            "three in flight against a limit of two must not admit a fourth"
        );

        // The success grows the window to 3 and releases its permit, leaving
        // two in flight.
        second.success();
        assert_eq!(ctrl.window_limit(op), 3);
        tokio::time::timeout(Duration::ZERO, next)
            .await
            .expect("admitted once in flight drops below the limit")
            .success();
    }
}
