# Design

ocync is a Rust-based OCI registry sync tool that copies container images between registries. It is designed around a single insight: the fastest sync is the one that transfers the fewest bytes and issues the fewest API calls. Wall-clock speed is a consequence of efficiency, not a goal separate from it.

This document explains how ocync works, why each design choice was made, and what the measurable impact is.

## Design philosophy

ocync optimizes along four axes, in strict priority order:

1. **Efficiency** - bytes transferred and API calls issued. Every blob we avoid pulling, every HEAD we avoid issuing, is the real win.
2. **Correctness** - staleness handling, auth invalidation, protocol conformance. Efficiency optimizations must degrade safely, not silently.
3. **Wall-clock speed** - a consequence of (1), not a goal separate from it. Reports that prioritize wall-clock without byte/request counts are misleading.
4. **UX** - clear errors, structured output, sensible defaults. Must be zero-cost when disabled so it never drags on (1).

When these conflict, the higher-priority axis wins. A mount that saves bytes but costs an extra API call is worth it. A cache that saves API calls but risks correctness is not.

## Runtime model

ocync runs on a single-threaded tokio runtime (`current_thread`). The workload is ~100% network I/O - hundreds of concurrent futures spend >99% of wall-clock time awaiting HTTP responses from registries. A single OS thread handles all of them without context-switch overhead, lock contention, or `Send`/`Sync` bound infection.

Shared mutable state uses `Rc<RefCell<>>` instead of `Arc<Mutex<>>`. This is a deliberate choice:

- **Zero synchronization overhead.** `RefCell` is a compile-time borrow check with a runtime flag. `Mutex` is a kernel-mediated lock. For a single-threaded program, the mutex is pure waste.
- **Static prevention of threading bugs.** `Rc` is `!Send`, which means the compiler rejects any attempt to use `tokio::spawn` (which requires `Send`). All concurrency uses `FuturesUnordered` with async move blocks, which run cooperatively on the current thread. You cannot accidentally introduce a data race.
- **Kubernetes resource alignment.** A single-threaded process maps directly to `resources.requests.cpu: 100m` in Kubernetes. There is no multi-core scaling to tune, no thread pool to size, and no gap between requested and utilized CPU. The process uses exactly what it asks for.

The one CPU-bound operation - SHA-256 digest computation - processes ~1.5 GB/s via aws-lc-rs. A 4 MB chunk takes ~2.7 ms, well within tokio's 10 ms cooperative scheduling budget. Cache deserialization (up to 8 MB) uses `spawn_blocking` to avoid blocking the event loop on startup.

If layer recompression (gzip to zstd) ships, the migration to multi-threaded is mechanical: `Rc` to `Arc`, `RefCell` to `RwLock`, add `Send` bounds. This is a one-day refactor, not a decision that needs to be made now.

## Pipeline architecture

### Why pipelining matters

The naive approach to sync is three sequential phases: discover all images, plan all transfers, execute all transfers. This wastes time in two ways:

1. **Discovery latency is hidden.** For Chainguard (no rate limits), discovery of 200+ multi-arch images takes ~10-20 seconds. For Docker Hub (200 manifest GETs per 6 hours), discovery can take hours. In plan-then-execute, execution waits for all of this.
2. **The cache starts empty.** In plan-then-execute, all blob HEAD checks happen before any transfers, so the transfer state cache is empty during planning. Shared base layers are checked N times instead of once.

ocync uses a pipelined architecture where discovery and execution overlap:

```
                     +-------------------------------------------------+
                     | Pipeline loop (select! over two pools)          |
                     |                                                 |
                     | Discovery pool (FuturesUnordered):              |
                     |   Source manifest pull + target HEAD per tag    |
                     |            |                                    |
                     |            v                                    |
                     |   Pending queue (VecDeque<ActiveItem>)          |
                     |            |                                    |
                     |            v                                    |
                     | Execution pool (FuturesUnordered):              |
                     |   Blob transfer + manifest push per (tag,target)|
                     +-------------------------------------------------+
```

The `select!` loop uses `biased;` to prefer execution completions (freeing semaphore permits for pending items) over new discovery results. Emptiness guards (`if !pool.is_empty()`) on every branch prevent busy-looping - an empty `FuturesUnordered` returns `Poll::Ready(None)` immediately, which without guards would spin the CPU while the other pool has legitimate work.

Execution begins after ~1 second of discovery instead of waiting 10-20 seconds. For Docker Hub as a source, execution proceeds on already-discovered images while discovery is rate-limited over hours.

### Progressive cache population

Early transfers teach the cache about blob locations. Later images benefit from accumulated knowledge. Consider 100 images sharing 20 base layers across 15 repos on one target:

- **Plan-then-execute:** 300 HEAD checks upfront (all blobs unknown)
- **Pipeline:** 20 HEAD checks (first encounter per blob) + 280 direct mounts from cache (zero discovery)

The pipeline saves 280 API calls because each transfer populates the cache for subsequent images.

### Frequency ordering

Within each image, blobs are transferred in descending order of reference count across all discovered images. The Alpine base layer (referenced by 80 images) is pushed before the nginx config layer (referenced by 5) before the app binary layer (referenced by 1).

This is the Nix store's closure-ordered transfer strategy applied to OCI blobs. On crash or interruption, the most-shared blobs have been pushed first, providing maximum mount opportunities for the next run.

## Adaptive concurrency (AIMD)

Static concurrency limits force operators to guess at registry capacity. Too low wastes throughput. Too high triggers rate-limit storms. ocync discovers actual capacity through feedback.

### Per-(registry, action) windows

Each registry action type gets its own independent concurrency window using AIMD (Additive Increase, Multiplicative Decrease):

- **On success:** `window += 1.0 / window` (fractional increase - one full window of successes to grow by 1)
- **On 429:** `window /= 2` (multiplicative decrease - but only once per congestion epoch)
- **Initial window:** 5.0 (conservative start)
- **Cap:** `max_concurrent` per registry config (default 50)

From initial 5.0, the window reaches 50 in ~1,200 successful responses. If the registry throttles at 30 concurrent, the controller oscillates between ~15 and ~30 (the classic AIMD sawtooth), settling to an effective average of ~22.5.

### Why per-action, not per-host

ECR rate limits vary dramatically by API action:

| ECR action | Rate limit |
|---|---|
| `UploadLayerPart` (blob chunks) | 500 TPS |
| `InitiateLayerUpload` (session start) | 100 TPS |
| `CompleteLayerUpload` (session finish) | 100 TPS |
| `PutImage` (manifest push) | 10 TPS |

A 429 on `PutImage` (10 TPS) must not throttle `UploadLayerPart` (500 TPS). With a single per-host window, one slow action starves all fast actions. With per-action windows, each adapts independently.

The window grouping is registry-specific:

| Registry | Windows | Rationale |
|---|---|---|
| ECR | 9 (one per OCI action) | Each action has distinct TPS limits |
| Docker Hub | 3 (HEAD free, manifest-read metered, others shared) | HEAD is unmetered; manifest GETs count against pull quota |
| GAR | 1 (all shared) | Per-project shared quota across all operation types |
| Unknown | 5 (coarse groups) | HEAD, read, upload, manifest-write, tag-list |

### Congestion epochs

When 10 concurrent futures hit a rate limit simultaneously, each 429 arrives in a separate event loop tick. Without protection, each independently halves the window: `50 -> 25 -> 12.5 -> 6.25 -> 3.1 -> 1.0` - catastrophic collapse from a single capacity event.

The fix is TCP Reno's congestion epoch: each window tracks `last_decrease: Instant`. On 429, halve only if `now - last_decrease > 100ms` (approximating one cloud API RTT). Ten simultaneous 429s cause exactly one halving (50 -> 25), not ten (50 -> 1). This adds two fields per window and a single branch in the decrease path.

### Three-level concurrency hierarchy

Concurrency is controlled at three levels that compose via dual permit acquisition:

1. **Global image semaphore** (`max_concurrent_transfers`, default 50) - bounds in-flight `(tag, target)` pairs. Prevents memory explosion regardless of how many registries are configured.
2. **Per-registry aggregate semaphore** (`max_concurrent` per registry, default 50) - bounds total HTTP requests to a single host. Safety ceiling for connection and memory pressure.
3. **Per-(registry, action) AIMD windows** - each action adapts independently within the aggregate ceiling.

Every HTTP request acquires permits from levels 2 and 3. Slow actions (PutImage at 10 TPS) release aggregate permits promptly, so fast actions (UploadLayerPart at 500 TPS) are never starved.

## Cross-repo blob mounting

When a blob already exists in another repository on the same target registry, OCI registries can "mount" it - creating a reference without transferring any data. Zero bytes over the wire.

### Leader-follower election

Multiple images in a sync run often share base layers. If all images push independently, shared blobs are uploaded N times. ocync uses leader-follower election to ensure shared blobs are uploaded once and mounted everywhere else.

The `elect_leaders()` function uses a greedy set-cover algorithm over shared blob digests:

1. Build a map of `blob_digest -> set of images that need it`
2. Greedily select the image that covers the most uncovered blobs (the leader)
3. All other images sharing those blobs become followers
4. Repeat until all shared blobs are covered

This provably covers every shared blob. There is no "uncovered follower" path - all followers' shared blobs are in the leader's blob union.

### Wave promotion

Followers cannot mount from a leader until the leader's manifest is committed (ECR requires a committed manifest for mount to succeed). The engine uses wave-based promotion:

- **Wave 1:** Leader images execute first. Their blobs are uploaded, manifests committed.
- **Wave 2:** Follower images execute. Shared blobs are mounted from leader repos (zero transfer). Only unique blobs need uploading.

### ECR BLOB_MOUNTING

ECR requires an opt-in account setting (`BLOB_MOUNTING=ENABLED`, launched January 2026) for cross-repo mount to succeed. When enabled, mount POST returns 201 if:

1. The `from` repo has a committed manifest referencing the blob
2. Both repos use identical encryption configuration
3. Same account and same region

Without `BLOB_MOUNTING` enabled (or for standalone blobs without manifests), ECR returns 202 and starts a regular upload session. ocync detects this and falls through to the standard upload path.

### Measured impact

On a 5-image Jupyter corpus (cold sync to ECR):

| Metric | Before (no leader-follower) | After |
|---|---:|---:|
| Requests | 1,049 | **591** (44% fewer) |
| Response bytes | 11.5 GB | **4.9 GB** (57% fewer) |
| Wall clock | 217.9s | **56.8s** (3.8x faster) |
| Mount success | 0/0 | **192/192** (100%) |

The 57% byte reduction comes from cross-image blob dedup (source blobs pulled once via staging) and 100% mount success (followers mount from leaders). The 3.8x wall-clock improvement comes from both the byte reduction and intra-image blob concurrency (6 concurrent blobs per image).

## Transfer state cache

The cache eliminates redundant API calls by recording what is known about blob locations across all targets.

### Two-tier design

**Tier 1 - hot cache (in-memory, within-run).** Populated by every transfer, mount, and HEAD check. Shared across all images via `Rc<RefCell<>>`.

The lookup sequence for each blob:
1. Cache says blob exists at this repo on this target -> **skip** (0 API calls)
2. Cache says blob exists at a *different* repo on this target -> **mount** (1 API call)
3. Unknown -> **HEAD check** at target, record result (1 API call)

Step 2 is the key: the cache tracks which repositories at each target registry are known to contain each blob. When mounting, any known repo can serve as the `from` parameter.

**Tier 2 - warm cache (persistent disk, cross-run).** Serialized to disk on sync completion using binary format (postcard + CRC32 integrity check). At typical scale (~50K entries), the file is ~1 MB and loads in <5 ms. At extreme scale (600K entries), ~8 MB with sub-100ms load.

Only confirmed states (`ExistsAtTarget`, `Completed`) are persisted. Transient states (`InProgress`, `Failed`) are stripped before serialization. The warm cache enables CronJob deployments to skip HEAD checks for known blobs on every run.

### Lazy invalidation

The cache may become stale - ECR lifecycle policies can delete images, external actors can remove blobs. Rather than TTL-expiring entries proactively, stale entries self-heal:

1. Mount or push fails for a blob the cache claims exists
2. Entry is invalidated
3. Operation falls back to fresh HEAD check and standard upload

This is zero-configuration and handles all staleness scenarios (lifecycle policies, manual deletion, registry GC between runs). An optional `cache_ttl` provides an additional safety net.

## Streaming transfers

### Single-target: zero disk

Bytes flow directly from source registry to target registry with no intermediate storage:

```
source GET -> [byte stream] -> target PUT
                  |
            SHA-256 on-the-fly
```

Memory usage is bounded by chunk size per active upload stream. The default upload strategy is POST + streaming PUT with `Transfer-Encoding: chunked` - two HTTP requests per blob, zero buffering.

### Multi-target: content-addressable staging

When a mapping has multiple targets (e.g., us-ecr + eu-ecr + ap-ecr), re-pulling each blob from source for each target wastes bandwidth. For a 2 GB ML layer across 3 regions: 6 GB source bandwidth instead of 2 GB.

ocync pulls each source blob once and writes it to a content-addressable disk file:

```
source GET -> {cache_dir}/blobs/sha256/{hex_digest}
                      |
        target 1 push <- read from disk
        target 2 push <- read from disk
        target N push <- read from disk
```

Writes use an atomic protocol (tmp file -> fsync -> rename -> dir fsync) for crash safety. Reads happen milliseconds after writes, so the data stays in OS page cache at memory speed (~4+ GB/s). The "disk penalty" is fictional for recently-written files.

Single-target deployments pay zero overhead - `BlobStage::disabled()` is a no-op with no disk writes, no eviction logic, no staging paths.

### Upload strategy per registry

Registries vary in upload protocol support. ocync adapts automatically:

| Registry | Strategy | Requests/blob | Notes |
|---|---|:---:|---|
| ECR, Docker Hub, Harbor, Quay | POST + streaming PUT | 2 | Default path |
| GHCR | POST + single PATCH + PUT | 3 | Multi-PATCH broken (last chunk overwrites all previous) |
| GAR | POST + buffered monolithic PUT | 2 | No PATCH support at all |
| ACR | POST + chunked PATCH + PUT | 3 | Streaming PUT body limit ~20 MB; chunked for larger blobs |

## Registry behavior

### Capability matrix

| Registry | Cross-repo mount | Batch discovery APIs | Rate limit model |
|---|---|---|---|
| ECR (private) | Yes (opt-in, same account/region) | Yes (BatchCheck, BatchGet, ListImages) | Per-action TPS (10-3000) |
| ECR Public | No | Partial (BatchCheck only) | Separate from private |
| Docker Hub | Yes (implicit global dedup) | No | 100/hr authed manifest GETs; HEADs free |
| GAR | No | No | Per-project shared quota |
| GHCR | Yes (implicit global dedup) | No | GitHub API rate limit |
| ACR | Yes | No | Per-registry |
| Chainguard | N/A (source only) | No | No rate limits |

### Docker Hub rate limits

Docker Hub tightened limits in April 2025:

| Tier | Limit |
|---|---|
| Anonymous | 10 pulls/hour/IP |
| Authenticated free | 100 pulls/hour |
| Pro/Team/Business | Unlimited (fair use) |

What counts: manifest GET only. Blob GETs are free (CDN-served). Manifest HEADs are free but subject to a separate abuse limit (~314 in 8 minutes triggers 429). Rate limit headers (`ratelimit-remaining`) appear on both GET and HEAD responses.

Authentication is mandatory for any serious sync workload.

### ECR rate limits

The 50:1 ratio between `UploadLayerPart` (500 TPS) and `PutImage` (10 TPS) means blob uploads can saturate while manifest pushes trickle. For 50 multi-arch images (550 PutImage calls), the minimum is 55 seconds at 10 TPS per region. This is why per-action AIMD windows matter - a single per-host window would let PutImage throttling stall all blob uploads.

## Competitive position

Benchmarked on c6in.4xlarge (Intel, 16 vCPUs, 32 GiB, up to 50 Gbps). Full corpus: 42 images, 55 tags across Docker Hub, cgr.dev, public ECR, nvcr.io. Cold sync to ECR us-east-1.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **4m 33s** | 36m 22s | 16m 6s |
| Peak RSS | **58 MB** | 305 MB | 27 MB |
| Requests | **4,131** | 11,190 | 7,719 |
| Response bytes | **16.9 GB** | 43.4 GB | 35.4 GB |
| Source blob GETs | **726** | 1,324 | 1,381 |
| Mounts (success/attempt) | **362/379** | 293/293 | 0/1,380 |
| Rate-limit 429s | **0** | 49 | 0 |

Key differentiators:

- **8x faster** than dregsy, **3.5x faster** than regsync
- **2.6x fewer bytes** than dregsy, **2.1x fewer** than regsync
- **Zero rate-limit 429s** (AIMD congestion control adapts before hitting limits)
- **95.5% mount success** vs regsync's 0% (regclient omits `from=` parameter for cross-registry syncs)
- **Zero duplicate blob GETs** (source-pull dedup via staging)

regsync's lower peak RSS (27 MB vs 58 MB) reflects its sequential architecture - one image, one blob at a time, with Go's ~10-15 MB baseline. ocync's 58 MB comes from concurrent blob transfers, staging maps, and the transfer state cache. The 58 MB vs 27 MB trade-off buys 3.5x faster wall clock.

Methodology: all traffic routed through bench-proxy (pure-Rust MITM) for byte-accurate request/response counting. Instance metadata captured from `ec2:DescribeInstanceTypes` (authoritative CPU/memory/network). Full results archived to `bench-results/runs/*.json`.

## Deployment model

### Single-core containers

The single-threaded runtime means a container with `cpu: 100m` request and `memory: 128Mi` is sufficient for most workloads. There is no multi-core scaling to configure and no gap between requested and utilized resources. The process is I/O-bound - it spends its time waiting for HTTP responses, not computing.

For large sync runs (hundreds of images, multi-GB layers), increase memory to `512Mi` to accommodate the transfer state cache and concurrent blob staging.

### Graceful shutdown

On SIGTERM or SIGINT:

1. Stop promoting new work from the pending queue
2. Stop polling discovery futures
3. Drain in-flight execution futures until completion or deadline
4. Persist transfer state cache to disk (highest-value action - losing cache costs minutes of redundant HEAD checks on next run)
5. Exit with appropriate code

Default drain deadline: 25 seconds, chosen to fit within Kubernetes' default 30-second `terminationGracePeriodSeconds` with 5 seconds of margin. The `select!` drain branch guards on `!execution_futures.is_empty()` so the engine exits immediately when all work completes rather than waiting for the full deadline.

### Deployment modes

| Mode | K8s resource | When to use |
|---|---|---|
| **CronJob** | `CronJob` | Scheduled sync every N minutes (primary mode) |
| **Watch** | `Deployment` + `ocync watch` | Continuous sync with health endpoints and config reload |
| **One-shot** | `Job` | CI-triggered, manual, or initial seeding |

Watch mode exposes `/healthz` (liveness) and `/readyz` (readiness) endpoints. Readiness reports 503 if the last sync cycle did not complete within `2 * interval`. SIGHUP triggers config reload without restart.

### FIPS 140-3

FIPS is the default build (`--features fips`). Cryptography uses aws-lc-rs with NIST Certificate #4816.

Linux binaries use `x86_64-unknown-linux-gnu` (not musl) because FIPS certification is validated against glibc. The `+crt-static` flag statically links glibc so the final binary has no dynamic dependencies and runs on `chainguard/static` (zero CVEs, no shell, no package manager).

At startup, `try_fips_mode()` runs a self-check. On failure, the process exits immediately with a clear message rather than silently operating in a non-compliant state.

macOS and Windows builds use `--features aws-lc` (non-FIPS aws-lc-rs) since FIPS certification is only relevant for Linux server deployments.
