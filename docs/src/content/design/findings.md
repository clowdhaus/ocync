---
title: Findings
description: Current benchmark results and registry behavior observations
order: 5
---

## Benchmark results

Measured 2026-04-18 on c6in.4xlarge (Intel, 16 vCPUs, 32 GiB, up to 50 Gbps). Full corpus: 42 images, 55 tags across Docker Hub, cgr.dev, public ECR, nvcr.io. Docker Hub authenticated (all tools). 1 iteration per tool. Cold sync to ECR us-east-1.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **4m 33s** | 36m 22s | 16m 6s |
| Peak RSS | 58.1 MB | 304.5 MB | **27.0 MB** |
| Requests | **4,131** | 11,190 | 7,719 |
| Response bytes | **16.9 GB** | 43.4 GB | 35.4 GB |
| Source blob GETs | **726** | 1,324 | 1,381 |
| Source blob bytes | **77.2 KB** | 120.1 KB | 160.5 KB |
| Mounts (success/attempt) | 362/379 | **293/293** | 0/1,380 |
| Duplicate blob GETs | **0** | 1 | 1 |
| Rate-limit 429s | **0** | 49 | **0** |

ocync is 8x faster than dregsy and 3.5x faster than regsync, with 2.6x fewer bytes and 2.7x fewer requests than dregsy.

regsync mounts all fail because regclient omits the `from=` parameter for cross-registry syncs. dregsy hit 49 rate-limit 429s from Docker Hub despite authenticated pulls.

regsync's lower RSS (27 MB) reflects its sequential architecture: one image, one blob at a time, with Go's ~10-15 MB baseline. ocync's 58 MB comes from concurrent blob transfers (6 per image), staging maps, and the transfer state cache. The 58 MB vs 27 MB trade-off buys 3.5x faster wall clock.

## Tool comparison

### Upload strategy

| Tool | Strategy | Requests/blob | Buffering |
|------|---------|:---:|-----------|
| **ocync** | POST + streaming PUT | **2** | None (streamed) |
| containerd | POST + streaming PUT | 2 | None (io.Pipe) |
| regsync (regclient) | POST + PUT | 2 | Full blob in memory |
| crane (go-containerregistry) | POST + streaming PATCH + PUT | 3 | None (streamed) |
| skopeo (containers/image) | POST + PATCH + PUT | 3 | None (streamed) |

### Concurrency and rate limiting

| Tool | Default concurrency | Adaptive backoff | Rate limit strategy |
|------|:---:|:---:|-----|
| **ocync** | 50 (AIMD adaptive) | Yes (per-registry, per-action) | AIMD congestion control on 429 |
| containerd | 5 layers | No | Lock-based dedup, no 429 handling |
| regsync | 3 per registry | No | Reads `ratelimit-remaining` header, pauses proactively |
| crane | 4 jobs | Retry with backoff (1s, 3s) | Retry on 408/429/5xx, 3 attempts |
| skopeo | per-image flag | No | No retry, aborts on 429 |

### Blob deduplication

| Tool | Cross-image push dedup | Cross-image pull dedup | Persistent cache |
|------|:---:|:---:|:---:|
| **ocync** | Yes (TransferStateCache) | No | Yes (postcard binary) |
| containerd | Yes (StatusTracker) | No | No |
| regsync | No | No | No (in-memory only) |
| crane | Yes (sync.Map by digest) | No | No |
| skopeo | No | No | BoltDB BlobInfoCache (mount hints) |

No tool deduplicates cross-image blob downloads from source. This is the primary remaining optimization opportunity: 168 redundant source GETs (5.6 GB) on the Jupyter corpus.

### Warm sync efficiency (Jupyter 5-image corpus)

| Tool | Requests | Bytes | Wall clock | How |
|------|:---:|:---:|:---:|-----|
| **ocync** | 81 | 371 KB | **2.5s** | Persistent cache skips blob I/O |
| regsync | **27** | **27 KB** | 4s | Manifest digest comparison only |
| dregsy | 200 | 163 KB | 5.2s | Re-HEADs all blobs |

## Registry behavior

### ECR blob mounting

ECR supports cross-repo blob mounting with the opt-in `BLOB_MOUNTING` account setting (launched January 2026). Mount POST returns 201 when:

1. `BLOB_MOUNTING` is `ENABLED` on the account
2. The source blob is referenced by a committed manifest (standalone blobs without manifests return 202)
3. The `from` parameter specifies the source repo name
4. Both repos have identical encryption config

Without `BLOB_MOUNTING` enabled, ECR returns 202 to every mount POST. ocync short-circuits mount attempts on ECR when the setting is not detected, saving one round-trip per shared blob.

ECR uses content-addressable storage at the S3 layer. Blobs uploaded to one repo are stored in a shared bucket, so HEAD for a blob digest returns 200 from any repo where it was previously referenced.

### Docker Hub rate limits (April 2025)

| Tier | Limit |
|------|-------|
| Anonymous | **10 pulls / hour / IP** |
| Authenticated free | **100 pulls / hour** |
| Pro/Team/Business | **Unlimited** (fair use) |

What counts as a pull: manifest GET only. Blob GETs and manifest HEADs are free. HEAD requests are subject to a separate abuse rate limit (~314 HEADs in 8 min triggers 429). Rate limit headers (`ratelimit-remaining`) appear on HEAD responses too.

Authentication is mandatory for any serious sync workload.

### ACR upload limit

Azure Container Registry enforces a ~20 MB request body limit on monolithic PUT. ocync falls back to chunked PATCH with `Content-Range` headers for larger blobs.

## Optimization backlog

Ranked by estimated impact.

| # | Optimization | Impact | Status |
|---|-------------|--------|--------|
| 1 | Cross-image blob download dedup | ~168 source GETs, ~5.6 GB on Jupyter corpus | Not started |
| 2 | Docker Hub authentication for source registries | 10x more pull quota (100/hr vs 10/hr) | Not started |
| 3 | BatchGetImage for warm sync | ~79 requests to ~1 call | Not started |
| 4 | Multi-scope Docker Hub tokens | 5 auth exchanges to 1 | Not started |
| 5 | Rate limit header parsing (proactive throttle) | Avoid 429s before they happen | Not started |
| 6 | HTTP/2 multiplexing verification | Connection reuse for 50 concurrent requests | Not started |
