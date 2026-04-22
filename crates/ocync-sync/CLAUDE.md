# ocync-sync

Sync orchestration engine - pipelined discovery/execution, leader-follower blob mounting, transfer state cache, and blob staging.

## Concurrency model

- Single-threaded tokio (`current_thread`). Workload is ~100% network I/O.
- All shared state uses `Rc<RefCell<>>`, never `Arc<Mutex<>>`.
- Exception: `RegistryClient` is wrapped in `Arc` (not `Rc`) because the underlying HTTP client and AIMD controller must be `Send`/`Sync`. The `AimdController` uses `std::sync::Mutex` internally for the same reason. This is intentional -- the client is a leaf dependency that does not participate in the engine's shared mutable state.
- `RefCell` borrows MUST be dropped before any `.await` point.
- The `!Send` constraint of `Rc<RefCell<>>` is a feature: prevents accidental `tokio::spawn`.
- Intra-image blob concurrency: `FuturesUnordered` + local `Semaphore(6)`.

## Engine architecture

- Pipelined: discovery and execution overlap via `tokio::select!` over two `FuturesUnordered` pools.
- `VecDeque<TransferTask>` pending queue between pools.
- `select!` uses `biased;` (prefer execution completions to free permits).
- Emptiness guards (`if !pool.is_empty()`) on every branch prevent busy-looping.
- Shutdown branch must check work-remaining or it blocks the else exit.

## Leader-follower blob mounting

- Greedy set-cover election in `elect_leaders()` provably covers every shared blob.
- No "uncovered follower" path exists - all followers' shared blobs are in the leader union.
- Do NOT add wave partitioning among followers (dead code).
- All tasks promoted simultaneously after discovery (leaders ordered first by `elect_leaders`). Per-blob `Notify` via `ClaimAction::Wait` synchronizes followers that need blobs still in-flight. Mount sources restricted to repos with committed manifests via `committed_repos` HashSet in cache.

## Notify contracts (critical)

- `tokio::sync::Notify::notify_waiters()` does NOT store permits.
- Every code path that transitions a blob out of `InProgress` MUST call `notify_blob`.
- Same applies to `BlobStage::notify_staged` / `notify_failed` for source-pull dedup.
- Missing notify = deadlock for concurrent waiters.

## Transfer state cache

- Two-tier: hot (in-memory) + warm (persistent disk, binary postcard + CRC32).
- Only `ExistsAtTarget` and `Completed` persisted; transient states stripped.
- Progressive population: HEAD checks inline during execution, not upfront batch.
- Lazy invalidation: stale entries self-heal on mount/push failure.

## Blob staging

- Content-addressable: `{cache_dir}/blobs/sha256/{hex_digest}`.
- Atomic write: tmp file -> fsync -> rename -> dir fsync.
- Zero overhead for single-target (`BlobStage::disabled()`).
- `claim_or_check` eliminates redundant source GETs (source-pull dedup).

## Testing

### Structure

- Integration tests live in `tests/sync_*.rs` (one file per subsystem). Shared helpers in `tests/helpers/`.
- Each test file starts with `mod helpers; use helpers::*;` -- helpers are NOT auto-discovered.
- Helper sub-modules use `#![allow(dead_code, unused_imports, unreachable_pub)]` (required for test module visibility).
- `MockBatchChecker`/`FailingBatchChecker` live in `sync_cache.rs` (not in shared helpers) since only cache tests use them.

### Writing new tests

Use builders for fixture data. A typical test is 45-70 lines:

```rust
#[tokio::test]
async fn my_new_test() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;

    // Builders own fixture data (real digests, real serialization).
    let img = ManifestBuilder::new(b"cfg").layer(b"layer").build();
    img.mount_source(&source, "repo", "v1").await;
    img.mount_target(&target, "repo", "v1").await;

    // Use run_sync() for default engine settings.
    let mapping = mapping_from_servers(&source, &target, "repo", vec![TagPair::same("v1")]);
    let report = run_sync(vec![mapping]).await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
}
```

### Builder hierarchy

- `ManifestBuilder` -- image manifests (config + N layers). Use for most tests.
- `ArtifactBuilder` -- OCI artifacts (config + 1 layer + artifact_type). Mounted by digest, not tag.
- `IndexBuilder` -- multi-arch indexes. Takes `&ManifestParts` children.
- `ReferrersIndexBuilder` -- referrers API responses. Chain `.artifact(&parts)` or `.descriptor(desc)`.

Builders produce `*Parts` structs with `.mount_source()` / `.mount_target()` convenience methods for the 80% case (fresh target, all blobs missing).

### When NOT to use builders

- Tests where mock topology IS the subject (partial existence, rate-limited push, broken child) -- use free functions from `mocks.rs` directly.
- Tests needing push-count precision (`expect(1)` on endpoints) -- `mount_target` is too permissive for this. Set up mocks manually.
- Tests using `simple_image_manifest` (fake digests for HEAD-check/discovery tests where blob content is irrelevant).

### Engine run helpers

- `run_sync(mappings)` -- default settings (max_concurrent=50, no cache/staging/shutdown).
- `run_sync_sequential(mappings)` -- max_concurrent=1 for deterministic ordering.
- `run_sync_with_cache(mappings, cache)` -- caller-provided cache, max_concurrent=50.
- `run_sync_with_shutdown(mappings, shutdown)` -- caller-provided shutdown signal.

Use `SyncEngine::new(...)` directly when the test needs custom concurrency, staging, `with_source_head_timeout`, or `with_drain_deadline`.

### Test design rules

- Mount tests: symmetric mocks (both repos accept upload AND mount) since leader-follower can pick either as leader. Assert aggregate stats, not per-image ordering.
- Concurrency tests: run at `max_concurrent > 1`. Serializing a flaky test hides a race.
- At least one negative assertion per test (prove the unwanted path is NOT taken).
- `assert_status!` macro for pattern-matching on `ImageStatus` with good diagnostics.

## Commands

```bash
# Unit + integration tests
cargo test --package ocync-sync

# Run a single test file (fast iteration during development)
cargo test --package ocync-sync --test sync_artifacts
```
