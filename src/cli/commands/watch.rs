//! The `watch` subcommand -- daemon mode for periodic sync with graceful shutdown.

use std::time::Duration;

use crate::cli::commands::synchronize;
use crate::cli::shutdown::ShutdownSignal;
use crate::cli::{CliError, ExitCode};
use crate::{SyncArgs, WatchArgs};

/// Run the watch command: loop forever, running sync then waiting for the
/// configured interval or a shutdown signal.
pub(crate) async fn run(args: &WatchArgs, shutdown: ShutdownSignal) -> Result<ExitCode, CliError> {
    let interval = Duration::from_secs(args.interval);
    tracing::info!(interval_secs = args.interval, "starting watch mode");

    loop {
        let sync_args = SyncArgs {
            config: args.config.clone(),
            dry_run: false,
            json: args.json,
        };

        match synchronize::run(&sync_args, Some(&shutdown)).await {
            Ok(code) => {
                tracing::info!(exit_code = ?code, "sync cycle complete");
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
                return Ok(ExitCode::Success);
            }
        }
    }
}
