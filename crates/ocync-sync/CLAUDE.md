# ocync-sync

Sync orchestration engine - pipelined discovery/execution, leader-follower blob mounting, transfer state cache, and blob staging.

## Concurrency model

- Single-threaded tokio (`current_thread`). Workload is ~100% network I/O.
- All shared state uses `Rc<RefCell<>>`, never `Arc<Mutex<>>`.
- Exception: `RegistryClient` is wrapped in `Arc` (not `Rc`) because the underlying HTTP client and AIMD controller must be `Send`/`Sync`. The `AimdController` uses `std::sync::Mutex` internally for the same reason. This is intentional -- the client is a leaf dependency that does not participate in the engine's shared mutable state.
- Drop every `RefCell` borrow before the next `.await` point.
- The `!Send` constraint of `Rc<RefCell<>>` is a feature: prevents accidental `tokio::spawn`.
- Intra-image blob concurrency: `FuturesUnordered` + local `Semaphore(6)`.

## Engine architecture

- Pipelined: discovery and execution overlap via `tokio::select!` over two `FuturesUnordered` pools.
- `VecDeque<TransferTask>` pending queue between pools.
- `select!` uses `biased;` (prefer execution completions to free permits).
- Emptiness guards (`if !pool.is_empty()`) on every branch prevent busy-looping.
- Shutdown branch must check work-remaining or it blocks the else exit.
- Same rule for the `run_heartbeat` branch: its guard is the disjunction of the execution and discovery branches' own preconditions (`!execution_futures.is_empty() || (!shutting_down && !discovery_paused && !discovery_futures.is_empty())`). Guarding it on "futures remain" alone leaves it armed after shutdown freezes discovery, starving `else => break` and hanging the engine for a full interval. `sync_progress.rs` pins this.

## Leader-follower blob mounting

- Greedy set-cover election in `elect_leaders()` provably covers every shared blob.
- No "uncovered follower" path exists - all followers' shared blobs are in the leader union.
- Don't add wave partitioning among followers; it would be dead code.
- All tasks promoted simultaneously after discovery (leaders ordered first by `elect_leaders`). Two-level synchronization:
  1. Per-(target, digest) upload claim via `ClaimAction::Wait`: whichever task claims a blob first uploads it, and every other task for that digest at that target, leader or follower, waits on `claim_notify` and re-checks.
  2. Per-(repo, blob) `watch<bool>` subscribed through `unresolved_watches_for`: a task waits for a source repo recorded this run to commit a manifest that references the blob before mounting it from there. ECR requires a committed manifest in the source repo that references the specific blob for mount to succeed (201), and returns 202 (Not Fulfilled) otherwise, so commit is a per-blob fact recorded where each manifest push succeeds (`settle_blobs` calling `confirm_blobs`): an image's referrer artifacts (SOCI indexes, attestations) commit after the image, and a blob only they reference is not a mount source until their manifest commits. `mount_sources` offers repos where the blob is committed first, then repos known to hold the blob only from an earlier run (`Remembered`), without waiting: nothing in this run commits them, and a refused mount marks the source stale and falls back to a push. Uses `watch` (not `Notify`) because commit status is boolean state, not an event -- `watch` retains the last value so late subscribers always see it.

## Manifest commit gating (ECR consistency window)

ECR's manifest-validation index is eventually consistent with blob upload state. A blob `PUT-201` can land before the validator's view catches up; a manifest `PUT` issued during that window fails with HTTP 404 carrying `BLOB_UPLOAD_UNKNOWN`. The retry path in `with_retry` catches this code as a defence-in-depth, but the structural fix is to gate the manifest commit on the authoritative blob-visibility API.

`push_manifests` calls `BatchBlobChecker::wait_for_blobs_available` before issuing the manifest `PUT` when both:
1. The target has a configured `batch_checker` (production: only ECR), AND
2. `RetryConfig::manifest_commit_wait` is non-zero.

The wait polls `BatchCheckLayerAvailability` with exponential backoff (`200ms -> 400ms -> ... -> 5s`) until every referenced layer is visible, or `manifest_commit_wait` is reached. On timeout the engine falls through to the standard manifest PUT and any residual `BLOB_UPLOAD_UNKNOWN` is caught by `with_retry`.

Production default: 30s. Tests default to `Duration::ZERO` via `fast_retry()` so the wait does not fire (mock checkers don't reflect engine uploads). Tests that need to exercise the wait construct their own `RetryConfig` with `manifest_commit_wait > 0`.

## Synchronization contracts

- Per-blob `Notify::notify_waiters()` does NOT store permits. Every path that ends an upload claim must call `release_blob_claim`, which notifies `claim_notify`. Source-pull dedup in staging holds a `ClaimGuard` from `shared.rs`, whose drop wakes waiters; drop it once the blob is staged or the pull fails. Missing notify = deadlock for concurrent waiters.
- Per-(repo, blob) `watch::Sender::send_replace(true)` is called by `confirm_blobs` (a manifest referencing the blob committed) and `notify_repo_failed` (every watch at the repo, and later subscriptions for it this run). Every path where a repo holds a blob this run but will never commit a manifest referencing it must call `notify_repo_failed`, or followers sit out the whole deadline: image blob failure, image manifest failure and any artifact failure after the image committed. Shutdown needs no call: the drain deadline drops every in-flight image, leaders and waiters together. Use `send_replace`, not `send`: `send` drops the value when no receiver exists yet, so a follower subscribing later would miss it. Stored that way, late subscribers see the value immediately.
- No wait on another image's manifest commit may be unbounded. Multiple images can be elected as leaders when they share distinct blob subsets with different followers, so commit waits can form a cycle (A waits for B while B waits for A). `resolve_mount_source` waits at most `mount_source_wait_deadline` (default 180s) per blob, then falls back to push. Do not add a commit wait outside it.
- **Semaphore safety boundary (enforced via function signature).** External waits (`Notify::notified()`, `watch::wait_for()`) must not hold the per-image blob semaphore. If all `BLOB_CONCURRENCY` permits are consumed by external waits, no other blob can make progress, creating a deadlock. After the semaphore is acquired, `transfer_claimed_blob` takes `BlobIoContext` (not `TransferContext`), so `TransferContext` (and its cache read/wait methods) is unreachable. `BlobSink` only exposes write/signal methods. All external waits happen in the pre-semaphore phase: `wait_for_blob_claim` (blob claim), `resolve_mount_source` (commit watch), `resolve_staging` (staging dedup).

## Transfer state cache

- Two-tier: hot (in-memory) + warm (persistent disk, binary postcard + CRC32).
- Only `Verified` and `Completed` persisted; `Uploading` and `Failed` are stripped.
- Progressive population: existence checks run during execution, per image. A target with a batch checker (ECR) checks all of an image's blobs in one call before the per-blob loop; otherwise each blob gets a HEAD inline.
- Lazy invalidation: stale entries self-heal on mount/push failure.

## Blob staging

- Content-addressable: `{cache_dir}/blobs/sha256/{hex_digest}`.
- Atomic write: tmp file -> fsync -> rename -> dir fsync.
- Zero overhead for single-target (`BlobStage::disabled()`).
- `claim_or_check` eliminates redundant source GETs (source-pull dedup).

## Testing

### Structure

- Integration tests live in `tests/sync_*.rs` (one file per subsystem). Shared helpers in `tests/helpers/`.
- Do not write tests that capture tracing output via `set_default()` -- tracing's global `MAX_LEVEL` and callsite interest caching make per-test subscriber capture unreliable when `cargo test` runs tests in parallel within the same process. Test behavior via engine outcomes, not log messages.
- Each test file starts with `mod helpers; use helpers::*;` -- helpers are NOT auto-discovered.
- Helper sub-modules use `#![allow(dead_code, unused_imports, unreachable_pub)]` (required for test module visibility).
- Batch checker mocks live in `tests/helpers/mocks.rs`: `MockChecker::new` (listed digests present, the rest missing), `MockChecker::leaving_out` (as `new`, but the unknown digests are left out of the answer), `MockChecker::failing` (every check fails) and `WaitTrackingChecker`, which also counts the manifest-commit gate's `wait_for_blobs_available` calls.

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
- `run_engine(mappings, retry, max_concurrent, cache, stage, shutdown)` -- every knob explicit, for custom retry, concurrency or staging.
- `run_with_shutdown_at(run, shutdown, want, received)` -- drives a run and triggers shutdown once the mock server has received `want` requests, so it lands mid-transfer however slow the host is.

Use `SyncEngine::new(...)` directly only for builder settings `run_engine` does not take, such as `with_source_head_timeout` or `with_drain_deadline`.

### Test design rules

- Mount tests: symmetric mocks (both repos accept upload AND mount) since leader-follower can pick either as leader. Assert aggregate stats, not per-image ordering.
- Concurrency tests: run at `max_concurrent > 1`. Serializing a flaky test hides a race.
- At least one negative assertion per test (prove the unwanted path is NOT taken).
- `assert_status!` macro for pattern-matching on `ImageStatus` with good diagnostics.

## Retry

`with_retry` lives in `src/retry.rs` (moved there from `engine.rs` so it sits with `RetryConfig` and the `should_retry*` predicates) and is `pub`, because the CLI's `analyze` walk needs it too.

It is not the only retry layer any more. The prepare phase runs before the engine exists, so tag listing, token exchange, and ACR's OAuth exchanges retry themselves inside `ocync-distribution` (see that crate's CLAUDE.md). A 429 during tag listing therefore no longer surfaces as an unresolved mapping and never reaches `with_retry`.

`should_retry_transport` delegates to `ocync_distribution::retry::is_transient_transport` rather than repeating the predicate. Keep it that way; two copies drift.
