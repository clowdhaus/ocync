//! The `watch` subcommand -- daemon mode for periodic sync with graceful shutdown.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

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

                match synchronize::run(&sync_args, progress, Some(&shutdown)).await {
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

            health_handle.abort();
            exit_code
        })
        .await;

    Ok(exit_code)
}
