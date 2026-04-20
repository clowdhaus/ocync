---
title: Watch mode
description: Discovery optimization via HEAD-before-GET, tag digest cache, and steady-state efficiency
order: 3
---

## The problem

Watch mode and CronJob deployments re-run the full sync pipeline every cycle. In steady state (everything already synced), each cycle pulls full source manifests for every tag, including all child manifests for multi-arch images, only to discover that nothing changed.

For a config with 50 repos x 20 tags x 5 platforms syncing to 3 ECR regions, that is 50 x 20 x (1 index GET + 5 child GETs) = 6,000 source manifest GETs per cycle, all wasted. Target HEAD checks add 50 x 20 x 3 = 3,000 more requests on top.

CronJob deployments have the same problem: each `ocync sync` invocation repeats all discovery work from scratch.

The goal is to skip expensive source manifest pulls when the source has not changed since the last sync, benefiting both watch mode (warm in-memory cache) and CronJob deployments (disk cache).

## Why naive HEAD comparison fails

A naive approach (HEAD the source, HEAD the targets, compare digests) fails when **platform filtering** is active.

Source HEAD returns the **unfiltered** index digest containing all platforms. After platform filtering, the engine rebuilds the index with only the matching platforms and recomputes the digest. Targets receive this **filtered** index with a **different** digest. Source HEAD digest != target HEAD digest, even when fully synced.

Since platform filtering (e.g., `platforms: [linux/amd64, linux/arm64]`) is the primary use case (Chainguard multi-arch images synced to ECR), a naive HEAD comparison would never skip anything.

## Tag digest cache

### Cache entry

After a successful sync, the engine caches a snapshot per tag keyed by `(source_authority, repo, tag)`. The snapshot records:

- **source_digest**: the digest returned by HEAD against the source (unfiltered for multi-arch)
- **filtered_digest**: the digest of the manifest that was actually pushed to targets (after platform filtering and index rebuild)
- **platform_filter_key**: a canonical representation of the active platform filter config (platforms sorted alphabetically, joined with commas; empty string when no filtering is active)

The cache key uses the source registry's `host:port` as the authority component. The URL scheme is excluded because no production registry serves both HTTP and HTTPS on the same port. The `Url::authority()` method is avoided because it includes userinfo (`user:password@host`), which would leak credentials into the cache file on disk and cause key instability when passwords rotate. Key components are NUL-separated to prevent ambiguity since OCI repository names contain `/`.

Multiple config mappings that sync the same source `(host, repo, tag)` to different targets share a single cache entry. If two mappings use different platform filters, their platform_filter_key values differ, causing one mapping's cache write to overwrite the other. The worst case is one extra source pull per cycle when mappings with different filters alternate. This is not a correctness bug, just reduced cache effectiveness. This is acceptable because same-source-different-filter mappings are uncommon.

### Optimized discovery flow

On subsequent cycles:

1. HEAD the source tag with a short timeout (default 5 seconds), a single cheap request
2. Look up the cache for `(source, repo, tag)`
3. If the HEAD digest matches the cached source_digest AND the platform filter key has not changed, the source is unchanged
4. HEAD all targets, comparing against the cached filtered_digest
5. If all targets match, skip entirely (saved M+1 full GETs for multi-arch)
6. If any target mismatches, fall through to the full source manifest pull and transfer for mismatched targets

For **unfiltered** and **single-arch** images, source_digest and filtered_digest are identical, so the cache works without platform awareness.

### Mixed fan-out behavior

When some targets match the cached filtered_digest and others do not, the engine performs one source pull shared across all mismatched targets. Targets that matched produce skip results directly without becoming transfer tasks. The pull-once invariant holds: one source pull, shared across all mismatched targets. After the full pull completes, the cache entry is updated with fresh values regardless of per-target push outcomes. Failed targets are caught by the next cycle's target HEAD verification.

### Cache update rules

- **Cache-hit skip** (all targets match): preserve the existing cache entry unchanged. No full pull was performed, but the cached filtered_digest remains correct because both source and config are unchanged.
- **Full pull completes** (cache miss, source changed, or target mismatch): update the cache immediately after the source pull succeeds, regardless of per-target push outcomes. The cache records what was pulled and filtered, not what targets confirmed received. Target-side correctness is guaranteed by per-cycle target HEAD verification; if a target push failed, the next cycle's target HEAD catches the mismatch and re-triggers transfer.
- **Pull failure**: do NOT create or update the cache entry. No filtered_digest is available, and writing a partial entry (HEAD digest without filtered digest) would cause incorrect skips on subsequent cycles.

The "update on successful pull" rule was chosen over "update only if at least one target push succeeded" because the engine processes targets independently with no return path from execution back to discovery. Since target HEADs re-verify every cycle, the simpler rule is both correct and architecturally clean.

### Platform filter config changes

If the user changes their `platforms:` config between syncs (e.g., adds `linux/arm64`), the platform_filter_key changes, forcing a cache miss even though the source digest is unchanged. Without this guard, the cache would incorrectly skip the sync and the new platform would never be transferred.

The key is computed by sorting platform strings alphabetically and joining with commas. This is deterministic across process restarts, Rust versions, and architectures without requiring a hash function.

### Deterministic serialization prerequisite

OCI manifest types contain annotation fields backed by hash maps, whose iteration order is randomized per process. In watch mode (single process) this is invisible, but for CronJob + PVC deployments where every pod is a new process, two pods filtering the same source index produce different JSON byte sequences and therefore different filtered_digest values. This defeats the cache for exactly the deployment model it is designed for.

The fix: use sorted maps for annotation fields on all OCI manifest types. This guarantees deterministic JSON serialization regardless of process lifetime or insertion order.

### Accept header prerequisite

Manifest HEAD requests must send the same Accept header as manifest GET requests. Without this, registries (especially Docker Hub) perform different content negotiation, where HEAD returns a Docker V2 manifest digest while GET returns an OCI index digest for the same tag. The cache would permanently miss on every cycle for multi-arch images, silently degrading to always-pull behavior with an extra HEAD per tag.

This is also a correctness fix independent of the optimization, since existing target HEAD checks have the same bug.

### Zero-platform filtering

When platform filtering is active but no source platforms match the filter (e.g., source has only `linux/s390x`, filter is `[linux/amd64]`), the engine must return an error rather than silently pushing an empty manifest list. This surfaces platform config mismatches immediately.

### skip_existing interaction

When both the tag digest cache and `skip_existing` could apply to the same target, `skip_existing` takes priority. The source HEAD still fires even when skip_existing is true because the cache entry needs the source digest for future cycles when skip_existing might be disabled.

The discovery cache hit/miss decision is orthogonal to the image skip reason. A tag with `skip_existing: true` where the cache matched still counts as a cache hit because the cache prevented the expensive source pull.

### SIGHUP cache clearing

> **Status: Planned.** SIGHUP handling is designed but not yet implemented. The tag digest cache and discovery optimization described in this document are implemented.

On SIGHUP config reload, the tag digest cache is cleared to force full source re-verification. Platform filter changes are detected by the platform_filter_key, but other config changes (source registry URL, repository mappings) could make cached entries stale. The blob dedup map is preserved across SIGHUP since blob existence is independent of config.

End-of-cycle pruning evicts entries for tags no longer present in the resolved mapping, preventing unbounded cache growth when source tags are deleted or tag filters change.

## Non-goals and rejected alternatives

### Webhook/event-driven sync

No cross-registry standard exists for push events. ECR supports EventBridge and GHCR has webhook events, but these are registry-specific, require additional infrastructure (SQS queues, webhook receivers), and do not cover the primary source (Chainguard). The polling model serves all registries uniformly.

### ETag/If-None-Match conditional GET

The standard HTTP mechanism for "has this resource changed?" cannot solve the platform filtering problem. When platform filtering is active, the filtered_digest differs from the source manifest digest, so a conditional GET against the source using the target's digest would always miss. For unfiltered single-arch images conditional GET would work, but the tag digest cache handles both filtered and unfiltered cases uniformly. The HEAD approach achieves comparable latency (~30ms headers-only) without requiring per-registry ETag support testing.

### ConfigMap-based cache persistence

Rejected due to etcd's 1 MiB size limit, write pressure on etcd, and race conditions with concurrent CronJob pods.

## Edge cases

### TOCTOU race

Between the source HEAD (returning digest A) and target HEADs, the source could update to digest B. The engine sees source=A, targets=A, and skips, missing the update. This is a missed optimization, not a correctness bug: the target has valid content (A), just not the latest (B). The next cycle detects the change because the source HEAD returns B while the cache has A. This window is inherent to any polling system.

A subtler variant exists: if the source updates A->B->A within a single poll interval, the cache could store a stale filtered_digest from the B-era pull. This requires two tag pushes reverting to the exact prior image within seconds, which is vanishingly rare in practice and self-correcting on the next source change.

### Broken Docker-Content-Digest headers

If a source registry returns inconsistent digests in HEAD responses, two failure modes apply:

- HEAD digest != cached digest: always falls through to full pull. Performance regression (extra HEAD per cycle) but not a correctness bug.
- HEAD digest == cached but wrong: targets have the correct content from the last full sync, so the skip is actually correct because the source content has not changed.

Target-side broken headers mirror the same analysis. ECR (the primary target) returns correct headers. Registries with inconsistent HEAD responses degrade to always-pull behavior. This is a performance regression, not a correctness bug.

### Optimization HEAD timeout

The optimization HEAD uses a short timeout (default 5 seconds) independent of the normal request timeout. Without this, a degraded-but-not-down source registry adds the full request timeout (potentially 30 seconds) per tag before falling through, turning a performance optimization into a performance regression. For 1,000 tags at 30 seconds, that is 8+ hours of wasted latency.

The timeout wraps the individual HEAD call, not the HTTP client's global timeout, so it does not affect normal request behavior.

### HEAD failure handling

Any HEAD failure (network error, HTTP 4xx/5xx, timeout, missing Docker-Content-Digest header) falls through to the full source manifest pull. The HEAD is an optimization, not a requirement. No retry is attempted on the HEAD itself; retries are reserved for the full pull which is the authoritative path.

### AIMD interaction

Optimization HEADs participate in the AIMD congestion window for the ManifestHead action. A 429 on the optimization HEAD shrinks the ManifestHead window normally. The fallthrough to full manifest pull operates on the independent ManifestPull AIMD window, so a throttled HEAD does not delay the authoritative GET path.

### Relationship to head_first

> **Status: Planned.** The `head_first` per-registry option is designed but not yet implemented. The discovery HEAD optimization described above is implemented.

The transfer optimization spec defines a per-registry `head_first` option to conserve rate-limit tokens. The discovery HEAD serves a different purpose: detecting source changes cheaply. These are independent features that happen to use the same HTTP method. When both are active, the discovery HEAD result is reused to avoid a redundant second HEAD.

## Steady-state savings

| Scenario | Before | After | Saved |
|----------|--------|-------|-------|
| Single-arch, all synced (warm cache) | 1 GET + N HEADs | 1 HEAD + N HEADs | 1 GET |
| Multi-arch (5 platforms), all synced (warm cache) | 1 index GET + 5 child GETs + N HEADs | 1 HEAD + N HEADs | 6 GETs |
| Multi-arch (5 platforms), 1 target stale | 6 GETs + N HEADs | 1 HEAD + N HEADs + 6 GETs | 0 (1 extra HEAD) |
| Cold cache (first run) | 6 GETs + N HEADs | 1 HEAD + 6 GETs + N HEADs | -1 HEAD |

The cold-cache first run adds 1 extra HEAD per tag, but this cost is repaid on the second cycle. The break-even point is cycle two.

**Wall-clock estimate** (Chainguard to 3-region ECR, 50 repos x 20 tags x 5 platforms):

- Source HEAD: ~30ms (headers only, ~200 bytes)
- Source GET (index): ~80ms (~2KB body)
- Source GET (child): ~60ms (~2KB body each)
- Steady-state cycle before: 50 x 20 x (80 + 5x60) = 50 x 20 x 380ms = ~380s of source requests
- Steady-state cycle after: 50 x 20 x 30ms = ~30s of source HEADs
- **~350s saved per cycle** (92% reduction in source-side request time)

Target HEADs are present in both paths and run concurrently, so they do not change the comparison.

## Cache behavior by deployment mode

| Mode | Cache state | Discovery cost (steady state) |
|------|-------------|-------------------------------|
| Watch (Deployment) | In-memory, always warm | 1 source HEAD + N target HEADs per tag |
| CronJob + PVC | Disk cache loads on start | 1 source HEAD + N target HEADs per tag |
| CronJob, no PVC | Cold cache every run | 1 source HEAD + N target HEADs + full pull if any target misses |

In watch mode, the cache lives in memory across cycles and persists to disk only on graceful shutdown (SIGTERM/SIGINT). This avoids per-cycle disk I/O while preserving crash recovery. A pod that receives SIGTERM writes its cache, and the replacement pod loads it on startup. An ungraceful kill (SIGKILL, OOM) loses the in-memory cache, but the prior graceful shutdown's cache file remains on disk.

For CronJob + PVC deployments, the pod mounts a ReadWriteOnce PVC and the cache persists between Job runs. This requires `concurrencyPolicy: Forbid` to prevent two pods from mounting simultaneously. Not available on EKS Fargate (no EBS PVCs), so those environments start cold, which is acceptable since the HEAD optimization still avoids full pulls when targets are already in sync.

### Cache format versioning

The tag digest cache extends the existing transfer state cache format. The cache version will increment from v1 to v2 when the source snapshot map is added, with the new source snapshot map appended after the existing blob dedup map. Reads accept both v1 and v2: a v1 cache loads with an empty snapshot map, preserving blob dedup data across the version transition. An older binary encountering a v2 cache falls back to empty cache, which is the existing behavior for version mismatches. The same TTL and atomic write (tmp + rename + fsync) apply to both sections.

### Observability

Discovery counters are reported in the sync summary and `--json` output:

- **discovery_cache_hits**: tags where the HEAD optimization avoided the full source pull entirely
- **discovery_cache_misses**: tags where a full source manifest pull was attempted (cold cache, source changed, config changed, HEAD failed, or target stale despite source cache match)
- **discovery_head_failures**: subset of misses caused by infrastructure issues (network errors, HTTP errors, timeouts, missing digest headers)
- **discovery_target_stale**: subset of misses where the source cache matched but target HEAD returned a mismatch

The sum of cache_hits and cache_misses always equals the total tag count. Operators can distinguish "source changed" (healthy) from "HEAD broken" (unhealthy) and from "target stale" (check ECR lifecycle policies or GC schedules).
