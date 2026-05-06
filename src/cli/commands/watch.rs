//! The `watch` subcommand - daemon mode for periodic sync with graceful shutdown.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use ocync_sync::cache::TransferStateCache;
use tokio::net::TcpListener;

use crate::cli::commands::synchronize::{self, WatchLogState};
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
    verbose: bool,
) -> Result<ExitCode, CliError> {
    let interval = Duration::from_secs(args.interval);
    tracing::info!(interval_secs = args.interval, "starting watch mode");

    let health_state = Rc::new(RefCell::new(HealthState::new(interval)));

    // Resolve cache path and TTL from the initial config load. These are
    // re-resolved each cycle so that config file changes take effect.
    let config = load_config(&args.config)?;
    let (_cache_dir, cache_path) = synchronize::resolve_cache_path(&config, &args.config);
    let cache_ttl = synchronize::resolve_cache_ttl(&config)?;

    // Bind the health listener early so bind errors surface before the
    // sync loop starts. This also avoids TOCTOU races with port allocation.
    let health_listener = match TcpListener::bind((args.health_bind, args.health_port)).await {
        Ok(l) => {
            tracing::info!(port = args.health_port, "health server listening");
            l
        }
        Err(err) => {
            return Err(CliError::Input(format!(
                "health server failed to bind on port {}: {err} (check if another process is using this port)",
                args.health_port,
            )));
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
            // Track the active cache path so we can detect config changes.
            let mut active_cache_path = cache_path;

            // Spawn the health server as a background local task.
            let health_handle = {
                let state = Rc::clone(&health_state);
                tokio::task::spawn_local(async move {
                    crate::cli::health::serve(health_listener, state).await;
                })
            };

            // Watch-mode log state: tracks which mappings have already
            // emitted a no-tags-matched WARN so we emit one per transition,
            // not one per cycle. Pruned each cycle to mappings still in config.
            let mut watch_log = WatchLogState::default();

            // Track consecutive config reload failures for backoff.
            let mut config_failures: u32 = 0;
            const BACKOFF_THRESHOLD: u32 = 3;
            const MAX_BACKOFF_SECS: u64 = 300; // 5 minutes

            let exit_code = loop {
                // Re-resolve cache_dir and cache_ttl each cycle so that
                // config file edits (e.g. changing cache_dir or cache_ttl)
                // take effect without restarting watch mode.
                if let Ok(cfg) = load_config(&args.config) {
                    config_failures = 0;
                    let (_dir, path) = synchronize::resolve_cache_path(&cfg, &args.config);
                    match synchronize::resolve_cache_ttl(&cfg) {
                        Ok(ttl) => {
                            // If the cache path changed, persist the old
                            // cache and reload from the new location.
                            if path != active_cache_path {
                                if let Err(e) = cache.borrow().persist(&active_cache_path) {
                                    tracing::warn!(
                                        error = %e,
                                        "failed to persist cache before path change"
                                    );
                                }
                                *cache.borrow_mut() = TransferStateCache::load(&path, ttl);
                                active_cache_path = path;
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                "invalid cache_ttl in reloaded config, using previous value"
                            );
                        }
                    }
                } else {
                    config_failures += 1;
                    tracing::error!(
                        consecutive_failures = config_failures,
                        "failed to reload config for cache resolution, using previous value"
                    );

                    // After repeated failures, apply exponential backoff
                    // before the next cycle to avoid log spam.
                    if config_failures >= BACKOFF_THRESHOLD {
                        let backoff_secs = interval
                            .as_secs()
                            .saturating_mul(1 << (config_failures - BACKOFF_THRESHOLD).min(4))
                            .min(MAX_BACKOFF_SECS);
                        let backoff = Duration::from_secs(backoff_secs);
                        tracing::warn!(
                            backoff_secs = backoff.as_secs(),
                            "config reload failing repeatedly, applying backoff"
                        );
                        tokio::select! {
                            () = tokio::time::sleep(backoff) => {}
                            () = shutdown.notified() => {
                                tracing::info!("shutdown signal received during backoff");
                                break ExitCode::Success;
                            }
                        }
                        continue;
                    }
                }

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
                    verbose,
                    Some(&mut watch_log),
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
            if let Err(e) = cache.borrow().persist(&active_cache_path) {
                tracing::warn!(error = %e, "failed to persist cache on shutdown");
            }

            health_handle.abort();
            exit_code
        })
        .await;

    Ok(exit_code)
}
