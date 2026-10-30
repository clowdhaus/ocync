//! Cooperative shutdown signal for the sync engine.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative shutdown signal using an atomic flag and async notification.
///
/// Components can poll [`is_triggered()`](Self::is_triggered) at safe points,
/// or `await` [`notified()`](Self::notified) for instant notification without
/// polling.
#[derive(Clone)]
pub struct ShutdownSignal {
    triggered: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl ShutdownSignal {
    /// Create a new shutdown signal in the untriggered state.
    pub fn new() -> Self {
        Self {
            triggered: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Set the shutdown flag and wake all waiters. All clones will observe the change.
    pub fn trigger(&self) {
        self.triggered.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Check whether shutdown has been requested.
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    /// Wait until shutdown is triggered. Returns immediately if already triggered.
    ///
    /// Registers the `Notify` listener before checking the flag so a
    /// `trigger()` between the check and the await is not missed.
    pub async fn notified(&self) {
        let future = self.notify.notified();
        if self.is_triggered() {
            return;
        }
        future.await;
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ShutdownSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownSignal")
            .field("triggered", &self.is_triggered())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written `Debug` shows the flag and hides the `Notify`.
    #[test]
    fn debug_shows_triggered_state() {
        let signal = ShutdownSignal::new();
        assert_eq!(format!("{signal:?}"), "ShutdownSignal { triggered: false }");
        signal.trigger();
        assert_eq!(format!("{signal:?}"), "ShutdownSignal { triggered: true }");
    }

    #[test]
    fn new_and_default_start_untriggered() {
        assert!(!ShutdownSignal::new().is_triggered(), "new");
        assert!(!ShutdownSignal::default().is_triggered(), "default");
    }

    #[test]
    fn clone_shares_state() {
        let signal = ShutdownSignal::new();
        let cloned = signal.clone();
        signal.trigger();
        assert!(cloned.is_triggered());
    }

    #[test]
    fn trigger_sets_flag_and_is_idempotent() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        assert!(signal.is_triggered(), "first trigger");
        signal.trigger();
        assert!(signal.is_triggered(), "second trigger");
    }

    #[tokio::test]
    async fn notified_returns_immediately_when_already_triggered() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        // Should return immediately, not hang.
        signal.notified().await;
        assert!(signal.is_triggered());
    }

    #[tokio::test]
    async fn notified_wakes_on_trigger() {
        let signal = ShutdownSignal::new();
        let signal_clone = signal.clone();

        let handle = tokio::spawn(async move {
            signal_clone.notified().await;
            true
        });

        // Small delay to let the spawned task start waiting.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        signal.trigger();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("timed out waiting for notified()")
            .expect("task panicked");
        assert!(result);
    }
}
