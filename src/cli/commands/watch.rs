//! The `watch` subcommand -- daemon mode for periodic sync with graceful shutdown.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use ocync_sync::cache::TransferStateCache;

use crate::cli::commands::synchronize;
use crate::cli::health::HealthState;
use crate::cli::shutdown::ShutdownSignal;
use crate::cli::{CliError, ExitCode};
use crate::{SyncArgs, WatchArgs};

/// Run the watch command: loop forever, running sync then waiting for the
/// configured interval or a shutdown signal.
pub(crate) async fn run(
    args: &WatchArgs,
    progress: &dyn ocync_sync::progress::ProgressReporter,
    shutdown: ShutdownSignal,
) -> Result<ExitCode, CliError> {
    let interval = Duration::from_secs(args.interval);
    tracing::info!(interval_secs = args.interval, "starting watch mode");

    let health_state = Rc::new(RefCell::new(HealthState::new(interval)));

    // A LocalSet is required for spawn_local (health server uses
    // Rc<RefCell<>> which is not Send).
    let local = tokio::task::LocalSet::new();
    let exit_code = local
        .run_until(async {
            // Load cache from disk once (warm start from prior session).
            let cache_dir = args
                .config
                .parent()
                .unwrap_or(Path::new("."))
                .join(".ocync/cache");
            let cache_path = cache_dir.join("transfer_state.bin");
            let cache = Rc::new(RefCell::new(TransferStateCache::load(
                &cache_path,
                Duration::from_secs(12 * 3600),
            )));

            // Spawn the health server as a background local task.
            let health_handle = {
                let state = Rc::clone(&health_state);
                let port = args.health_port;
                tokio::task::spawn_local(async move {
                    if let Err(err) = crate::cli::health::serve(port, state).await {
                        tracing::error!(error = %err, "health server failed");
                    }
                })
            };

            let exit_code = loop {
                let sync_args = SyncArgs {
                    config: args.config.clone(),
                    dry_run: false,
                    json: args.json,
                };

                match synchronize::run(
                    &sync_args,
                    progress,
                    Some(&shutdown),
                    Some(Rc::clone(&cache)),
                )
                .await
                {
                    Ok(code) => {
                        tracing::info!(exit_code = ?code, "sync cycle complete");
                        if matches!(code, ExitCode::Success | ExitCode::PartialFailure) {
                            health_state.borrow_mut().record_success();
                        }
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "sync cycle failed");
                    }
                }

                // Wait for the interval to elapse, or return early on shutdown.
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = shutdown.notified() => {
                        tracing::info!("shutdown signal received, exiting watch mode");
                        break ExitCode::Success;
                    }
                }
            };

            // Persist cache on graceful shutdown.
            if let Err(e) = cache.borrow().persist(&cache_path) {
                tracing::warn!(error = %e, "failed to persist cache on shutdown");
            }

            health_handle.abort();
            exit_code
        })
        .await;

    Ok(exit_code)
}
