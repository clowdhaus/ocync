---
title: Performance
# BENCH-DESCRIPTION:START
# run 2026-11-04-171727
description: How ocync finishes a cold sync to ECR 4.1 times faster than regsync in 5 rounds, with 27% fewer requests and 10.0 GB fewer bytes, and cross-repo blob mounting.
# BENCH-DESCRIPTION:END
order: 6
---

## Benchmark results

<!-- Written by `cargo xtask bench-remote --publish` from the published run, and by nothing else. -->
<!-- BENCH:START -->
Measured 2026-11-04 (run 2026-11-04-171727, commit 75b5448) on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 40 images, 54 tags. Cold sync from the controlled source (ECR us-east-1) to ECR us-east-2. Median of 5 rounds. ocync ran at its default of 20 images at once and every other tool at its own defaults. All traffic routed through bench-proxy for byte-accurate measurement.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **3m 47s** | 20m 11s | 14m 27s |
| Peak RSS | 61.7 MB | 38.4 MB | **27.7 MB** |
| Requests | **6921** | 11910 | 9525 |
| Response bytes | 55.1 GB | **55.0 GB** | 65.1 GB |
| Source blob GETs | 1436 | **1391** | 1700 |
| Source blob bytes | 55.1 GB | **55.0 GB** | 65.1 GB |
| Mounts (success/attempt) | **356/356** | 341/341 | 0/0 |
| Duplicate blob GETs | **0** | 32 | **0** |
| CDN hits/misses | **0/0** | **0/0** | **0/0** |
| Rate-limit 429s | **0** | **0** | **0** |

> **Artifact sync (OCI 1.1 referrers).** ocync issues `GET /v2/<repo>/referrers/...` to discover attestations (SBOM, SLSA) and transfers any referenced artifacts. Referrers are not copied by dregsy and regsync as configured here. The Source blob GETs and Source blob bytes columns above include this artifact content for ocync.
>
> Referrers calls -- ocync: 52, dregsy: 0, regsync: 0.
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
| `ocync` | 20 images at once; per registry, 1 request initial, 50 cap ([AIMD](/design/overview#adaptive-concurrency-aimd) adaptive) | Yes (per-registry, per-action) | [AIMD](/design/overview#adaptive-concurrency-aimd) congestion control on 429, plus bounded jittered retry (4 attempts) of throttled prepare-phase reads |
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
Measured 2026-11-04 (run 2026-11-04-171727, commit 75b5448) on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 40 images, 54 tags. Warm sync (no changes) from the controlled source (ECR us-east-1) to ECR us-east-2. Median of 5 rounds. ocync ran at its default of 20 images at once and every other tool at its own defaults. All traffic routed through bench-proxy for byte-accurate measurement.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **0s** | 1m 37s | 5s |
| Peak RSS | **14.3 MB** | 37.1 MB | 20.7 MB |
| Requests | **55** | 3301 | 110 |
| Response bytes | 4.1 KB | 1.7 MB | **0 B** |
| Source blob GETs | **0** | 263 | **0** |
| Source blob bytes | **0 B** | 1.7 MB | **0 B** |
| Mounts (success/attempt) | **0/0** | **0/0** | **0/0** |
| Duplicate blob GETs | **0** | 32 | **0** |
| CDN hits/misses | **0/0** | **0/0** | **0/0** |
| Rate-limit 429s | **0** | **0** | **0** |
<!-- BENCH-WARM:END -->

`ocync` skips tag enumeration when exact tags are configured (no wildcard, semver, or latest filters), using the configured tag names directly instead of paginating through the registry's full tag list. Combined with the transfer state cache and source manifest HEAD checks, this reduces a no-op warm sync to about one request per configured tag, regardless of how many tags exist at the source.

## Why `ocync` is fast

### Pipelined architecture

Discovery and execution overlap. While images are being transferred, `ocync` is already discovering the next batch of work. There is no idle time between phases.

### Global blob deduplication

Container images share layers heavily. A sync run touching 40 images may reference only a fraction as many unique blobs. `ocync` tracks every blob globally and transfers each unique blob exactly once.

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
