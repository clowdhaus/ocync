---
title: Performance
description: How ocync achieves 4x faster sync than comparable tools with fewer requests and cross-repo blob mounting.
order: 6
---

## Benchmark results

<!-- Auto-updated by `cargo xtask bench`; shows first scenario (cold sync) only. -->
<!-- BENCH:START -->
Measured 2026-04-19 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, up to 50 Gbps). Full corpus: 39 images, 51 tags across quay.io, Docker Hub, cgr.dev, public ECR. Cold sync to ECR us-east-1. All traffic routed through bench-proxy for byte-accurate measurement.

| Metric | `ocync` | `dregsy` | `regsync` |
|---|---:|---:|---:|
| Wall clock | **7m 9s** | 32m 15s | 25m 35s |
| Peak RSS | 50.6 MB | 293.5 MB | **28.0 MB** |
| Requests | **7,140** | 11,604 | 9,532 |
| Response bytes | **55.3 GB** | **55.3 GB** | 65.3 GB |
| Source blob GETs | **1,370** | **1,370** | 1,694 |
| Source blob bytes | **55.3 GB** | **55.3 GB** | 65.3 GB |
| Mounts (success/attempt) | **349/365** | 324/324 | 0/0 |
| Duplicate blob GETs | **0** | **0** | **0** |
| Rate-limit 429s | **0** | **0** | **0** |
<!-- BENCH:END -->

## How tools compare

### Upload strategy

| Tool | Strategy | Requests/blob | Buffering |
|------|---------|:---:|-----------|
| `ocync` | POST + streaming PUT | 2 | None (streamed) |
| containerd | POST + streaming PUT | 2 | None (io.Pipe) |
| regsync (regclient) | POST + PUT | 2 | None (streamed) |
| crane (go-containerregistry) | POST + streaming PATCH + PUT | 3 | None (streamed) |
| skopeo (containers/image) | POST + PATCH + PUT | 3 | None (streamed) |

### Concurrency and rate limiting

| Tool | Default concurrency | Adaptive backoff | Rate limit strategy |
|------|:---:|:---:|-----|
| `ocync` | 5 initial, 50 cap ([AIMD](../design/overview#adaptive-concurrency-aimd) adaptive) | Yes (per-registry, per-action) | [AIMD](../design/overview#adaptive-concurrency-aimd) congestion control on 429 |
| containerd | 3 layers | No | Lock-based dedup, retries on 429 (no backoff) |
| regsync | 3 per registry | No | Reads `RateLimit-Remaining` header, pauses proactively |
| crane | 4 jobs | Retry with backoff (0.1s-0.9s transport, 1s-9s operation) | Retry on 408/5xx, 3 attempts |
| skopeo | 6 layers | Yes (exponential, 2s-60s) | Retries 429 for GET/HEAD; uploads not retried |

### Blob deduplication

| Tool | Cross-image push dedup | Cross-image pull dedup | Persistent cache |
|------|:---:|:---:|:---:|
| `ocync` | Yes (TransferStateCache) | Yes (BlobStage) | Yes (postcard binary) |
| containerd | Yes (StatusTracker) | No | No |
| regsync | No | No | No (in-memory only) |
| crane | Yes (sync.Map by digest) | No | No |
| skopeo | No | No | SQLite BlobInfoCache (mount hints) |

### Warm sync efficiency

<!-- BENCH-WARM:START -->
| Tool | Requests | Bytes | Wall clock | How |
|------|:---:|:---:|:---:|-----|
| `ocync` | 81 | 371 KB | **2.5s** | Persistent cache skips blob I/O |
| regsync | **27** | **27 KB** | 4s | Manifest digest comparison only |
| dregsy | 200 | 163 KB | 5.2s | Re-HEADs all blobs |

Measured 2026-03 on a 5-image Jupyter corpus to ECR, before source-pull deduplication. These numbers predate source-pull dedup; current ocync warm performance may be better.
<!-- BENCH-WARM:END -->

## Why `ocync` is fast

### Pipelined architecture

Discovery and execution overlap. While images are being transferred, `ocync` is already discovering the next batch of work. There is no idle time between phases.

### Global blob deduplication

Container images share layers heavily. A sync run touching 39 images may reference only a fraction as many unique blobs. `ocync` tracks every blob globally and transfers each unique blob exactly once.

### Cross-repo blob mounting

When a blob already exists in another repository on the same registry, `ocync` mounts it (a server-side copy) instead of uploading. A leader-follower election algorithm ensures exactly one image uploads each shared blob while all others mount from the leader. One image is elected leader for each shared blob and performs the actual upload; followers wait and then mount from the leader's repository.

### Adaptive rate limiting

Per-(registry, action) [AIMD](../design/overview#adaptive-concurrency-aimd) (additive increase, multiplicative decrease) concurrency windows discover actual registry capacity through feedback, using the same algorithm TCP uses for congestion control. ECR has 9 independent rate limits (one per API action), and `ocync` tracks each independently. This avoids both under-utilization and 429 errors.

### Transfer state cache

A persistent cache records which blobs exist at each target. Subsequent sync runs skip redundant HEAD checks for known-good blobs, reducing API calls on warm runs.

### Streaming transfers

In single-target mode, bytes flow directly from source to target with no intermediate disk buffering. In multi-target mode, blobs stage to disk once and push to all targets in parallel.

## Tuning

`ocync` auto-discovers registry capacity through [AIMD](../design/overview#adaptive-concurrency-aimd) feedback. Manual tuning is rarely needed. The key settings (see [configuration](../configuration) for full reference):

- **Concurrency**: starts conservatively and ramps up. Each registry action has its own window. Override with `global.max_concurrent_transfers` or per-registry `max_concurrent`.
- **Platform filter**: sync only the architectures you deploy. `linux/amd64` alone halves transfer volume for multi-arch images.
- **Tag filter**: use `latest: N` and `semver` ranges to sync only what you need.
- **Transfer state cache**: enabled by default. Warm runs transfer only new or changed blobs. Control TTL with `global.cache_ttl`.
