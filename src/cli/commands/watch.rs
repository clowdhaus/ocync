//! The `watch` subcommand: daemon mode for periodic sync with graceful shutdown.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ocync_sync::cache::TransferStateCache;
use tokio::net::TcpListener;

use crate::cli::commands::synchronize::{self, WatchLogState};
use crate::cli::config::load_config;
use crate::cli::health::HealthState;
use crate::cli::shutdown::ShutdownSignal;
use crate::cli::{CliError, ExitCode};
use crate::{SyncArgs, WatchArgs};

/// Loop forever: sync, then wait for the interval or a shutdown signal.
pub(crate) async fn run(
    args: &WatchArgs,
    progress: &dyn ocync_sync::progress::ProgressReporter,
    shutdown: ShutdownSignal,
    verbose: bool,
) -> Result<ExitCode, CliError> {
    let interval = Duration::from_secs(args.interval);
    tracing::info!(interval_secs = args.interval, "starting watch mode");

    let health_state = Rc::new(RefCell::new(HealthState::new(interval)));

    // Re-resolved each cycle so config edits take effect.
    let config = load_config(&args.config)?;
    let (_cache_dir, cache_path) = synchronize::resolve_cache_path(&config, &args.config);
    let cache_ttl = synchronize::resolve_cache_ttl(&config);

    // Bound early so bind errors surface before the sync loop starts.
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

    // `spawn_local` needs a LocalSet: the health state is `Rc<RefCell<>>`.
    let local = tokio::task::LocalSet::new();
    let exit_code = local
        .run_until(async {
            // Warm start from the prior session.
            let cache = Rc::new(RefCell::new(TransferStateCache::load(
                &cache_path,
                cache_ttl,
            )));
            // To detect a changed `cache_dir`.
            let mut active_cache_path = cache_path;

            let health_handle = {
                let state = Rc::clone(&health_state);
                tokio::task::spawn_local(async move {
                    crate::cli::health::serve(health_listener, state).await;
                })
            };

            // One no-tags-matched WARN per transition, not per cycle; pruned
            // each cycle to mappings still in config.
            let mut watch_log = WatchLogState::default();

            // After a long silence, a "still alive" INFO gives log scrapers an
            // anchor. Independent of the sync interval.
            const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3600);
            let mut last_emit_at = Instant::now();

            let mut config_failures: u32 = 0;
            const BACKOFF_THRESHOLD: u32 = 3;
            const MAX_BACKOFF_SECS: u64 = 300;

            let exit_code = loop {
                // Re-resolve the cache path each cycle so a moved `cache_dir` takes
                // effect without a restart. `cache_ttl` is read only when a cache
                // file is loaded: the warm cache is persisted with a fresh
                // timestamp every cycle, so an edited TTL applies at the next
                // load, never to the cache in memory.
                match load_config(&args.config) {
                    Ok(cfg) => {
                        config_failures = 0;
                        let (_dir, path) = synchronize::resolve_cache_path(&cfg, &args.config);
                        // A moved cache is persisted, then reloaded from the new path.
                        if path != active_cache_path {
                            if let Err(e) = cache.borrow().persist(&active_cache_path) {
                                tracing::warn!(
                                    error = %e,
                                    "failed to persist cache before path change"
                                );
                            }
                            let ttl = synchronize::resolve_cache_ttl(&cfg);
                            *cache.borrow_mut() = TransferStateCache::load(&path, ttl);
                            active_cache_path = path;
                        }
                    }
                    Err(e) => {
                        config_failures += 1;
                        tracing::error!(
                            error = %e,
                            consecutive_failures = config_failures,
                            "failed to reload config; keeping the current cache path"
                        );

                        // Exponential backoff after repeated failures avoids log spam.
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
                }

                let sync_args = SyncArgs {
                    config: args.config.clone(),
                    dry_run: false,
                    json: args.json,
                };

                watch_log.begin_cycle();
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
                        // Completion is already conveyed by the per-mapping line,
                        // the cycle tail and /healthz.
                        if matches!(code, ExitCode::Success | ExitCode::PartialFailure) {
                            health_state.borrow_mut().record_success();
                        }
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "sync cycle failed");
                    }
                }

                // Any log output this cycle is already an anchor; otherwise
                // heartbeat once silent past the threshold.
                if watch_log.cycle_emit_count() > 0 {
                    last_emit_at = Instant::now();
                } else if last_emit_at.elapsed() >= HEARTBEAT_INTERVAL {
                    let idle_secs = last_emit_at.elapsed().as_secs();
                    tracing::info!(
                        idle_secs,
                        "watch alive: no state changes; sync loop healthy"
                    );
                    last_emit_at = Instant::now();
                }

                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = shutdown.notified() => {
                        tracing::info!("shutdown signal received, exiting watch mode");
                        break ExitCode::Success;
                    }
                }
            };

            if let Err(e) = cache.borrow().persist(&active_cache_path) {
                tracing::warn!(error = %e, "failed to persist cache on shutdown");
            }

            health_handle.abort();
            exit_code
        })
        .await;

    Ok(exit_code)
}
