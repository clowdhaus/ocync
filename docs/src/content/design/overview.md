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

Engine-level shared mutable state uses `Rc<RefCell<>>` instead of `Arc<Mutex<>>` -- zero synchronization overhead, static prevention of threading bugs (`Rc` is `!Send`, rejecting accidental `tokio::spawn`), and direct Kubernetes resource alignment (`cpu: 100m` maps exactly to one thread). The distribution layer's AIMD controller uses `std::sync::Mutex` because it must be `Sync` inside `Arc<RegistryClient>`; the lock is never held across `.await` points, so contention is impossible on `current_thread`. The one CPU-bound operation (SHA-256 at ~1.5 GB/s via aws-lc-rs) stays within tokio's cooperative scheduling budget. If layer recompression ships, the migration to multi-threaded is mechanical (`Rc` to `Arc`, `RefCell` to `RwLock`, add `Send` bounds).

## Pipeline architecture

### Why pipelining matters

The naive approach -- discover all images, plan all transfers, execute all transfers -- wastes time because execution waits for all discovery to complete (10-20 seconds for Chainguard, hours for Docker Hub) and the transfer state cache is empty during planning. `ocync` uses a pipelined architecture where discovery and execution overlap:

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

The `select!` loop uses `biased;` to prefer execution completions over new discovery results, maximizing throughput. Execution begins after ~1 second of discovery instead of waiting for all images to be enumerated. See [pipeline architecture in the engine doc](/design/engine#pipeline-architecture) for select loop discipline and backpressure details.

### Progressive cache population

Early transfers teach the cache about blob locations. Later images benefit from accumulated knowledge -- the pipeline saves hundreds of API calls compared to plan-then-execute by populating the cache as work proceeds. See [progressive cache population in the engine doc](/design/engine#progressive-cache-population-amplified-by-pipelining) for scale examples.

### Frequency ordering

Within each image, blobs are transferred in descending order of reference count across all discovered images. On crash or interruption, the most-shared blobs have been pushed first, providing maximum mount opportunities for the next run. See [frequency ordering in the engine doc](/design/engine#frequency-ordering) for details.

## Adaptive concurrency (AIMD)

Static concurrency limits force operators to guess at registry capacity. Too low wastes throughput. Too high triggers rate-limit storms. `ocync` discovers actual capacity through feedback using per-(registry, action) AIMD windows (Additive Increase, Multiplicative Decrease):

- **On success:** `window += 1.0 / window` (fractional increase)
- **On 429:** `window /= 2` (multiplicative decrease, once per congestion epoch)
- **Cap:** `max_concurrent` per registry (default 50)

If the registry throttles at 30 concurrent, the controller oscillates between ~15 and ~30 (the classic AIMD sawtooth), settling to an effective average of ~22.5. See [AIMD formula in the engine doc](/design/engine#aimd-formula) for the full derivation.

### Why per-action, not per-host

ECR rate limits vary dramatically by API action:

| ECR action | Rate limit |
|---|---|
| `UploadLayerPart` (blob chunks) | 500 TPS |
| `InitiateLayerUpload` (session start) | 100 TPS |
| `CompleteLayerUpload` (session finish) | 100 TPS |
| `PutImage` (manifest push) | 10 TPS |

A 429 on `PutImage` must not throttle `UploadLayerPart`. Per-action windows ensure each adapts independently.

Window grouping is registry-specific and matches each provider's actual rate-limit granularity: ECR private (9 windows, one per API action); ECR Public (5 windows, read paths share, write paths split); Docker Hub (3 windows, HEAD unmetered, manifest-read quota'd, rest shared); GAR / GCR (1 shared per-project window); GHCR (1 shared window, since GitHub enforces a single aggregate cap across reads and writes); ACR (2 windows, ReadOps and WriteOps); unknown (5-window coarse grouping). Congestion epochs prevent catastrophic window collapse when multiple 429s arrive simultaneously (TCP Reno's approach -- halve once per epoch, not once per response). Concurrency is controlled at four levels -- global image semaphore, per-registry aggregate semaphore, per-action AIMD windows, and an opt-in per-window token bucket for documented caps. See [per-registry window groupings](/design/engine#per-registry-window-groupings), [congestion epochs](/design/engine#congestion-epochs), and [four-level hierarchy](/design/engine#four-level-hierarchy) in the engine doc.

### Token-bucket layer for documented caps

AIMD discovers a healthy concurrency level via 429 feedback but cannot bound TPS when one rate cap is shared across multiple windows the controller treats independently. Cross-repo aggregation under high parallelism can exceed a documented per-account cap before AIMD halves. A `TokenBucket` per `WindowKey` enforces the documented ceiling directly, gated BEFORE concurrency permits so a paced action does not occupy a slot another window could service. ECR, ECR Public, GHCR, GAR, and ACR ship with documented caps (verified against the upstream registry docs as of 2026-04-26); registries without a published cap fall back to AIMD-only.

## Cross-repo blob mounting

When a blob already exists in another repository on the same target registry, OCI registries can "mount" it -- zero bytes over the wire. `ocync` maximizes mount success through leader-follower election and per-blob synchronization. See [cross-repo blob mounting in the engine doc](/design/engine#cross-repo-blob-mounting) for the full implementation.

### Leader-follower election

Multiple images in a sync run often share base layers. If all images push independently, shared blobs are uploaded N times. `ocync` uses leader-follower election via a greedy set-cover algorithm (`elect_leaders()`) to ensure shared blobs are uploaded once by leaders and mounted everywhere else by followers. The algorithm provably covers every shared blob -- there is no "uncovered follower" path. See [leader-follower election in the engine doc](/design/engine#leader-follower-election) for the algorithm details.

### Progressive promotion

Followers cannot mount from a leader until the leader's manifest is committed. All tasks are promoted simultaneously after discovery, with leaders ordered first so they acquire semaphore permits and claim blob uploads before followers. Followers that need a blob still in-flight wait on per-blob `Notify` handles via `ClaimAction::Wait`. Mount sources are restricted to repos with committed manifests, ensuring mount attempts only target repos that can fulfill them. See [progressive promotion in the engine doc](/design/engine#progressive-promotion) for details.

### ECR BLOB_MOUNTING

ECR requires an opt-in account setting (`BLOB_MOUNTING=ENABLED`, launched January 2026) for cross-repo mount to succeed. Mount POST returns 201 when the `from` repo has a committed manifest, both repos use identical encryption, and both are in the same account and region. Without `BLOB_MOUNTING` enabled, ECR returns 202 and starts a regular upload; `ocync` detects this and falls through to the standard upload path. See [ECR blob mounting in the engine doc](/design/engine#ecr-blob-mounting) for full requirements.

In testing (5-image Jupyter corpus, cold sync to ECR), leader-follower election reduced requests by 44%, response bytes by 57%, and improved wall clock by 3.8x with 100% mount success. See [measured impact in the engine doc](/design/engine#measured-impact) for the full breakdown.

## Transfer state cache

The cache eliminates redundant API calls by recording what is known about blob locations across all targets. A two-tier design provides both within-run and cross-run knowledge:

**Tier 1 - hot cache (in-memory).** Populated by every transfer, mount, and HEAD check via `Rc<RefCell<>>`. The 3-step lookup drives all blob transfer decisions: (1) known at this repo -- skip, (2) known at a different repo on the same target -- mount, (3) unknown -- HEAD check and record. Step 2 is the key optimization: the cache tracks which repositories contain each blob, enabling direct mount without a HEAD check.

**Tier 2 - warm cache (persistent disk).** Serialized on sync completion using binary format (~1 MB at typical scale). Only confirmed states are persisted. The warm cache enables CronJob deployments to skip HEAD checks for known blobs on every run.

Stale entries self-heal via lazy invalidation: failed mounts or pushes invalidate the entry and fall back to fresh HEAD checks. See [transfer state cache in the engine doc](/design/engine#transfer-state-cache) for persistence rules, corruption detection, cold start behavior, and [lazy invalidation](/design/engine#lazy-invalidation).

## Streaming transfers

For single-target mappings, bytes stream directly from source to target with no disk I/O -- two HTTP requests per blob (POST + streaming PUT), memory bounded by chunk size. For multi-target mappings (e.g., us-ecr + eu-ecr + ap-ecr), `ocync` pulls each source blob once and writes it to a content-addressable disk file; all target pushes read from disk independently, with recently-written data served from OS page cache at memory speed. Single-target deployments pay zero staging overhead. See [streaming transfers and staging](/design/engine#streaming-transfers-and-staging) and [upload strategy per registry](/design/engine#upload-strategy-per-registry) in the engine doc.

## Deployment model

### Single-core containers

The single-threaded runtime means a container with `cpu: 100m` request and `memory: 128Mi` is sufficient for most workloads. There is no multi-core scaling to configure and no gap between requested and utilized resources. The process is I/O-bound - it spends its time waiting for HTTP responses, not computing.

For large sync runs (hundreds of images, multi-GB layers), increase memory to `512Mi` to accommodate the transfer state cache and concurrent blob staging.

### Graceful shutdown

On SIGTERM or SIGINT, the engine stops accepting new work, drains in-flight transfers with a 25-second deadline (fitting Kubernetes' default 30s `terminationGracePeriodSeconds`), and persists the transfer state cache to disk. See [graceful shutdown in the engine doc](/design/engine#graceful-shutdown) for the full 5-step sequence.

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

ocync preserves manifest bytes bit-for-bit. No conversion between Docker v2 and OCI formats is performed, and none is planned.

Digest preservation is the foundation of the OCI content-addressable model. An image's digest is its identity. If a sync tool changes the digest, consumers cannot verify that the target image is byte-identical to the source. Signature verification breaks (signatures reference the original digest), and deployment pipelines that pin by digest pull a different image than intended.

All production registries accept both Docker v2 and OCI manifests natively. Format conversion would break every digest-based optimization (skip detection, transfer state cache, immutable tag handling, head-first) with no real-world benefit.

### ECR immutable tag handling

When pushing to an ECR repository with `image_tag_mutability: IMMUTABLE`, pushing an existing tag returns `ImageTagAlreadyExistsException`. `ocync` treats this as **success** because the image is already there with that tag. The exception is logged at DEBUG level (not an error, not even a warning) and the image is marked as skipped with reason `tag_immutable_exists`.

This makes re-runs fully idempotent. A sync that is interrupted and restarted will pick up where it left off, and tags that were already pushed in the previous partial run are silently accepted.

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

## Skip optimization hierarchy

When deciding whether to sync a tag, `ocync` evaluates two tiers in order. The first match wins.

### Tier 1: immutable_tags pattern match (0 API calls)

```yaml
defaults:
  tags:
    immutable_tags: "v?[0-9]*.[0-9]*.[0-9]*"
```

When a tag matches the `immutable_tags` glob pattern AND the tag name appears in the target's tag list (already fetched during planning): skip immediately. No manifest HEAD, no digest comparison. Zero additional API calls.

Semver tags (`v1.2.3`, `3.12.1`) are conventionally immutable. Once a version is published, changing its contents breaks every consumer that pinned to it. The convention is strong enough that checking their digest on every sync run is waste. Operators who use mutable semver tags (uncommon but not unheard of) should not configure `immutable_tags`.

### Tier 2: default HEAD + digest compare

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
- **No external tool dependencies.** Everything is native Rust over HTTPS. No subprocess calls, no shell-out, no binary dependency to version-match.
- **No local disk buffering.** Blobs stream source to target without touching disk (single-target mode). Multi-target mode uses content-addressable staging, but single-target deployments pay zero disk I/O.
- **No date sort.** Date-based sorting requires O(n) manifest + config fetches to retrieve creation timestamps, which is prohibitively expensive for large tag sets. A repository with 2,000 tags would need 2,000 additional API calls just to sort. Use `alpha` for date-stamped tags (YYYYMMDD sorts correctly in lexicographic order).
- **No OTLP or Prometheus export (v1).** Structured JSON output is the v1 observability surface. OTLP and Prometheus export are not planned for v1.
- **No per-mapping schedules in watch mode.** One global interval for simplicity and predictability. Per-mapping schedules create complex interactions (overlapping syncs to the same target, uneven resource usage) for marginal benefit.

## Output philosophy

**Silence means success.** Output means action was taken or something went wrong. Anything expected and successful is silent.

Existing tools get this catastrophically wrong: some dump walls of "existing blob" messages for every blob that already exists at the target, producing hundreds of lines that communicate nothing actionable. Others flood with repeated status noise regardless of whether the operator needs to act. Others swallow errors while being verbose about everything else -- the one category of output that demands attention is the one they suppress.

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
