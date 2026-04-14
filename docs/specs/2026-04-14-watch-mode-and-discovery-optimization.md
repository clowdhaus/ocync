# Watch Mode and Discovery Optimization

## Problem

The `watch` command re-runs the entire sync pipeline every cycle. In steady state (everything synced), each cycle pulls full source manifests for every tag — including all child manifests for multi-arch images — only to discover that nothing changed. For a config with 50 repos x 20 tags x 5 platforms syncing to 3 ECR regions, that's 50 x 20 x (1 index GET + 5 child GETs) = 6,000 source manifest GETs per cycle, all wasted. Target HEAD checks add 50 x 20 x 3 = 3,000 HEADs on top.

CronJob deployments have the same problem: each `ocync sync` invocation repeats all discovery work from scratch.

## Goals

1. Skip expensive source manifest pulls when the source hasn't changed since last sync
2. Benefit both `watch` (warm in-memory cache) and `sync` CronJobs (disk cache)
3. Provide a health endpoint for watch mode K8s Deployments
4. No new crate dependencies

## Non-goals

- Webhook/event-driven sync — no cross-registry standard exists; polling is the only universal approach. ECR supports EventBridge push events and GHCR has webhook events, but these are registry-specific, require additional infrastructure (SQS queues, webhook receivers), and don't cover the primary source (Chainguard). For ECR-to-ECR cross-region use cases, EventBridge could be explored as a future enhancement, but the polling model serves all registries uniformly.
- ETag/If-None-Match conditional GET — the standard HTTP mechanism for "has this resource changed?" would avoid body transfer for unchanged manifests. However, it cannot solve the platform filtering problem: when platform filtering is active, `filtered_digest` (filtered) differs from the source manifest digest, so a conditional GET against the source using the target's digest would always miss. For unfiltered single-arch images, conditional GET would work, but the primary use case (multi-arch with platform filtering) requires the tag digest cache approach regardless. Adding conditional GET as a separate optimization for the unfiltered path would add complexity for a subset of users while the tag digest cache already handles both filtered and unfiltered cases uniformly. The HEAD approach achieves comparable latency (~30ms headers-only) without requiring per-registry ETag support testing.
- Prometheus metrics endpoint — deferred to a separate spec; initial observability uses `SyncStats` counters in `--json` structured output and `tracing` logs
- Progress bars — deferred; primary deployment is CI/CD and K8s where structured output is more useful than interactive progress
- ConfigMap-based cache persistence — 1 MiB etcd size limit, write pressure on etcd, race conditions with concurrent CronJob pods

## Implementation order

1. **Section 1** (Accept header fix) — prerequisite for section 2; correctness fix independent of optimization
2. **Section 2 prerequisites** (deterministic serialization via BTreeMap annotations, zero-platform error) — correctness fixes independent of optimization; blocking prerequisites for section 2's CronJob cache path
3. **Section 2** (HEAD-before-GET optimization) — core optimization; requires section 1 and prerequisites
4. **Section 3** (Cache format extension) — persists section 2's results to disk for CronJob deployments
5. **Section 4** (Observability) — wires section 2's hit/miss signals into SyncStats
6. **Section 5** (Watch mode) — already implemented; requires cache parameter change to `synchronize::run()` and health server resilience fix
7. **Section 6** (Cache behavior table) — documentation only, no code changes

## Design

### 1. Fix: Accept headers on manifest HEAD (prerequisite for section 2)

**Problem:** `manifest_head()` (manifest.rs:68) calls `self.head()` which sends **no Accept header**. `manifest_pull()` sends the full OCI/Docker media type list via `manifest_accept_header()`. Registries (especially Docker Hub) return different content types — and therefore different digests — depending on Accept header content negotiation. A HEAD without Accept may return a Docker V2 manifest digest while GET with Accept returns an OCI index digest for the same tag.

**Fix:** Add an `accept` parameter to the `head()` method in `client.rs` (matching the `get()` signature which already has `accept: Option<&str>`). Update `manifest_head()` to pass `manifest_accept_header()`. This ensures HEAD and GET always negotiate the same content type.

**Blocking dependency:** Section 2 (HEAD-before-GET optimization) **cannot work correctly without this fix**. The optimization compares source HEAD digests against cached GET digests. If HEAD and GET negotiate different content types, they return different digests — the cache will permanently miss on every cycle for multi-arch images, silently degrading to always-pull behavior with an extra HEAD per tag. This fix must be implemented and tested before the optimization layer.

**Impact:** This is a correctness fix independent of the optimization — the existing target HEAD checks in `discover_tag()` have the same bug. Target HEADs without Accept could return different digests than what was pushed via GET with Accept.

**Files:**
- `crates/ocync-distribution/src/client.rs` — add `accept: Option<&str>` to `head()` (4 call sites: manifest.rs:75, blob.rs:88, and 2 in client_integration.rs)
- `crates/ocync-distribution/src/manifest.rs` — pass `Some(&manifest_accept_header())` to `manifest_head()`
- `crates/ocync-distribution/src/blob.rs` — pass `None` to `blob_exists()` (blob HEAD checks don't need Accept)

### 2. Discovery optimization: HEAD-before-GET with tag digest cache

#### The core problem

`discover_tag()` currently:
1. Calls `pull_source_manifest()` — 1 index GET + M child GETs for multi-arch (expensive)
2. Gets `source_digest` from the pulled manifest data
3. HEAD-checks all N targets against `source_digest`
4. If all match → Skip

The full source pull (step 1) is wasted in steady state. We want to skip it when the source hasn't changed.

#### Why a simple HEAD comparison doesn't work

A naive approach — HEAD source, HEAD targets, compare digests — fails when **platform filtering** is active:

- Source HEAD returns the **unfiltered** index digest (all platforms)
- `pull_source_manifest()` filters to matching platforms, rebuilds the index, recomputes the digest
- Targets receive the **filtered** index with a **different** digest
- Source HEAD digest ≠ target HEAD digest, even when fully synced

Since platform filtering (e.g., `platforms: [linux/amd64, linux/arm64]`) is the primary use case (Chainguard multi-arch → ECR), a naive HEAD comparison would never skip.

#### Solution: tag digest cache

After a successful sync, cache:
```
(source_authority, repo, tag) → SourceSnapshot {
    source_digest: Digest,       // what HEAD returns (unfiltered)
    filtered_digest: Digest,       // what was pushed to targets (may be filtered)
    platform_filter_key: String,  // canonical sorted platform filter string ("" if none)
}
```

**Source registry identifier:** The cache key requires a source registry identifier. `RegistryClient.base_url` is `pub(crate)` within `ocync-distribution`, inaccessible from `ocync-sync`. Add a `pub fn registry_authority(&self) -> String` accessor to `RegistryClient` that returns `host:port` (e.g., `"registry.example.com:8443"` or `"docker.io"`). Implementation: extract `host_str()` and `port_or_known_default()` from `self.base_url`, returning `Err` if the host is missing (a URL without a host is not a valid registry endpoint). Format as `"{host}:{port}"`. The method returns `Result<String, Error>` — a missing host is a bug in client construction, not a runtime condition to paper over with a fallback. Note: the cache key does not include the URL scheme; `http://reg:443` and `https://reg:443` would produce the same key. This is acceptable because no production registry serves both HTTP and HTTPS on the same port — document this assumption in the method's doc comment. Do NOT use `Url::authority()` which includes userinfo (`user:password@host`) — this would leak credentials into the cache file on disk and cause cache key instability when passwords rotate. The accessor returns `String` (not `&str`) because the `host:port` format requires formatting. Add a corresponding `source_authority: String` field to `ResolvedMapping` (populated from `source_client.registry_authority()` during mapping resolution in `synchronize.rs`) and propagate it through `DiscoveryParams`.

On the next cycle:
1. HEAD source → get `current_source_digest` (1 cheap request)
2. Look up cache: `(source, repo, tag) → SourceSnapshot`
3. If `current_source_digest == cached.source_digest` AND `current_platform_key == cached.platform_filter_key` → source hasn't changed and config hasn't changed
4. HEAD all targets → compare to `cached.filtered_digest` (the filtered digest)
5. All targets match → **Skip** (saved M+1 full GETs)
6. Any target mismatch → fall through to full `pull_source_manifest()` + transfer

**Platform filter config change:** If the user changes `platforms:` between syncs (e.g., adds `linux/arm64`), the `platform_filter_key` changes, forcing a cache miss even though the source digest is unchanged. Without this, the cache would skip the sync and the new platform would never be transferred — a correctness bug.

**Platform filter key determinism:** The key is computed by sorting platform strings alphabetically and joining with commas (e.g., `"linux/amd64,linux/arm64"`). This is trivially deterministic across process restarts, Rust versions, and platforms — no hash function needed. Empty string for `None` or empty slice.

```rust
/// Compute the canonical platform filter key for cache comparison.
///
/// Sorts platforms alphabetically and joins with comma. Returns empty
/// string for `None` or empty slice (no platform filtering).
pub fn platform_filter_key(filters: Option<&[PlatformFilter]>) -> String {
    let Some(filters) = filters else { return String::new() };
    if filters.is_empty() { return String::new(); }
    let mut sorted: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
    sorted.sort();
    sorted.join(",")
}
```

For **unfiltered** images (no platform filtering): `source_digest == filtered_digest` and `platform_filter_key is empty`, so the cache trivially works without platform awareness.

For **single-arch** images: `source_digest == filtered_digest` (no index to filter), same trivial case.

#### Prerequisite: deterministic filtered index serialization

When platform filtering rebuilds an index manifest, `pull_source_manifest()` serializes the filtered `ImageIndex` via `serde_json::to_vec()` and computes `filtered_digest` from the result (engine.rs:876-882). Both `ImageIndex` and `Descriptor` contain `annotations: Option<HashMap<String, String>>` fields. `HashMap` iteration order in Rust is randomized per-process (`RandomState` hasher), so two different processes filtering the same source index can produce different JSON byte sequences — and therefore different `filtered_digest` values — even though the logical content is identical.

In **watch mode** (single process), this is invisible: `filtered_digest` computed in cycle 1 stays consistent in memory. But for the **CronJob + PVC deployment model**, every pod is a new process. The cache written by pod N has `filtered_digest = sha256:abc...`, but pod N+1 re-serializes the same filtered index with a different HashMap order, producing `sha256:def...`. When pod N+1's full pull completes, the new `filtered_digest` differs from what targets received from pod N, causing a spurious target mismatch on every cycle. The optimization is defeated for exactly the use case it's designed for.

**Fix (prerequisite, in `spec.rs`):** Change the `annotations` field type from `Option<HashMap<String, String>>` to `Option<BTreeMap<String, String>>` on `ImageIndex` (spec.rs:348), `Descriptor` (spec.rs:136), and `ImageManifest` (spec.rs:324). All three OCI types carry annotations — changing only two would leave an inconsistency that could cause non-deterministic serialization if `ImageManifest` is ever re-serialized for digest computation. `BTreeMap` iterates in sorted key order, guaranteeing deterministic JSON serialization via `serde_json::to_vec()` regardless of insertion order or process lifetime. This is a type change across the crate — all code that constructs or accesses annotations must use `BTreeMap` — but annotations are small, rarely constructed, and `BTreeMap` has equivalent API for the read-heavy access patterns in this codebase. Grep the full codebase for `annotations` usage and update all sites.

This fix is independent of the optimization — it also affects target manifest content determinism — but is a **blocking prerequisite** for the CronJob + PVC cache path.

#### Bundled fix: zero-platform filtering must error

When platform filtering is active but **no source platforms match the filter** (e.g., source has only `linux/s390x`, filter is `[linux/amd64]`), `pull_source_manifest()` currently produces an index with `manifests: vec![]` — an empty manifest list. This empty index gets serialized, pushed to targets, and cached. Targets silently receive an index with no platform entries, which is not a valid sync result.

**Fix (in `pull_source_manifest()`):** After filtering, if `!index.manifests.is_empty() && descriptors.is_empty() && platforms.is_some()`, return an error: `"platform filter {filter_list} matched no manifests in index for {repo}:{tag} (source has: {source_platform_list})"`. The `!index.manifests.is_empty()` guard prevents a false positive on genuinely empty source indexes (unusual but valid per the OCI image index spec). This surfaces platform config mismatches immediately rather than silently pushing empty manifests. The error propagates as a sync failure for that tag, visible in the report and `--json` output.

#### Optimized discover_tag() flow

```
1. tokio::time::timeout(discovery_head_timeout,
     source_client.manifest_head(repo, tag))  // 1 HEAD with Accept headers, short timeout
2. if HEAD fails OR times out → signal discovery_head_failure, fall through to full pull (no retry — optimization, not requirement)
3. source_digest = head_result.digest
4. platform_key = platform_filter_key(current_filters)    // canonical sorted string, "" if no filtering
5. cache_entry = tag_cache.get(source, repo, tag)          // scoped borrow, dropped before any .await
6. if cache miss
   OR source_digest != cache_entry.source_digest
   OR platform_key != cache_entry.platform_filter_key:
     → full pull_source_manifest() + existing logic (source changed, config changed, or first run)
7. compare_digest = cache_entry.filtered_digest
8. for each target: manifest_head()           // N HEAD requests with Accept headers (concurrent)
9. all match compare_digest → Skip (preserve existing cache entry — do NOT overwrite if all targets match)
10. any mismatch → fall through to full pull_source_manifest() + transfer for mismatched targets.
    Note: in the cache-hit path (steps 7-9), source_data does NOT exist yet — only the
    cached filtered_digest was used for target HEAD comparison. Step 10 re-enters the existing
    pull path: call pull_source_manifest() to get source_data, then create TransferTask
    entries for mismatched targets with the freshly-pulled Rc<PulledManifest>. Targets that
    matched in step 8 produce ImageResult::Skipped entries directly — they never become
    TransferTask entries. The pull-once invariant holds: one source pull, shared across all
    mismatched targets via Rc.
    After the full pull completes, update the cache entry with fresh values (step 6 update
    rule) regardless of per-target push outcomes. The cache records what was PULLED and
    FILTERED, not what targets confirmed received. Target-side correctness is guaranteed by
    step 8's per-cycle target HEAD verification — if a target push failed, the next cycle's
    target HEAD will find the mismatch and re-trigger transfer. Updating the cache after a
    successful pull avoids an unnecessary source re-pull on the next cycle when the source
    hasn't changed.
```

**RefCell discipline:** Cache borrows (`cache.borrow()` / `cache.borrow_mut()`) must be scoped and dropped before any `.await` point. The existing engine code follows this pattern (e.g., engine.rs:1004-1013). Copy the pattern:
```rust
let cached = {
    let c = cache.borrow();
    c.source_snapshot(source, repo, tag).cloned()
};  // borrow dropped before manifest_head().await
```

#### Error handling

- **Source HEAD fails** (network error, 401, 5xx): log at `debug` level, fall through to full pull. The HEAD is an optimization — failure means "we can't shortcut, proceed normally." No retry on the source HEAD itself; retries are reserved for the full pull which is the authoritative path.
- **Source HEAD returns 404**: tag was listed but has no manifest (transient state). Fall through to full pull, which will surface the real error.
- **Source HEAD returns 200 but missing `Docker-Content-Digest` header**: treat as HEAD failure (log at `debug`, fall through to full pull). The current `manifest_head()` implementation (manifest.rs:84-89) returns an error on invalid/missing digest headers — this error is caught by the optimization and triggers the fallthrough path, same as any other HEAD failure. No special handling needed beyond ensuring the error doesn't propagate as a sync failure.
- **Cache miss**: first sync for this tag. Full pull, populate cache after success.

#### Optimization HEAD timeout

The optimization HEAD must use a **short timeout** (default 5 seconds) independent of the normal request timeout. Without this, a degraded-but-not-down source registry adds the full request timeout (potentially 30s) per tag before falling through — turning a performance optimization into a performance regression. For 1,000 tags at 30s timeout, that's 8+ hours of wasted latency.

The timeout is configurable via a builder method on the engine config:

```rust
/// Timeout for the optimization source HEAD request.
///
/// This HEAD is a performance optimization, not a correctness requirement.
/// If the HEAD doesn't complete within this duration, the engine falls
/// through to the full source manifest pull. Default: 5 seconds.
pub fn discovery_head_timeout(mut self, timeout: Duration) -> Self {
    self.discovery_head_timeout = timeout;
    self
}
```

The optimization HEAD request should use `tokio::time::timeout()` wrapping the `manifest_head()` call, not a per-request HTTP timeout, so it doesn't affect the client's normal timeout configuration.

#### AIMD interaction

Optimization source HEADs are real HTTP requests to the source registry. They use the `ManifestHead` registry action and participate in the same AIMD congestion window as all other HEAD requests — no special treatment. A 429 on the optimization HEAD shrinks the `ManifestHead` window normally. The fallthrough to full `manifest_pull()` (which uses `ManifestPull` action) operates on its own independent AIMD window, so a throttled HEAD does not delay the authoritative GET path.

#### Relationship to `head_first` (transfer optimization spec)

The transfer optimization spec defines a per-registry `head_first` config option to conserve rate-limit tokens at the source by replacing GETs with HEADs. The discovery HEAD in this spec serves a different purpose: populating the tag digest cache to skip full manifest pulls when the source hasn't changed.

These are **independent features** that happen to use the same HTTP method:

- **Discovery HEAD** (this spec): unconditional, short timeout (`discovery_head_timeout`), graceful fallback to full GET on any failure. Purpose: detect source changes cheaply.
- **`head_first`** (transfer optimization spec): opt-in per registry, for rate-limited sources where HEADs are free but GETs consume tokens. Purpose: conserve rate-limit tokens on the authoritative GET path.

When both features are active, the discovery HEAD result can be reused by `head_first` logic to avoid a second HEAD. When the discovery cache is warm and all targets match, `head_first` is irrelevant because the full pull is skipped entirely. When the discovery cache misses, the HEAD result from step 1 is available to the `head_first` logic in the pull path. The implementation must not issue a redundant second HEAD when both features apply to the same tag.

#### TOCTOU race

Between source HEAD (digest A) and target HEADs, the source could update to digest B. We'd see source=A, targets=A, skip — missing the update. This is a **missed optimization** (per CLAUDE.md terminology), not a correctness bug: the target has valid content (A), just not the latest (B). The next cycle detects the change because the source HEAD returns B while the cache has A. This window is inherent to any polling system and is acceptable.

**A→B→A tag reversal:** A more subtle race exists: if between cycle N's HEAD (returns digest A) and the full GET in step 10, the source updates to B, and then before cycle N+1 reverts to A, the cache stores `{source_digest: A, filtered_digest: sha256(filtered(B))}`. Cycle N+1's HEAD returns A (matching cache), and targets have B's content (matching `filtered_digest`), so the cache skips — but source now has A while targets have B. This requires a tag overwrite reverting to prior content within a single poll interval, which is vanishingly rare in practice (it requires two tag pushes to the same tag within seconds, reverting to the exact prior image). The scenario is self-correcting on the next source change. Accepted as an inherent limitation of polling-based change detection with cached state.

#### Registries with broken Docker-Content-Digest headers

If a source registry returns an incorrect digest in the HEAD response, two failure modes:
- HEAD digest ≠ cached digest → always falls through to full pull. Performance regression (extra HEAD per cycle) but not a correctness bug.
- HEAD digest == cached but wrong → targets have the correct content from the last full sync; the skip is actually correct because the source content hasn't changed (the digest is wrong but consistent).

The optimization assumes `manifest_head()` returns consistent digests for unchanged content. Registries with inconsistent HEAD responses degrade to always-pull behavior. A diagnostic `debug!` log when HEAD digest differs from GET digest after a fallback would help detect this in production.

#### Target-side broken Docker-Content-Digest headers

If a **target** registry returns an incorrect digest in HEAD responses, the cache-hit path compares `cached.filtered_digest` against the wrong target HEAD digest. Two failure modes mirror the source-side analysis:

- Target HEAD digest ≠ `cached.filtered_digest` → always falls through to full pull. Performance regression (unnecessary source pull every cycle) but not a correctness bug — the re-sync pushes the correct manifest.
- Target HEAD digest == `cached.filtered_digest` but wrong → the target actually has the correct content (it was pushed by a previous sync), so the skip is correct. The digest in the header is wrong but the content matches.

ECR (the primary target) returns correct `Docker-Content-Digest` headers. This degradation mode is relevant mainly for self-hosted registries with non-conformant implementations. The same diagnostic `debug!` log applies: log when a target HEAD returns a digest that differs from what was pushed after a full sync.

### 3. Cache format extension

**Current format** (`CACHE_VERSION = 1`):
```
[header_len][CacheHeader][BlobDedupMap][CRC32]
```

**New format** (`CACHE_VERSION = 2`):
```
[header_len][CacheHeader][BlobDedupMap][SourceSnapshotMap][CRC32]
```

`SourceSnapshotMap` is `HashMap<String, SourceSnapshot>` serialized with postcard. The key is `"{source_authority}\0{repo}\0{tag}"` (NUL-separated to avoid ambiguity — repo names contain `/`). The authority includes host and port (e.g., `"registry.example.com:8443"`) to prevent cache key collisions for registries on different ports.

**Shared key across mappings:** Multiple config mappings that sync the same source `(host, repo, tag)` to different targets share a single cache entry. If two mappings have different `platforms:` filters, their `platform_filter_key` values differ, causing one mapping's cache write to overwrite the other's entry. This is correct: on the next cycle, the first mapping sees a key mismatch (cache miss) and re-pulls. The worst case is one extra source pull per cycle when mappings with different filters alternate — not a correctness bug, just reduced cache effectiveness. This is acceptable because same-source-different-filter mappings are uncommon.

`SourceSnapshot`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceSnapshot {
    /// Source manifest digest as returned by HEAD (unfiltered for multi-arch).
    source_digest: Digest,
    /// Digest of the filtered manifest intended for targets.
    /// This is `pulled_manifest.pull.digest` after platform filtering and index rebuild.
    /// Records what was pulled and filtered, not what targets confirmed received —
    /// target-side correctness is ensured by step 8's per-cycle target HEAD verification.
    filtered_digest: Digest,
    /// Canonical platform filter key active when this entry was written.
    /// Computed via `platform_filter_key()` (platforms sorted alphabetically, joined
    /// with commas, e.g., `"linux/amd64,linux/arm64"`). Empty string means no platform
    /// filtering. Used to invalidate the cache when the user changes their `platforms:`
    /// config between syncs.
    platform_filter_key: String,
}
```

**Version migration:** The version check (cache.rs:225) now accepts both v1 and v2:
- **v1 → v2:** Use `postcard::take_from_bytes::<BlobDedupMap>(body)` instead of `postcard::from_bytes`. This returns `(BlobDedupMap, &[u8])` — the deserialized map and the remaining unconsumed bytes. If the remaining slice is empty (v1 format), return an empty `SourceSnapshotMap`. If remaining bytes exist (v2 format), deserialize them as `SourceSnapshotMap` via `postcard::from_bytes`. Note: `postcard::from_bytes` consumes exactly the needed bytes and rejects trailing data, so it cannot be used for the first section when a second section may follow — `take_from_bytes` is required.
- **v2 → v1 binary:** An older binary sees `version = 2`, fails the version check (cache.rs:225-233), and falls back to empty cache (existing behavior).
- The version constant stays at `CACHE_VERSION = 2` for writes. Reads accept `1` or `2`.

**Persistence:** Tag digest entries are written alongside blob dedup entries. The same `max_age` TTL applies. The same atomic write (tmp + rename + fsync) applies. **End-of-cycle pruning:** after each sync cycle, tag digest entries for tags no longer present in the resolved mapping's filtered tag list are evicted from the cache. This prevents unbounded cache growth when source tags are deleted or tag filters change.

**Cache update points:**

- **Cache-hit skip** (step 9 in the flow): **preserve the existing cache entry unchanged**. The full pull was never performed, so no fresh filtered digest is available. The cached `filtered_digest` is still correct because the source hasn't changed (verified by HEAD) and the platform config hasn't changed (verified by key).

- **Full pull completes** (step 10, or step 6 on cache miss/source change): record `(source, repo, tag) → { source_digest: head_digest, filtered_digest: source_data.pull.digest, platform_filter_key: platform_filter_key(current_filters) }`. Here `source_data.pull.digest` is the digest from `PulledManifest` after platform filtering and index rebuild — this is what targets should receive. Update **immediately after the source pull succeeds**, regardless of per-target push outcomes. The cache records what was pulled and filtered, not what targets confirmed received. Target-side correctness is guaranteed by step 8's per-cycle target HEAD verification.

  **Design rationale:** An alternative "update only if at least one target push succeeded" rule was considered and rejected. The engine processes targets independently via `FuturesUnordered` with no return path from `execute_item()` to `discover_tag()` — implementing per-tag aggregation of target outcomes would require adding an aggregation layer that doesn't exist. Since step 8 re-verifies targets every cycle via HEAD, the simpler "update on successful pull" rule is both correct and implementable within the current architecture. If all target pushes fail, the next cycle's target HEADs catch the staleness and re-trigger transfer.

- **Mixed fan-out** (some targets skipped, some transferred): the full pull was performed (needed for the stale targets), so `source_data.pull.digest` is available. Update the cache entry with fresh values. Per-target push outcomes don't affect the cache — failed targets are caught by step 8 on the next cycle.

- **Pull failure** (step 6 triggers full pull but `pull_source_manifest()` fails): do NOT create or update the cache entry. No `filtered_digest` is available, so writing a partial entry (HEAD digest without synced digest) would cause incorrect skips on the next cycle. The existing cache entry (if any) remains unchanged — it will either match the next HEAD (if the source didn't change) or miss (if it did). This is the correct behavior: a failed pull means "we don't know the current state, so don't cache anything."

The cache must be passed to `discover_tag()` (currently only `execute_item()` receives it). Add `cache: Rc<RefCell<TransferStateCache>>` to `DiscoveryParams` (owned `Rc`, not a reference — `DiscoveryParams` is an owned-data struct consumed by `async move` blocks, so it cannot hold references without lifetimes). Clone the `Rc` when constructing `DiscoveryParams`, matching the existing pattern used for `execute_item()` (engine.rs:401). Also add `source_authority: String` to `DiscoveryParams` (populated from `ResolvedMapping.source_authority`) for constructing cache keys. Also add `discovery_head_timeout: Duration` to `DiscoveryParams` (populated from the engine config) for the `tokio::time::timeout` wrapping the optimization HEAD. The platform filter set is already available via `DiscoveryParams.platforms`.

#### skip_existing interaction

When both the tag digest cache and `skip_existing` could apply to the same target, `skip_existing` takes priority. The execution order in the existing `discover_tag()` (engine.rs:600-621) already evaluates `skip_existing` first (via the `Ok(Some(_)) if skip_existing =>` match arm), before checking digest equality. The optimization HEAD path preserves this ordering:

1. Source HEAD + cache check (steps 1-6 above) determines whether we need target HEADs at all
2. Target HEADs are evaluated with the existing priority: `skip_existing` → digest match → active transfer
3. If `skip_existing` triggers, the image status is `Skipped { reason: SkipReason::SkipExisting }`, not `DigestMatch`
4. The source HEAD still fires even when `skip_existing` is true — this is intentional, because the cache entry needs the source digest for future cycles when `skip_existing` might be disabled

Stats attribution: `discovery_cache_hits` counts tags where the source HEAD matched the cache and no full source manifest pull was needed — regardless of why the image was ultimately skipped. A tag with `skip_existing: true` where the cache matched still counts as a cache hit because the cache prevented the expensive source pull. The *image skip reason* (`SkipExisting` vs `DigestMatch`) is orthogonal to the *discovery optimization outcome* (cache hit vs miss). `discovery_cache_misses` counts tags where a full source pull was required (cache miss, source changed, HEAD failed, or HEAD timed out).

### 4. Observability

**Problem:** With the HEAD optimization, a sync cycle where everything is skipped looks identical to a cycle where the optimization is broken and silently suppressing syncs. Operators need visibility.

**Solution:** Add discovery counts to `SyncStats`:

```rust
pub struct SyncStats {
    // ... existing fields ...
    /// Tags where the HEAD optimization avoided the full source pull entirely —
    /// source HEAD matched cache, config unchanged, AND all target HEADs matched
    /// the cached `filtered_digest`. No source manifest GET was performed.
    pub discovery_cache_hits: u64,
    /// Tags where the discovery cache could not shortcut — a full source manifest
    /// pull was attempted. This counts the cache *decision* (could not avoid the
    /// full pull path), not whether the subsequent pull succeeded or failed.
    /// Reasons include: cold cache (first run), source digest changed, platform
    /// config changed, source HEAD failed/timed out, or target HEAD mismatch
    /// (target stale despite source-side cache match). In the target-stale case,
    /// the source-side cache matched but a full pull was still needed to re-push
    /// to stale targets. Operators seeing a high miss count with zero head failures
    /// should check target-side health (lifecycle policies, manual overwrites)
    /// rather than assuming the cache is broken.
    /// `discovery_cache_hits + discovery_cache_misses == total_unique_tags` always.
    pub discovery_cache_misses: u64,
    /// Tags where the source HEAD request failed to return a usable digest.
    /// Includes: network errors, HTTP 4xx/5xx, timeouts (exceeded `discovery_head_timeout`),
    /// and missing/invalid `Docker-Content-Digest` headers.
    /// Subset of `discovery_cache_misses` — separates infrastructure failures from
    /// legitimate misses (cold start, source changed, config changed).
    /// A sustained high ratio (`discovery_head_failures / discovery_cache_misses`)
    /// indicates source registry degradation.
    pub discovery_head_failures: u64,
    /// Tags where the source-side cache matched (source HEAD digest unchanged,
    /// platform config unchanged) but at least one target HEAD returned a mismatch,
    /// forcing a full source pull to re-push stale targets.
    /// Subset of `discovery_cache_misses` — separates target-side staleness
    /// (lifecycle policies, manual overwrites, GC) from source-side changes.
    /// A sustained high count indicates target-side churn (check ECR lifecycle
    /// policies, external push tools, or registry GC schedules).
    pub discovery_target_stale: u64,
}
```

**Wiring:** These counters are **not** computed by `compute_stats()` (which iterates `ImageResult` entries). Instead, `discover_tag()` returns a `DiscoveryResult` wrapper that pairs the existing `DiscoveryOutcome` with a `DiscoveryPath`:

```rust
/// Signal from discover_tag() indicating which discovery path was taken.
enum DiscoveryPath {
    /// Source HEAD matched cache, config unchanged, no full source pull needed.
    CacheHit,
    /// Cache could not shortcut — full source manifest pull was attempted.
    /// Reasons: cold cache, source changed, config changed, or target mismatch
    /// despite source-side cache match. Increments `discovery_cache_misses`.
    /// Note: counts the cache *decision* (could not shortcut), not whether the
    /// subsequent pull succeeded — a pull that fails still counts as a miss.
    CacheMiss,
    /// Source HEAD failed (network error, 4xx/5xx, timeout, missing digest header).
    /// Increments BOTH `discovery_cache_misses` AND `discovery_head_failures`.
    HeadFailure,
    /// Source-side cache matched but at least one target HEAD returned a mismatch.
    /// Increments BOTH `discovery_cache_misses` AND `discovery_target_stale`.
    TargetStale,
}

/// Wrapper returned by discover_tag() to carry both the outcome and the
/// discovery cache signal without polluting DiscoveryOutcome variants.
struct DiscoveryResult {
    outcome: DiscoveryOutcome,
    path: DiscoveryPath,
}
```

The engine pipeline loop (the `select!` block that polls `discovery_futures`) matches on `DiscoveryResult`, increments local `u64` counters based on `path`, then processes `outcome` as before. After the pipeline loop completes, the counters are injected into `SyncStats` (returned by `compute_stats()`) before building the final `SyncReport`. This avoids adding discovery-layer concerns to per-image result types. The `discovery_head_failures` counter is a subset of `discovery_cache_misses` — every head failure is also a cache miss, so `discovery_cache_hits + discovery_cache_misses` still equals the total tag count.

These appear in `--json` output and are logged at `info` level in the sync summary line. An operator can:
- Compare `discovery_cache_hits + discovery_cache_misses` against expected tag count to verify the optimization is working
- Check `discovery_head_failures` to diagnose infrastructure problems — a high failure rate means the source registry is degraded and the optimization is adding latency (one timed-out HEAD per tag) without benefit
- Distinguish "source changed" (healthy, optimization working correctly) from "HEAD broken" (unhealthy, optimization adding overhead)

Also add `tracing::debug!` logging when:
- HEAD optimization fires (tag, source digest, "cache hit — skipping source pull")
- HEAD optimization falls through (tag, reason: "cache miss", "source changed", "HEAD failed", "HEAD timed out")

Per-tag decisions use `debug!` (too noisy at `info` for 1,000+ tags). The aggregate counts in the summary line provide `info`-level visibility.

### 5. Watch mode daemon

With the discovery optimization, watch mode becomes a thin wrapper around `sync` with a warm in-memory cache.

**Already implemented** (on the `feat/health-server` branch):
- `src/cli/health.rs` — `HealthState`, `handle_request()`, `format_response()`, `serve()`
- `src/cli/commands/watch.rs` — `LocalSet` + `spawn_local` for health server, `record_success()` on `Success`/`PartialFailure`
- `src/main.rs` — `--health-port` flag (default 8080)

**No changes needed** to the health server. The watch loop and `synchronize::run()` require a **cache ownership change** to enable the warm in-memory cache across cycles:

**Current state:** `synchronize::run()` creates a fresh `Rc<RefCell<TransferStateCache>>` internally every call (synchronize.rs:84-87), loading from disk each cycle. This means the tag digest cache is never warm in memory between watch cycles.

**Required change:** Add an optional `cache: Option<Rc<RefCell<TransferStateCache>>>` parameter to `synchronize::run()`. When `Some`, use the provided cache and **skip per-cycle disk persistence** — the watch loop owns the cache lifetime. When `None` (the `sync` command path), create a fresh cache and persist to disk as today. The watch loop creates the cache once before the loop (loading from disk if a cache file exists from a prior session — warm start), passes it on every cycle via `Some(Rc::clone(&cache))`, and the `Rc` survives between calls because the watch loop owns it. The `sync` command passes `None` (unchanged behavior).

**Disk persistence in watch mode:** The cache is persisted to disk only on graceful shutdown (SIGTERM/SIGINT), not every cycle. This avoids per-cycle disk I/O (~50-100ms) while preserving crash recovery — a pod that receives SIGTERM (K8s standard shutdown) writes its cache, and the replacement pod loads it on startup. An ungraceful kill (SIGKILL, OOM) loses the in-memory cache, but the prior graceful shutdown's cache file is still on disk. For PVC-backed deployments, this provides warm restarts without per-cycle persistence overhead.

**SIGHUP cache clearing:** On SIGHUP config reload, the tag digest cache (source snapshot entries) must be cleared to force full source re-verification on the next cycle. Platform filter changes are detected by `platform_filter_key`, but other config changes (source registry URL, repository mappings) could make cached entries stale. The blob dedup map is preserved across SIGHUP (blob existence is independent of config).

**Health server resilience fix:** The `serve()` function in `health.rs` currently propagates `accept()` and `read()` errors via `?`, which terminates the health server permanently on any transient I/O error (e.g., `EMFILE`, `ECONNABORTED`, client disconnect). Per CLAUDE.md's cleanup loop resilience principle, these should log and continue:
- `listener.accept().await` errors: `tracing::warn!` and `continue` the loop
- `stream.read().await` errors: `tracing::warn!` and `continue` the loop (skip response, process next connection)
- `stream.write_all().await` errors: already handled correctly (logged, not propagated)

The discovery optimization is engine-level — it makes every `synchronize::run()` call cheap in steady state, benefiting watch mode automatically.

### 6. Cache behavior by deployment mode

| Mode | Cache state | Discovery cost (steady state) |
|------|------------|-------------------------------|
| Watch (Deployment) | In-memory, always warm | 1 source HEAD + N target HEADs per tag |
| CronJob + PVC | Disk cache loads on start | 1 source HEAD + N target HEADs per tag |
| CronJob, no PVC | Cold cache | 1 source HEAD + N target HEADs + full pull if any target misses |

The PVC model: CronJob template mounts a ReadWriteOnce PVC. Each Job pod mounts the same PVC at e.g. `/data`. Cache writes to `/data/.ocync/cache/`. Requires `concurrencyPolicy: Forbid` to prevent two pods from mounting simultaneously. Not available on EKS Fargate (no EBS PVCs) — those environments start cold, which is acceptable since the HEAD optimization still avoids full pulls when targets are already in sync.

## Steady-state savings per tag

| Scenario | Before | After | Saved |
|----------|--------|-------|-------|
| Single-arch, all synced (warm cache) | 1 GET + N HEADs | 1 HEAD + N HEADs | 1 GET |
| Multi-arch (5 plat), all synced (warm cache) | 1 index GET + 5 child GETs + N HEADs | 1 HEAD + N HEADs | 6 GETs |
| Multi-arch (5 plat), 1 target stale | 6 GETs + N HEADs | 1 HEAD + N HEADs + 6 GETs | 0 (1 extra HEAD) |
| Cold cache (first run) | 6 GETs + N HEADs | 1 HEAD + 6 GETs + N HEADs | −1 HEAD |

**Common-case overhead:** The steady-state common case (warm cache, all synced) replaces 1 index GET + M child GETs with 1 HEAD — a net gain. The cold-cache first run adds 1 extra HEAD per tag (negative savings), but this is amortized: the first cycle's 30ms HEAD cost is repaid on every subsequent cycle that saves 350ms of GETs. The break-even point is the second cycle.

**Wall-clock estimate** (Chainguard → 3-region ECR, 50 repos x 20 tags x 5 platforms):
- Source HEAD: ~30ms (headers only, ~200 bytes)
- Source GET (index): ~80ms (~2KB body)
- Source GET (child): ~60ms (~2KB body each)
- Steady-state cycle before: 50 x 20 x (80 + 5x60) = 50 x 20 x 380ms = ~380s of source requests
- Steady-state cycle after: 50 x 20 x 30ms = ~30s of source HEADs
- **~350s saved per cycle** (92% reduction in source-side request time)

Target HEADs are present in both paths and run concurrently, so they don't change the comparison.

## Files changed

### Distribution crate (ocync-distribution)

| File | Change |
|------|--------|
| `crates/ocync-distribution/src/client.rs` | Add `accept: Option<&str>` parameter to `head()` (4 call sites); add `pub fn registry_authority(&self) -> String` accessor (host:port, no userinfo) |
| `crates/ocync-distribution/src/manifest.rs` | Pass `Some(&manifest_accept_header())` to `manifest_head()` |
| `crates/ocync-distribution/src/blob.rs` | Update `blob_exists()` call to pass `None` for accept |
| `crates/ocync-distribution/src/spec.rs` | Change `annotations` field type from `HashMap<String, String>` to `BTreeMap<String, String>` on `ImageIndex`, `Descriptor`, and `ImageManifest`; update all construction/access sites |

### Sync engine (ocync-sync)

| File | Change |
|------|--------|
| `crates/ocync-sync/src/cache.rs` | Add `SourceSnapshot`, `SourceSnapshotMap`, `platform_filter_key()`, version bump 1→2 (read accepts 1 or 2), persist/load |
| `crates/ocync-sync/src/engine.rs` | HEAD-before-GET with `discovery_head_timeout` in `discover_tag()`, `DiscoveryResult` wrapper with `DiscoveryPath`, pass cache + `source_authority` to `DiscoveryParams`, update cache on skip/success, accumulate counters; in `pull_source_manifest()`: error on zero-platform filtering result; add `source_authority: String` to `ResolvedMapping` |
| `crates/ocync-sync/src/lib.rs` | Add `discovery_cache_hits`/`discovery_cache_misses`/`discovery_head_failures`/`discovery_target_stale` to `SyncStats` |

### CLI (ocync binary)

| File | Change |
|------|--------|
| `src/cli/health.rs` | Already on branch; fix: log-and-continue on `accept()` and `read()` errors instead of propagating |
| `src/cli/commands/watch.rs` | Already on branch; change: create `Rc<RefCell<TransferStateCache>>` before loop, pass to `synchronize::run()` |
| `src/cli/commands/synchronize.rs` | Add `cache: Option<Rc<RefCell<TransferStateCache>>>` parameter to `run()`; skip disk load/persist when cache provided; populate `source_authority` on `ResolvedMapping` from `source_client.registry_authority()` |
| `src/main.rs` | Already on branch (--health-port) |
| `Cargo.toml` | Already on branch (tokio io-util, net) |
| `src/cli/commands/copy.rs` | Update `ResolvedMapping` construction to populate `source_authority` from `source_client.registry_authority()` |

## Testing

### Global test constraints

All engine integration tests in this section must follow these constraints (in addition to CLAUDE.md testing standards):

- **Distinct source/target values:** Use different names for source and target repos (e.g., `source_repo: "src/nginx"`, `target_repo: "tgt/nginx"`) and different source/target registries. This prevents mock assertions on repo names from passing vacuously when source == target (per CLAUDE.md non-tautological assertion standard).
- **Sum invariant assertion:** Every test must include `assert_eq!(stats.discovery_cache_hits + stats.discovery_cache_misses, expected_total_discover_tag_invocations)` as an explicit cross-check.
- **Exact blob counts:** Assert `transferred == K` and `skipped == K` with exact values derived from the test fixture setup. Never use `> 0`.

### Test helper

```rust
/// Construct a `SourceSnapshot` for tests with only the fields that vary.
fn test_snapshot(source_digest: &str, filtered_digest: &str) -> SourceSnapshot {
    SourceSnapshot {
        source_digest: test_digest(source_digest),
        filtered_digest: test_digest(filtered_digest),
        platform_filter_key: String::new(),
    }
}

/// Variant with platform filter key for platform-filtering tests.
fn test_snapshot_filtered(source: &str, filtered: &str, key: String) -> SourceSnapshot {
    SourceSnapshot {
        source_digest: test_digest(source),
        filtered_digest: test_digest(filtered),
        platform_filter_key: key,
    }
}
```

**`ResolvedMapping` construction helper:** Adding `source_authority: String` to `ResolvedMapping` breaks 58+ construction sites in engine integration tests. Add a test helper:

```rust
/// Construct a `ResolvedMapping` for tests with sensible defaults.
/// Only `source_repo`, `target_repo`, and `tags` vary between tests;
/// other fields use test defaults.
fn test_mapping(
    source_repo: &str,
    target_repo: &str,
    tags: Vec<TagPair>,
) -> ResolvedMapping {
    ResolvedMapping {
        source_authority: "source.registry.io:443".to_string(),
        // ... other fields with test defaults ...
    }
}
```

This follows the CLAUDE.md test helper standard: "When adding a new field to a widely-used struct, immediately add a test helper function with the default value."

### Accept header fix

- wiremock test: `manifest_head()` sends Accept header — assert with `Mock::given(header("Accept", ...))` that the request includes OCI index and Docker manifest list media types. The test must **fail** if the Accept header is missing (use `header_exists("Accept")` guard on the mock, and a separate mock without Accept that returns 404 to catch regressions).
- wiremock test: `manifest_head()` and `manifest_pull()` return the same digest for a multi-type manifest — mount a wiremock that returns different digests depending on Accept header presence, assert both methods receive the same digest.
- wiremock test: `manifest_head()` returns 200 but with missing `Docker-Content-Digest` header — assert the method returns `Err`, not `Ok(None)`. Verify the error message includes the invalid digest string.

### HEAD-before-GET optimization

**Fast path (cache hit, all synced):**
- Single-arch: source HEAD matches cache, all target HEADs match `filtered_digest`. Assert `.expect(0)` on source manifest GET. Assert `.expect(1)` on source HEAD. Assert `DiscoveryOutcome::Skip`.
- Multi-arch (index): same, plus `.expect(0)` on all M child manifest GETs. Verify savings are real for the primary use case.
- Assert `discovery_cache_hits == N`, `discovery_cache_misses == 0`, and `discovery_head_failures == 0` in `SyncStats`.

**Slow path (cache miss or source changed):**
- First run (no cache): source HEAD succeeds, cache miss. Assert `.expect(1)` on source HEAD AND `.expect(1)` on source GET. Full pull taken. Cache populated after success. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0` (HEAD succeeded, miss is due to cold cache).
- Source changed: source HEAD returns new digest, cache has old (pre-populate cache with old entry). Assert `.expect(1)` on HEAD, `.expect(1)` on GET. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == N`, `discovery_head_failures == 0` (HEAD succeeded, miss is due to digest change).
- Source HEAD fails (500): `.expect(1)` on HEAD, `.expect(1)` on GET. Full pull taken, no error surfaced. Assert image status is `Synced` (not `Failed`). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 1`.
- Source HEAD 404: `.expect(1)` on HEAD, `.expect(1)` on GET. Full pull taken, no error surfaced. Assert image status is `Synced` (not `Failed`). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 1`.
- Source HEAD 401 (auth failure): `.expect(1)` on HEAD, `.expect(1)` on GET. The optimization HEAD has no retry — the 401 is treated as a HEAD failure, falling through to the full pull path where the auth retry/invalidation machinery handles re-authentication. Assert image status is `Synced` (not `Failed`). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 1`. This tests the bridge between the optimization's no-retry HEAD and the authoritative pull path's auth retry.
- Source HEAD 403 (forbidden): identical assertions to 401. Common with ECR cross-account permission misconfigurations. Assert fallthrough to full pull.
- Source HEAD times out: configure `discovery_head_timeout` to 2 seconds (custom, below default 5s). Mount source HEAD with wiremock `Delay::duration(3s)` — exceeds the custom 2s timeout but is well below the normal request timeout (30s). Assert `.expect(1)` on source HEAD, `.expect(1)` on source GET. Assert the total discovery time is bounded by ~2s + GET time, not ~30s + GET time — this boundary value **differentiates** the custom timeout from the default (per CLAUDE.md: "use delays between the custom and default values so the test only passes if the custom value is actually used"). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 1` (timeout counts as both a head failure and a cache miss).

**Bridge test (HEAD failure with valid cache entry):**
- Pre-populate cache with valid entry for `(source, repo, tag)`. Mount source HEAD returning 500. Assert: the engine falls through to full `pull_source_manifest()`, NOT using `cached.filtered_digest` to skip. Assert `.expect(1)` on source HEAD AND `.expect(1)` on source GET. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 1`. This test **must fail** if the implementation incorrectly uses stale cache data when the HEAD fails — the HEAD failure means "we don't know if the source changed, so we must re-pull."

**Platform filtering interaction:**
- Config has `platforms: [linux/amd64]` on a 3-platform source image. First sync: full pull, filter, push filtered index. Cache records `source_digest` (unfiltered), `filtered_digest` (filtered), and `platform_filter_key`. Second sync: source HEAD matches `source_digest`, platform key matches, target HEADs match `filtered_digest`. Assert:
  - `.expect(0)` on source index GET (negative assertion: full pull NOT taken)
  - `.expect(0)` on all child manifest GETs (negative assertion: children NOT re-fetched)
  - `discovery_cache_hits == 1`, `discovery_cache_misses == 0`, `discovery_head_failures == 0`
  - This is the critical test — it proves the cache correctly bridges the unfiltered/filtered digest gap.

**Platform filter config change:**
- First sync with `platforms: [linux/amd64]`. Cache populated. Second sync with `platforms: [linux/amd64, linux/arm64]` (user added arm64). Source HEAD digest is unchanged (same source image). Assert:
  - Cache MISS due to `platform_filter_key` mismatch
  - `.expect(1)` on source index GET (full source pull triggered)
  - `.expect(N)` on target manifest PUTs (newly filtered index pushed to all targets)
  - Target manifest PUT body contains both amd64 and arm64 platform entries (verify the filtered content is different from cycle 1)
  - `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`
  - This proves the cache doesn't silently skip when the user changes their platform config.

**`skip_existing` interaction:**
- `skip_existing: true` with cache hit, target has different digest: source HEAD matches cache, target HEAD returns *any* manifest (with a different digest than `filtered_digest`). Assert image status is `Skipped { reason: SkipReason::SkipExisting }`, NOT `DigestMatch`. Assert `.expect(1)` on source HEAD (fires regardless). Assert `.expect(1)` on target HEAD (called before skip_existing decision). Assert `.expect(0)` on source GET (cache hit prevented source pull — negative assertion). Assert `.expect(0)` on target manifest PUT (skip_existing prevented push — negative assertion). Assert `discovery_cache_hits == 1`, `discovery_cache_misses == 0`, `discovery_head_failures == 0`.
- `skip_existing: true` with cache hit, target matches filtered_digest: source HEAD matches cache, target HEAD returns `filtered_digest` (matches). Assert image status is `Skipped { reason: SkipReason::SkipExisting }` — the code evaluates `skip_existing` first (engine.rs:602 `Ok(Some(_)) if skip_existing =>` match arm fires before the digest comparison arm at line 622). When both could apply, `skip_existing` wins due to match arm ordering. Assert `.expect(0)` on source GET. Assert `discovery_cache_hits == 1`, `discovery_cache_misses == 0`, `discovery_head_failures == 0`.
- `skip_existing: true` with cache miss: source HEAD succeeds, no cache entry. Assert `.expect(1)` on source GET (full source pull happens). Assert target is skipped via `SkipExisting`. Assert `.expect(0)` on target manifest PUT (negative assertion). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`. Assert cache is populated after the full pull (so future cycles benefit).

**Mixed fan-out (N>1 targets, different state):**
- 3 targets with different state: Target A has correct digest (matches `filtered_digest`), Target B returns 404 (never synced), Target C has wrong digest (stale). Source HEAD matches cache. Assert:
  - Target A → `Skipped { reason: SkipReason::DigestMatch }`
  - Target B → `Synced` (full transfer)
  - Target C → `Synced` (full transfer)
  - `.expect(1)` on source HEAD (optimization check)
  - `.expect(1)` on source index GET (pulled once for B and C — pull-once fan-out invariant)
  - `.expect(1)` on each source child manifest GET (M total, pulled once, shared via `Rc<PulledManifest>`)
  - Source GETs are per-tag, NOT per-target. Once any target needs the full pull, all children are fetched once. Target A benefits from the shared `Rc<PulledManifest>` but doesn't prevent the pull.
  - Each target has its own wiremock server with independent assertions
  - Per-target blob stats: Target A has `transferred: 0, skipped: 0`, Targets B and C have `transferred == K` where K is the exact blob count of the test image (known from the wiremock fixture setup)
  - Cache entry is updated with fresh values after the full pull (since full pull was performed for B/C)
  - `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`, `discovery_target_stale == 1` (source cache hit, but target mismatch forced full pull → counts as miss and target stale)

**All targets fail (pull succeeds, all pushes fail):**
- Source HEAD succeeds, cache miss (first run). Full source pull succeeds. All N target manifest PUTs return 500. Assert:
  - Image status is `Failed` for all targets
  - Cache IS updated (source pull succeeded — cache records what was pulled, not what targets confirmed)
  - `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`
  - On cycle 2: source HEAD matches updated cache (source-side cache hit). Target HEADs find targets still stale (404 or wrong digest) → step 10 triggers full pull. Assert `.expect(1)` on source GET (full pull required despite source cache matching, because targets are stale). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1` (target mismatch converted source hit into overall miss). The cache correctly identified the source hasn't changed but couldn't avoid the pull because targets need content.

**Multi-tag mixed HEAD outcomes:**
- 3 tags discovered concurrently (`max_concurrent > 1`). Tag A: source HEAD succeeds, cache hit (pre-populated). Tag B: source HEAD returns 500. Tag C: source HEAD times out (wiremock delay > `discovery_head_timeout`). Assert:
  - Tag A: `.expect(1)` on HEAD, `.expect(0)` on GET (cache hit, skip source pull)
  - Tag B: `.expect(1)` on HEAD, `.expect(1)` on GET (HEAD failed, full pull)
  - Tag C: `.expect(1)` on HEAD, `.expect(1)` on GET (HEAD timed out, full pull)
  - `discovery_cache_hits == 1`, `discovery_cache_misses == 2`, `discovery_head_failures == 2`
  - All 3 images complete successfully (Synced or Skipped)

**Concurrent discovery (cache hits + misses):**
- `max_concurrent > 1` with 3 tags discovering simultaneously. Tag A: cache hit (pre-populated). Tag B: cache miss (first run). Tag C: cache hit (pre-populated). Assert:
  - Tag A: `.expect(1)` on source HEAD, `.expect(0)` on source GET
  - Tag B: `.expect(1)` on source HEAD, `.expect(1)` on source GET
  - Tag C: `.expect(1)` on source HEAD, `.expect(0)` on source GET
  - Cache is populated with 3 entries after completion (B's entry is new, A and C preserved)
  - No panics on concurrent `RefCell` borrows (borrows are scoped, not held across `.await`)
  - `discovery_cache_hits == 2`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`

**Concurrent discovery (all cache misses):**
- `max_concurrent > 1` with 3+ tags discovering simultaneously, all with cache misses (first run). Assert:
  - All 3 source HEADs fire (`.expect(1)` each)
  - All 3 source GETs fire (`.expect(1)` each, cache miss)
  - Cache is populated with 3 entries after completion
  - No panics on concurrent `RefCell` borrows (borrows are scoped, not held across `.await`)
  - `discovery_cache_hits == 0`, `discovery_cache_misses == 3`, `discovery_head_failures == 0`

**Source changes across cycles (3-cycle test):**
- Cycle 1: cold cache, full pull. Source digest = D1. Cache populated with `source_digest = D1`. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`.
- Cycle 2: reconfigure source wiremock to return digest D2 on HEAD. Source HEAD returns D2, cache has D1 → cache miss. Full pull triggered. Assert `.expect(1)` on source GET. Cache updated with `source_digest = D2`. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`.
- Cycle 3: source HEAD still returns D2 (unchanged). Cache has D2 → cache hit. Assert `.expect(0)` on source GET (negative assertion: full pull NOT taken). Assert `discovery_cache_hits == 1`, `discovery_cache_misses == 0`, `discovery_head_failures == 0`. This proves the cache correctly updates on source change and re-engages on the subsequent cycle.

**Target HEAD 401 during cache-hit path:**
- Pre-populate cache with valid entry. Source HEAD matches cache. Target HEAD returns 401 (expired credentials). Assert: the 401 is treated as a target mismatch (not a fatal error), triggering fallthrough to full source pull + re-transfer. Assert image status is `Synced` (not `Failed` — the auth retry in the execution path handles re-authentication). Assert `.expect(1)` on source GET. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0` (source HEAD succeeded; it's the target HEAD that failed).

**Retag with cache (source tag != target tag):**
- Sync `reg.io/repo:v1.0` to target as `latest` (retag via `TagPair { source: "v1.0", target: "latest" }`). First sync: full pull. Cache records entry keyed by source tag `v1.0`. Second sync: source HEAD for tag `v1.0` matches cache, target HEAD for tag `latest` matches `filtered_digest`. Assert skip. Assert the cache key uses the source tag, and the target HEAD uses the target tag — not the source tag. Assert `discovery_cache_hits == 1`, `discovery_cache_misses == 0`, `discovery_head_failures == 0`.

**AIMD isolation (optimization HEAD 429 does not throttle GET):**
- Source HEAD returns 429. Assert: HEAD failure triggers fallthrough to full `manifest_pull()` via GET. The GET path operates on the `ManifestPull` AIMD window, NOT the `ManifestHead` window. Assert `.expect(1)` on source HEAD, `.expect(1)` on source GET. Assert image status is `Synced`. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 1`. The critical assertion is **state-based**: after the sync completes, inspect the AIMD controller's `ManifestPull` window and verify it was NOT shrunk (still at initial size). The `ManifestHead` window should be shrunk. This proves the 429 isolation property without relying on fragile timing assertions.

**Batch blob checker interaction after cache-hit fallthrough:**
- Pre-populate cache. Source HEAD matches cache (source unchanged). Target A matches, target B returns 404 (mismatch). Fallthrough to full pull. Execution phase runs batch blob checker for target B (ECR mock). Assert:
  - `.expect(1)` on source GET (delayed full pull triggered by target mismatch)
  - `.expect(1)` on batch blob check endpoint for target B
  - `.expect(0)` on per-blob HEAD endpoints for target B (batch checker pre-populated the blob cache — negative assertion)
  - `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`
  - This tests the bridge between the optimization's delayed-pull path and the batch blob checker infrastructure.

**Pull failure leaves cache empty:**
- Source HEAD succeeds (cache miss, first run). Full `pull_source_manifest()` fails (source manifest GET returns 500). Assert: image status is `Failed`. Assert: cache entry for `(source, repo, tag)` does NOT exist (read back the cache and verify `.get()` returns `None`). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`. On cycle 2: source HEAD succeeds again, cache is still empty (miss), full pull retries. This proves the spec's "do NOT update on pull failure" rule is enforced — a partial entry (HEAD digest without filtered digest) would cause incorrect skips.

**Zero-platform filtering error:**
- Source is a multi-arch index with platforms `[linux/s390x]`. Config has `platforms: [linux/amd64]`. Assert: `pull_source_manifest()` returns an error (not an empty index). Assert the error message includes both the filter and the source's available platforms. Assert image status is `Failed` with actionable error context.

**Deterministic filtered index serialization:**
- Source is a multi-arch index with annotations on descriptors (both `ImageIndex.annotations` and at least one `Descriptor.annotations` populated with 3+ keys in non-alphabetical insertion order, e.g., `{"zebra": "z", "alpha": "a", "middle": "m"}`). Pull and filter. Parse the serialized JSON output and verify annotation keys appear in **sorted** (alphabetical) order — this directly proves the `BTreeMap` type change is effective. Additionally, call `pull_source_manifest()` twice and assert both calls produce byte-identical output and identical `filtered_digest`. Note: within a single process, same-type collection iteration is deterministic, so the byte-identity assertion alone would pass even without the BTreeMap fix — the sorted-key-order assertion is what catches the regression. This is a unit test on `pull_source_manifest()`, not an engine integration test.

**Cache key with port (multi-port registries):**
- Two source registries on the same hostname with different ports (e.g., `harbor.internal:8443` and `harbor.internal:5000`). Both sync the same repo and tag. Assert the cache stores two separate entries (distinct keys). Assert a cache hit on one does not affect the other.

**Sum invariant assertion:**
- Every engine integration test must include `assert_eq!(stats.discovery_cache_hits + stats.discovery_cache_misses, expected_total_discover_tag_invocations)` as an explicit cross-check alongside the per-counter assertions. This catches double-counting or missed-counting bugs that per-counter assertions alone cannot detect.

**Counter granularity:**
- Counters increment once per `discover_tag()` invocation. The current engine does NOT deduplicate `discover_tag()` calls — if two config mappings share the same source `(authority, repo, tag)`, `discover_tag()` is called once per mapping (engine.rs:372-391 iterates all mappings x tags). The counters therefore reflect mapping-tag pairs, not unique source tags. The sum `discovery_cache_hits + discovery_cache_misses` must equal the total number of `discover_tag()` invocations (i.e., the total number of mapping-tag pairs processed).

**Mandatory quad-counter assertion:**
- **Every** engine integration test must assert exact values for **all four** counters: `discovery_cache_hits`, `discovery_cache_misses`, `discovery_head_failures`, and `discovery_target_stale`. No counter may be left unasserted — an unasserted counter is an unverified code path. When a test doesn't exercise HEAD failures, assert `discovery_head_failures == 0`. When a test exercises only slow-path behavior (no cache hits), assert `discovery_cache_hits == 0`. When a test has no target-side staleness, assert `discovery_target_stale == 0`. This catches counter increment bugs that would otherwise go undetected.

**Stats assertions:**
- All engine integration tests assert both `report.images[N].blob_stats` and `report.stats` including all three discovery counters with exact `assert_eq!` values. The sum `discovery_cache_hits + discovery_cache_misses` must equal the total number of unique source tags discovered. `discovery_head_failures` must be <= `discovery_cache_misses` (it's a subset).

**JSON output format:**
- CLI integration test asserting `--json` output contains `discovery_cache_hits`, `discovery_cache_misses`, and `discovery_head_failures` fields with exact `assert_eq!` on the field names and types. Verify the fields appear at the `stats` level (not per-image).

### Cache format

- v2 roundtrip: write cache with tag digest entries (including a non-empty `platform_filter_key`), reload, verify all fields are preserved exactly — especially verify the `platform_filter_key` string survives the roundtrip
- v1→v2 migration: construct a v1 cache file (write with `CACHE_VERSION = 1`, no `SourceSnapshotMap` section), load with the v2 reader. Verify empty tag digest map AND verify blob dedup data is fully preserved. This test must use a real v1-format byte sequence, not a mock.
- v2 with expired TTL: verify tag digest map is also discarded (not just blob dedup map)
- `platform_filter_key` correctness: compute key for `["linux/amd64", "linux/arm64"]` — assert result is `"linux/amd64,linux/arm64"`. Compute key for `["linux/arm64", "linux/amd64"]` (reversed input) — assert identical result (sort-order independence). Compute key for `["linux/amd64"]` vs `["linux/amd64", "linux/arm64"]` — assert different. Compute key for `None` and `Some(&[])` — assert both return empty string (empty-vs-none equivalence).
- `platform_filter_key` golden value: compute key for `["linux/amd64", "linux/arm64"]` and assert the result equals `"linux/amd64,linux/arm64"`. This golden-value test catches regressions where the key format changes (e.g., separator character changes, sort order changes). The expected value is derived directly from the definition and requires no separate computation step.
- v2 coexistence: write a cache with BOTH blob dedup entries (3+ blobs with various statuses) AND source snapshot entries (3+ entries with different source_digest/filtered_digest values and platform filter keys), reload, verify ALL blob entries AND ALL source snapshot entries are fully intact. This tests that the two sections coexist correctly in the v2 format — the individual section tests don't catch serialization boundary issues between sections.
- Multi-entry key distinctness: write tag digest entries for two similar-prefix keys (e.g., `("reg.io:443", "lib/nginx", "v1")` and `("reg.io:443", "lib", "nginx")` — both have the same total character count). Reload and verify both entries survive independently with correct values. This validates the NUL separator prevents key collisions.

### Watch mode integration

- Two consecutive sync cycles via the watch loop. Both cycles use the same wiremock servers but with **per-cycle expectations** — reset wiremock expectations between cycles (via `MockServer::reset()` or separate `Mock::given()...mount()` calls with cycle-specific `.expect(N)` counts).
  - **Cycle 1** (cold cache): `.expect(1)` on source HEAD, `.expect(1)` on source index GET, `.expect(M)` on source child GETs, `.expect(N)` on target HEADs, `.expect(N)` on target manifest PUTs. Full discovery + transfer. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`. Cache populated via `Rc<RefCell<TransferStateCache>>` owned by the watch loop — survives between `synchronize::run()` calls because the `Rc` is not dropped.
  - **Cycle 2** (warm cache): `.expect(1)` on source HEAD, `.expect(0)` on source index GET (this assertion **proves** the cache from cycle 1 is warm), `.expect(0)` on all child GETs, `.expect(N)` on target HEADs. Assert `discovery_cache_hits == 1`, `discovery_cache_misses == 0`, `discovery_head_failures == 0`.
  - Health state: assert `HealthState::last_success` is updated after each cycle completes successfully.
  - The cache persists to disk only on graceful shutdown, not between cycles. Assert no cache file is written between cycle 1 and cycle 2 (negative assertion: catches accidental per-cycle disk persistence). After triggering a graceful shutdown signal, assert the cache file IS written to disk. On a fresh start with the persisted file, assert the cache loads and cycle 1 benefits from warm state. This tests the full lifecycle: warm in-memory between cycles, persist on shutdown, warm start on restart.

### Watch mode target-side staleness

- Verifies that target-side mutations (lifecycle policy GC, manual overwrite) are caught by step 8's per-cycle target HEAD verification, even with a warm cache.
  - **Cycle 1**: Full sync, all targets receive content. Cache populated.
  - **Between cycles**: Reconfigure target wiremock to return 404 on manifest HEAD (simulating GC deletion).
  - **Cycle 2**: Source HEAD matches cache (cache hit on source side). Target HEAD returns 404 → mismatch → falls through to full source pull + re-push. Assert:
    - `.expect(1)` on source GET (full pull triggered despite cache hit — negative: `.expect(0)` would mean cache bypassed target verification)
    - Image status is `Synced` (re-pushed successfully), NOT `Skipped` — this is the critical negative assertion: if the implementation incorrectly returns `Skipped` when the cache hits, targets remain empty
    - `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`, `discovery_target_stale == 1` (the target mismatch forced a full pull — source-side cache matched but target staleness makes this an overall miss)
    - Cache entry is updated after the re-pull with the same digests (source unchanged, filtered digest unchanged)
  - This test proves the cache **never bypasses target verification** — target-side staleness is caught every cycle by step 8's target HEAD check, not by the cache.

### Broken Docker-Content-Digest degradation

- **Source broken headers:** Source wiremock returns a different digest in HEAD vs GET for the same tag. First sync: cache miss (HEAD returns digest A, no cache entry), full pull succeeds (GET returns digest B), cache records `source_digest = A` (from HEAD), `filtered_digest = B` (from GET). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0`. Second sync: HEAD returns digest C (inconsistent), cache mismatch (`C != A`), full pull triggered. Assert the optimization degrades to always-pull (no skip) without errors. Assert `debug!` log emitted noting digest mismatch. Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0` (HEAD succeeded — it returned wrong data, but didn't fail).
- **Target broken headers:** Target wiremock returns a different digest in HEAD than what was pushed. Pre-populate cache with correct entry. Source HEAD matches cache (source unchanged). Target HEAD returns wrong digest (doesn't match `filtered_digest`). Assert: falls through to full source pull + re-push to target. Assert image status is `Synced` (not `Skipped`). Assert `discovery_cache_hits == 0`, `discovery_cache_misses == 1`, `discovery_head_failures == 0` (target mismatch forced full pull despite source cache hit). This is a performance regression (unnecessary re-push) but not a correctness bug.

### Health server

Existing tests on branch: `handle_request` unit tests, `format_response` assertion, `HealthState` transitions.

**Additional tests needed after resilience fix:**
- Accept error resilience: simulate an accept error (e.g., by dropping the TcpListener and rebinding, or by observing that the server continues serving after a connection reset). Assert the health server remains operational after the error — a subsequent health check succeeds.
- Malformed request: connect via TCP, send garbage bytes, assert the server returns 404 (not a crash or hang) and continues accepting new connections.
- `--health-port` CLI parsing: add `parse_watch_custom_health_port` test verifying `--health-port 9090` is wired correctly, and a test verifying the default is 8080.
