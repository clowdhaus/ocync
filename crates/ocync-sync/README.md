# ocync-sync

OCI registry sync orchestration engine for Rust.

This crate is part of [ocync](https://github.com/clowdhaus/ocync), a fast OCI registry sync tool. The primary interface is the `ocync` CLI - this crate is the underlying library, exposed for embedding in other Rust applications.

## What it does

Pipelined sync engine that coordinates image transfers between OCI registries with blob deduplication, cross-repo mounting, and adaptive concurrency.

## Key features

- Pipelined architecture: discovery and execution overlap (no idle time)
- Global blob deduplication across all images in a sync run
- Leader-follower election for cross-repo blob mounting
- Content-addressable disk staging for multi-target blob reuse
- Persistent transfer state cache (survives across runs)
- Tag filtering: glob patterns, semver ranges, exclude patterns, sort, latest-N
- Cooperative graceful shutdown with configurable drain deadline
- Structured sync reports with per-image and aggregate statistics

## Feature flags

None. All functionality is always enabled.

## Minimum Rust version

1.94 (edition 2024)

## Example

```rust
use ocync_sync::engine::{ResolvedMapping, SyncEngine};
use ocync_sync::retry::RetryConfig;
use ocync_sync::SyncReport;

let engine = SyncEngine::new(RetryConfig::default(), 50);

let mappings: Vec<ResolvedMapping> = vec![/* ... */];
let report: SyncReport = engine
    .run(mappings, cache, staging, &progress, None)
    .await;

println!(
    "synced: {}, skipped: {}",
    report.stats.images_synced, report.stats.images_skipped
);
```

## Architecture note

The engine uses single-threaded tokio (`current_thread`) because the workload is nearly 100% network I/O. Shared state uses `Rc<RefCell<>>` for zero-overhead interior mutability. This matches Kubernetes deployments where a CPU request of 100m is sufficient.

## Public API surface

The primary entry points and types are listed below.

| Type | Description |
|------|-------------|
| `SyncEngine` | Main engine - runs a set of mappings to completion |
| `ResolvedMapping` / `TargetEntry` / `TagPair` | Sync configuration (source, targets, tags) |
| `SyncReport` / `ImageResult` / `SyncStats` | Structured results and aggregate statistics |
| `FilterConfig` | Tag filtering pipeline (glob, semver, exclude, sort, latest-N) |
| `TransferStateCache` | Persistent blob dedup across runs |
| `BlobStage` | Content-addressable disk staging for multi-target reuse |
| `ShutdownSignal` | Graceful shutdown coordination |
| `ProgressReporter` | Trait for progress callbacks |
| `RetryConfig` | Exponential backoff configuration |

## Links

- [ocync CLI](https://github.com/clowdhaus/ocync) - the primary interface
- License: Apache-2.0
