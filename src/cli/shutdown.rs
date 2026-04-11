use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative shutdown signal using an atomic flag.
///
/// Components check `is_triggered()` at safe points to exit gracefully.
#[derive(Clone)]
pub struct ShutdownSignal {
    triggered: Arc<AtomicBool>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            triggered: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn trigger(&self) {
        self.triggered.store(true, Ordering::SeqCst);
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Install OS signal handlers that trigger the given shutdown signal.
///
/// Listens for SIGINT (ctrl-c) and SIGTERM on Unix.
pub fn install_signal_handlers(shutdown: ShutdownSignal) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

            tokio::select! {
                _ = ctrl_c => {
                    tracing::info!("received SIGINT, initiating graceful shutdown");
                }
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM, initiating graceful shutdown");
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
}
