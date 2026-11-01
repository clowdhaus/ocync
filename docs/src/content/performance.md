---
title: Performance
description: How ocync finishes a cold sync to ECR 2.89 to 3.07 times faster than regsync in 5 rounds, with about 25% fewer requests and about 9.6 GB fewer bytes, and cross-repo blob mounting.
order: 6
---

## Benchmark results

<!-- Auto-updated by `cargo xtask bench`; shows first scenario (cold sync) only. -->
<!-- BENCH:START -->
Measured 2026-10-31 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 25 images, 39 tags. Cold sync to ECR us-east-2. All traffic routed through bench-proxy for byte-accurate measurement. 5 rounds, the tool order shuffled in each. A cell is the median over the rounds, with the lowest and highest in brackets; a paired cell (a/b) brackets the range of each value, in the order shown.

| Metric | ocync | ocync@7634416 | regsync |
|---|---:|---:|---:|
| Wall clock | 3m 33s (3m 28s..3m 36s) | 3m 31s (3m 16s..3m 46s) | 10m 24s (10m 19s..10m 43s) |
| Peak RSS | 57.1 MB (49.4 MB..60.2 MB) | 44.7 MB (43.2 MB..46.8 MB) | **27.1 MB (27.1 MB..27.5 MB)** |
| Requests | **5795 (5794..5804)** | 6411 (6404..6447) | 7770 (7770..7772) |
| Response bytes | 53.3 GB (53.3 GB..53.3 GB) | **53.3 GB (53.3 GB..53.3 GB)** | 62.9 GB (62.9 GB..62.9 GB) |
| Source blob GETs | **1196 (1196..1196)** | 1211 (1211..1211) | 1372 (1372..1372) |
| Source blob bytes | **53.3 GB (53.3 GB..53.3 GB)** | 53.3 GB (53.3 GB..53.3 GB) | 62.9 GB (62.9 GB..62.9 GB) |
| Mounts (success/attempt) | 267/268 (262..267 / 268..268) | **404/404 (365..410 / 365..410)** | 0/0 (0..0 / 0..0) |
| Duplicate blob GETs | 0 (0..0) | 0 (0..0) | 0 (0..0) |
| CDN hits/misses | 1073/0 (1072..1073 / 0..1) | **1082/0 (1082..1082 / 0..0)** | 1008/0 (1002..1008 / 0..6) |
| Rate-limit 429s | 0 (0..0) | 0 (0..0) | 0 (0..0) |

Per-round ratio of the other tool to ocync (above 1: ocync lower). The range is the lowest to the highest of 5 rounds and holds the true median ratio with 93.75% confidence. A difference is claimed only when every round agrees, and the last column is the chance that identical builds would agree this way in every round.

| Tool | Metric | Median ratio | Range | ocync is | Chance if identical |
|---|---|---:|---:|---|---:|
| ocync@7634416 | Wall clock | 0.98 | 0.94..1.06 | no measured difference | -- |
| ocync@7634416 | Requests | 1.11 | 1.10..1.11 | fewer | 6.25% |
| ocync@7634416 | Response bytes | 1.00 | 1.00..1.00 | more | 6.25% |
| regsync | Wall clock | 2.93 | 2.89..3.07 | faster | 6.25% |
| regsync | Requests | 1.34 | 1.34..1.34 | fewer | 6.25% |
| regsync | Response bytes | 1.18 | 1.18..1.18 | fewer | 6.25% |

> **Artifact sync (OCI 1.1 referrers).** ocync issues `GET /v2/<repo>/referrers/...` to discover attestations (SBOM, SLSA) and transfers any referenced artifacts. Comparable tools do not implement this. The Source blob GETs and Source blob bytes columns above include this artifact content for ocync.
>
> Referrers calls -- ocync: 37, ocync@7634416: 39, regsync: 0.
<!-- BENCH:END -->

`ocync` is 6c93872, at its default of 20 images at once; `ocync@7634416` is ocync as of April 2026. cgr.dev is skipped, so the corpus is 25 images and 39 tags. Uploaded to ECR per round (blob upload PUT request bytes): ocync 53,292,924,413 B in four rounds and 53,293,057,239 B in the one where 6 of 268 mounts fell back to uploads, the April build 57,018,027,869 to 57,812,918,335 B, regsync 62,883,218,811 B.

The two ocync builds differ in six months of code. One difference is the cap on images synced at once: 50 in April, 20 today. The April build finishes in the same time but uploads 3.7 to 4.5 GB more per round and sends about 10% more requests. Raising today's cap from 10 to 20 made a cold sync about 13% faster on both ECR and Artifact Registry ([#129](https://github.com/clowdhaus/ocync/issues/129)).

dregsy is left out because its run did not pass the content check, which requires every image to arrive at the digest the corpus lock names. Earlier dregsy figures predate that check. The comparison between tools is on ECR; Artifact Registry has been measured with ocync alone.

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
Measured 2026-10-31 on c6in.4xlarge (x86_64, 16 vCPUs, 32 GiB, Up to 50 Gigabit). Full corpus: 25 images, 39 tags. Warm sync (no changes) to ECR us-east-2. All traffic routed through bench-proxy for byte-accurate measurement. 5 rounds, the tool order shuffled in each. A cell is the median over the rounds, with the lowest and highest in brackets; a paired cell (a/b) brackets the range of each value, in the order shown.

| Metric | ocync | ocync@7634416 | regsync |
|---|---:|---:|---:|
| Wall clock | 2s (2s..2s) | **2s (2s..2s)** | 7s (6s..8s) |
| Peak RSS | **15.6 MB (15.5 MB..15.9 MB)** | 17.6 MB (17.5 MB..17.8 MB) | 20.9 MB (20.2 MB..21.3 MB) |
| Requests | 108 (108..108) | 108 (108..108) | 117 (117..117) |
| Response bytes | 116.3 KB (116.3 KB..116.3 KB) | 116.3 KB (116.3 KB..116.3 KB) | **101.6 KB (101.6 KB..101.6 KB)** |
| Source blob GETs | 0 (0..0) | 0 (0..0) | 0 (0..0) |
| Source blob bytes | 106.8 KB (106.8 KB..106.8 KB) | 106.8 KB (106.8 KB..106.8 KB) | **101.6 KB (101.6 KB..101.6 KB)** |
| Mounts (success/attempt) | 0/0 (0..0 / 0..0) | 0/0 (0..0 / 0..0) | 0/0 (0..0 / 0..0) |
| Duplicate blob GETs | 0 (0..0) | 0 (0..0) | 0 (0..0) |
| CDN hits/misses | 0/0 (0..0 / 0..0) | 0/0 (0..0 / 0..0) | 0/0 (0..0 / 0..0) |
| Rate-limit 429s | 0 (0..0) | 0 (0..0) | 0 (0..0) |

Per-round ratio of the other tool to ocync (above 1: ocync lower). The range is the lowest to the highest of 5 rounds and holds the true median ratio with 93.75% confidence. A difference is claimed only when every round agrees, and the last column is the chance that identical builds would agree this way in every round.

| Tool | Metric | Median ratio | Range | ocync is | Chance if identical |
|---|---|---:|---:|---|---:|
| ocync@7634416 | Wall clock | 0.99 | 0.98..0.99 | slower | 6.25% |
| ocync@7634416 | Requests | 1.00 | 1.00..1.00 | no measured difference | -- |
| ocync@7634416 | Response bytes | 1.00 | 1.00..1.00 | no measured difference | -- |
| regsync | Wall clock | 3.02 | 2.69..3.38 | faster | 6.25% |
| regsync | Requests | 1.08 | 1.08..1.08 | fewer | 6.25% |
| regsync | Response bytes | 0.87 | 0.87..0.87 | more | 6.25% |
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
