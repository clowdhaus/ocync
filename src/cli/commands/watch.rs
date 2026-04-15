//! The `watch` subcommand -- daemon mode for periodic sync with graceful shutdown.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use ocync_sync::cache::TransferStateCache;
use tokio::net::TcpListener;

use crate::cli::commands::synchronize;
use crate::cli::config::load_config;
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

    // Load config once upfront for cache path and TTL resolution.
    // Uses the same resolution logic as the sync command.
    let config = load_config(&args.config)?;
    let (_cache_dir, cache_path) = synchronize::resolve_cache_path(&config, &args.config);
    let cache_ttl = synchronize::resolve_cache_ttl(&config);

    // Bind the health listener early so bind errors surface before the
    // sync loop starts. This also avoids TOCTOU races with port allocation.
    let health_listener = match TcpListener::bind(("0.0.0.0", args.health_port)).await {
        Ok(l) => {
            tracing::info!(port = args.health_port, "health server listening");
            Some(l)
        }
        Err(err) => {
            tracing::error!(
                port = args.health_port,
                error = %err,
                "health server failed to bind, watch mode continuing without health endpoint"
            );
            None
        }
    };

    // A LocalSet is required for spawn_local (health server uses
    // Rc<RefCell<>> which is not Send).
    let local = tokio::task::LocalSet::new();
    let exit_code = local
        .run_until(async {
            // Load cache from disk once (warm start from prior session).
            let cache = Rc::new(RefCell::new(TransferStateCache::load(
                &cache_path,
                cache_ttl,
            )));

            // Spawn the health server as a background local task.
            let health_handle = health_listener.map(|listener| {
                let state = Rc::clone(&health_state);
                tokio::task::spawn_local(async move {
                    crate::cli::health::serve(listener, state).await;
                })
            });

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

            if let Some(h) = health_handle {
                h.abort();
            }
            exit_code
        })
        .await;

    Ok(exit_code)
}
