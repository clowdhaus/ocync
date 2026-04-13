//! Graceful shutdown signal handling via OS signals.
//!
//! Re-exports [`ShutdownSignal`] from `ocync_sync` and provides
//! the OS-level signal handler installation.

pub(crate) use ocync_sync::shutdown::ShutdownSignal;

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
