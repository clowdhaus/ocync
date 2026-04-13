# ocync-sync

OCI registry sync orchestration engine.

This crate is part of [ocync](https://github.com/clowdhaus/ocync), a fast OCI registry sync tool. The primary interface is the `ocync` CLI — this crate is the underlying library, exposed for embedding in other Rust applications.

## What it does

Sync orchestration engine that coordinates image transfers between OCI registries. Handles tag filtering, transfer planning, blob deduplication, concurrent execution with graceful shutdown, and persistent caching.

## Capabilities

- Tag filtering pipeline: glob patterns, semver ranges, prerelease handling, exclude patterns, sort, latest-N
- Pipelined concurrent sync engine with overlapping discovery and execution
- Global blob deduplication across all images in a sync run
- Cross-repo blob mounting when source blob exists elsewhere on target registry
- Content-addressable disk staging for multi-target blob reuse
- Persistent transfer state cache with TTL and lazy invalidation
- Cooperative graceful shutdown with configurable drain deadline
- Structured sync reports with per-image and aggregate statistics

## Example

```rust
use ocync_sync::engine::{ResolvedMapping, SyncEngine};
use ocync_sync::retry::RetryConfig;
use ocync_sync::SyncReport;

let engine = SyncEngine::new(RetryConfig::default(), 50);

let mappings: Vec<ResolvedMapping> = vec![/* ... */];
let report: SyncReport = engine.run(mappings, cache, staging, &progress, None).await;

println!("synced: {}, skipped: {}", report.stats.images_synced, report.stats.images_skipped);
```

## Links

- [ocync CLI](https://github.com/clowdhaus/ocync) — the primary interface
- [API documentation](https://docs.rs/ocync-sync)
- License: Apache-2.0
