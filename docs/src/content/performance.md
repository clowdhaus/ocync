---
title: Performance
description: How ocync finishes a cold sync to ECR about 2.9 times faster than regsync, with 27% fewer requests, about 10 GB fewer bytes, and cross-repo blob mounting.
order: 6
---

## Benchmark results

<!-- Auto-updated by `cargo xtask bench`; shows first scenario (cold sync) only. -->
<!-- BENCH:START -->
Measured 2026-10-31 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 40 images, 54 tags. Cold sync to ECR us-east-2. All traffic routed through bench-proxy for byte-accurate measurement. 4 of 5 rounds (stopped), the tool order shuffled in each. A cell is the median over the rounds, with the lowest and highest in brackets.

| Metric | ocync | ocync@7634416 | regsync |
|---|---:|---:|---:|
| Wall clock | 4m 9s (4m 8s..4m 12s) | **3m 31s (3m 27s..3m 35s)** | 12m (11m 51s..12m 9s) |
| Peak RSS | 55.8 MB (52.6 MB..59.7 MB) | 64.5 MB (63.2 MB..66.3 MB) | **28.2 MB (27.9 MB..28.5 MB)** |
| Requests | **6999 (6994..7036)** | 7596 (7582..7602) | 9566 (9565..9567) |
| Response bytes | 55.0 GB (55.0 GB..55.0 GB) | **55.0 GB (55.0 GB..55.0 GB)** | 65.1 GB (65.1 GB..65.1 GB) |
| Source blob GETs | **1428 (1428..1428)** | 1443 (1443..1443) | 1692 (1692..1692) |
| Source blob bytes | **55.0 GB (55.0 GB..55.0 GB)** | 55.0 GB (55.0 GB..55.0 GB) | 65.1 GB (65.1 GB..65.1 GB) |
| Mounts (success/attempt) | 352/356 (315..354) | **489/489 (483..502)** | 0/0 (0..0) |
| Duplicate blob GETs | 0 (0..0) | 0 (0..0) | 0 (0..0) |
| CDN hits/misses | 1072/0 (1003..1072) | **1082/0 (1078..1082)** | 1008/0 (1008..1008) |
| Rate-limit 429s | 0 (0..0) | 0 (0..0) | 0 (0..0) |

Per-round ratio of the other tool to ocync (above 1: ocync lower). The range is the lowest to the highest of 4 rounds and holds the true median ratio with 87.50% confidence. No verdict is given below 5 rounds.

| Tool | Metric | Median ratio | Range |
|---|---|---:|---:|
| ocync@7634416 | Wall clock | 0.85 | 0.83..0.87 |
| ocync@7634416 | Requests | 1.09 | 1.08..1.09 |
| ocync@7634416 | Response bytes | 1.00 | 1.00..1.00 |
| regsync | Wall clock | 2.88 | 2.87..2.90 |
| regsync | Requests | 1.37 | 1.36..1.37 |
| regsync | Response bytes | 1.18 | 1.18..1.18 |

> **Artifact sync (OCI 1.1 referrers).** ocync issues `GET /v2/<repo>/referrers/...` to discover attestations (SBOM, SLSA) and transfers any referenced artifacts. Comparable tools do not implement this. The Source blob GETs and Source blob bytes columns above include this artifact content for ocync.
>
> Referrers calls -- ocync: 52, ocync@7634416: 54, regsync: 0.
<!-- BENCH:END -->

The fifth round stopped when a source tag was republished mid-round. `ocync@7634416` is ocync as of April 2026. It finishes about 15% sooner because it syncs 50 images at once; today's default is 10. April makes 9% more requests and uploads 58.9 GB per round against 55.0 GB at 10. Raising the cap shortens the sync but loses cross-repo mounts ([#230](https://github.com/clowdhaus/ocync/issues/230)).

dregsy is left out: the content check found it rewrote 2 of the 54 images. Earlier dregsy figures predate that check. Only ECR has been measured as a target.

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
Measured 2026-10-31 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 40 images, 54 tags. Warm sync (no changes) to ECR us-east-2. All traffic routed through bench-proxy for byte-accurate measurement. 4 of 5 rounds (stopped), the tool order shuffled in each. A cell is the median over the rounds, with the lowest and highest in brackets.

| Metric | ocync | ocync@7634416 | regsync |
|---|---:|---:|---:|
| Wall clock | 2s (2s..2s) | **2s (2s..2s)** | 12s (11s..12s) |
| Peak RSS | **16.9 MB (16.8 MB..17.1 MB)** | 18.5 MB (18.3 MB..18.7 MB) | 20.9 MB (20.6 MB..21.1 MB) |
| Requests | 154 (154..154) | 154 (154..154) | 177 (177..177) |
| Response bytes | 143.1 KB (143.1 KB..143.1 KB) | 143.1 KB (143.1 KB..143.1 KB) | **128.3 KB (128.3 KB..128.3 KB)** |
| Source blob GETs | 0 (0..0) | 0 (0..0) | 0 (0..0) |
| Source blob bytes | 133.7 KB (133.7 KB..133.7 KB) | 133.7 KB (133.7 KB..133.7 KB) | **128.3 KB (128.3 KB..128.3 KB)** |
| Mounts (success/attempt) | 0/0 (0..0) | 0/0 (0..0) | 0/0 (0..0) |
| Duplicate blob GETs | 0 (0..0) | 0 (0..0) | 0 (0..0) |
| CDN hits/misses | 0/0 (0..0) | 0/0 (0..0) | 0/0 (0..0) |
| Rate-limit 429s | 0 (0..0) | 0 (0..0) | 0 (0..0) |

Per-round ratio of the other tool to ocync (above 1: ocync lower). The range is the lowest to the highest of 4 rounds and holds the true median ratio with 87.50% confidence. No verdict is given below 5 rounds.

| Tool | Metric | Median ratio | Range |
|---|---|---:|---:|
| ocync@7634416 | Wall clock | 0.99 | 0.99..1.00 |
| ocync@7634416 | Requests | 1.00 | 1.00..1.00 |
| ocync@7634416 | Response bytes | 1.00 | 1.00..1.00 |
| regsync | Wall clock | 4.97 | 4.71..5.27 |
| regsync | Requests | 1.15 | 1.15..1.15 |
| regsync | Response bytes | 0.90 | 0.90..0.90 |
<!-- BENCH-WARM:END -->

`ocync` skips tag enumeration when exact tags are configured (no wildcard, semver, or latest filters), using the configured tag names directly instead of paginating through the registry's full tag list. Combined with the transfer state cache and source manifest HEAD checks, this reduces a no-op warm sync to ~150 requests regardless of how many tags exist at the source.

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
