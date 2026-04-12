//! Graceful shutdown signal handling via OS signals.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative shutdown signal using an atomic flag.
///
/// Components check `is_triggered()` at safe points to exit gracefully.
#[derive(Clone)]
pub(crate) struct ShutdownSignal {
    triggered: Arc<AtomicBool>,
}

impl ShutdownSignal {
    /// Create a new shutdown signal in the untriggered state.
    pub(crate) fn new() -> Self {
        Self {
            triggered: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set the shutdown flag. All clones will observe the change.
    pub(crate) fn trigger(&self) {
        self.triggered.store(true, Ordering::Release);
    }

    /// Check whether shutdown has been requested.
    pub(crate) fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
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

/// Install OS signal handlers that trigger the given shutdown signal.
///
/// Listens for SIGINT (ctrl-c) and SIGTERM on Unix. If SIGTERM handler
/// registration fails, falls back to SIGINT only with a warning.
pub(crate) fn install_signal_handlers(shutdown: ShutdownSignal) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = ctrl_c => {
                            tracing::info!("received SIGINT, initiating graceful shutdown");
                        }
                        _ = sigterm.recv() => {
                            tracing::info!("received SIGTERM, initiating graceful shutdown");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "failed to register SIGTERM handler: {err}, falling back to SIGINT only"
                    );
                    let _ = ctrl_c.await;
                    tracing::info!("received SIGINT, initiating graceful shutdown");
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("received ctrl-c, initiating graceful shutdown");
        }

        shutdown.trigger();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_signal_default_not_triggered() {
        let signal = ShutdownSignal::new();
        assert!(!signal.is_triggered());
    }

    #[test]
    fn shutdown_signal_trigger_sets_flag() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        assert!(signal.is_triggered());
    }

    #[test]
    fn shutdown_signal_clone_shares_state() {
        let signal = ShutdownSignal::new();
        let cloned = signal.clone();
        signal.trigger();
        assert!(cloned.is_triggered());
    }

    #[test]
    fn shutdown_signal_double_trigger_idempotent() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        signal.trigger();
        assert!(signal.is_triggered());
    }

    #[test]
    fn shutdown_signal_default_trait() {
        let signal = ShutdownSignal::default();
        assert!(!signal.is_triggered());
    }

    #[test]
    fn shutdown_signal_debug_format() {
        let signal = ShutdownSignal::new();
        let debug = format!("{signal:?}");
        assert!(debug.contains("triggered: false"));
        signal.trigger();
        let debug = format!("{signal:?}");
        assert!(debug.contains("triggered: true"));
    }
}
