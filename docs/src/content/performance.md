---
title: Performance
description: How ocync achieves 8x faster sync than alternatives with fewer bytes transferred and zero rate-limit errors.
order: 6
---

## Benchmark results

On a 42-image / 55-tag cold sync to ECR (c6in.4xlarge):

| Tool | Wall time | Requests | Bytes | 429s |
|---|---|---|---|---|
| ocync | **4m 33s** | **4,131** | **16.9 GB** | **0** |
| regsync | 16m 06s | 7,719 | 35.4 GB | **0** |
| dregsy | 36m 22s | 11,190 | 43.4 GB | 49 |

ocync is 8x faster than dregsy and 3.5x faster than regsync, with 2-2.6x fewer bytes transferred and zero rate-limit errors.

## Why ocync is fast

### Pipelined architecture

Discovery and execution overlap. While images are being transferred, ocync is already discovering the next batch of work. There is no idle time between phases.

### Global blob deduplication

Container images share layers heavily. A sync run touching 42 images may reference only a fraction as many unique blobs. ocync tracks every blob globally and transfers each unique blob exactly once.

### Cross-repo blob mounting

When a blob already exists in another repository on the same registry, ocync mounts it (a server-side copy) instead of uploading. A leader-follower election algorithm ensures exactly one image uploads each shared blob while all others mount from the leader. One image is elected leader for each shared blob and performs the actual upload; followers wait and then mount from the leader's repository.

### Adaptive rate limiting

Per-(registry, action) AIMD (additive increase, multiplicative decrease) concurrency windows discover actual registry capacity through feedback, using the same algorithm TCP uses for congestion control. ECR has 9 independent rate limits (one per API action), and ocync tracks each independently. This avoids both under-utilization and 429 errors.

### Transfer state cache

A persistent cache records which blobs exist at each target. Subsequent sync runs skip redundant HEAD checks for known-good blobs, reducing API calls on warm runs.

### Streaming transfers

In single-target mode, bytes flow directly from source to target with no intermediate disk buffering. In multi-target mode, blobs stage to disk once and push to all targets in parallel.

## Tuning

ocync auto-discovers registry capacity through AIMD feedback. Manual tuning is rarely needed. The key settings (see [configuration](../configuration) for full reference):

- **Concurrency**: starts conservatively and ramps up. Each registry action has its own window. Override with `global.max_concurrent_transfers` or per-registry `max_concurrent`.
- **Platform filter**: sync only the architectures you deploy. `linux/amd64` alone halves transfer volume for multi-arch images.
- **Tag filter**: use `latest: N` and `semver` ranges to sync only what you need.
- **Transfer state cache**: enabled by default. Warm runs transfer only new or changed blobs. Control TTL with `global.cache_ttl`.
