# Transfer Optimization Design

---

## Summary

Replace the strict 3-phase engine with a pipelined architecture that interleaves discovery and execution, backed by a two-tier blob-level transfer state cache, disk-staged blob reuse for multi-target scenarios, and adaptive per-action concurrency. The engine should never be idle when useful work exists.

Primary use case: Chainguard / Minimus source → multi-region ECR targets, hundreds of images, heavy layer sharing.

### Scope

This spec covers six deliverables:

1. Pipeline engine replacing the 3-phase discover/plan/execute model
2. Two-tier blob-level cache (in-memory hot + persistent warm) with lazy invalidation
3. Adaptive per-action AIMD concurrency control per registry
4. Disk staging for pull-once push-N multi-target blob reuse
5. Upload strategy fixes (GHCR correctness, monolithic for small blobs)
6. Graceful shutdown with cache persistence

### Design priorities (in order)

1. Performance and throughput - every byte crosses the network as few times as possible
2. API efficiency - every operation teaches; knowledge reduces future API calls
3. Zero overhead for the common case - single-target pays no disk, memory, or complexity penalty

---

## Design Principles

1. Never idle when work exists. Rate limits on one API action must not stall progress on others. If PutImage (10 TPS) is saturated, blob uploads keep filling the pipeline.

2. Every operation teaches. Knowledge gained from each transfer (which blobs exist where, which repos have which layers) reduces future API calls. The transfer state cache is the single source of truth, progressively populated, never redundantly queried.

3. Each byte crosses the network once. Source blobs are pulled once per sync run regardless of target count. Cross-repo mounts replace pushes when possible. Disk staging ensures multi-target never means multi-pull.

4. Zero overhead for the common case. Single-target deployments pay no disk penalty, no staging overhead, no eviction logic. Optimizations activate only when their prerequisites are met.

5. Adapt to registries, don't configure around them. The engine discovers actual registry capacity through feedback (429 responses, rate-limit headers) rather than requiring operators to guess at rate limits and concurrency settings.

---

## Runtime Model

The engine uses `#[tokio::main(flavor = "current_thread")]`. The workload is ~100% network I/O - hundreds of concurrent futures spend >99% of wall-clock time awaiting HTTP responses. A single thread handles this without contention overhead.

CPU-bound operations use `tokio::task::spawn_blocking`:
- Persistent cache deserialization: 8MB binary cache loads in ~30ms. `spawn_blocking` prevents blocking the event loop on startup.
- SHA-256 digest computation: aws-lc-rs processes ~1.5 GB/s. A single `Sha256::update(4MB)` call takes ~2.7ms - well within tokio's 10ms cooperative scheduling budget. Only requires `spawn_blocking` if chunk sizes grow significantly or layer recompression is added.

Shared state uses single-threaded interior mutability:
- `Rc<RefCell<TransferStateCache>>` - two-tier blob-level transfer state cache, read-heavy
- `Arc<RegistryClient>` - already `Arc`-wrapped (shared across futures, not for thread safety)

The `!Send` constraint of `Rc<RefCell<>>` is a feature: it statically prevents accidental use of `tokio::spawn` (which requires `Send`). All concurrency uses `FuturesUnordered` with async move blocks, which work on `current_thread` without `Send` bounds.

If layer recompression (gzip→zstd) ships, switching to multi-threaded is a mechanical refactor: `Rc` → `Arc`, `RefCell` → `RwLock`, add `Send` bounds. This is a 1-day change, not a decision that needs to be made now.

---

## Architecture

### Pipeline Loop

Discovery and execution run concurrently in a single `select!` loop over two `FuturesUnordered` pools with emptiness guards. Execution starts the moment the first image is discovered - no waiting for all discovery to complete.

Each `select!` branch uses an `if !pool.is_empty()` precondition to prevent busy-looping. An empty `FuturesUnordered` returns `Poll::Ready(None)` immediately - without guards, the empty branch wins every poll iteration, spinning the CPU while the other pool has legitimate work awaiting I/O. The `else` arm (both pools empty and no pending items) is the clean termination condition.

```
Config
  │
  ├── Sort tags by repo + semver (free, per mapping)
  │
  v
┌───────────────────────────────────────────────────┐
│ Pipeline Loop (select! over two pools)            │
│                                                   │
│ Discovery Pool (FuturesUnordered):                │
│   Per mapping, per tag (semver order):            │
│     1. Source HEAD or GET → get source digest     │
│     2. HEAD each target → compare digests         │
│        → skip if unchanged                        │
│     3. Source GET (if HEAD-only) → SourceData     │
│     4. Resolve index children (full tree)         │
│     5. Update blob frequency map                  │
│     6. → ActiveItem to pending queue              │
│                                                   │
│ Execution Pool (FuturesUnordered):                │
│   Bounded by global concurrency cap (semaphore).  │
│   Per (tag, target), blobs in frequency order:    │
│     1. Cache check → skip / mount / HEAD          │
│     2. Push: stream (1 target) or stage (N)       │
│     3. Update cache                               │
│     4. Push manifests (children first, then index) │
│     On 429: AIMD halve for (registry, action)     │
│                                                   │
│ Each discovery result may promote pending → exec. │
│ Each execution completion frees a global permit,  │
│ allowing the next pending item to enter execution.│
└─────────────────────┬─────────────────────────────┘
                      │
                      v
            Persist transfer state cache
```

A `VecDeque<ActiveItem>` holds items that discovery has resolved but execution hasn't started. When a discovery future resolves, its `ActiveItem` enters the pending queue and the loop attempts to promote pending items into execution (bounded by semaphore). When an execution future completes, its permit is released and the next pending item is promoted.

### Why a Pipeline, Not Phases

The primary bottleneck for Chainguard → multi-region ECR is the target side: ECR `PutImage` at 10 TPS, `InitiateLayerUpload` at 100 TPS. Discovery from Chainguard (no rate limits) completes in ~10-20 seconds for hundreds of images. The pipeline overlaps this discovery time with early execution:

1. Progressive cache population: Early transfers teach the cache about which blobs exist where. Later images benefit from accumulated knowledge - the more images processed, the fewer API calls needed. In the 3-phase model, all HEAD checks happen before any transfers, so the cache is empty during planning.

2. PutImage pipeline smoothing: As early images complete blob transfers and push manifests (consuming PutImage tokens at 10 TPS), later images' blob transfers proceed unblocked. The pipeline naturally spreads PutImage load across the full sync duration.

3. Immediate execution start: Execution begins after ~1 second of discovery instead of waiting 10-20 seconds for all discovery to complete. For a minutes-long sync, this is a small but free improvement.

For Docker Hub as a secondary source (200 manifest GETs / 6h), the pipeline is even more valuable: discovery can take hours, and execution proceeds on already-discovered images while discovery is rate-limited.

### Discovery Flow

Discovery performs tag-level change detection. Every run HEAD-checks all target manifests - target-side verification is always live. Source-side discovery uses a tag digest cache to avoid redundant full manifest pulls in steady state (see discovery optimization spec). The optimized flow:

1. HEAD source manifest → get `current_source_digest` from `Docker-Content-Digest` header (unconditional, with short timeout and graceful fallback to full GET on failure)
2. Compare `current_source_digest` against cached `source_digest` and `platform_filter_hash` - if match, use cached `filtered_digest` for target comparison without pulling the full manifest tree
3. HEAD each target manifest → compare to `filtered_digest` (cached or freshly computed)
4. If all targets match → skip (saved M+1 GETs for multi-arch images)
5. If any target mismatches → GET source manifest (full pull), proceed with transfer for stale targets

For rate-limited source registries (Docker Hub, GHCR), `head_first: true` additionally replaces the authoritative GET with a HEAD when the digest is already cached, conserving rate-limit tokens. This is opt-in per registry because HEAD digest reliability varies - Docker Hub and Quay have historically had bugs where `Docker-Content-Digest` on HEAD differs from the body digest on GET during manifest schema conversion. If the `Docker-Content-Digest` header is missing or malformed on a HEAD response, fall back to GET silently - never error out.

Runtime verification for `head_first`: on the first HEAD response from a source registry, check whether `ratelimit-remaining` header is present. If it appears on HEAD (meaning HEADs are being counted), disable `head_first` for that registry and fall back to GET-only. Self-verifying rather than assumption-dependent.

The discovery HEAD (unconditional, step 1) is independent of `head_first` (opt-in). The discovery HEAD populates the tag digest cache and has its own timeout/fallback semantics. `head_first` conserves rate-limit tokens on the authoritative GET path. Both use the same HTTP method but serve different purposes.

For the primary use case (Chainguard → multi-region ECR, 200+ multi-arch images), the discovery cache reduces steady-state source requests from ~1,200 GETs (200 index + 1,000 child manifest GETs) to ~200 HEADs - a 10x reduction in source-side latency per watch cycle.

### Execution Flow

Semver ordering within each mapping means the first images to reach execution are related (`nginx:1.24`, `nginx:1.25`, `nginx:1.26`) and share ~95% of layers. The dedup map converges fast because ordering naturally front-loads shared content.

Within each image, blobs are prioritized by descending reference count across all discovered images:

- Image-level ordering: "process `nginx:1.26` before `myapp:latest` because nginx shares more layers" - correct but coarse
- Blob-level ordering: "push the Alpine base layer (referenced by 80 images) before the nginx config layer (referenced by 5) before the myapp binary layer (referenced by 1)" - finer-grained, globally optimal

A `HashMap<Digest, usize>` tracks blob frequency, updated as discovery resolves new items. Execution consults this map when ordering blob transfers within each image. The overhead is negligible - one hash lookup per blob.

On crash/interruption: the most-shared blobs have been pushed first, providing maximum mount opportunities for the next run. This is the Nix store's closure-ordered transfer strategy applied to OCI blobs.

Index (multi-arch) manifests have a dependency structure: child manifests must be pushed before the index. Discovery resolves the full index tree - pulling the index manifest AND all child manifests - before sending the item to execution. Execution pushes children first (each child's blobs, then child manifest), then the top-level index manifest by tag.

### Same-Registry Optimization

When source and target are repos on the same registry (e.g., `ecr-us/team-a/nginx` → `ecr-us/team-b/nginx`), blob "transfer" should be a pure mount - zero data crosses the network. The dedup map handles this naturally: after the first image, source blobs are recorded as existing at the source repo on this registry. Subsequent images for different repos on the same registry hit the "known at different repo → mount" path.

The AIMD controller is shared for same-registry source and target (one controller per registry), so rate limit budgets are correctly shared.

### Future Directions

#### DAG Scheduler

The pipeline is the pragmatic v1 architecture. The natural evolution is a dependency-graph scheduler where every unit of work (check blob, transfer blob, push manifest) is a node with dependency edges, and a global scheduler handles ordering, concurrency, and priority. Optimizations like pipelining, ordering, dedup, and work-stealing become emergent properties of the scheduler rather than bolt-on features.

The `SyncEngine` trait should be structured so the pipeline can be replaced with a DAG scheduler in a future version without changing the `RegistryClient` layer or CLI.

#### io-uring via tokio-uring

The disk staging and blob transfer paths will migrate to `tokio-uring` for native async I/O on Linux. This replaces `spawn_blocking`-based file operations with io_uring submission queue entries and enables kernel-level splice/zero-copy paths (socket → file → socket without userspace buffer copies).

Architectural compatibility with the current design:
- `tokio-uring` runs on `current_thread` - same runtime model, `Rc<RefCell<>>` works unchanged
- Owned buffers are already the pattern in async move blocks - io_uring requires owned buffers for registered I/O
- The `RegistryClient` HTTP layer (reqwest) stays on standard tokio I/O initially; io_uring benefits apply to disk staging first

The v1 design must not make choices that prevent io-uring adoption. Specifically: no `Arc<Mutex<>>` on I/O buffers, no assumptions about `spawn_blocking` availability in hot paths, and the disk staging interface should abstract over the I/O backend so the swap is mechanical.

macOS development uses standard tokio file I/O as a fallback (compile-time feature gate or runtime detection).

---

## Component Designs

### 1. Transfer State Cache

Eliminate redundant API discovery calls by caching knowledge from every operation. Two tiers, both blob-level.

#### Tier 1 - Hot Cache (in-memory, within-run)

The existing `BlobDedupMap` behind `Rc<RefCell<>>`, populated by every transfer, mount, and HEAD check within this sync run.

```
1. Check cache → known at this repo?           → skip    (0 API calls)
2. Check cache → known at different repo?       → mount   (1 API call)
3. Unknown → HEAD check at target               → record  (1 API call)
   → exists? record in cache, skip
   → missing? mount from cache source or push              (1-2 API calls)
4. Update cache with result
```

Step 2 is the key optimization: when the cache knows a blob exists at a *different* repo on the same target, skip the HEAD check and go directly to mount. This eliminates the need for a separate "plan phase" that batch-checks all blobs upfront.

Pipelining amplifies this: in plan-then-execute, all HEAD checks happen before any transfers, so the cache is empty during planning. In the pipeline, early transfers populate the cache, and later images benefit from accumulated knowledge. The more images processed, the fewer API calls needed.

Scale example: 100 images, 20 shared base layers, 15 repos on one target. Plan-then-execute: 300 HEAD checks upfront. Pipeline with progressive cache: 20 HEAD checks (first encounter per blob) + 280 direct mounts (zero discovery). Savings: 280 API calls.

#### Tier 2 - Warm Cache (persistent disk, cross-run)

Serialized to disk on sync completion. Loaded on startup for CronJob deployments. Watch mode keeps the hot tier in memory naturally; the warm tier provides crash recovery.

Format: binary (postcard or bincode), not JSON. At typical scale (~50K entries), the file is ~1MB and loads in <5ms. At extreme scale (600K entries), ~8MB with sub-100ms load. Deserialization uses `spawn_blocking` to avoid blocking the async runtime.

Staleness is handled by lazy invalidation. If a mount or push fails for a blob the cache claims exists, invalidate that entry and retry with a fresh HEAD check. Self-healing, zero configuration. Handles external actors deleting blobs, ECR lifecycle policies garbage-collecting images (lifecycle has a 24h SLA - images become expired within 24 hours of meeting expiration criteria; trigger mechanism is undocumented), and registry GC between runs. An optional TTL on persistent entries provides an additional safeguard: `global.cache_ttl: 12h`.

Storage location: next to the config file by default (`{config_dir}/cache/`), configurable via `global.cache_dir`. For K8s: `emptyDir` is sufficient (cache is rebuilt from scratch if lost, just costs extra HEAD checks on the first run).

Cold start: when the warm cache is empty (first run or cache lost), progressive cache population handles discovery - each blob is HEAD-checked on first encounter and cached for subsequent images. With frequency ordering, shared base layers are checked first, and the cache converges fast. At typical scale (5,000 unique blobs, 50 concurrent), cold start adds ~5 seconds of HEAD checks interleaved with blob transfers.

#### Serialization

The persistent store must handle:
- Versioning: a version field in the binary header for forward compatibility. Unknown versions → discard and rebuild.
- TTL: the binary header stores a `written_at: u64` (Unix epoch seconds) timestamp. On load, if `now - written_at > cache_ttl`, discard the entire file and rebuild. Per-entry timestamps are unnecessary - the entire file is written atomically at one point in time, and lazy invalidation handles individual stale entries regardless of age. This saves 8 bytes per entry (0 overhead vs ~400KB at 50K entries).
- Corruption detection: CRC32 checksum appended to the file. Failed check → discard and rebuild.
- Concurrent access: `concurrencyPolicy: Forbid` is required for K8s CronJobs sharing a PVC. Document this as a requirement. For `Allow` or `Replace` policies, the warm cache is undefined (but the engine operates correctly - it simply does more HEAD checks). Detect concurrent access via advisory file lock; log warning and skip persistence if locked.
- `BlobStatus::Failed` and `InProgress` entries are not persisted. Stale errors and incomplete transfers from a previous run should not poison the next run. Only `ExistsAtTarget` and `Completed` represent confirmed server-side state.

#### API Surface

```rust
/// Two-tier transfer state cache (hot + warm, both blob-level).
pub struct TransferStateCache {
    /// Blob-level dedup map (existing BlobDedupMap, serves both tiers).
    dedup: BlobDedupMap,
}

impl TransferStateCache {
    /// Get the current status of a blob at a target.
    pub fn blob_status(&self, target: &str, digest: &Digest) -> Option<&BlobStatus>

    /// Find a different repo at this target that has the blob (for cross-repo mount).
    pub fn blob_mount_source(&self, target: &str, digest: &Digest, current_repo: &str) -> Option<&str>

    /// Record that a blob exists at a target repo (from HEAD check).
    pub fn set_blob_exists(&mut self, target: &str, digest: Digest, repo: String)

    /// Record that a blob transfer is in progress.
    pub fn set_blob_in_progress(&mut self, target: &str, digest: Digest, repo: String)

    /// Record that a blob was successfully transferred.
    pub fn set_blob_completed(&mut self, target: &str, digest: Digest, repo: String)

    /// Record that a blob transfer failed.
    pub fn set_blob_failed(&mut self, target: &str, digest: Digest, error: String)

    /// Invalidate a specific blob entry (called on mount/push failure).
    pub fn invalidate_blob(&mut self, target: &str, digest: &Digest)

    /// Serialize to binary file. Only ExistsAtTarget and Completed entries are persisted.
    pub fn persist(&self, path: &Path) -> Result<(), Error>

    /// Load from binary file. Discards entire file if written_at + max_age < now.
    /// Returns Ok(empty cache) if file is missing, corrupt, or expired - never errors
    /// on cache problems (the engine operates correctly without a warm cache).
    pub fn load(path: &Path, max_age: Duration) -> Self
}
```

Callers wrap in `Rc<RefCell<TransferStateCache>>`. Borrows are never held across `.await` points - extract value, drop borrow, then await.

---

### 2. Disk Staging for Multi-Target

Pull each blob from source once, regardless of target count.

When a mapping has multiple targets (e.g., us-ecr + eu-ecr + ap-ecr), the current engine re-pulls each blob from source for each target because streams are consumed. For a 2GB ML layer across 3 regions: 6GB source bandwidth instead of 2GB.

For multi-target mappings, the source blob is pulled once and written to a content-addressable disk file. All target pushes read from the disk file independently, at their own pace.

```
{cache_dir}/blobs/sha256/{hex_digest}
```

Flow:
1. First target needs blob X → check if disk file exists
2. Missing → pull from source, write to disk file
3. All targets read from the disk file and push to their targets concurrently
4. After all targets complete, file is retained for future runs (evicted by policy)

Write path: `{digest}.tmp.{random}` → `fsync` → `rename` to `{digest}` → `fsync` parent directory. Atomic - incomplete writes are never visible as cache entries. After rename, target readers get page-cached reads at memory speed (4+ GB/s) - the data stays in page cache because reads happen milliseconds after writes.

Crash recovery: on startup, delete all `.tmp.*` files in the cache directory. Named by digest, so orphaned files are identifiable.

Eviction: when cache size exceeds `global.blob_cache_threshold` (default 2GB, `0` disables), evict by reference count - blobs referenced by fewer discovered manifests are evicted first. Eviction runs between sync cycles (after cache persistence, before the next run), never during execution. In-flight transfers are never interrupted by eviction.

Single-target deployments pay zero overhead. No disk writes, no eviction logic. The blob reuse path is only activated when `mapping.targets.len() > 1`.

Disk write throughput is naturally bounded by network download speed. Each blob is streamed from the source registry (ECR push throughput: 24-28 MB/s per stream) to disk - the network is always the bottleneck, not the disk (EBS gp3 baseline: 125 MB/s, burst: 593 MB/s). With 50 concurrent downloads, peak disk write rate is ~1.4 GB/s only if all streams complete their final chunks simultaneously - in practice, streams stagger naturally. No separate disk pacing mechanism is needed. If disk throughput becomes a concern (e.g., constrained I/O environments), the global image semaphore indirectly bounds disk write concurrency.

---

### 3. Adaptive Per-Action Concurrency

Maximize utilization without requiring operators to guess at registry capacity.

#### AIMD Controller

Replace the static client-level semaphore with an adaptive concurrency controller per `(registry, action)` pair. Actions are fine-grained - matching the actual API action granularity of each registry:

```rust
enum OperationType {
    ManifestHead,        // HEAD /manifests/{ref} - free on Docker Hub, counted elsewhere
    ManifestRead,        // GET /manifests/{ref}
    ManifestWrite,       // PUT /manifests/{ref} (ECR: PutImage 10 TPS)
    BlobHead,            // HEAD /blobs/{digest} - target existence checks
    BlobRead,            // GET /blobs/{digest}
    BlobUploadInit,      // POST /blobs/uploads/ (ECR: 100 TPS)
    BlobUploadChunk,     // PATCH /blobs/uploads/{uuid} (ECR: 500 TPS)
    BlobUploadComplete,  // PUT /blobs/uploads/{uuid}?digest= (ECR: 100 TPS)
}
```

A per-registry mapping function groups actions into AIMD windows:

- ECR: each action maps to its own window (8 independent windows). Critical because `InitiateLayerUpload` (100 TPS) and `UploadLayerPart` (500 TPS) have a 5x rate disparity - a 429 on session initiation must not throttle chunk uploads.
- Docker Hub: `ManifestHead` and `BlobHead` share a window (free, no rate limit). `ManifestRead` gets its own (counted against 200/6h pull limit). Other actions share one window.
- GAR: all actions map to a single shared window (capped at 5). GAR uses a shared per-project quota across all operation types.
- Unknown registries: coarse grouping - HEAD operations share one window, reads share another, `BlobUploadInit`/`Chunk`/`Complete` share one, manifest writes get their own.

The mapping function is ~20 lines. The window key is derived from HTTP method + URL pattern, which ocync already tracks in request labels.

Each `(registry, window_key)` pair maintains an independent concurrency window using AIMD (Additive Increase, Multiplicative Decrease) with TCP Reno-style congestion avoidance:

1. Start: `window` = 5.0 (conservative)
2. On success: `window += 1.0 / window` (fractional additive increase)
3. On 429/throttle: halve **once per congestion epoch** (see below)
4. Cap: `max_concurrent` from per-registry config (default 50)

From initial 5.0, the window reaches 50 in ~1,200 successful responses - a gradual ramp that avoids overshoot. At 50ms latency with 5 concurrent requests, this takes ~12 seconds. If the registry throttles at 30 concurrent, the controller oscillates between ~15 and ~30 (the classic AIMD sawtooth), settling to an effective average of ~22.5 concurrent requests.

The fractional increase (`+1/window`) means it takes a full window's worth of successes to increase by 1. This is standard TCP congestion avoidance. A naive `+1 per success` would be slow-start (exponential growth), causing aggressive oscillation.

##### Congestion Epoch

When multiple concurrent futures hit a rate limit simultaneously (e.g., 10 futures all receive 429 from the same ECR `InitiateLayerUpload` burst), each 429 is processed in a separate tokio event loop tick. Without protection, each independently halves the window: `50 → 25 → 12.5 → 6.25 → 3.1 → 1.0` - catastrophic collapse from a single capacity event.

The fix is TCP Reno's congestion epoch: each AIMD window tracks `last_decrease: Instant`. On 429, halve only if `now - last_decrease > epoch_duration` (100ms, approximating one cloud API RTT). Multiple 429s within the same epoch are a single congestion event - the window halves once and subsequent 429s within the epoch are ignored for window adjustment (the requests still retry with backoff).

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

This adds 2 fields per window (`last_decrease: Instant`, `epoch_duration: Duration`) and a single branch in the decrease path. Ten simultaneous 429s cause exactly one halving (50 → 25), not ten (50 → 1).

#### Three-Level Concurrency Control

Concurrency is controlled at three levels that compose naturally:

1. **Global image semaphore** (default: 50): bounds how many `(tag, target)` pairs are in-flight simultaneously, preventing memory explosion. This is the engine-level `max_concurrent_transfers` config.

2. **Per-registry aggregate semaphore** (`max_concurrent` per registry, default: 50): bounds total concurrent HTTP requests to a single registry host across all action types. This is a safety ceiling for connection/memory pressure, not a rate-limit mechanism. With HTTP/2 multiplexing, 100+ concurrent requests share ~6-8 TCP connections - the aggregate cap is conservative. A request must acquire a permit from this semaphore before proceeding to the per-action AIMD check.

3. **Per-(registry, action) AIMD windows**: each action type adapts independently within the aggregate ceiling. A 429 on `InitiateLayerUpload` halves that window only - `UploadLayerPart` continues at its own pace. Each window's effective cap is `min(aimd_window, aggregate_permits_available)`.

The levels compose via dual permit acquisition: every request acquires one permit from the registry aggregate semaphore AND one from the per-action AIMD window. The aggregate semaphore prevents resource exhaustion; the AIMD windows prevent per-action rate-limit storms. Slow actions (PutImage at 10 TPS) release aggregate permits promptly, so fast actions (UploadLayerPart at 500 TPS) are never starved.

#### Budget Circuit Breaker

Parse `ratelimit-remaining` from Docker Hub manifest responses (and `X-RateLimit-Remaining` from GHCR). Single decision point:

- If `remaining < 0.1 * planned_discovery_count`: log warning and pause discovery. Execution continues for already-discovered images.
- Resume when a subsequent response shows budget has refilled.

This is not a dynamic priority scheduler. It is a circuit breaker with one threshold. For registries without rate-limit headers (ECR, GAR, Chainguard): AIMD handles capacity discovery automatically through 429 feedback.

#### Auth Token Refresh

ECR `GetAuthorizationToken` returns 12-hour tokens with proactive refresh at 75% lifetime (9 hours). The existing `EcrAuthProvider` in `ocync-distribution` already handles concurrent token refresh correctly via `RwLock` + double-check locking: the first future to detect expiry acquires the write lock and refreshes, subsequent futures re-check the cache after acquiring the lock and find the fresh token - exactly one `GetAuthorizationToken` call regardless of concurrent waiters. No additional engine-level coordination is needed for the pipeline.

---

### 4. Upload Strategy

Correct broken chunked uploads on GHCR and reduce round-trips for small blobs.

OCI registries vary in chunked upload support. crane and skopeo both avoid multi-PATCH chunked uploads entirely - they send the full blob body in a single PATCH request (no `Content-Range` header). This sidesteps broken chunked implementations but requires buffering the entire blob in memory.

| Registry | Multi-PATCH chunked | Single-PATCH (full body) | POST+PUT monolithic | Notes |
|---|---|---|---|---|
| ECR | Yes | Yes | Yes | |
| Docker Hub | Yes | Yes | Yes | |
| GHCR | Broken - last PATCH overwrites all previous | Yes (1 PATCH only) | Yes | [Documented conformance gap](https://gist.github.com/shizhMSFT/b77f2d7f993268a1bdd45a7462866906) |
| GAR | No (PATCH errors) | No | Yes (buffer + PUT) | Already handled by GAR fallback |
| ACR | Yes | Yes | Yes | |
| Harbor | Yes | Yes | Yes | |
| Quay | Yes | Yes | Yes | |

#### GHCR Fallback (correctness fix)

GHCR's chunked upload implementation is broken: the last PATCH overwrites all previous chunks. Any blob larger than `chunk_size` pushed via multi-PATCH produces silently corrupt data - `CompleteLayerUpload` succeeds but the blob contains only the final chunk.

Detect GHCR targets (`ghcr.io` hostname) and force single-PATCH behavior: stream the entire blob body in one PATCH request without `Content-Range` headers. This matches how crane and skopeo handle all registries universally. Blob size is known from the source manifest descriptor (OCI spec requires the `size` field), so `Content-Length` can be set on the streaming body.

This is analogous to the existing GAR fallback (`*-docker.pkg.dev` detection in `blob_push_stream`), but GHCR accepts PATCH (just not multiple PATCHes), so the fallback streams via a single PATCH rather than buffering for monolithic PUT.

#### Monolithic POST+PUT for Small Blobs

For blobs where the size is known and ≤1MB, use POST+PUT monolithic upload (skip PATCH entirely). Two HTTP requests instead of three.

Value is latency reduction, not TPS relief. The POST (`InitiateLayerUpload`) is still required - monolithic does not eliminate it. The saving is one fewer round-trip per blob (~50-100ms). For 1,000 config blobs (2-10KB each), this saves ~75 seconds of aggregate latency across concurrent transfers.

Threshold is 1MB. Config blobs are universally 2-10KB. Small utility layers (certs, scripts) are typically under 1MB. Memory overhead of buffering at this threshold is negligible (50 concurrent × 1MB = 50MB).

---

## Operational Behavior

### Graceful Shutdown

On SIGTERM/SIGINT, the engine stops accepting new work and drains in-flight transfers with a deadline.

Shutdown sequence:

1. Signal received → set `shutting_down = true`, start drain deadline (25 seconds, leaving 5s buffer from K8s default 30s grace period)
2. `if !shutting_down` guard on the discovery `select!` branch - no new items enter the pending queue
3. Drain execution futures: `select!` on `execution_futures.next()` + `sleep_until(drain_deadline)`. If timeout fires before all futures complete, log warning and break.
4. Persist cache: filter to `ExistsAtTarget` and `Completed` entries only, serialize via `tmp + fsync + rename + dir fsync` (~50-100ms)
5. Return `ExitCode` indicating clean drain or partial abandonment

Shutdown priorities (in order):

1. Save cache state - highest value action (~50-100ms). Losing accumulated knowledge costs minutes of redundant HEAD checks on next run.
2. Complete manifest pushes for images whose blobs are fully transferred - a single small PUT (<100KB) turns partially-useful work into fully-usable images.
3. Let in-flight blob transfers finish if time permits.
4. Do NOT issue DELETE requests for abandoned upload sessions - all registries auto-expire them server-side (ECR: tied to 12h auth token, others: server GC). The OCI Distribution Spec says registries "SHOULD eventually timeout unfinished uploads."

Recommend `terminationGracePeriodSeconds: 60` in K8s CronJob specs (default 30s is tight with 50 concurrent transfers). Document this in the deployment guide.

The existing `ShutdownSignal` infrastructure in `src/cli/shutdown.rs` (`Arc<AtomicBool>` + `Arc<Notify>`) integrates directly with the pipeline's `select!` loop.

### Key Invariants

1. `RefCell` borrows never held across `.await` points. Every read/write of the transfer state cache follows: borrow → extract value into local variable → drop borrow → then await. This prevents panics from overlapping borrows. Enforce by code review and a wrapper type that only exposes methods returning owned values.

2. Source manifests pulled once per tag. `Rc<SourceData>` is shared across all target futures for the same tag. The pipeline preserves pull-once fan-out.

3. Mount failure is always recoverable. The transfer state cache may have stale entries. Every mount attempt must fall back to HEAD → pull+push on failure. The cache entry is invalidated on failure (lazy invalidation). No "mount failed, image failed" paths.

4. Single-target = zero overhead. No disk staging, no eviction. The majority deployment (single ECR target) pays only the adaptive concurrency overhead (~negligible).

5. Source HEAD serves two independent purposes with different defaults. (a) **Discovery cache population** (unconditional): every discovery cycle issues a source HEAD to detect changes via `Docker-Content-Digest`. If the HEAD fails, times out, or returns an unreliable digest, the engine falls through to a full GET - the HEAD is an optimization, not a requirement. This is safe to run unconditionally because failure is always recoverable. (b) **Rate-limit conservation** (`head_first`, opt-in): for rate-limited source registries where HEADs are free but GETs consume tokens, `head_first: true` replaces the authoritative GET with a HEAD when the digest is already cached. This remains opt-in because HEAD digest reliability varies across registries, and incorrect digests on the rate-limit path would cause silent skips. The discovery HEAD and `head_first` are independent features that happen to use the same HTTP method - the discovery HEAD has its own timeout (`discovery_head_timeout`) and fallback semantics.

6. Discovery and execution are independently rate-paced. Discovery has its own AIMD controller per source registry. Execution has its own per target. The pipeline's pending queue is the only coupling point - natural backpressure from the global semaphore.

7. Target-side verification is always live. Every run HEAD-checks all target manifests - no persistent cache skips target verification. Source-side manifest digests may be cached (the tag digest cache) to avoid redundant full manifest pulls when the source hasn't changed, but target HEADs are performed every cycle regardless of cache state. Target-side staleness from lifecycle policies, external pushes, or GC is always detected. See the watch mode and discovery optimization spec for the tag digest cache design.

8. Index manifests are fully resolved before execution. Discovery pulls the full index tree (index + all children) before sending to the pending queue. Execution pushes children first, then the index - matching OCI spec dependency ordering.

9. Error escalation is per-image, not per-target or per-tag. A blob failure stops that (tag, target) pair - no manifest push, image recorded as failed in the sync report. Other targets for the same tag are independent and continue. Other tags are independent and continue. A 429 on one AIMD window does not affect other windows or other registries. The engine always completes the full sync run and reports all failures, never aborts early on a single image failure.

### Failure Interaction Matrix

Optimizations can amplify each other's failure modes. This matrix captures non-obvious interactions.

| Trigger | Affected Component | Failure Mode | Mitigation |
|---|---|---|---|
| ECR token expiry (12h) | All concurrent futures | Mass 401 → thundering herd on GetAuthorizationToken | Auth token refresh lock |
| Persistent cache stale + mount failure | Lazy invalidation retry | Retry also fails if source blob GC'd → cascading image failures | On double failure: mark blob as permanently unknown, proceed to full HEAD→pull→push. Do not propagate `Failed` status from stale cache to new sync. |
| CronJob overlap (`concurrencyPolicy: Allow`) | Warm cache file | Two runs read/write simultaneously → second write overwrites first's updates | Document `Forbid` as required. Detect concurrent access via advisory file lock; log warning and skip persistence if locked. |
| Disk full during blob staging | Disk staging | Partial temp file → subsequent reads corrupt | Atomic rename pattern (`.tmp.{random}` → `{digest}`). On ENOSPC: disable disk staging for remainder of run (circuit breaker), continue with fresh source pulls per target. |
| HEAD/GET digest mismatch (Docker Hub) | Discovery HEAD and `head_first` | Discovery HEAD: graceful degradation to always-pull (extra HEAD per cycle, no correctness bug). `head_first`: false skip → target has stale content. | Discovery HEAD: unconditional with fallback, no operator action needed. `head_first`: opt-in only, `ocync_head_skip_total` metric, runtime `ratelimit-remaining` self-verification. |
| GAR shared quota + high pipeline concurrency | All GAR operations | Increased request rate exhausts shared quota faster than sequential | Detect GAR targets, cap concurrency at 5 for all operation types. |

### Observability

Every optimization must be debuggable in production. Required metrics:

| Metric | Type | Purpose |
|---|---|---|
| `ocync_unchanged_skip_total` | Counter | Images skipped (source == target digest) |
| `ocync_head_skip_total` | Counter | Source GETs avoided by HEAD optimization (zero tokens) |
| `ocync_cache_hit_total{tier=hot\|warm}` | Counter | Cache hits by tier |
| `ocync_cache_invalidation_total` | Counter | Lazy invalidation events (stale data detected) |
| `ocync_mount_total{result=success\|fallback}` | Counter | Mount success vs fallback to push |
| `ocync_disk_staging_total` | Counter | Blobs written to disk staging |
| `ocync_concurrency_window{registry,action}` | Gauge | Current AIMD window size per (registry, action) |

---

## Configuration

```yaml
global:
  max_concurrent_transfers: 50       # global image-level semaphore
  cache_dir: .ocync/cache            # persistent cache + blob staging
  cache_ttl: 12h                     # warm tier TTL (0 = no TTL, lazy only)
  blob_cache_threshold: 2GB          # disk staging limit (0 = disabled)

registries:
  chainguard:
    url: cgr.dev
    max_concurrent: 50               # AIMD upper bound
  dockerhub:
    url: docker.io
    head_first: true                 # enable source HEAD optimization
  ecr-us:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
    max_concurrent: 100              # ECR can handle more
  ecr-eu:
    url: 123456789012.dkr.ecr.eu-west-1.amazonaws.com
    max_concurrent: 100

mappings:
  - from: cgr.dev/chainguard/nginx
    to:
      - registry: ecr-us
        repository: mirror/nginx
      - registry: ecr-eu
        repository: mirror/nginx
```

All fields are optional. Defaults are tuned for the primary use case (Chainguard → multi-region ECR). An empty config with just `mappings` works.

---

## Appendices

### A. Registry Rate Limit Reference

#### ECR (per-account, per-region, token bucket model)

All adjustable via AWS Service Quotas. ECR does not return `Retry-After` headers on 429 - returns `ThrottlingException`. Exponential backoff with jitter is the correct strategy.

| API Action | Default Rate | Used For |
|---|---|---|
| GetDownloadUrlForLayer | 3,000 TPS | Blob pull |
| BatchGetImage | 2,000 TPS | Manifest bulk fetch (100/call) |
| BatchCheckLayerAvailability | 1,000 TPS | Blob existence (100/call) |
| UploadLayerPart | 500 TPS | Blob push chunks |
| GetAuthorizationToken | 500 TPS | Auth |
| InitiateLayerUpload | 100 TPS | Upload session start |
| CompleteLayerUpload | 100 TPS | Upload session finish |
| PutImage | 10 TPS | Manifest push (the bottleneck) |

The 50:1 ratio between UploadLayerPart (500 TPS) and PutImage (10 TPS) means blob uploads can saturate while manifest pushes trickle. For 50 multi-arch images (550 PutImage calls), the minimum is 55 seconds at 10 TPS per region.

ECR upload sessions are tied to the auth token (12-hour expiry). No documented session timeout beyond that.

#### Chainguard

No rate limits on any operation type. No rate-limit headers in responses. The primary bottleneck is always the target side.

#### Docker Hub

- Manifest GET: 200/6h (authenticated), 100/6h (anonymous)
- Manifest HEAD: free (does not count toward pull limit) - runtime-verify this by checking if `ratelimit-remaining` header appears on HEAD responses
- Blob GET: unlimited (served through CloudFlare CDN)
- Returns `ratelimit-limit` and `ratelimit-remaining` headers on manifest responses

#### Other Registries

| Registry | Manifest limit | Blob limit | Independent? | Rate headers? |
|---|---|---|---|---|
| GAR | Per-project shared quota | Same quota | No - shared | No |
| GHCR | GitHub API rate limit | Separate | Yes | `X-RateLimit-*` |
| ACR | Per-registry | Per-registry | Unclear | No |
| Quay.io | Per-org | Per-org | Unclear | No |

GAR uses a shared quota across all operation types. The engine detects GAR targets (`*-docker.pkg.dev`) and caps concurrency at 5 for all operation types to avoid quota exhaustion.

### B. Registry Behavior Matrix

| Optimization | Chainguard (source) | Docker Hub (source) | ECR (target) | GAR | GHCR |
|---|---|---|---|---|---|
| Tag-level HEAD check | Always | Always | Always | Always | Always |
| Source HEAD | Near-zero (no rate limits) | High (free, saves tokens) | Near-zero (no pull limit) | Low | Medium |
| Transfer state cache | N/A (source only) | N/A (source only) | High (cross-repo mount) | High | High |
| Persistent cache | N/A | N/A | High (lifecycle + lazy invalidation) | Medium | Medium |
| Blob reuse (disk staging) | N/A (source only) | N/A (source only) | High (multi-region) | Low | Low |
| Adaptive concurrency | N/A (no rate limits) | N/A (source only) | High (varied TPS per operation) | Medium (shared quota) | Medium |
| Budget circuit breaker | N/A (no rate limits) | High (200/6h, headers) | N/A (no headers, use AIMD) | N/A | Medium |
| GHCR single-PATCH fallback | N/A | N/A | N/A | N/A | Required (correctness) |
| Monolithic small blobs | N/A | N/A | Medium (latency) | Medium | Medium |
| Pipelining benefit | Low (discovery is fast) | High (rate-limited discovery) | High (PutImage 50:1 ratio) | Low (shared quota) | Medium |

### C. Prior Art

No existing OCI transfer tool implements the combination of pipelining, adaptive per-action concurrency, disk-staged blob reuse, and persistent blob-level caching described here. The closest prior art:

| Tool | Relevant Technique | What We Learned |
|---|---|---|
| skopeo | Persistent `BlobInfoCache` in BoltDB | Validates persistent cache tier. Only tool with cross-run blob knowledge. |
| Nydus | Persistent content-addressable blob cache | Second validation of persistent cache, more sophisticated than skopeo. |
| regsync | Proactive `reqPerSec` pacing | Validates rate pacing. Nobody does adaptive/AIMD. |
| containerd | Mandatory local content-addressable store | Validates local caching model. Our approach is selective (multi-target only), not mandatory. |
| crane | `MultiWrite` in-memory cross-image blob dedup | Validates within-run dedup. No persistence, no pipelining. |
| Buildkit | Intra-image layer push pipelining | Only tool with any form of pipelining, but within a single image, not across images. |
| Harbor | Open issue #18094 for rate-limit problems | Confirms the problem is real and unsolved. 10 concurrent workers with no coordination. |
| regclient | In-memory blob descriptor cache, manifest rewriting | Validates streaming manifest modification and in-session dedup. |
| distribution/distribution | Proxy mode with conditional GET (`If-None-Match`) | Validates the conditional-fetch pattern (analogous to our source HEAD). |

### D. Relationship to Implementation Plan

This design updates the engine architecture described in the v1 implementation plan (`docs/superpowers/plans/2026-04-12-ocync-v1-implementation.md`). Specifically:

- Runtime stays `current_thread`. `Rc<RefCell<BlobDedupMap>>` evolves to `Rc<RefCell<TransferStateCache>>`. No `Arc<RwLock<>>` migration.
- The 3-phase discover/plan/execute model is replaced by the 2-stage pipeline loop. `FuturesUnordered` is retained (not `JoinSet` - no multi-threaded requirement). The owned-data-in-futures approach remains. What changes is the control flow: discovery and execution overlap in a `select!` loop instead of running sequentially. The plan phase is eliminated - progressive cache population replaces upfront batch HEAD checks.
- The adaptive per-action AIMD controller replaces both the static client semaphore and the planned `governor` token buckets. AIMD discovers actual registry capacity through 429 feedback - `governor` would require operators to configure known rates, contradicting design principle 5. The `max_concurrent` per-registry config provides a hard cap when needed.

The implementation plan should be updated to reflect this design before work begins.

### E. Considered and Not Adopted

| Considered | Why Not |
|---|---|
| Multi-threaded tokio runtime | Workload is ~100% network I/O. SHA-256 per-chunk cost (~2.7ms) is within tokio's 10ms cooperative budget. `spawn_blocking` handles cache deserialization. Multi-threaded would require `Arc<RwLock<>>` everywhere, introducing lock contention, write starvation risk, and `Send` bound infection - all for a workload that doesn't benefit from parallelism. Revisit only if layer recompression (gzip→zstd) ships. |
| Bounded channel between discovery and execution | The primary source (Chainguard) has no rate limits, so discovery completes in ~10-20 seconds. A producer-consumer channel adds backpressure infrastructure, capacity tuning, and async coordination for a phase that's rarely the bottleneck. The `select!` loop with a pending queue is simpler and provides natural backpressure through the global semaphore. |
| Upload session pre-allocation | ECR `InitiateLayerUpload` at 100 TPS sounds like a bottleneck, but the pipeline naturally staggers execution across discovery time. With 50 concurrent images entering execution over 10+ seconds, actual InitiateLayerUpload rate is ~5/sec - 20x under the limit. Pre-allocation would race with execution in the pipeline model and add a burst of 500 POSTs during discovery that could itself trigger throttling. |
| ECR native replication detection | The primary use case is external source (Chainguard) → multi-region ECR. ECR native replication only works for ECR→ECR copies within the same account. Auto-detecting replication via `DescribeRegistry` adds IAM requirements, a non-OCI API dependency, and implicit target-skipping that silently breaks if replication rules are misconfigured. If needed later, implement as explicit config (`ecr_replicates_to: [ecr-eu]`) rather than auto-detection. |
| Throughput-proportional target scheduling | Partitioning concurrency permits by measured throughput doesn't reduce wall-clock sync time - the sync completes when the slowest target finishes, regardless of how many permits the fast target gets. For the primary use case (3 ECR regions with similar latency), there's no throughput disparity to exploit. |
| Chunk-level upload resume | Medium implementation complexity. Only helps for multi-GB blobs on ECR/Hub/ACR (not GAR/GHCR which don't support chunked uploads). The existing retry (restart pull+push) is sufficient. Revisit when users report large-blob failure issues. |
| Full dynamic priority scheduler | The budget circuit breaker + AIMD handles rate-limited registries. Full priority scheduling is significant complexity for marginal gains over the pipeline + frequency ordering approach. |
| Speculative blob push | Start pushing before HEAD confirms blob is missing. Trades bandwidth for latency. The pipeline's staggered execution naturally spreads InitiateLayerUpload load, addressing the same latency concern without wasting bandwidth. |
| Windowed pipeline / frequency-weighted image ordering | Repo+semver sorting + blob-level frequency ordering captures the majority of dedup locality. Buffering and re-sorting images adds batch boundaries, window sizing, and re-sorting complexity for marginal gains. |
| Transfer journal / WAL | True crash resume with exactly-once semantics. Valuable for long-running syncs (hours), but the CronJob model (5-minute frequent syncs + persistent cache) handles crash recovery adequately. Revisit for bulk initial sync use case. |
| Adaptive chunk sizing | Dynamic chunk size based on throughput. Marginal gains - the monolithic threshold for small blobs and GHCR single-PATCH fallback (Upload Strategy section) address the concrete upload strategy needs. |
| GTID digest map (persistent target-level skip cache) | Persistent `(source_repo, tag, target) → digest` map that **skips target verification** for unchanged images. Rejected because target-side staleness is a correctness bug - ECR lifecycle policies can delete images between runs, and skipping target verification would silently fail to repair the gap. **Revised**: the tag digest cache (see discovery optimization spec) caches source-side state only - it skips redundant source manifest pulls when the source digest is unchanged, but target HEADs are performed every cycle. This addresses concern (1) (target verification preserved), concern (3) (the discovery cache saves M+1 GETs per multi-arch tag, far more than `head_first`'s single GET), and concern (4) (source GETs are not free for multi-arch images: 200 images × 5 platforms × 60ms = ~60s of source latency per cycle even without rate limits). The original cost analysis underestimated source pull cost by counting only the index GET, not the M child manifest GETs required for multi-arch resolution. |
| Broadcast channel for multi-target streaming | In-memory `tokio::sync::broadcast` on top of disk staging to let fast targets read chunks without disk I/O. Dropped because: (1) ECR push throughput (24-28 MB/s) is 5-25x slower than page cache reads (4+ GB/s) - the "disk penalty" is fictional for recently-written files; (2) broadcast receivers lag within seconds (source at 100-200 MB/s, targets at 24-28 MB/s) and fall back to disk anyway; (3) wall-clock savings ~0.8s per 100MB blob (the fsync time), negligible vs 4s push time; (4) ~300 lines of lag detection, fallback, lifecycle management for <1s savings. Disk staging + OS page cache provides equivalent performance. |
| ECR batch API cold start | `BatchCheckLayerAvailability` (100 digests/call at 1,000 TPS) to bulk-populate the blob cache on first run. Dropped because: (1) per-repository scope means 180+ calls for 15 repos × 3 targets, not the "1-2 calls" initially estimated; (2) requires blocking until all discovery completes (10-20s for Chainguard), destroying the pipeline's overlap advantage; (3) progressive cache population with frequency ordering handles cold start in ~5 seconds of interleaved HEAD checks; (4) the persistent warm cache prevents true cold start from recurring after the first run; (5) `BatchGetImage` has content negotiation gaps (no index/manifest list types in `acceptedMediaTypes`). |
