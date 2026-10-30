---
title: Performance
description: How ocync finishes a cold sync to ECR about 3 times faster than regsync and 4 times faster than dregsy, with fewer requests and cross-repo blob mounting.
order: 6
---

## Benchmark results

<!-- Auto-updated by `cargo xtask bench`; shows first scenario (cold sync) only. -->
<!-- BENCH:START -->
Measured 2026-10-30 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 40 images, 55 tags. Cold sync to ECR us-east-2. All traffic routed through bench-proxy for byte-accurate measurement.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **4m 10s** | 18m 5s | 12m 29s |
| Peak RSS | 54.2 MB | 344.2 MB | **27.6 MB** |
| Requests | **6995** | 11541 | 9588 |
| Response bytes | 55.0 GB | **54.9 GB** | 65.1 GB |
| Source blob GETs | 1428 | **1399** | 1692 |
| Source blob bytes | 55.0 GB | **54.9 GB** | 65.1 GB |
| Mounts (success/attempt) | **356/356** | 341/341 | 0/0 |
| Duplicate blob GETs | **0** | 48 | **0** |
| CDN hits/misses | 898/174 | 546/504 | **1006/2** |
| Rate-limit 429s | **0** | **0** | **0** |

> **Artifact sync (OCI 1.1 referrers).** ocync issues `GET /v2/<repo>/referrers/...` to discover attestations (SBOM, SLSA) and transfers any referenced artifacts. Comparable tools do not implement this. The Source blob GETs and Source blob bytes columns above include this artifact content for ocync.
>
> Referrers calls -- ocync: 52, dregsy: 0, regsync: 0.
<!-- BENCH:END -->

The table is one run per tool on 30 October 2026 (ocync 0.8.2). Tools run in a random order, and in this run ocync went first and met a colder CDN than the tools after it: 174 CDN misses, against regsync's 2. The pre-warm meant to equalize that currently warms only each image's first tag, and the [benchmark design](/design/benchmark) covers the fix and the repeated rounds that will replace single runs.

Two comparison runs exist since the fix that ended ocync's 429s (26 April 2026), on corpora of 51 and 55 tags. In them ocync was 3.4 and 3.0 times faster than regsync and 4.0 and 4.3 times faster than dregsy, and made 23% and 27% fewer requests than regsync and 35% and 39% fewer than dregsy. No tool drew a 429 in either run. ocync ran last in the April run and first in the October one, which is enough on its own to move its time. Earlier runs measured code from before the 429 fix and are not used here. Only ECR has been measured as a target.

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
| `ocync` | 1 initial, 50 cap ([AIMD](/design/overview#adaptive-concurrency-aimd) adaptive) | Yes (per-registry, per-action) | [AIMD](/design/overview#adaptive-concurrency-aimd) congestion control on 429, plus bounded jittered retry (4 attempts) of throttled prepare-phase reads |
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
Measured 2026-10-30 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 40 images, 55 tags. Warm sync (no changes) to ECR us-east-2. All traffic routed through bench-proxy for byte-accurate measurement.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **2s** | 1m 46s | 11s |
| Peak RSS | **17.3 MB** | 36.8 MB | 20.7 MB |
| Requests | **156** | 3486 | 179 |
| Response bytes | 143.1 KB | 2.0 MB | **128.3 KB** |
| Source blob GETs | **0** | 279 | **0** |
| Source blob bytes | 133.7 KB | 2.0 MB | **128.3 KB** |
| Mounts (success/attempt) | **0/0** | **0/0** | **0/0** |
| Duplicate blob GETs | **0** | 48 | **0** |
| CDN hits/misses | 0/0 | **225/0** | 0/0 |
| Rate-limit 429s | **0** | **0** | **0** |
<!-- BENCH-WARM:END -->

`ocync` skips tag enumeration when exact tags are configured (no wildcard, semver, or latest filters), using the configured tag names directly instead of paginating through the registry's full tag list. Combined with the transfer state cache and source manifest HEAD checks, this reduces a no-op warm sync to ~150 requests regardless of how many tags exist at the source.

## Why `ocync` is fast

### Pipelined architecture

Discovery and execution overlap. While images are being transferred, `ocync` is already discovering the next batch of work. There is no idle time between phases.

### Global blob deduplication

Container images share layers heavily. A sync run touching 39 images may reference only a fraction as many unique blobs. `ocync` tracks every blob globally and transfers each unique blob exactly once.

### Cross-repo blob mounting

When a blob already exists in another repository on the same registry, `ocync` mounts it (a server-side copy) instead of uploading. Images are elected as leaders using a greedy set-cover algorithm; each leader uploads all its blobs and commits its manifest. Followers mount shared blobs from leader repositories once the leader's manifest is committed, and from repositories an earlier sync left the blob in without waiting.

### Adaptive rate limiting

Per-(registry, action) [AIMD](/design/overview#adaptive-concurrency-aimd) (additive increase, multiplicative decrease) concurrency windows discover actual registry capacity through feedback, using the same algorithm TCP uses for congestion control. ECR has 9 independent rate limits (one per API action), and `ocync` tracks each independently. This avoids under-utilization and minimizes 429 errors through capacity discovery.

### Transfer state cache

A persistent cache records which blobs exist at each target. Subsequent sync runs skip redundant HEAD checks for known-good blobs, reducing API calls on warm runs.

### Streaming transfers

In single-target mode, bytes flow directly from source to target with no intermediate disk buffering. In multi-target mode, blobs stage to disk once and push to all targets in parallel.

## Tuning

`ocync` auto-discovers registry capacity through [AIMD](/design/overview#adaptive-concurrency-aimd) feedback. Manual tuning is rarely needed. The key settings (see [configuration](/configuration) for full reference):

- **Concurrency**: starts conservatively and ramps up. Each registry action has its own window. Override with `global.max_concurrent_transfers` or per-registry `max_concurrent`.
- **Platform filter**: sync only the architectures you deploy. `linux/amd64` alone halves transfer volume for multi-arch images.
- **Tag filter**: use `latest: N` and `semver` ranges to sync only what you need.
- **Transfer state cache**: enabled by default. Warm runs transfer only new or changed blobs. Control TTL with `global.cache_ttl`.
