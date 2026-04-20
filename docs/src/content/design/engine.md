---
title: Sync engine
description: Pipeline architecture, adaptive concurrency, transfer state cache, blob staging, and cross-repo mounting
order: 2
---

## Pipeline architecture

The sync engine uses a pipelined architecture where discovery and execution run concurrently in a single `tokio::select!` loop over two `FuturesUnordered` pools. Execution starts the moment the first image is discovered, with no waiting for all discovery to complete. The engine should never be idle when useful work exists.

The engine runs on `#[tokio::main(flavor = "current_thread")]`. The workload is ~100% network I/O, where hundreds of concurrent futures spend >99% of wall-clock time awaiting HTTP responses. A single thread handles this without contention overhead. Shared state uses `Rc<RefCell<>>`, and the `!Send` constraint is a feature: it statically prevents accidental use of `tokio::spawn` (which requires `Send`). All concurrency uses `FuturesUnordered` with async move blocks, which work on `current_thread` without `Send` bounds.

```
Config
  |
  +-- Sort tags by repo + semver (free, per mapping)
  |
  v
+---------------------------------------------------+
| Pipeline Loop (select! over two pools)            |
|                                                   |
| Discovery Pool (FuturesUnordered):                |
|   Per mapping, per tag (semver order):            |
|     1. Source HEAD or GET -> get source digest     |
|     2. HEAD each target -> compare digests         |
|        -> skip if unchanged                        |
|     3. Source GET (if HEAD-only) -> SourceData     |
|     4. Resolve index children (full tree)         |
|     5. Update blob frequency map                  |
|     6. -> TransferTask to pending queue             |
|                                                   |
| Execution Pool (FuturesUnordered):                |
|   Bounded by global concurrency cap (semaphore).  |
|   Per (tag, target), blobs in frequency order:    |
|     1. Cache check -> skip / mount / HEAD          |
|     2. Push: stream (1 target) or stage (N)       |
|     3. Update cache                               |
|     4. Push manifests (children first, then index) |
|   On 429: AIMD halve for (registry, action)       |
|                                                   |
| Each discovery result may promote pending -> exec. |
| Each execution completion frees a global permit,  |
| allowing the next pending item to enter execution.|
+-------------------------+-------------------------+
                          |
                          v
                Persist transfer state cache
```

A `VecDeque<TransferTask>` holds items that discovery has resolved but execution has not started. When a discovery future resolves, its `TransferTask` enters the pending queue and the loop attempts to promote pending items into execution (bounded by semaphore). When an execution future completes, its permit is released and the next pending item is promoted.

### Why pipelining beats plan-then-execute

The primary bottleneck for Chainguard -> multi-region ECR is the target side: ECR `PutImage` at 10 TPS, `InitiateLayerUpload` at 100 TPS. Discovery from Chainguard (no rate limits) completes in ~10-20 seconds for hundreds of images. The pipeline overlaps this discovery time with early execution, producing three distinct advantages:

**Progressive cache population.** Early transfers teach the cache about which blobs exist where. Later images benefit from accumulated knowledge, and the more images processed, the fewer API calls needed. In the 3-phase model, all HEAD checks happen before any transfers, so the cache is empty during planning. Scale example: 100 images, 20 shared base layers, 15 repos on one target. Plan-then-execute: 300 HEAD checks upfront. Pipeline with progressive cache: 20 HEAD checks (first encounter per blob) + 280 direct mounts (zero discovery). Savings: 280 API calls.

**PutImage pipeline smoothing.** As early images complete blob transfers and push manifests (consuming PutImage tokens at 10 TPS), later images' blob transfers proceed unblocked. The pipeline naturally spreads PutImage load across the full sync duration rather than concentrating it at the end.

**Immediate execution start.** Execution begins after ~1 second of discovery instead of waiting 10-20 seconds for all discovery to complete. For a minutes-long sync this is a small but free improvement. For Docker Hub as a secondary source (100 manifest GETs / 6 hours authenticated), the pipeline is even more valuable: discovery can take hours, and execution proceeds on already-discovered images while discovery is rate-limited.

### Select loop discipline

The `select!` loop uses `biased;` to prefer execution completions over discovery completions, because freeing permits allows pending items to enter execution, maximizing throughput. Each `select!` branch uses an `if !pool.is_empty()` precondition to prevent busy-looping. An empty `FuturesUnordered` returns `Poll::Ready(None)` immediately, and without guards, the empty branch wins every poll iteration, spinning the CPU while the other pool has legitimate work awaiting I/O.

The `else` arm (both pools empty and no pending items) is the clean termination condition. The shutdown branch must check work-remaining or it blocks the else exit. Shutdown and drain deadline branches use guard conditions, never `std::future::pending()` inside async blocks.

## Adaptive concurrency (AIMD)

Concurrency is controlled at three levels that compose naturally, replacing any need for operators to guess at registry capacity.

### Three-level hierarchy

**Level 1, global image semaphore** (default: 50). Bounds how many `(tag, target)` pairs are in-flight simultaneously, preventing memory explosion. This is the engine-level `max_concurrent_transfers` config.

**Level 2, per-registry aggregate semaphore** (`max_concurrent` per registry, default: 50). Bounds total concurrent HTTP requests to a single registry host across all action types. This is a safety ceiling for connection/memory pressure, not a rate-limit mechanism. With HTTP/2 multiplexing, 100+ concurrent requests share ~6-8 TCP connections, so the aggregate cap is conservative. A request must acquire a permit from this semaphore before proceeding to the per-action AIMD check.

**Level 3, per-(registry, action) AIMD windows.** Each action type adapts independently within the aggregate ceiling. A 429 on `InitiateLayerUpload` halves that window only, while `UploadLayerPart` continues at its own pace. Each window's effective cap is `min(aimd_window, aggregate_permits_available)`.

The levels compose via dual permit acquisition: every request acquires one permit from the registry aggregate semaphore AND one from the per-action AIMD window. The aggregate semaphore prevents resource exhaustion; the AIMD windows prevent per-action rate-limit storms. Slow actions (PutImage at 10 TPS) release aggregate permits promptly, so fast actions (UploadLayerPart at 500 TPS) are never starved.

### AIMD formula

Each `(registry, window_key)` pair maintains an independent concurrency window using AIMD (Additive Increase, Multiplicative Decrease):

1. **Start:** `window` = 5.0 (conservative)
2. **On success:** `window += 1.0 / window` (fractional additive increase)
3. **On 429/throttle:** halve once per congestion epoch (see below)
4. **Cap:** `max_concurrent` from per-registry config (default 50)

From initial 5.0, the window reaches 50 in ~1,200 successful responses, a gradual ramp that avoids overshoot. At 50ms latency with 5 concurrent requests, this takes ~12 seconds. If the registry throttles at 30 concurrent, the controller oscillates between ~15 and ~30 (the classic AIMD sawtooth), settling to an effective average of ~22.5 concurrent requests.

The fractional increase (`+1/window`) means it takes a full window's worth of successes to increase by 1. This is standard TCP congestion avoidance. A naive `+1 per success` would be slow-start (exponential growth), causing aggressive oscillation.

### Congestion epochs

When multiple concurrent futures hit a rate limit simultaneously (e.g., 10 futures all receive 429 from the same ECR `InitiateLayerUpload` burst), each 429 is processed in a separate tokio event loop tick. Without protection, each independently halves the window: `50 -> 25 -> 12.5 -> 6.25 -> 3.1 -> 1.0`, causing catastrophic collapse from a single capacity event.

The fix is TCP Reno's congestion epoch: each AIMD window tracks `last_decrease: Instant`. On 429, halve only if `now - last_decrease > epoch_duration` (100ms, approximating one cloud API RTT). Multiple 429s within the same epoch are a single congestion event, so the window halves once and subsequent 429s within the epoch are ignored for window adjustment (the requests still retry with backoff). Ten simultaneous 429s cause exactly one halving (50 -> 25), not ten (50 -> 1). This adds 2 fields per window (`last_decrease: Instant`, `epoch_duration: Duration`) and a single branch in the decrease path.

```rust
fn on_throttle(&mut self) {
    let now = Instant::now();
    if now.duration_since(self.last_decrease) > self.epoch_duration {
        self.window = (self.window / 2.0).max(1.0);
        self.last_decrease = now;
    }
    // else: same congestion epoch, window already adjusted
}
```

### Per-registry window groupings

Actions are fine-grained, matching the actual API action granularity of each registry via a `RegistryAction` enum (ManifestHead, ManifestRead, ManifestWrite, BlobHead, BlobRead, BlobUploadInit, BlobUploadChunk, BlobUploadComplete, TagList). A per-registry mapping function groups actions into AIMD windows:

| Registry | Windows | Rationale |
|---|---|---|
| ECR | 9 (one per action) | Critical: `InitiateLayerUpload` (100 TPS) and `UploadLayerPart` (500 TPS) have a 5x rate disparity, so a 429 on session initiation must not throttle chunk uploads |
| Docker Hub | 3 (HEAD unmetered; manifest-read separate; others shared) | HEAD and BlobHead are free (no rate limit). ManifestRead gets its own window (counted against 100-pull/6h quota). Other actions share one window |
| GAR | 1 (all shared) | GAR uses a shared per-project quota across all operation types; initial window of 5, grows adaptively to `max_concurrent` |
| ACR, GHCR, others | 5 (HEAD, READ, UPLOAD, MANIFEST_WRITE, TAG_LIST) | Coarse grouping as safe default for registries without documented per-action limits |

The mapping function is ~20 lines. The window key is derived from HTTP method + URL pattern, which `ocync` already tracks in request labels.

### Budget circuit breaker

Parse `ratelimit-remaining` from Docker Hub manifest responses (and `x-ratelimit-remaining` from GHCR). Single decision point: if `remaining < max(remaining_discovery_count / 10, 1)`, log warning and pause discovery. The floor of 1 ensures the breaker can fire even for small syncs (fewer than 10 images). Discovery items already in `pending` wait for promotion (which requires all discovery to complete). Resume when a subsequent response shows budget has refilled, or when the execution pool is empty (preventing stall). The resume condition checks execution emptiness alone, not `pending`, because pending items cannot be promoted until all discovery completes -- requiring both to be empty would deadlock when the breaker pauses discovery with items already queued. In practice, the anti-stall path serializes remaining discovery one-at-a-time, preventing burst consumption of the rate-limit budget.

The header is extracted in `RegistryClient::send_with_aimd` on every successful response and stored in an `AtomicU64` on the client. The engine reads this value on source clients after each discovery completion and compares against the threshold. The `discovery_paused` guard disables the discovery `select!` branch, which on `current_thread` prevents paused futures from being polled.

This is not a dynamic priority scheduler. It is a circuit breaker with one threshold. For registries without rate-limit headers (ECR, GAR, Chainguard): AIMD handles capacity discovery automatically through 429 feedback.

## Cross-repo blob mounting

When source and target share a registry, or when multiple images share base layers on the same target registry, cross-repo blob mounting eliminates redundant data transfer entirely. Instead of re-uploading a blob that already exists in a different repository on the same registry, a mount operation links it in constant time. Zero bytes cross the network.

### Leader-follower election

The engine uses a greedy set-cover algorithm (`elect_leaders()`) to elect leader images, selecting those with the most shared blobs across all discovered images. Leaders execute first (wave 1), uploading their blobs and committing manifests. Followers execute second (wave 2), mounting shared blobs from leader repositories instead of re-uploading.

The greedy set-cover provably covers every shared blob. No "uncovered follower" path exists because all followers' shared blobs are in the leader union. This means there is no code path where a follower needs a blob that was not already uploaded by a leader.

### Wave promotion

**Wave 1, leaders.** Leaders upload all their blobs from source and commit manifests to the target registry. Once a leader's manifest is committed, its repository becomes a valid mount source for all blobs it contains.

**Wave 2, followers.** After wave 1 completes, followers start executing. For each shared blob, the follower attempts a mount from the leader's repository (which now has a committed manifest). Preferred mount sources (leader repos with committed manifests) are tried first, avoiding 202 rejections from repos whose manifests have not yet been committed. This raised mount success from 40% to 100% in testing.

### ECR blob mounting

AWS launched opt-in `BLOB_MOUNTING` account setting in January 2026. Enable via `aws ecr put-account-setting --name BLOB_MOUNTING --value ENABLED`. Mount POST returns 201 when all conditions are met: BLOB_MOUNTING is ENABLED on the account, the source blob is referenced by a committed manifest (standalone blobs without manifests return 202), the `from` parameter specifies the source repo name, and both repos have identical encryption config (AES256 default works). Requirements: same account + region, identical encryption config, pusher needs `ecr:GetDownloadUrlForLayer` on source repo. Not supported for pull-through cache repos.

Mount attempts are made unconditionally regardless of provider; the 202 fallback is cheap (~100ms) and successful mounts save entire blob uploads.

### Measured impact

Tested on c6in.large, Jupyter corpus (5 images, 1 tag each), cold sync to ECR us-east-1:

| Metric | Before (no leader-follower) | After (leader-follower + blob concurrency) |
|---|---|---|
| Requests | 1,049 | 591 |
| Response bytes | 11.5 GB | 4.9 GB |
| Wall clock | 217.9s | 56.8s |
| Mount attempts | 0 (short-circuited) | 192 |
| Mount success | 0 | 192 (100%) |

44% fewer API calls, 57% fewer bytes via cross-image staging dedup and 100% mount success, 3.8x faster wall clock via 6-concurrent blob transfers within each image.

## Transfer state cache

The transfer state cache eliminates redundant API discovery calls by recording knowledge from every operation. It is the single source of truth for blob existence across all target registries, progressively populated as transfers proceed.

### Two-tier design

**Tier 1, hot cache (in-memory, within-run).** The in-memory `BlobDedupMap` behind `Rc<RefCell<>>`, populated by every transfer, mount, and HEAD check within the current sync run. The 3-step lookup drives all blob transfer decisions:

1. Check cache: known at this repo? Skip (0 API calls)
2. Check cache: known at different repo on same target? Mount (1 API call)
3. Unknown: HEAD check at target, record result (1 API call). If exists, record in cache and skip. If missing, mount from cache source or push (1-2 API calls)

Step 2 is the key optimization: when the cache knows a blob exists at a different repo on the same target, skip the HEAD check and go directly to mount. This eliminates the need for a separate plan phase that batch-checks all blobs upfront.

**Tier 2, warm cache (persistent disk, cross-run).** Serialized to disk on sync completion using a binary format (postcard + CRC32 integrity check). Loaded on startup for CronJob deployments. Watch mode keeps the hot tier in memory naturally; the warm tier provides crash recovery.

At typical scale (~50K entries), the file is ~1MB and loads in <5ms. At extreme scale (600K entries), ~8MB with sub-100ms load. Deserialization uses `spawn_blocking` to avoid blocking the async runtime. The binary header stores a version field for forward compatibility (unknown versions discard and rebuild) and a `written_at: u64` timestamp. On load, if `now - written_at > cache_ttl`, the entire file is discarded and rebuilt. Per-entry timestamps are unnecessary because the entire file is written atomically at one point in time, and lazy invalidation handles individual stale entries regardless of age.

Corruption detection uses a CRC32 checksum appended to the file. Failed check discards and rebuilds. Concurrent access from overlapping CronJob runs is detected via advisory file lock; log warning and skip persistence if locked. `concurrencyPolicy: Forbid` is recommended for K8s CronJobs sharing a PVC.

### Lazy invalidation

Staleness is handled by lazy invalidation. If a mount or push fails for a blob the cache claims exists, the entry is invalidated and the operation retries with a fresh HEAD check. Self-healing, zero configuration. Handles external actors deleting blobs, ECR lifecycle policies garbage-collecting images, and registry GC between runs. An optional TTL on persistent entries provides an additional safeguard via `global.cache_ttl`.

On double failure (retry also fails, e.g., source blob GC'd), the blob is marked as permanently unknown and the engine proceeds to full HEAD -> pull -> push. Failed status from a stale cache is never propagated to a new sync.

### Persistence rules

Only `ExistsAtTarget` and `Completed` entries are persisted. `InProgress` and `Failed` are transient states stripped before serialization. Stale errors and incomplete transfers from a previous run must not poison the next run.

### Progressive cache population amplified by pipelining

Pipelining amplifies cache value: in plan-then-execute, all HEAD checks happen before any transfers, so the cache is empty during planning. In the pipeline, early transfers populate the cache, and later images benefit from accumulated knowledge. The more images processed, the fewer API calls needed.

Cold start behavior: when the warm cache is empty (first run or cache lost), progressive cache population handles discovery. Each blob is HEAD-checked on first encounter and cached for subsequent images. With frequency ordering, shared base layers are checked first, and the cache converges fast. At typical scale (5,000 unique blobs, 50 concurrent), cold start adds ~5 seconds of HEAD checks interleaved with blob transfers. The persistent warm cache prevents true cold start from recurring after the first run.

## Streaming transfers and staging

The transfer pipeline handles blob data differently depending on whether a mapping has one target or multiple targets. Single-target mode is the zero-overhead fast path. Multi-target mode trades disk I/O for source bandwidth savings.

### Single-target zero-disk mode

Blobs are piped directly from source to target with no local disk usage:

```
source registry GET -> [bytes stream] -> target registry PATCH/PUT
                         |
                   SHA-256 computed on-the-fly for verification
```

Memory usage is bounded by chunk size per active upload stream. No disk writes, no eviction logic. The majority deployment (single ECR target) pays only the adaptive concurrency overhead.

### Multi-target content-addressable staging

When a mapping has multiple targets (e.g., us-ecr + eu-ecr + ap-ecr), the source blob is pulled once and written to a content-addressable disk file at `{cache_dir}/blobs/sha256/{hex_digest}`. All target pushes read from the disk file independently, at their own pace.

The write path uses an atomic protocol: `{digest}.tmp.{random}` -> `fsync` -> `rename` to `{digest}` -> `fsync` parent directory. Incomplete writes are never visible as cache entries. After rename, target readers get page-cached reads at memory speed (4+ GB/s) because the data stays in page cache when reads happen milliseconds after writes.

Source-pull dedup via `BlobStage::claim_or_check` eliminates redundant source GETs. When multiple images share a blob, the first to claim it pulls from source. Subsequent claimers wait for the pull to complete, then read from disk. `tokio::sync::Notify::notify_waiters()` does NOT store permits, so every code path that transitions a blob out of `InProgress` MUST call `notify_staged` or `notify_failed`. Missing notify causes deadlock for concurrent waiters.

Crash recovery: on startup, delete all `.tmp.*` files in the cache directory. Named by digest, so orphaned files are identifiable. Eviction runs between sync cycles when cache size exceeds `staging_size_limit` (default 2GB), evicting blobs referenced by fewer discovered manifests first. In-flight transfers are never interrupted by eviction.

Single-target deployments pay zero overhead. `BlobStage::disabled()` is used when `mapping.targets.len() == 1`, with no disk writes, no eviction logic, and no claim/wait maps.

### Upload strategy per registry

Each registry has different upload protocol requirements. The engine detects the target registry and selects the appropriate strategy:

| Registry | Strategy | Requests/blob | Notes |
|---|---|---|---|
| ECR | POST + streaming PUT | 2 | `Transfer-Encoding: chunked`, zero buffering |
| Docker Hub | POST + streaming PUT | 2 | Same streaming path as ECR |
| GHCR | POST + single PATCH + PUT | 2 | Multi-PATCH broken (last PATCH overwrites previous chunks). Single PATCH without `Content-Range` headers. Blob size from source manifest descriptor sets `Content-Length` on the streaming body |
| GAR | POST + buffered monolithic PUT | 2 | PATCH errors entirely. Full blob buffered in memory |
| ACR | POST + streaming PUT | 2 | Known ~20 MB body limit; chunked PATCH not yet implemented |

The GHCR fallback is a correctness fix, not an optimization. GHCR's chunked upload implementation is broken: the last PATCH overwrites all previous chunks. Any blob larger than `chunk_size` pushed via multi-PATCH produces silently corrupt data because `CompleteLayerUpload` succeeds but the blob contains only the final chunk. This is a documented conformance gap.

## Frequency ordering

Within each image, blobs are prioritized by descending reference count across all discovered images. A `HashMap<Digest, usize>` tracks blob frequency, updated as discovery resolves new items. Execution consults this map when ordering blob transfers within each image. The overhead is negligible, requiring one hash lookup per blob.

The ordering ensures that the Alpine base layer (referenced by 80 images) is pushed before the nginx config layer (referenced by 5) before the app binary layer (referenced by 1). This is the Nix store's closure-ordered transfer strategy applied to OCI blobs: transfer the most-depended-upon content first, so downstream consumers can begin work sooner.

On crash or interruption, the most-shared blobs have been pushed first, providing maximum mount opportunities for the next run. A crash at 50% completion with frequency ordering leaves a much more useful cache state than a crash at 50% with arbitrary ordering because the shared base layers that benefit the most images are already present at the target.

Index (multi-arch) manifests have a dependency structure: child manifests must be pushed before the index. Discovery resolves the full index tree (pulling the index manifest AND all child manifests) before sending the item to execution. Execution pushes children first (each child's blobs, then child manifest), then the top-level index manifest by tag. This matches OCI spec dependency ordering.

Semver ordering within each mapping means the first images to reach execution are related (`nginx:1.24`, `nginx:1.25`, `nginx:1.26`) and share ~95% of layers. The dedup map converges fast because ordering naturally front-loads shared content. Image-level ordering ("process nginx:1.26 before myapp:latest because nginx shares more layers") is correct but coarse. Blob-level ordering is finer-grained and globally optimal.

## Graceful shutdown

On SIGTERM or SIGINT, the engine stops accepting new work and drains in-flight transfers with a deadline. The default drain deadline is 25 seconds, chosen to fit within Kubernetes' default 30s `terminationGracePeriodSeconds` with 5 seconds of margin. For production deployments with many concurrent transfers, `terminationGracePeriodSeconds: 60` is recommended in the K8s CronJob spec.

### 5-step shutdown sequence

1. **Stop promotion.** Signal received, set `shutting_down = true`, start drain deadline timer. The `if !shutting_down` guard on the discovery `select!` branch prevents new items from entering the pending queue.

2. **Stop discovery.** Discovery futures are no longer polled. Any in-flight discovery requests complete but their results are discarded.

3. **Drain execution.** Active execution futures continue until completion or drain deadline. The `select!` loop runs on `execution_futures.next()` + `sleep_until(drain_deadline)`. The drain deadline branch guards on `!execution_futures.is_empty()` so the engine exits immediately when all work completes rather than waiting for the full deadline. If timeout fires before all futures complete, remaining futures are abandoned and logged.

4. **Persist cache.** `TransferStateCache` is serialized via `tmp + fsync + rename + dir fsync` (~50-100ms). Only `ExistsAtTarget` and `Completed` entries are written.

5. **Exit.** Return `ExitCode` indicating clean drain or partial abandonment.

### Shutdown priorities

Cache persistence is the highest-priority action during shutdown (~50-100ms). Losing accumulated knowledge costs minutes of redundant HEAD checks on the next run. Manifest pushes for images whose blobs are fully transferred are second priority because a single small PUT (<100KB) turns partially-useful work into fully-usable images. In-flight blob transfers finish if time permits.

No DELETE requests are issued for abandoned upload sessions. All registries auto-expire them server-side (ECR: tied to 12h auth token, others: server GC). The OCI Distribution Spec says registries "SHOULD eventually timeout unfinished uploads."

The existing `ShutdownSignal` infrastructure in `crates/ocync-sync/src/shutdown.rs` (`Arc<AtomicBool>` + `Arc<tokio::sync::Notify>`) integrates directly with the pipeline's `select!` loop. `Arc` is used here (rather than `Rc`) because the shutdown signal must be `Send` for signal handler registration via `tokio::spawn`, which is distinct from the engine's `Rc<RefCell<>>` shared state.

## Key invariants

1. **RefCell borrows never held across `.await` points.** Every read/write of the transfer state cache follows: borrow, extract value into local variable, drop borrow, then await. This prevents panics from overlapping borrows. Enforced by code review and a wrapper type that only exposes methods returning owned values.

2. **Source manifests pulled once per tag.** `Rc<PulledManifest>` is shared across all target futures for the same tag. The pipeline preserves pull-once fan-out.

3. **Mount failure is always recoverable.** The transfer state cache may have stale entries. Every mount attempt must fall back to HEAD -> pull+push on failure. The cache entry is invalidated on failure (lazy invalidation). No "mount failed, image failed" paths exist.

4. **Single-target = zero overhead.** No disk staging, no eviction, no claim/wait maps. The majority deployment (single ECR target) pays only the adaptive concurrency overhead.

5. **Source HEAD serves two independent purposes.** (a) Discovery cache population (unconditional): every discovery cycle issues a source HEAD to detect changes via `Docker-Content-Digest`. If the HEAD fails, times out, or returns an unreliable digest, the engine falls through to a full GET. (b) Rate-limit conservation (`head_first`, opt-in): on cache miss, HEAD-checks all targets against the source HEAD digest before performing the full source manifest GET. If every target already holds the matching digest, the expensive GET is skipped entirely, conserving rate-limit tokens on source registries where HEADs are free but GETs count (e.g., Docker Hub). Platform-filtered mappings bypass `head_first` because the target holds a filtered index digest that differs from the unfiltered source HEAD. The two features share a single source HEAD request but are independent, and the discovery HEAD has its own timeout and fallback semantics.

6. **Discovery and execution are independently rate-paced.** Discovery has its own AIMD controller per source registry. Execution has its own per target. The pipeline's pending queue is the only coupling point, providing natural backpressure from the global semaphore.

7. **Target-side verification is always live.** Every run HEAD-checks all target manifests. No persistent cache skips target verification. Source-side manifest digests may be cached (the tag digest cache) to avoid redundant full manifest pulls when the source has not changed, but target HEADs are performed every cycle regardless of cache state.

8. **Index manifests are fully resolved before execution.** Discovery pulls the full index tree (index + all children) before sending to the pending queue. Execution pushes children first, then the index, matching OCI spec dependency ordering.

9. **Error escalation is per-image, not per-target or per-tag.** A blob failure stops that (tag, target) pair, preventing manifest push and recording the image as failed in the sync report. Other targets for the same tag are independent and continue. Other tags are independent and continue. A 429 on one AIMD window does not affect other windows or other registries. The engine always completes the full sync run and reports all failures, never aborts early on a single image failure.

## Failure interaction matrix

Optimizations can amplify each other's failure modes. This matrix captures non-obvious interactions between engine components.

| Trigger | Failure Mode | Mitigation |
|---|---|---|
| ECR token expiry (12h) | Mass 401 across all concurrent futures, thundering herd on GetAuthorizationToken | Auth token refresh lock: first future acquires write lock and refreshes, subsequent futures re-check and find fresh token. Exactly one GetAuthorizationToken call regardless of concurrent waiters |
| Stale cache + mount failure | Retry also fails if source blob GC'd, cascading image failures | On double failure: mark blob as permanently unknown, proceed to full HEAD -> pull -> push. Do not propagate `Failed` status from stale cache to new sync |
| CronJob overlap (`concurrencyPolicy: Allow`) | Two runs read/write cache file simultaneously, second write overwrites first's updates | Document `Forbid` as required. Detect concurrent access via advisory file lock; log warning and skip persistence if locked |
| Disk full during staging | Partial temp file, subsequent reads corrupt | Atomic rename pattern (`.tmp.{random}` -> `{digest}`). On ENOSPC: disable disk staging for remainder of run (circuit breaker), continue with fresh source pulls per target |
| HEAD/GET digest mismatch (Docker Hub) | Discovery HEAD: graceful degradation to always-pull (extra HEAD per cycle, no correctness bug). `head_first`: false skip means target has stale content | Discovery HEAD: unconditional with fallback, no operator action needed. `head_first`: opt-in only, with `ocync_head_skip_total` metric and runtime `ratelimit-remaining` self-verification |
| GAR shared quota + high pipeline concurrency | Increased request rate exhausts shared quota faster than sequential processing | Detect GAR targets (`*-docker.pkg.dev`), cap concurrency at 5 for all operation types via single shared AIMD window |

## Alternatives considered

### Multi-threaded tokio runtime

Rejected. Workload is ~100% network I/O. SHA-256 per-chunk cost (~2.7ms for 4MB via aws-lc-rs at ~1.5 GB/s) is within tokio's 10ms cooperative scheduling budget. `spawn_blocking` handles cache deserialization. Multi-threaded would require `Arc<RwLock<>>` everywhere, introducing lock contention, write starvation risk, and `Send` bound infection, all for a workload that does not benefit from parallelism. If layer recompression (gzip -> zstd) ships, switching is a mechanical refactor: `Rc` -> `Arc`, `RefCell` -> `RwLock`, add `Send` bounds.

### Bounded channel between discovery and execution

Rejected. The primary source (Chainguard) has no rate limits, so discovery completes in ~10-20 seconds. A producer-consumer channel adds backpressure infrastructure, capacity tuning, and async coordination for a phase that is rarely the bottleneck. The `select!` loop with a pending queue is simpler and provides natural backpressure through the global semaphore.

### Upload session pre-allocation

Rejected. ECR `InitiateLayerUpload` at 100 TPS sounds like a bottleneck, but the pipeline naturally staggers execution across discovery time. With 50 concurrent images entering execution over 10+ seconds, actual InitiateLayerUpload rate is ~5/sec, 20x under the limit. Pre-allocation would race with execution in the pipeline model and add a burst of 500 POSTs during discovery that could itself trigger throttling.

### ECR native replication detection

Rejected. The primary use case is external source (Chainguard) -> multi-region ECR. ECR native replication only works for ECR -> ECR copies within the same account. Auto-detecting replication via `DescribeRegistry` adds IAM requirements, a non-OCI API dependency, and implicit target-skipping that silently breaks if replication rules are misconfigured. If needed later, implement as explicit config (`ecr_replicates_to: [ecr-eu]`) rather than auto-detection.

### Throughput-proportional target scheduling

Rejected. Partitioning concurrency permits by measured throughput does not reduce wall-clock sync time because the sync completes when the slowest target finishes, regardless of how many permits the fast target gets. For the primary use case (3 ECR regions with similar latency), there is no throughput disparity to exploit.

### Chunk-level upload resume

Rejected. Medium implementation complexity. Only helps for multi-GB blobs on ECR/Hub/ACR (not GAR/GHCR which do not support chunked uploads). The existing retry (restart pull+push) is sufficient. Revisit when users report large-blob failure issues.

### Full dynamic priority scheduler

Rejected. The budget circuit breaker + AIMD handles rate-limited registries. Full priority scheduling is significant complexity for marginal gains over the pipeline + frequency ordering approach.

### Speculative blob push

Rejected. Start pushing before HEAD confirms blob is missing. Trades bandwidth for latency. The pipeline's staggered execution naturally spreads InitiateLayerUpload load, addressing the same latency concern without wasting bandwidth.

### Transfer journal / WAL

Rejected. True crash resume with exactly-once semantics. Valuable for long-running syncs (hours), but the CronJob model (5-minute frequent syncs + persistent cache) handles crash recovery adequately. Revisit for bulk initial sync use case.

### Adaptive chunk sizing

Rejected. Dynamic chunk size based on throughput. Marginal gains; the monolithic threshold for small blobs and registry-specific upload fallbacks address the concrete upload strategy needs.

### GTID digest map (persistent target-level skip cache)

Rejected. Persistent `(source_repo, tag, target) -> digest` map that skips target verification for unchanged images. Rejected because target-side staleness is a correctness bug, since ECR lifecycle policies can delete images between runs, and skipping target verification would silently fail to repair the gap. The tag digest cache (see watch-discovery design) caches source-side state only, skipping redundant source manifest pulls when the source digest is unchanged, but target HEADs are performed every cycle.

### Broadcast channel for multi-target streaming

Rejected. In-memory `tokio::sync::broadcast` on top of disk staging to let fast targets read chunks without disk I/O. ECR push throughput (24-28 MB/s) is 5-25x slower than page cache reads (4+ GB/s), so the "disk penalty" is fictional for recently-written files. Broadcast receivers lag within seconds (source at 100-200 MB/s, targets at 24-28 MB/s) and fall back to disk anyway. Wall-clock savings ~0.8s per 100MB blob (the fsync time), negligible vs 4s push time. ~300 lines of lag detection, fallback, lifecycle management for <1s savings. Disk staging + OS page cache provides equivalent performance.

### ECR batch API cold start

Rejected. `BatchCheckLayerAvailability` (100 digests/call at 1,000 TPS) to bulk-populate the blob cache on first run. Per-repository scope means 180+ calls for 15 repos x 3 targets, not the "1-2 calls" initially estimated. Requires blocking until all discovery completes (10-20s for Chainguard), destroying the pipeline's overlap advantage. Progressive cache population with frequency ordering handles cold start in ~5 seconds of interleaved HEAD checks. The persistent warm cache prevents true cold start from recurring after the first run. `BatchGetImage` has content negotiation gaps (no index/manifest list types in `acceptedMediaTypes`).

### Prior art

No existing OCI transfer tool implements the combination of pipelining, adaptive per-action concurrency, disk-staged blob reuse, and persistent blob-level caching described here. The closest prior art and what was learned from each:

| Tool | Relevant Technique | What We Learned |
|---|---|---|
| skopeo | Persistent `BlobInfoCache` in BoltDB | Validates persistent cache tier. Only tool with cross-run blob knowledge. No cross-image dedup within a run, no pipelining |
| Nydus | Persistent content-addressable blob cache | Second validation of persistent cache, more sophisticated than skopeo. Confirms content-addressable staging is a proven pattern |
| `regsync` | Proactive `reqPerSec` pacing, `ratelimit-remaining` header parsing | Validates rate pacing. Nobody does adaptive/AIMD. `regsync` pauses proactively but does not adapt window size |
| containerd | Mandatory local content-addressable store, `StatusTracker` for cross-image push dedup | Validates local caching model and within-run dedup. Our approach is selective (multi-target only), not mandatory |
| crane | `MultiWrite` with in-memory `sync.Map` cross-image blob dedup | Validates within-run dedup. No persistence, no pipelining, no adaptive concurrency |
| Buildkit | Intra-image layer push pipelining | Only tool with any form of pipelining, but within a single image, not across images. Confirms pipelining is valuable even at the single-image scale |
| Harbor | Open issue #18094 for rate-limit problems | Confirms the problem is real and unsolved in the ecosystem. 10 concurrent workers with no coordination |
| regclient | In-memory blob descriptor cache, manifest rewriting, streaming body handling | Validates streaming manifest modification and in-session dedup. POST + PUT upload strategy (2 requests/blob) matches our approach |
| distribution/distribution | Proxy mode with conditional GET (`If-None-Match`) | Validates the conditional-fetch pattern, analogous to our source HEAD for discovery cache population |

## Future directions

> **Status: Planned.** The DAG scheduler and `SyncEngine` trait described below are future directions, not current architecture.

### DAG scheduler evolution

The pipeline is the pragmatic v1 architecture. The natural evolution is a dependency-graph scheduler where every unit of work (check blob, transfer blob, push manifest) is a node with dependency edges, and a global scheduler handles ordering, concurrency, and priority. Optimizations like pipelining, frequency ordering, dedup, and work-stealing become emergent properties of the scheduler rather than bolt-on features.

The `SyncEngine` trait is structured so the pipeline can be replaced with a DAG scheduler in a future version without changing the `RegistryClient` layer or CLI. The current `VecDeque<TransferTask>` pending queue, `FuturesUnordered` pools, and `select!` loop map naturally to the DAG scheduler's internal state, and the refactor would add edges and topological ordering without changing the external contract.

### io-uring via tokio-uring

The disk staging and blob transfer paths will migrate to `tokio-uring` for native async I/O on Linux. This replaces `spawn_blocking`-based file operations with io_uring submission queue entries and enables kernel-level splice/zero-copy paths (socket -> file -> socket without userspace buffer copies).

Architectural compatibility with the current design is strong: `tokio-uring` runs on `current_thread` (same runtime model, `Rc<RefCell<>>` works unchanged), owned buffers are already the pattern in async move blocks (io_uring requires owned buffers for registered I/O), and the `RegistryClient` HTTP layer (reqwest) stays on standard tokio I/O initially. io_uring benefits apply to disk staging first.

The v1 design avoids choices that prevent io-uring adoption: no `Arc<Mutex<>>` on I/O buffers, no assumptions about `spawn_blocking` availability in hot paths. The disk staging interface abstracts over the I/O backend so the swap is mechanical. macOS development uses standard tokio file I/O as a fallback via compile-time feature gate.
