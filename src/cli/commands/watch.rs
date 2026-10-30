//! The `watch` subcommand - daemon mode for periodic sync with graceful shutdown.

use std::cell::RefCell;
use std::path::Path;
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

/// Whether the warm cache must be reloaded because the re-read config moved
/// its file or changed its TTL. The TTL is applied only when the cache is
/// loaded, so an edited TTL on an unchanged path needs the reload too.
fn cache_needs_reload(active: (&Path, Duration), next: (&Path, Duration)) -> bool {
    active != next
}

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
    let cache_ttl = synchronize::resolve_cache_ttl(&config);

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
            // Track the active cache path and TTL so we can detect config changes.
            let mut active_cache_path = cache_path;
            let mut active_cache_ttl = cache_ttl;

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

            // Heartbeat: when no log line has been emitted for an extended
            // period (idle steady-state watch), emit a "still alive" INFO
            // so log scrapers have a recent anchor confirming the process
            // is doing its job. Cadence is decoupled from the sync interval.
            const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3600);
            let mut last_emit_at = Instant::now();

            // Track consecutive config reload failures for backoff.
            let mut config_failures: u32 = 0;
            const BACKOFF_THRESHOLD: u32 = 3;
            const MAX_BACKOFF_SECS: u64 = 300; // 5 minutes

            let exit_code = loop {
                // Re-resolve cache_dir and cache_ttl each cycle so that
                // config file edits (e.g. changing cache_dir or cache_ttl)
                // take effect without restarting watch mode.
                match load_config(&args.config) {
                    Ok(cfg) => {
                        config_failures = 0;
                        let (_dir, path) = synchronize::resolve_cache_path(&cfg, &args.config);
                        let ttl = synchronize::resolve_cache_ttl(&cfg);
                        // If the cache path or TTL changed, persist the old
                        // cache and reload with the new values.
                        if cache_needs_reload(
                            (&active_cache_path, active_cache_ttl),
                            (&path, ttl),
                        ) {
                            if let Err(e) = cache.borrow().persist(&active_cache_path) {
                                tracing::warn!(
                                    error = %e,
                                    "failed to persist cache before reload"
                                );
                            }
                            *cache.borrow_mut() = TransferStateCache::load(&path, ttl);
                            active_cache_path = path;
                            active_cache_ttl = ttl;
                        }
                    }
                    Err(e) => {
                    config_failures += 1;
                    tracing::error!(
                        error = %e,
                        consecutive_failures = config_failures,
                        "failed to reload config for cache resolution; the cache keeps its current path and TTL, and the sync below reads the file again"
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
                        // Cycle completion is conveyed by the dedup-aware
                        // per-mapping line + cycle tail (when stats change)
                        // and by /healthz; no separate INFO needed.
                        if matches!(code, ExitCode::Success | ExitCode::PartialFailure) {
                            health_state.borrow_mut().record_success();
                        }
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "sync cycle failed");
                    }
                }

                // Heartbeat gate: if anything emitted this cycle, the user
                // already has a recent log anchor; reset the timer. Else
                // check the elapsed-since-last-emit and fire the heartbeat
                // when we've been silent past the threshold.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A changed cache path or TTL reloads the cache and an unchanged pair
    /// does not. Breaks if an edited `cache_ttl` on the same path is ignored,
    /// or an unchanged config reloads the cache every cycle.
    #[test]
    fn the_cache_reloads_when_its_path_or_ttl_changes() {
        let hour = Duration::from_secs(3600);
        let day = Duration::from_secs(86_400);
        let a = Path::new("/cache/a");
        let b = Path::new("/cache/b");
        let cases = [
            ("neither changed", (a, hour), (a, hour), false),
            ("path changed", (a, hour), (b, hour), true),
            ("ttl changed", (a, hour), (a, day), true),
            ("both changed", (a, hour), (b, day), true),
        ];
        for (name, active, next, expected) in cases {
            assert_eq!(cache_needs_reload(active, next), expected, "{name}");
        }
    }
}
