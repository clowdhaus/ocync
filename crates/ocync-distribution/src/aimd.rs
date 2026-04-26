//! AIMD concurrency controller - adaptive rate limiting via additive increase, multiplicative decrease.
//!
//! Each registry action gets its own [`AimdWindow`], which grows the allowed concurrency
//! by one unit per successful response and halves on a 429 (Too Many Requests). A
//! congestion epoch prevents multiple halvings from the same burst of 429s.
//!
//! [`AimdController`] manages one set of windows per registry host. The aggregate
//! concurrency cap is enforced by a shared [`tokio::sync::Semaphore`]; the per-action
//! windows refine that limit further.

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
    /// Create a new window with the given initial value and cap.
    ///
    /// Uses the default congestion epoch of 100 ms.
    pub(crate) fn new(initial: f64, cap: usize) -> Self {
        Self::with_epoch(initial, cap, DEFAULT_EPOCH)
    }

    /// Create a new window with a custom congestion epoch.
    ///
    /// The epoch controls how long after a halving before another halving can
    /// occur. Shorter epochs are useful in tests.
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

    /// Record a successful response - additive increase.
    ///
    /// Increases the window by `1.0 / window`, which produces linear growth in
    /// the number of round trips needed to double the window (TCP-style AIMD).
    pub(crate) fn on_success(&mut self) {
        self.window += 1.0 / self.window;
    }

    /// The raw floating-point window value.
    ///
    /// Exposed for testing only - prefer [`limit`](Self::limit) in production code.
    #[cfg(test)]
    pub(crate) fn window_value(&self) -> f64 {
        self.window
    }

    /// Record a 429 throttle response - multiplicative decrease.
    ///
    /// Halves the window (minimum 1.0). If a halving already occurred within the
    /// current epoch, this call is ignored - a single burst of 429s only triggers
    /// one halving.
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
/// Used alongside [`AimdWindow`] to bound TPS where AIMD's concurrency
/// window cannot. AIMD discovers a healthy concurrency level via 429
/// feedback; the bucket enforces a hard rate ceiling derived from
/// documented registry quotas (see `bucket_config_for_window`).
///
/// Uses [`tokio::time::Instant`] (not [`std::time::Instant`]) for the
/// `last_refill` timestamp. Refill arithmetic and the internal
/// [`tokio::time::sleep`] then share a single clock source, which is
/// required for `#[tokio::test(start_paused = true)]` -- with mixed
/// clocks the bucket would livelock under paused virtual time because
/// the std clock never advances while `sleep` returns immediately.
///
/// The mutex is released before sleeping, so concurrent consumers do
/// not serialize on lock contention.
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
            // IMPORTANT: lock released before sleep so other tasks make
            // progress. A held lock here would serialize all consumers.
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

/// Opaque key that groups operations into a shared AIMD window.
///
/// Different registries have different rate-limit granularities. Use
/// [`window_key_for_registry`] to obtain the correct key for a given host and
/// operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowKey(u8);

// - ECR window keys: independent TPS limit per action (9 windows) --
const ECR_MANIFEST_HEAD: u8 = 0;
const ECR_MANIFEST_READ: u8 = 1;
const ECR_MANIFEST_WRITE: u8 = 2;
const ECR_BLOB_HEAD: u8 = 3;
const ECR_BLOB_READ: u8 = 4;
const ECR_BLOB_UPLOAD_INIT: u8 = 5;
const ECR_BLOB_UPLOAD_CHUNK: u8 = 6;
const ECR_BLOB_UPLOAD_COMPLETE: u8 = 7;
const ECR_TAG_LIST: u8 = 8;

// - Docker Hub window keys: HEADs free, manifest reads quota'd, rest shared --
const DOCKER_HUB_HEADS: u8 = 10;
const DOCKER_HUB_MANIFEST_READ: u8 = 11;
const DOCKER_HUB_OTHER: u8 = 12;

// - GAR window key: single shared project quota --
const GAR_SHARED: u8 = 20;

// - ECR Public window keys: per-action TPS (5 windows) --
const ECR_PUBLIC_PULL: u8 = 40; // manifest reads/heads, blob reads/heads, tag list
const ECR_PUBLIC_MANIFEST_WRITE: u8 = 41;
const ECR_PUBLIC_BLOB_UPLOAD_INIT: u8 = 42;
const ECR_PUBLIC_BLOB_UPLOAD_CHUNK: u8 = 43;
const ECR_PUBLIC_BLOB_UPLOAD_COMPLETE: u8 = 44;

// - GHCR window keys: per-namespace reads, per-token writes --
const GHCR_READ: u8 = 50;
const GHCR_WRITE: u8 = 51;

// - ACR window keys: aggregate ReadOps and WriteOps per registry --
const ACR_READ: u8 = 60;
const ACR_WRITE: u8 = 61;

// - Unknown registry window keys: coarse grouping --
const UNKNOWN_HEADS: u8 = 30;
const UNKNOWN_READS: u8 = 31;
const UNKNOWN_UPLOADS: u8 = 32;
const UNKNOWN_MANIFEST_WRITE: u8 = 33;
const UNKNOWN_TAG_LIST: u8 = 34;

/// Map a registry host and action to an AIMD window key.
///
/// The mapping reflects each registry's actual rate-limit granularity:
///
/// - **ECR**: every action has an independent limit -- 9 distinct windows.
/// - **ECR Public**: 5 windows (pull paths share, plus 4 write windows).
/// - **Docker Hub**: HEADs unmetered/shared; manifest reads quota'd; rest shared.
/// - **GAR / GCR**: single shared project quota.
/// - **GHCR**: per-namespace reads, per-token writes -- 2 windows.
/// - **ACR**: aggregate `ReadOps` and `WriteOps` per registry -- 2 windows.
/// - **Unknown** (Chainguard, Quay, generic): coarse grouping -- HEADs / reads
///   / uploads / manifest writes / tag listing.
pub fn window_key_for_registry(host: &str, action: RegistryAction) -> WindowKey {
    match detect_provider_kind(host) {
        Some(ProviderKind::Ecr) => {
            let key = match action {
                RegistryAction::ManifestHead => ECR_MANIFEST_HEAD,
                RegistryAction::ManifestRead => ECR_MANIFEST_READ,
                RegistryAction::ManifestWrite => ECR_MANIFEST_WRITE,
                RegistryAction::BlobHead => ECR_BLOB_HEAD,
                RegistryAction::BlobRead => ECR_BLOB_READ,
                RegistryAction::BlobUploadInit => ECR_BLOB_UPLOAD_INIT,
                RegistryAction::BlobUploadChunk => ECR_BLOB_UPLOAD_CHUNK,
                RegistryAction::BlobUploadComplete => ECR_BLOB_UPLOAD_COMPLETE,
                RegistryAction::TagList => ECR_TAG_LIST,
            };
            WindowKey(key)
        }
        Some(ProviderKind::EcrPublic) => {
            let key = match action {
                RegistryAction::ManifestHead
                | RegistryAction::ManifestRead
                | RegistryAction::BlobHead
                | RegistryAction::BlobRead
                | RegistryAction::TagList => ECR_PUBLIC_PULL,
                RegistryAction::ManifestWrite => ECR_PUBLIC_MANIFEST_WRITE,
                RegistryAction::BlobUploadInit => ECR_PUBLIC_BLOB_UPLOAD_INIT,
                RegistryAction::BlobUploadChunk => ECR_PUBLIC_BLOB_UPLOAD_CHUNK,
                RegistryAction::BlobUploadComplete => ECR_PUBLIC_BLOB_UPLOAD_COMPLETE,
            };
            WindowKey(key)
        }
        Some(ProviderKind::DockerHub) => {
            let key = match action {
                RegistryAction::ManifestHead | RegistryAction::BlobHead => DOCKER_HUB_HEADS,
                RegistryAction::ManifestRead => DOCKER_HUB_MANIFEST_READ,
                _ => DOCKER_HUB_OTHER,
            };
            WindowKey(key)
        }
        Some(ProviderKind::Gar) | Some(ProviderKind::Gcr) => {
            // Single shared project quota; GCR is now an alias for GAR.
            WindowKey(GAR_SHARED)
        }
        Some(ProviderKind::Ghcr) => {
            let key = match action {
                RegistryAction::ManifestHead
                | RegistryAction::ManifestRead
                | RegistryAction::BlobHead
                | RegistryAction::BlobRead
                | RegistryAction::TagList => GHCR_READ,
                RegistryAction::ManifestWrite
                | RegistryAction::BlobUploadInit
                | RegistryAction::BlobUploadChunk
                | RegistryAction::BlobUploadComplete => GHCR_WRITE,
            };
            WindowKey(key)
        }
        Some(ProviderKind::Acr) => {
            let key = match action {
                RegistryAction::ManifestHead
                | RegistryAction::ManifestRead
                | RegistryAction::BlobHead
                | RegistryAction::BlobRead
                | RegistryAction::TagList => ACR_READ,
                RegistryAction::ManifestWrite
                | RegistryAction::BlobUploadInit
                | RegistryAction::BlobUploadChunk
                | RegistryAction::BlobUploadComplete => ACR_WRITE,
            };
            WindowKey(key)
        }
        _ => {
            // Chainguard, Quay, generic -- coarse grouping.
            let key = match action {
                RegistryAction::ManifestHead | RegistryAction::BlobHead => UNKNOWN_HEADS,
                RegistryAction::ManifestRead | RegistryAction::BlobRead => UNKNOWN_READS,
                RegistryAction::BlobUploadInit
                | RegistryAction::BlobUploadChunk
                | RegistryAction::BlobUploadComplete => UNKNOWN_UPLOADS,
                RegistryAction::ManifestWrite => UNKNOWN_MANIFEST_WRITE,
                RegistryAction::TagList => UNKNOWN_TAG_LIST,
            };
            WindowKey(key)
        }
    }
}

/// Token-bucket configuration `(rate_per_sec, burst)` for the given window
/// key, or `None` if rate gating is not configured for this window (AIMD
/// alone governs concurrency).
///
/// Returned as a tuple rather than a constructed [`TokenBucket`] so this
/// function is a pure lookup -- testable without instantiating tokio
/// types and reusable from non-async contexts. Callers in async code wrap
/// the result via `.map(|(r, b)| Arc::new(TokenBucket::new(r, b)))`.
///
/// Values are 20% under documented caps to absorb tail bursts without
/// artificial throttling at steady state. Per-cap source citations live
/// in `docs/superpowers/specs/2026-04-26-aimd-rate-bucket-design.md`;
/// that spec is the authoritative ledger of which caps come from which
/// official documentation versus community measurement. Verify the
/// citations against the live registry docs as part of the bench gate.
fn bucket_config_for_window(key: WindowKey) -> Option<(f64, f64)> {
    let cfg = match key.0 {
        // ECR private (per-account, per-region; AWS docs)
        ECR_MANIFEST_WRITE => (8.0, 10.0),
        ECR_BLOB_UPLOAD_INIT => (80.0, 20.0),
        ECR_BLOB_UPLOAD_COMPLETE => (80.0, 20.0),
        ECR_BLOB_UPLOAD_CHUNK => (400.0, 50.0),
        // ECR Public (per-account, per-region; AWS docs)
        ECR_PUBLIC_PULL => (8.0, 10.0),
        ECR_PUBLIC_MANIFEST_WRITE => (8.0, 10.0),
        ECR_PUBLIC_BLOB_UPLOAD_INIT => (8.0, 10.0),
        ECR_PUBLIC_BLOB_UPLOAD_COMPLETE => (8.0, 10.0),
        ECR_PUBLIC_BLOB_UPLOAD_CHUNK => (200.0, 30.0),
        // GHCR (per-namespace reads, per-token writes; community-measured)
        GHCR_READ => (580.0, 50.0),
        GHCR_WRITE => (26.0, 10.0),
        // GAR/GCR (per-project, per-region; Google docs)
        GAR_SHARED => (240.0, 30.0),
        // ACR (per-registry, Premium SKU defaults; Microsoft historical docs)
        ACR_READ => (130.0, 30.0),
        ACR_WRITE => (26.0, 10.0),
        // Everything else: AIMD only.
        _ => return None,
    };
    Some(cfg)
}

/// State stored per AIMD window inside [`AimdController`].
struct WindowState {
    window: AimdWindow,
    /// Semaphore enforcing the current window limit.
    semaphore: Arc<Semaphore>,
    /// Optional token bucket for hard TPS ceilings (None = AIMD only).
    bucket: Option<Arc<TokenBucket>>,
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
/// Maintains an aggregate concurrency cap (shared across all operations) and
/// per-action [`AimdWindow`]s that adapt to 429 feedback. The aggregate cap
/// prevents thundering-herd on a single host; the per-action windows discover
/// the fine-grained limits for each API endpoint.
///
/// Uses [`Mutex`] for interior mutability so the controller is `Sync` and can
/// live inside `Arc<RegistryClient>`. The lock is never held across await
/// points, so contention is impossible on `current_thread` and negligible on
/// multi-thread runtimes.
#[derive(Debug)]
pub struct AimdController {
    host: String,
    /// Hard cap on total simultaneous in-flight requests to this registry.
    aggregate: Arc<Semaphore>,
    /// Per-action windows, keyed by [`WindowKey`].
    windows: Mutex<HashMap<WindowKey, WindowState>>,
    max_concurrent: usize,
}

impl AimdController {
    /// Create a new controller for the given registry host.
    ///
    /// `max_concurrent` is the hard aggregate limit. Per-action windows start
    /// at the same initial value and grow/shrink independently.
    pub fn new(host: &str, max_concurrent: usize) -> Self {
        Self {
            host: host.to_owned(),
            aggregate: Arc::new(Semaphore::new(max_concurrent)),
            windows: Mutex::new(HashMap::new()),
            max_concurrent,
        }
    }

    /// Acquire concurrency permits for the given operation.
    ///
    /// Blocks until both the aggregate semaphore and the per-action window
    /// semaphore have capacity. Returns an [`AimdPermit`] that must be
    /// resolved via [`AimdPermit::success`] or [`AimdPermit::throttled`].
    pub async fn acquire(&self, op: RegistryAction) -> AimdPermit<'_> {
        let key = window_key_for_registry(&self.host, op);

        // Ensure the window entry exists, then capture the semaphore + bucket.
        let (action_semaphore, bucket) = {
            let mut map = self.windows.lock().expect("aimd lock poisoned");
            let state = map.entry(key).or_insert_with(|| {
                let initial = DEFAULT_INITIAL_WINDOW.min(self.max_concurrent as f64);
                let window = AimdWindow::new(initial, self.max_concurrent);
                let limit = window.limit();
                let bucket = bucket_config_for_window(key)
                    .map(|(rate, burst)| Arc::new(TokenBucket::new(rate, burst)));
                WindowState {
                    window,
                    semaphore: Arc::new(Semaphore::new(limit)),
                    bucket,
                }
            });
            (Arc::clone(&state.semaphore), state.bucket.clone())
        };

        // Bucket gate: acquire BEFORE concurrency permits so a paced action
        // does not occupy a slot that could service another window. Released
        // to other tasks during sleep (lock is dropped before await inside
        // TokenBucket::acquire).
        if let Some(b) = &bucket {
            b.acquire().await;
        }

        // Acquire aggregate permit first, then per-action.
        let aggregate_permit = Arc::clone(&self.aggregate)
            .acquire_owned()
            .await
            .expect("aggregate semaphore closed");
        let action_permit = action_semaphore
            .acquire_owned()
            .await
            .expect("action semaphore closed");

        AimdPermit {
            key,
            windows: &self.windows,
            aggregate_permit,
            action_permit: Some(action_permit),
            reported: false,
            host: self.host.clone(),
            action: op,
        }
    }

    /// Current window limit for the given operation type.
    ///
    /// Returns the limit stored in the window, or the default initial value if
    /// no window has been allocated for this operation yet.
    pub fn window_limit(&self, op: RegistryAction) -> usize {
        let key = window_key_for_registry(&self.host, op);
        let map = self.windows.lock().expect("aimd lock poisoned");
        map.get(&key).map(|s| s.window.limit()).unwrap_or_else(|| {
            let initial = DEFAULT_INITIAL_WINDOW.min(self.max_concurrent as f64);
            AimdWindow::new(initial, self.max_concurrent).limit()
        })
    }
}

/// RAII permit returned by [`AimdController::acquire`].
///
/// Must be resolved by calling [`success`](AimdPermit::success) or
/// [`throttled`](AimdPermit::throttled) before the permit is dropped. If
/// dropped without a report, the outcome is treated as a success - non-rate-limit
/// errors (network timeouts, auth failures) should not penalise the window.
pub struct AimdPermit<'a> {
    key: WindowKey,
    windows: &'a Mutex<HashMap<WindowKey, WindowState>>,
    /// Held for RAII: returned to the aggregate semaphore on drop.
    #[allow(dead_code)]
    aggregate_permit: OwnedSemaphorePermit,
    /// `Option` so `throttled()` can forget the permit to shrink the semaphore.
    action_permit: Option<OwnedSemaphorePermit>,
    reported: bool,
    /// Registry hostname for diagnostic logging in [`throttled`](Self::throttled).
    host: String,
    /// The operation type for diagnostic logging in [`throttled`](Self::throttled).
    action: RegistryAction,
}

impl std::fmt::Debug for AimdPermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AimdPermit")
            .field("key", &self.key)
            .field("reported", &self.reported)
            .finish_non_exhaustive()
    }
}

impl<'a> AimdPermit<'a> {
    /// Report a successful response and release both permits.
    pub fn success(mut self) {
        self.reported = true;
        if let Some(state) = self
            .windows
            .lock()
            .expect("aimd lock poisoned")
            .get_mut(&self.key)
        {
            let old_limit = state.window.limit();
            state.window.on_success();
            let new_limit = state.window.limit();
            // Grow the semaphore if the window expanded.
            if new_limit > old_limit {
                state.semaphore.add_permits(new_limit - old_limit);
            }
        }
    }

    /// Report a 429 throttle response and release both permits.
    ///
    /// Triggers a multiplicative decrease in the per-action window (subject to
    /// the congestion epoch). When the window shrinks, the per-action semaphore
    /// is replaced with a new one sized to the new limit. Outstanding permits
    /// from the old semaphore complete naturally - they hold their own `Arc`
    /// reference and won't interfere with the new semaphore. This mirrors TCP
    /// congestion control: the window shrinks immediately but packets already
    /// in flight are not recalled.
    pub fn throttled(mut self) {
        self.reported = true;
        if let Some(state) = self
            .windows
            .lock()
            .expect("aimd lock poisoned")
            .get_mut(&self.key)
        {
            let old_limit = state.window.limit();
            state.window.on_throttle();
            let new_limit = state.window.limit();
            if new_limit < old_limit {
                tracing::warn!(
                    registry = %self.host,
                    action = ?self.action,
                    old_window = old_limit,
                    new_window = new_limit,
                    "AIMD halved on 429"
                );
                // Replace the semaphore so new acquires are bounded by the
                // reduced limit. Forget our permit so it doesn't return to
                // the old (now-orphaned) semaphore.
                state.semaphore = Arc::new(Semaphore::new(new_limit));
                if let Some(permit) = self.action_permit.take() {
                    permit.forget();
                }
            }
        }
    }
}

impl Drop for AimdPermit<'_> {
    fn drop(&mut self) {
        // Treat unreported drops as success - errors unrelated to rate limits
        // (timeouts, auth failures, etc.) should not shrink the window.
        if !self.reported {
            if let Some(state) = self
                .windows
                .lock()
                .expect("aimd lock poisoned")
                .get_mut(&self.key)
            {
                let old_limit = state.window.limit();
                state.window.on_success();
                let new_limit = state.window.limit();
                // Grow the semaphore if the window expanded.
                if new_limit > old_limit {
                    state.semaphore.add_permits(new_limit - old_limit);
                }
            }
        }
        // aggregate_permit and action_permit are dropped automatically
        // (Rust drops fields in declaration order).
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
        assert!(
            waited >= expected,
            "expected wait >= {expected:?}, got {waited:?}"
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

    #[test]
    fn ecr_public_window_keys_route_per_action() {
        use RegistryAction::*;
        let host = "public.ecr.aws";
        let cases: &[(RegistryAction, u8)] = &[
            (ManifestHead, ECR_PUBLIC_PULL),
            (ManifestRead, ECR_PUBLIC_PULL),
            (BlobHead, ECR_PUBLIC_PULL),
            (BlobRead, ECR_PUBLIC_PULL),
            (TagList, ECR_PUBLIC_PULL),
            (ManifestWrite, ECR_PUBLIC_MANIFEST_WRITE),
            (BlobUploadInit, ECR_PUBLIC_BLOB_UPLOAD_INIT),
            (BlobUploadChunk, ECR_PUBLIC_BLOB_UPLOAD_CHUNK),
            (BlobUploadComplete, ECR_PUBLIC_BLOB_UPLOAD_COMPLETE),
        ];
        for &(action, expected) in cases {
            let got = window_key_for_registry(host, action).0;
            assert_eq!(got, expected, "ECR Public {action:?} -> key");
        }
    }

    #[test]
    fn ghcr_window_keys_route_read_vs_write() {
        use RegistryAction::*;
        let host = "ghcr.io";
        let cases: &[(RegistryAction, u8)] = &[
            (ManifestHead, GHCR_READ),
            (ManifestRead, GHCR_READ),
            (BlobHead, GHCR_READ),
            (BlobRead, GHCR_READ),
            (TagList, GHCR_READ),
            (ManifestWrite, GHCR_WRITE),
            (BlobUploadInit, GHCR_WRITE),
            (BlobUploadChunk, GHCR_WRITE),
            (BlobUploadComplete, GHCR_WRITE),
        ];
        for &(action, expected) in cases {
            let got = window_key_for_registry(host, action).0;
            assert_eq!(got, expected, "GHCR {action:?} -> key");
        }
    }

    #[test]
    fn acr_window_keys_route_read_vs_write() {
        use RegistryAction::*;
        let host = "myreg.azurecr.io";
        let cases: &[(RegistryAction, u8)] = &[
            (ManifestHead, ACR_READ),
            (ManifestRead, ACR_READ),
            (BlobHead, ACR_READ),
            (BlobRead, ACR_READ),
            (TagList, ACR_READ),
            (ManifestWrite, ACR_WRITE),
            (BlobUploadInit, ACR_WRITE),
            (BlobUploadChunk, ACR_WRITE),
            (BlobUploadComplete, ACR_WRITE),
        ];
        for &(action, expected) in cases {
            let got = window_key_for_registry(host, action).0;
            assert_eq!(got, expected, "ACR {action:?} -> key");
        }
    }

    // - bucket_config_for_window: structural invariants --

    #[test]
    fn bucket_table_has_all_required_ecr_upload_windows() {
        // ECR private upload paths must have a bucket; the bench failure that
        // motivated this PR was 5x429s on InitiateLayerUpload under cross-repo
        // aggregation. Removing any of these silently re-opens that hazard.
        for key in [
            ECR_MANIFEST_WRITE,
            ECR_BLOB_UPLOAD_INIT,
            ECR_BLOB_UPLOAD_COMPLETE,
            ECR_BLOB_UPLOAD_CHUNK,
        ] {
            assert!(
                bucket_config_for_window(WindowKey(key)).is_some(),
                "ECR upload key {key} must have a bucket"
            );
        }
    }

    #[test]
    fn bucket_table_excludes_unmetered_windows() {
        // ECR HEAD is documented as unmetered; Docker Hub HEAD is unmetered;
        // UNKNOWN_* registries have no documented cap and must fall back to
        // AIMD-only.
        for key in [
            ECR_MANIFEST_HEAD,
            ECR_BLOB_HEAD,
            DOCKER_HUB_HEADS,
            DOCKER_HUB_MANIFEST_READ,
            DOCKER_HUB_OTHER,
            UNKNOWN_HEADS,
            UNKNOWN_READS,
            UNKNOWN_UPLOADS,
            UNKNOWN_MANIFEST_WRITE,
            UNKNOWN_TAG_LIST,
        ] {
            assert!(
                bucket_config_for_window(WindowKey(key)).is_none(),
                "key {key} must NOT have a bucket (no documented cap)"
            );
        }
    }

    #[test]
    fn ecr_chunk_rate_exceeds_init_rate() {
        // AWS documents InitiateLayerUpload at 100 TPS and UploadLayerPart at
        // 500 TPS. Chunk MUST be the more permissive of the two; reversing
        // this is a documented misconfiguration that would slow uploads.
        let init = bucket_config_for_window(WindowKey(ECR_BLOB_UPLOAD_INIT)).unwrap();
        let chunk = bucket_config_for_window(WindowKey(ECR_BLOB_UPLOAD_CHUNK)).unwrap();
        assert!(
            chunk.0 > init.0,
            "ECR chunk rate ({}) must exceed init rate ({})",
            chunk.0,
            init.0
        );
    }

    #[test]
    fn bucket_values_are_sane() {
        // Walk every configured key and assert: rate > 0, burst > 0, both
        // finite, and burst is bounded relative to rate. A bucket only
        // emits sustained traffic at `rate_per_sec`; the burst absorbs a
        // brief over-rate prefix before depletion. We therefore cannot
        // assert `burst <= rate` (low-rate windows like ECR ManifestWrite
        // deliberately set burst at ~1 documented-cap-second), but a
        // burst more than 10x the per-second rate is structurally unsafe
        // because it lets a cold path emit a multi-second over-cap spike
        // before the bucket reins it in.
        let configured: &[u8] = &[
            ECR_MANIFEST_WRITE,
            ECR_BLOB_UPLOAD_INIT,
            ECR_BLOB_UPLOAD_COMPLETE,
            ECR_BLOB_UPLOAD_CHUNK,
            ECR_PUBLIC_PULL,
            ECR_PUBLIC_MANIFEST_WRITE,
            ECR_PUBLIC_BLOB_UPLOAD_INIT,
            ECR_PUBLIC_BLOB_UPLOAD_COMPLETE,
            ECR_PUBLIC_BLOB_UPLOAD_CHUNK,
            GHCR_READ,
            GHCR_WRITE,
            GAR_SHARED,
            ACR_READ,
            ACR_WRITE,
        ];
        for &k in configured {
            let (rate, burst) = bucket_config_for_window(WindowKey(k)).unwrap_or_else(|| {
                panic!("key {k} listed as configured but bucket_config_for_window returned None")
            });
            assert!(
                rate.is_finite() && rate > 0.0,
                "key {k}: rate {rate} not positive-finite"
            );
            assert!(
                burst.is_finite() && burst > 0.0,
                "key {k}: burst {burst} not positive-finite"
            );
            assert!(
                burst <= 10.0 * rate,
                "key {k}: burst {burst} exceeds 10x rate {rate}; permits multi-second over-cap spike"
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
        // AIMD halving rebuilds the per-action Semaphore (the action_permit is
        // forgotten). The bucket must NOT be rebuilt -- it represents an
        // independent rate-cap concern that survives concurrency adjustment.
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
}
