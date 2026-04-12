//! The `watch` subcommand -- daemon mode for periodic sync with graceful shutdown.

use std::time::Duration;

use crate::cli::commands::synchronize;
use crate::cli::shutdown::ShutdownSignal;
use crate::cli::{CliError, ExitCode};
use crate::{OutputFormat, SyncArgs, WatchArgs};

/// Run the watch command: loop forever, running sync then waiting for the
/// configured interval or a shutdown signal.
pub(crate) async fn run(args: &WatchArgs, shutdown: ShutdownSignal) -> Result<ExitCode, CliError> {
    let interval = Duration::from_secs(args.interval);
    tracing::info!(interval_secs = args.interval, "starting watch mode");

    loop {
        let sync_args = SyncArgs {
            config: args.config.clone(),
            dry_run: false,
            output: None,
            output_format: Some(OutputFormat::Text),
        };

        match synchronize::run(&sync_args).await {
            Ok(code) => {
                tracing::info!(exit_code = ?code, "sync cycle complete");
            }
            Err(err) => {
                tracing::error!(error = %err, "sync cycle failed");
            }
        }

        if wait_or_shutdown(interval, &shutdown).await {
            tracing::info!("shutdown signal received, exiting watch mode");
            return Ok(ExitCode::Success);
        }
    }
}

/// Wait for the given duration, polling the shutdown signal every 500ms.
///
/// Returns `true` if shutdown was triggered before the duration elapsed,
/// `false` if the full duration elapsed without a shutdown signal.
async fn wait_or_shutdown(duration: Duration, shutdown: &ShutdownSignal) -> bool {
    let poll_interval = Duration::from_millis(500);
    let mut elapsed = Duration::ZERO;

    while elapsed < duration {
        if shutdown.is_triggered() {
            return true;
        }
        tokio::time::sleep(poll_interval).await;
        elapsed += poll_interval;
    }

    shutdown.is_triggered()
}
