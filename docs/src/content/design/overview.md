---
title: Design overview
description: User-facing design overview explaining how ocync works and why each choice was made
order: 1
---

`ocync` is a Rust-based OCI registry sync tool that copies container images between registries. It is designed around a single insight: the fastest sync is the one that transfers the fewest bytes and issues the fewest API calls. Wall-clock speed is a consequence of efficiency, not a goal separate from it.

This document explains how `ocync` works, why each design choice was made, and what the measurable impact is.

## Design philosophy

`ocync` optimizes along four axes, in strict priority order:

1. **Efficiency** - bytes transferred and API calls issued. Every blob we avoid pulling, every HEAD we avoid issuing, is the real win.
2. **Correctness** - staleness handling, auth invalidation, protocol conformance. Efficiency optimizations must degrade safely, not silently.
3. **Wall-clock speed** - a consequence of (1), not a goal separate from it. Reports that prioritize wall-clock without byte/request counts are misleading.
4. **UX** - clear errors, structured output, sensible defaults. Must be zero-cost when disabled so it never drags on (1).

When these conflict, the higher-priority axis wins. A mount that saves bytes but costs an extra API call is worth it. A cache that saves API calls but risks correctness is not.

## Runtime model

`ocync` runs on a single-threaded tokio runtime (`current_thread`). The workload is ~100% network I/O - hundreds of concurrent futures spend >99% of wall-clock time awaiting HTTP responses from registries. A single OS thread handles all of them without context-switch overhead, lock contention, or `Send`/`Sync` bound infection.

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

`ocync` uses a pipelined architecture where discovery and execution overlap:

```
                     +-------------------------------------------------+
                     | Pipeline loop (select! over two pools)          |
                     |                                                 |
                     | Discovery pool (FuturesUnordered):              |
                     |   Source manifest pull + target HEAD per tag    |
                     |            |                                    |
                     |            v                                    |
                     |   Pending queue (VecDeque<TransferTask>)         |
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

Static concurrency limits force operators to guess at registry capacity. Too low wastes throughput. Too high triggers rate-limit storms. `ocync` discovers actual capacity through feedback.

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

Multiple images in a sync run often share base layers. If all images push independently, shared blobs are uploaded N times. `ocync` uses leader-follower election to ensure shared blobs are uploaded once and mounted everywhere else.

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

Without `BLOB_MOUNTING` enabled (or for standalone blobs without manifests), ECR returns 202 and starts a regular upload session. `ocync` detects this and falls through to the standard upload path.

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

`ocync` pulls each source blob once and writes it to a content-addressable disk file:

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

Registries vary in upload protocol support. `ocync` adapts automatically:

| Registry | Strategy | Requests/blob | Notes |
|---|---|:---:|---|
| ECR, Docker Hub, Harbor, Quay | POST + streaming PUT | 2 | Default path |
| GHCR | POST + single PATCH + PUT | 3 | Multi-PATCH broken (last chunk overwrites all previous) |
| GAR | POST + buffered monolithic PUT | 2 | No PATCH support at all |
| ACR | POST + streaming PUT | 2 | Known ~20 MB body limit; chunked PATCH not yet implemented |

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

See [Performance](../performance) for the full benchmark table. Summary (39 images, 51 tags, cold sync to ECR on c6in.4xlarge):

- **3.6-4.5x faster** wall clock than comparable tools
- **25-38% fewer API requests** through global blob dedup and mount-first strategy
- **Cross-repo blob mounting** avoids re-pulling shared layers from source (95.6% mount success rate)
- **Zero duplicate blob GETs** (source-pull dedup via staging)
- **Zero rate-limit 429s** (AIMD congestion control adapts before hitting limits)

The lower peak RSS of sequential tools reflects their one-image-at-a-time architecture. `ocync`'s ~51 MB comes from concurrent blob transfers, staging maps, and the transfer state cache -- the memory trade-off buys the wall-clock advantage.

Methodology: all traffic routed through bench-proxy (pure-Rust MITM) for byte-accurate request/response counting. Instance metadata captured from `ec2:DescribeInstanceTypes`. Run records archived to `bench/results/ecr.json`.

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

Watch mode exposes `/healthz` (liveness) and `/readyz` (readiness) endpoints.

### FIPS 140-3

FIPS is the default build (`--features fips`). Cryptography uses aws-lc-rs with NIST Certificate #4816.

Linux binaries use `x86_64-unknown-linux-gnu` (not musl) because FIPS certification is validated against glibc. The `+crt-static` flag statically links glibc so the final binary has no dynamic dependencies and runs on `chainguard/static` (zero CVEs, no shell, no package manager).

At startup, `install_crypto_provider()` registers `aws-lc-rs` as the process-wide rustls crypto provider. The call is idempotent; subsequent calls are silently ignored. When built with `--features fips`, the provider uses the FIPS-validated aws-lc module; with `--features non-fips`, it uses the standard aws-lc backend.

macOS and Windows builds use `--no-default-features --features non-fips` (standard aws-lc-rs without FIPS mode) since FIPS certification is only relevant for Linux server deployments.

> **Status: Planned.** The features in this section are designed but not yet implemented.

## OCI artifacts and referrers

OCI 1.1 introduced the referrers API for attaching artifacts (signatures, SBOMs, attestations) to container images. Each artifact manifest includes a `subject` field referencing the parent image digest. This creates a discoverable graph of metadata without polluting the tag namespace.

### Discovery

After syncing an image manifest, `ocync` queries the source registry's referrers API:

```
GET /v2/{repo}/referrers/{digest}
```

This returns an image index listing all artifacts that reference the synced manifest. Common artifact types include cosign signatures, SPDX and CycloneDX SBOMs, in-toto/SLSA attestations, and Notation signatures.

### Configuration

```yaml
defaults:
  artifacts:
    enabled: true                      # default: true (sync all artifacts)
    include:                           # only sync these artifact types (if specified)
      - "application/vnd.dev.cosign.artifact.sig.v1+json"
      - "application/spdx+json"
    exclude: []                        # exclude specific types
```

`artifacts.enabled` defaults to `true`, meaning artifacts are synced alongside their parent images unless explicitly disabled. Include and exclude lists filter by artifact media type when finer control is needed.

### `require_artifacts` flag

When `require_artifacts: true` is set, `ocync` enforces that every synced image has at least one referrer (signature, SBOM, or attestation). If a source image has no referrers, the sync fails with an error rather than silently producing an unsigned image at the target.

Setting `require_artifacts: true` with `artifacts.enabled: false` in the same scope is a CONFIG_ERROR (exit 3) because it is contradictory to require artifacts while disabling their transfer.

### Signature stripping warning

When `artifacts.enabled: false` is configured, `ocync` emits a WARNING at config validation time. Disabling artifact sync strips signatures and SBOMs from synced images, which means targets receive images without the provenance metadata that downstream consumers may depend on for verification. The `--dry-run` flag queries referrers for each image and reports exactly what would be stripped, so operators can make an informed decision before committing to this configuration.

### Transfer order

Artifacts have a `subject` field referencing the parent image digest, so they must be pushed AFTER the parent image exists at the target:

1. Push image blobs
2. Push image manifest
3. Discover referrers at source
4. For each matching artifact: push artifact blobs, then push artifact manifest (with `subject` pointing to parent)

Pushing artifacts before their subject manifest exists would produce a dangling reference that most registries reject.

### Fallback for older registries

Not all registries support the OCI 1.1 referrers API. Some older registries use the "tag fallback" scheme, where referrers are stored as tags named `sha256-{digest}`. `ocync` handles both:

1. Try the referrers API first (`GET /v2/{repo}/referrers/{digest}`)
2. If 404, try tag fallback (`GET /v2/{repo}/manifests/sha256-{digest}`)
3. If neither works, log at INFO and continue (artifacts are not available at this source)

This two-step fallback ensures artifact sync works across the widest range of registries without requiring operator configuration.

## Manifest format preservation

### Problem

Pushing a Docker v2 Schema 2 manifest to a registry expecting OCI format (or vice versa) changes the digest. The manifest bytes are different, so the SHA-256 is different, so the content-addressable identity of the image changes. Some registries reject manifests with unexpected media types entirely. Existing tools handle this poorly: digests change silently or copies fail with opaque errors.

Digest preservation matters because it is the foundation of the OCI content-addressable model. An image's digest is its identity. If a sync tool changes the digest, consumers cannot verify that the target image is byte-identical to the source. Signature verification breaks (signatures reference the original digest), and deployment pipelines that pin by digest pull a different image than intended.

### Policy

Default: **preserve the original format exactly.** The manifest bytes are transferred verbatim to maintain digest integrity.

| Setting | Behavior |
|---|---|
| `preserve` (default) | Push manifest bytes as-is. Digest unchanged. |
| `oci` | Convert Docker v2 Schema 2 to OCI Image Manifest before push. Digest WILL change. Log WARNING. |
| `docker-v2` | Convert OCI to Docker v2 Schema 2 before push. Digest WILL change. Log WARNING. |

Per-registry override is available for broken registries that require a specific format:

```yaml
registries:
  legacy-harbor:
    url: harbor.internal.io
    manifest_format: oci
```

### ECR immutable tag handling

When pushing to an ECR repository with `image_tag_mutability: IMMUTABLE`, pushing an existing tag returns `ImageTagAlreadyExistsException`. `ocync` treats this as **success** because the image is already there with that tag. The exception is logged at DEBUG level (not an error, not even a warning) and the image is marked as skipped with reason `tag_immutable_exists`.

This makes re-runs fully idempotent without requiring `skip_existing: true`. A sync that is interrupted and restarted will pick up where it left off, and tags that were already pushed in the previous partial run are silently accepted.

## Platform subset filtering

### Default: all platforms

When `platforms` is omitted from the config, the full image index (all platforms) is copied. This is the safe, faithful mirror default. `platforms: all` is not a valid config value; simply omit the field to get all platforms.

### Subset: trimmed index

When `platforms` is specified, `ocync` builds a new image index containing only the requested platforms:

```yaml
mappings:
  - from: chainguard/nginx
    to: mirror/nginx
    platforms: [linux/amd64, linux/arm64]
```

The source image index is pulled in full, filtered to the requested platforms, and only the blobs and manifests for those platforms are transferred. The pushed index contains only the matching platform entries and will have a **different digest** than the source index (fewer entries means different bytes means different SHA-256). This is expected and logged at INFO.

### Missing platforms

If a requested platform is not available in the source image index, `ocync` logs a WARNING and continues with the platforms that are available. A source image that offers `linux/amd64` but not `linux/arm64` will sync the amd64 platform and warn about the missing arm64 entry.

If **zero** requested platforms match any manifest in the source index, `ocync` returns an error with actionable context: the configured platform filter, the platforms actually available in the source index, and the source reference. An empty filtered index is never pushed to targets because it would leave targets with an invalid manifest that appears synced but contains no usable platform entries. This surfaces platform configuration mismatches immediately rather than silently degrading into an unusable state.

> **Status: Planned.** The `immutable_tags` optimization described below is designed but not yet implemented. `skip_existing` and default HEAD + digest compare are implemented.

## Skip optimization hierarchy

When deciding whether to sync a tag, `ocync` evaluates three tiers in order. The first match wins.

### Tier 1: immutable_tags pattern match (0 API calls)

```yaml
defaults:
  tags:
    immutable_tags: "v?[0-9]*.[0-9]*.[0-9]*"
```

When a tag matches the `immutable_tags` glob pattern AND the tag name appears in the target's tag list (already fetched during planning): skip immediately. No manifest HEAD, no digest comparison. Zero additional API calls.

Semver tags (`v1.2.3`, `3.12.1`) are conventionally immutable. Once a version is published, changing its contents breaks every consumer that pinned to it. The convention is strong enough that checking their digest on every sync run is waste. Operators who use mutable semver tags (uncommon but not unheard of) should not configure `immutable_tags`.

### Tier 2: skip_existing (1 API call, digest ignored)

```yaml
mappings:
  - from: library/redis
    to: mirror/redis
    skip_existing: true
```

When `skip_existing: true` is set: send a manifest HEAD request. If the target returns 200 (tag exists), skip. The digest is NOT compared; any version of the image at that tag is considered sufficient. Use case: mirrors where initial population is the goal, not keeping tags updated with source changes.

### Tier 3: default HEAD + digest compare

The default behavior. Send a manifest HEAD to the target. Compare the digest with the source manifest digest. Same digest means the image is unchanged, so skip. Different digest means the image was updated at source, so sync the new version. 404 means the tag does not exist at the target, so sync it.

### Performance impact

For a repository with 2,000 semver tags where only 5 are new:

- **Without optimization:** 2,000 HEAD requests (~30s at ECR rate limits)
- **With `immutable_tags`:** 5 HEAD requests for the new tags (~0.1s), 1,995 skipped from tag list

The difference is 300x fewer API calls and ~30 seconds saved per repository per sync cycle. For a configuration with 20 repositories, this saves ~10 minutes of pure API latency per run.

## Non-goals

These are explicit non-goals, listed with rationale for each.

- **No deletion/pruning.** Registries handle lifecycle (ECR lifecycle policies, Harbor retention). A sync tool that deletes is a sync tool that can destroy production images on misconfiguration.
- **No image building.** `ocync` syncs existing images. Building is a separate concern with separate tooling.
- **No signature verification.** `ocync` transfers signatures faithfully but does NOT verify them. Verification is the consumer's responsibility. A sync tool that verifies signatures would need to manage trust roots, which is out of scope.
- **No Docker daemon integration.** Pure OCI Distribution API only. The Docker daemon is a local concern; registry sync is a remote-to-remote operation.
- **No skopeo dependency.** Everything is native Rust over HTTPS. No subprocess calls, no shell-out, no binary dependency to version-match.
- **No local disk buffering.** Blobs stream source to target without touching disk (single-target mode). Multi-target mode uses content-addressable staging, but single-target deployments pay zero disk I/O.
- **No date sort.** Date-based sorting requires O(n) manifest + config fetches to retrieve creation timestamps, which is prohibitively expensive for large tag sets. A repository with 2,000 tags would need 2,000 additional API calls just to sort. Use `alpha` for date-stamped tags (YYYYMMDD sorts correctly in lexicographic order).
- **No OTLP export (v1).** Prometheus metrics are the priority. OTLP is deferred to a future version.
- **No per-mapping schedules in watch mode.** One global interval for simplicity and predictability. Per-mapping schedules create complex interactions (overlapping syncs to the same target, uneven resource usage) for marginal benefit.

## Output philosophy

**Silence means success.** Output means action was taken or something went wrong. Anything expected and successful is silent.

Existing tools get this catastrophically wrong:

- **skopeo** dumps walls of "existing blob" messages. Every blob that already exists at the target produces a line of output. A sync of 50 images where 47 are unchanged produces hundreds of lines that communicate nothing actionable.
- **`regsync`** floods with repeated status noise, emitting progress updates for every operation regardless of whether the operator needs to act on them.
- **`dregsy`** swallows errors while being verbose about everything else. The one category of output that demands attention is the one it suppresses.

A successful `ocync` sync of 50 images where 47 are unchanged produces the summary only, not 500 lines of noise:

```
synced   chainguard/nginx:1.27 -> mirror/nginx:1.27       (187 MB, 14s)
synced   chainguard/python:3.12 -> mirror/python:3.12     (245 MB, 19s)
synced   chainguard/node:22 -> mirror/node:22             (312 MB, 22s)

-- Stats --
  Images:  3 synced, 47 skipped, 0 failed
  Layers:  12 transferred (89 unique references), 34 mounted
  Data:    744 MB transferred
  Time:    47s elapsed
```

Three lines of action taken, one summary block. The 47 unchanged images are accounted for in the summary ("47 skipped") but do not each produce a line of output. Operators scan this in seconds. Errors, when they occur, are impossible to miss because they are not buried in noise.
