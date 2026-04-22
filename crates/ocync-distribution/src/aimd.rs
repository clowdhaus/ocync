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

/// Default initial concurrency window size (conservative, per spec).
const DEFAULT_INITIAL_WINDOW: f64 = 5.0;

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
/// - **ECR**: every action has an independent limit - 9 distinct windows.
/// - **Docker Hub**: HEAD operations are unmetered and share a window; manifest
///   reads have a separate 100-pull/6h quota; other operations share.
/// - **GAR**: all actions share a single per-project quota.
/// - **Unknown**: coarse grouping - HEADs share, reads share, uploads share,
///   manifest writes and tag listing get their own windows.
pub fn window_key_for_registry(host: &str, action: RegistryAction) -> WindowKey {
    match detect_provider_kind(host) {
        Some(ProviderKind::Ecr) => {
            // ECR: independent TPS limit per action
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
        Some(ProviderKind::DockerHub) => {
            // Docker Hub: HEADs are free, manifest reads quota'd, rest shared
            let key = match action {
                RegistryAction::ManifestHead | RegistryAction::BlobHead => DOCKER_HUB_HEADS,
                RegistryAction::ManifestRead => DOCKER_HUB_MANIFEST_READ,
                _ => DOCKER_HUB_OTHER,
            };
            WindowKey(key)
        }
        Some(ProviderKind::Gar) => {
            // GAR: single shared project quota
            WindowKey(GAR_SHARED)
        }
        _ => {
            // All other registries (GHCR, ACR, Chainguard, ECR Public,
            // GCR, unknown): coarse grouping
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

/// State stored per AIMD window inside [`AimdController`].
struct WindowState {
    window: AimdWindow,
    /// Semaphore enforcing the current window limit.
    semaphore: Arc<Semaphore>,
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

        // Ensure the window entry exists, then read its semaphore.
        let action_semaphore = {
            let mut map = self.windows.lock().expect("aimd lock poisoned");
            let state = map.entry(key).or_insert_with(|| {
                let initial = DEFAULT_INITIAL_WINDOW.min(self.max_concurrent as f64);
                let window = AimdWindow::new(initial, self.max_concurrent);
                let limit = window.limit();
                WindowState {
                    window,
                    semaphore: Arc::new(Semaphore::new(limit)),
                }
            });
            Arc::clone(&state.semaphore)
        };

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
    fn window_converges_from_5_to_50() {
        let cap = 50;
        let mut w = AimdWindow::with_epoch(5.0, cap, Duration::from_millis(1));
        // AIMD additive increase (w += 1/w) takes ~1237 steps to go from 5 to 50.
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
}
