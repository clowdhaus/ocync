---
title: Findings
description: Current benchmark results and registry behavior observations
order: 5
---

## Tool comparison

### Upload strategy

| Tool | Strategy | Requests/blob | Buffering |
|------|---------|:---:|-----------|
| **`ocync`** | POST + streaming PUT | **2** | None (streamed) |
| containerd | POST + streaming PUT | 2 | None (io.Pipe) |
| `regsync` (regclient) | POST + PUT | 2 | Full blob in memory |
| crane (go-containerregistry) | POST + streaming PATCH + PUT | 3 | None (streamed) |
| skopeo (containers/image) | POST + PATCH + PUT | 3 | None (streamed) |

### Concurrency and rate limiting

| Tool | Default concurrency | Adaptive backoff | Rate limit strategy |
|------|:---:|:---:|-----|
| **`ocync`** | 50 ([AIMD](./overview#adaptive-concurrency-aimd) adaptive) | Yes (per-registry, per-action) | [AIMD](./overview#adaptive-concurrency-aimd) congestion control on 429 |
| containerd | 5 layers | No | Lock-based dedup, no 429 handling |
| `regsync` | 3 per registry | No | Reads `ratelimit-remaining` header, pauses proactively |
| crane | 4 jobs | Retry with backoff (1s, 3s) | Retry on 408/429/5xx, 3 attempts |
| skopeo | per-image flag | No | No retry, aborts on 429 |

### Blob deduplication

| Tool | Cross-image push dedup | Cross-image pull dedup | Persistent cache |
|------|:---:|:---:|:---:|
| **`ocync`** | Yes (TransferStateCache) | No | Yes (postcard binary) |
| containerd | Yes (StatusTracker) | No | No |
| `regsync` | No | No | No (in-memory only) |
| crane | Yes (sync.Map by digest) | No | No |
| skopeo | No | No | BoltDB BlobInfoCache (mount hints) |

No tool deduplicates cross-image blob downloads from source. This is the primary remaining optimization opportunity: 168 redundant source GETs (5.6 GB) on the Jupyter corpus.

### Warm sync efficiency (Jupyter 5-image corpus)

| Tool | Requests | Bytes | Wall clock | How |
|------|:---:|:---:|:---:|-----|
| **`ocync`** | 81 | 371 KB | **2.5s** | Persistent cache skips blob I/O |
| `regsync` | **27** | **27 KB** | 4s | Manifest digest comparison only |
| `dregsy` | 200 | 163 KB | 5.2s | Re-HEADs all blobs |

### Error handling

`dregsy` occasionally logs `invalid character 'b' looking for beginning of value` during benchmark runs. This is a Go `encoding/json` error from `containers/image` (the library underlying skopeo, which dregsy wraps). Known upstream issue: `containers/image` calls `json.Unmarshal` on HTTP response bodies without checking Content-Type or status code first (containers/skopeo#360, containers/skopeo#7, xelalexv/dregsy#50). The error is non-fatal -- dregsy retries and continues -- but the log message includes no URL, status code, or registry name, making it unactionable for operators.

Observed during proxy-mediated benchmark runs. The bench-proxy streams response bodies unmodified (no content transformation), but the proxy has not been fully excluded as a contributing factor. To confirm: cross-reference proxy JSONL timestamps with dregsy error timestamps and verify the upstream returned a non-JSON response.

## Registry behavior

### ECR blob mounting

ECR supports cross-repo blob mounting with the opt-in `BLOB_MOUNTING` account setting (launched January 2026). Mount POST returns 201 when:

1. `BLOB_MOUNTING` is `ENABLED` on the account
2. The source blob is referenced by a committed manifest (standalone blobs without manifests return 202)
3. The `from` parameter specifies the source repo name
4. Both repos have identical encryption config

Without `BLOB_MOUNTING` enabled, ECR returns 202 to every mount POST. `ocync` always attempts mount regardless -- the 202 fallback is cheap (~100ms) and avoids needing to detect the account setting.

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

Azure Container Registry enforces a ~20 MB request body limit on monolithic PUT. `ocync` falls back to chunked PATCH with `Content-Range` headers for larger blobs.

## Optimization backlog

Ranked by estimated impact.

| # | Optimization | Impact | Status |
|---|-------------|--------|--------|
| 1 | Cross-image blob download dedup | ~168 source GETs, ~5.6 GB on Jupyter corpus | Done (`staging.rs`: `claim_or_check`) |
| 2 | Docker Hub authentication for source registries | 10x more pull quota (100/hr vs 10/hr) | Done (`docker.rs`: `DOCKER_HUB_ALIASES`) |
| 3 | BatchCheckLayerAvailability for warm sync | ~79 requests to ~1 call | Done (`ecr.rs`: `BatchBlobChecker`) |
| 4 | Multi-scope Docker Hub tokens | 5 auth exchanges to 1 | Done (`auth/mod.rs`: `scopes_cache_key`) |
| 5 | Rate limit header parsing (proactive throttle) | Avoid 429s before they happen | Not started |
| 6 | HTTP/2 multiplexing | Connection reuse for concurrent requests | Not started |
