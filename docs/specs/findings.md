# ocync empirical findings log

A living record of what real-world testing and benchmarking tell us about the
OCI registry ecosystem, and how those observations inform ocync's design.

Every entry pairs an **observation** (evidence from the wire) with its
**implication** (what that means for ocync's goals) and any **action taken**
(design change, short-circuit, deferred investigation). When prior assumptions
are falsified, they're corrected here so future PRs start from reality rather
than aspiration.

## How to use this doc

- **Before writing a new optimization**: read this doc first. The most
  relevant section is the registry provider you're targeting. Assumptions
  that already fell off a table here should not be silently re-adopted.
- **After a benchmark or probe run**: if the evidence changes our
  understanding of a registry's behavior, or invalidates a design
  assumption, add an entry here in the same PR as the behavior change.
- **Strategic design docs are primary; this doc is secondary**. When a
  finding implies a design change, amend `ocync-design.md` or
  `transfer-optimization-design.md` via a separate, explicitly-approved
  change. This doc records the evidence trail.

## Goals ocync optimizes for (ordered by priority)

These are the axes we measure efficiency against:

1. **Bytes transferred over the network.** Every push, pull, and
   redundant round-trip is a measurable cost. Mount optimizations, blob
   dedup, HEAD-before-GET, and persistent cache all exist to reduce
   bytes. Wall-clock speed is a *consequence* of reducing bytes, not the
   goal itself.
2. **Rate-limit friendliness.** Fewer API calls, correctly distributed
   across (registry, action) pairs, let us run at high concurrency
   without crashing into 429 walls. AIMD, batch existence checks, and
   persistent cache all exist to reduce API calls.
3. **Wall-clock speed.** Downstream of (1) and (2). If we transfer fewer
   bytes with fewer API calls, we're faster. Reports that prioritize
   wall-clock without showing the underlying byte/request counts are
   misleading - a proxy-bound benchmark can't tell you whether your
   tool is actually efficient, only that *something* is slow.

# Findings

## 2026-04-16 - ECR does not fulfill OCI cross-repo blob mount

### Observation

Across two independent triggers, 193 OCI cross-repo mount POSTs against
AWS ECR (`POST .../blobs/uploads/?mount=<digest>&from=<repo>`) returned
status `202 Accepted` with a fresh upload `Location`. Zero returned `201
Created` (the spec-compliant mount-success response).

| Trigger | Corpus | Mount attempts | 201 | 202 | Errors |
|---|---|---|---|---|---|
| Jupyter benchmark (commit `56bbfac`) | 5 images × 6 tags, Chainguard → ECR us-east-1 | 178 | 0 | 178 | 0 |
| Ad-hoc probe, 0s wait (commit `c2d33d6`) | synthetic 1 KiB blob | 5 | 0 | 5 | 0 |
| Ad-hoc probe, 10s wait | same | 5 | 0 | 5 | 0 |
| Ad-hoc probe, 60s wait | same | 5 | 0 | 5 | 0 |
| **Total** | | **193** | **0** | **193** | **0** |

The ad-hoc probe deliberately varied the interval between source blob
commit and mount attempt (0s / 10s / 60s) to test the hypothesis that
ECR requires settling time before fulfilling mount. It does not - all
three delays produced the same result. The probe tool was subsequently
removed from the tree; see the re-validate section below for the
manual procedure if the observation needs to be re-run.

### Implication

**The OCI cross-repo mount optimization is a no-op on ECR.** It likely
always has been; the benchmark's proxy-limited throughput masked the
wasted round-trip cost because ECR returned 202 fast (~100 ms), and the
engine's fallback path (HEAD → push) was the actual transfer mechanism
regardless.

This is also a design assumption falsification against
`docs/specs/transfer-optimization-design.md  sectionSame-Registry Optimization`,
which describes cross-repo mount as a primary optimization for
`ecr-us/team-a/nginx` → `ecr-us/team-b/nginx`. In practice, that blob
must be re-uploaded on ECR, not mounted.

**Bytes saved by mount on ECR: 0.** The blob data crosses the network on
the fallback path. ECR's internal dedup (storage-backend level) does NOT
surface as a mount success over the OCI protocol.

**API calls saved by mount on ECR: 0.** The mount POST + fallback PATCH
is one more round-trip than a direct POST + PATCH would have been. Net
cost of attempted mount on ECR: ~1 round-trip per shared blob per
target.

### Action taken

- `ProviderKind::fulfills_cross_repo_mount()` uses an exhaustive `match`
  so adding a new provider forces a compile-time decision. `Ecr` and
  `EcrPublic` return `false`; all other known providers return `true`.
  ECR Public is inferred from private ECR (same AWS backend team). If a
  future observation shows ECR Public does fulfill mount, flip the arm
  and add a re-validate entry.
- `blob_mount` short-circuits when the target hostname resolves to a
  provider that does not fulfill mount - returns `MountResult::NotMounted`
  without issuing the POST, saving one round-trip per shared blob per
  target.
- `MountResult` is two-variant (`Mounted` / `NotMounted`). A mid-review
  iteration tried a three-variant distinction (separate `Skipped` for
  short-circuit vs `NotFulfilled` for server-202) intended to preserve
  the in-progress claim on short-circuit. Empirical wiremock testing
  showed the distinction did not change observable bytes or request
  counts: staging handles pull-once dedup at the filesystem layer, and
  `set_blob_completed` after push re-populates whatever the invalidation
  briefly wiped. The simpler two-variant enum ships.
- Protocol coverage: `tests/registry2_mount.rs` pins `Mounted` and
  `NotMounted` against the OCI reference `registry:2` container.
  `engine_integration::sync_warm_cache_ecr_target_short_circuits_mount`
  pins the engine-level wire behavior (0 mount POSTs on ECR).

### To re-validate

If AWS behavior changes, observation typically comes from a sync where a
mount would have otherwise succeeded. To re-run the original diagnostic
manually against a throwaway ECR account:

1. Push a small blob to a source repo in the target ECR account.
2. `aws ecr get-login-password | docker login --username AWS --password-stdin <registry>`.
3. `curl -sS -X POST "<registry>/v2/<target-repo>/blobs/uploads/?mount=<digest>&from=<source-repo>" -I`.
4. If the response is `201 Created` (not `202 Accepted`), flip
   `ProviderKind::fulfills_cross_repo_mount` to allow ECR and update
   `tests/registry2_mount.rs` style coverage with an ECR case.

One `201` across multiple accounts/regions is sufficient grounds to
re-evaluate. AWS may fulfill mount only under specific conditions (same
account, specific regions, specific IAM scopes) - analyze the response
body and `Location` header before flipping the short-circuit globally.

### Cost of this particular finding (measured)

3-arm benchmark on c6in.large, cold Chainguard Jupyter corpus (5 images,
6 tags each) → ECR us-east-1, one iteration:

| Arm | Wall clock | Requests | Bytes | Mount POSTs attempted |
|-----|-----------|----------|-------|-----------------------|
| `main` (pre-PR, no short-circuit) | 207.0s | 3,397 | 11.5 GB | 148 (all 202) |
| two-variant fix (`15debfc`) | 194.5s | 3,249 | 11.5 GB | 0 |
| three-variant fix (`80896bc`) | 207.1s | 3,249 | 11.5 GB | 0 |

Savings vs baseline: **148 avoided POSTs, identical bytes.** The
short-circuit is a rate-limit / request-count optimization, not a bytes
optimization - which matches the theoretical analysis.

The two-variant and three-variant fixes were observationally identical
on every axis (requests, bytes, mount counts), confirming the
three-variant distinction did not pay for its complexity.

The procedural significance: the original mount optimization shipped
without a wire-level test against real ECR, so the silent no-op went
undetected until a proxy log made it visible.

---

## 2026-04-16 - 3-tool cold/warm comparison and upload strategy analysis

### Observation

Fair 3-tool comparison on c6in.4xlarge, Jupyter corpus (5 images, 1 tag
each, `latest`), cold sync to ECR us-east-1, 1 iteration per tool.
All three tools exited 0 (no partial failures). Branch: `benchmark-comparison`.

Pre-optimization, ocync used 3,249 requests (chunked PATCH upload,
broken auth cache, redundant blob HEADs) vs regsync's 1,302 for the
same 11.5 GB multi-arch transfer. dregsy's 1,538 requests / 5.9 GB
were not comparable - it synced only 1 platform. No tool deduplicated
cross-image blob downloads from source (168 redundant GETs / 5.6 GB
on this corpus).

### Action taken

Four optimizations implemented (branch `benchmark-comparison`):

1. **Streaming PUT upload.** POST + single streaming PUT with
   `Transfer-Encoding: chunked`, zero buffering. Replaced chunked
   POST+PATCH×N+PUT (eliminated ~1,400 PATCH requests) and the
   monolithic buffer path. GHCR/GAR fallbacks retained.

2. **EARLY_REFRESH_WINDOW lowered from 15 min to 30 sec.** Docker Hub
   tokens have 300s TTL. The prior 15-minute window caused
   `should_refresh()` to always return true, completely bypassing the
   token cache (272 redundant auth exchanges → 5).

3. **Batch-check HEAD skip.** When ECR batch API confirms blobs are
   absent, the per-blob HEAD is skipped (247 requests eliminated on
   cold sync).

4. **Removed dead code.** `MONOLITHIC_THRESHOLD`, `DEFAULT_CHUNK_SIZE`,
   `chunk_size` field/builder, `send_patch_chunk`, `ContentRangeMatcher`.
   Net -213 lines.

### Current competitive position (cold sync, full corpus)

Measured 2026-04-18 on c6in.4xlarge (Intel, 16 vCPUs, 32 GiB, up to 50 Gbps).
Full corpus: 42 images, 55 tags across Docker Hub, cgr.dev, public ECR, nvcr.io.
Docker Hub authenticated (all tools). 1 iteration per tool.

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **4m 33s** | 36m 22s | 16m 6s |
| Peak RSS | **58.1 MB** | 304.5 MB | 27.0 MB |
| Requests | **4,131** | 11,190 | 7,719 |
| Response bytes | **16.9 GB** | 43.4 GB | 35.4 GB |
| Source blob GETs | **726** | 1,324 | 1,381 |
| Source blob bytes | **77.2 KB** | 120.1 KB | 160.5 KB |
| Mounts (success/attempt) | **362/379** | 293/293 | 0/1,380 |
| Duplicate blob GETs | **0** | 1 | 1 |
| Rate-limit 429s | **0** | 49 | 0 |

ocync wins every metric except peak RSS (regsync is lighter at 27 MB).
Key differentiators:

- **8x faster** than dregsy, **3.5x faster** than regsync
- **2.7x fewer requests** than dregsy, **1.9x fewer** than regsync
- **2.6x fewer response bytes** than dregsy, **2.1x fewer** than regsync
- **95.5% mount success** (362/379) vs dregsy 100% (293/293) vs regsync 0%
- **Zero rate-limit 429s** vs dregsy's 49 (AIMD congestion control works)
- **Zero duplicate blob GETs** (source-pull dedup prevents redundant downloads)

regsync mounts all fail (regclient omits `from=` parameter for
cross-registry syncs). dregsy hit 49 rate-limit 429s from Docker Hub
despite authenticated pulls.

**Memory:** regsync's lower RSS (27 MB) reflects its sequential
architecture - one image, one blob at a time, with Go's ~10-15 MB
baseline. ocync's 58 MB comes from concurrent blob transfers
(BLOB_CONCURRENCY=6), the staging claim/wait maps, transfer state
cache, and blob frequency map held in memory simultaneously. dregsy's
304 MB is skopeo buffering full layers in memory before pushing. The
58 MB vs 27 MB trade-off buys 3.5x faster wall clock.

Historical data: `bench-results/runs/*.json` (gitignored, on instance)

---

## 2026-04-16 - Research findings: ECR, OCI upload, Docker Hub

Three parallel research agents surveyed ECR optimization, OCI upload
best practices across tools, and Docker Hub rate limiting. Actionable
findings only - full reports in session artifacts.

### ECR blob mounting - works, with conditions (tested 2026-04-17)

AWS launched opt-in `BLOB_MOUNTING` account setting (January 2026).
Enable: `aws ecr put-account-setting --name BLOB_MOUNTING --value ENABLED`

**Tested and confirmed working.** Mount POST returns 201 when:
1. `BLOB_MOUNTING` is `ENABLED` on the account
2. The source blob is referenced by a committed manifest (image must
   be fully pushed, not just standalone blobs)
3. The `from` parameter specifies the source repo name
4. Both repos have identical encryption config (AES256 default works)

**Standalone blobs without manifests return 202 (mount fails).** This
is why our initial curl tests failed - we pushed raw blobs without
committing a manifest. dregsy's benchmark confirmed 172/172 mounts
succeeded because it pushes complete images sequentially.

regsync's mounts all failed (0/247) because it omits the `from=`
parameter entirely - it sends `?mount=<digest>` without telling ECR
which repo to mount from.

Our `ProviderKind::fulfills_cross_repo_mount` returning `false` for
ECR should be updated to detect `BLOB_MOUNTING` and conditionally
re-enable mount attempts. This is a significant optimization for
sync workflows with shared base layers.

Requirements: same account + region, identical encryption config,
pusher needs `ecr:GetDownloadUrlForLayer` on source repo. Not
supported for pull-through cache repos.

ECR uses content-addressable storage at the S3 layer - blobs
uploaded to one repo are stored in a shared bucket
(`prod-<region>-starport-layer-bucket`). HEAD for a blob digest
returns 200 from any repo where the blob was previously referenced.
This is what we observed in the benchmark (dregsy's 227 HEAD 200s
on target ECR).

### Docker Hub rate limits tightened (April 2025)

| Tier | Old | New (Apr 2025+) |
|------|-----|----------------|
| Anonymous | 100 pulls / 6h | **10 pulls / hour / IP** |
| Authenticated free | 200 pulls / 6h | **100 pulls / hour** |
| Pro/Team/Business | Higher | **Unlimited** (fair use) |

What counts as a pull: manifest GET only. Blob GETs and manifest
HEADs are free. HEAD requests are subject to a separate abuse rate
limit (~314 HEADs in 8 min triggers 429). Rate limit headers
(`ratelimit-remaining`) appear on HEAD responses too.

Authentication is now mandatory for any serious sync workload.

### ACR requires chunked upload for blobs > 20 MB

Azure Container Registry enforces a ~20 MB request body limit on
monolithic PUT. Our streaming PUT needs a `ProviderKind::Acr`
fallback to chunked PATCH for larger blobs.

### Comprehensive tool comparison

**Upload strategy:**

| Tool | Strategy | Requests/blob | Buffering |
|------|---------|:---:|-----------|
| **ocync** | POST + streaming PUT | **2** | None (streamed) |
| containerd | POST + streaming PUT | 2 | None (io.Pipe) |
| regsync (regclient) | POST + PUT | 2 | Full blob in memory |
| crane (go-containerregistry) | POST + streaming PATCH + PUT | 3 | None (streamed) |
| skopeo (containers/image) | POST + PATCH + PUT | 3 | None (streamed) |

**Concurrency and rate limiting:**

| Tool | Default concurrency | Adaptive backoff | Rate limit strategy |
|------|:---:|:---:|-----|
| **ocync** | 50 (AIMD adaptive) | Yes (per-registry, per-action) | AIMD congestion control on 429 |
| containerd | 5 layers | No | Lock-based dedup, no 429 handling |
| regsync | 3 per registry | No | Reads `ratelimit-remaining` header, pauses proactively |
| crane | 4 jobs | Retry with backoff (1s, 3s) | Retry on 408/429/5xx, 3 attempts |
| skopeo | per-image flag | No | No retry, aborts on 429 |

**Blob deduplication:**

| Tool | Cross-image push dedup | Cross-image pull dedup | Persistent cache |
|------|:---:|:---:|:---:|
| **ocync** | Yes (TransferStateCache) | No | Yes (postcard binary) |
| containerd | Yes (StatusTracker) | No | No |
| regsync | No | No | No (in-memory only) |
| crane | Yes (sync.Map by digest) | No | No |
| skopeo | No | No | BoltDB BlobInfoCache (mount hints) |

No tool deduplicates cross-image blob downloads from source. This is
the #1 remaining optimization opportunity - 168 redundant source GETs
(5.6 GB) on the Jupyter corpus.

**Multi-arch handling:**

| Tool | Default behavior | Configurable |
|------|-----------------|:---:|
| **ocync** | Copies all platforms in manifest list | No (always multi-arch) |
| regsync | Copies all platforms | No |
| dregsy (skopeo) | Copies native platform only | Yes (`platform: all`) |
| crane | Copies all platforms | Yes (`--platform`) |
| containerd | Copies specified platform | Yes |

**Registry-specific fallbacks:**

| Tool | GHCR | GAR | ACR | ECR mount |
|------|------|-----|-----|-----------|
| **ocync** | Single PATCH (no Content-Range) | Buffered monolithic | Not yet handled | Short-circuits (no POST) |
| regsync | Chunked fallback | Monolithic | Chunked fallback | Attempts mount |
| crane | Unknown | Unknown | Unknown | Attempts mount |
| skopeo | Single PATCH | Historically broken | Unknown | BlobInfoCache mount hints |

**Auth:**

| Tool | Docker Hub tokens | ECR auth | Token caching |
|------|:-:|:-:|-----|
| **ocync** | Per-scope, 30s early refresh | AWS SDK (Basic auth) | Per-scope HashMap, mutex coalesced |
| regsync | Per-registry or per-repo (`repoAuth`) | Docker credential helper | Per-host, single token |
| crane | Per-registry transport | Docker credential helper | Per-transport |
| skopeo | Per-invocation | Docker credential helper | None across invocations |

**Warm sync efficiency (Jupyter 5-image corpus):**

| Tool | Requests | Bytes | Wall clock | How |
|------|:---:|:---:|:---:|-----|
| **ocync** | 81 | 371 KB | **2.5s** | Persistent cache skips blob I/O |
| regsync | **27** | **27 KB** | 4s | Manifest digest comparison only |
| dregsy | 200 | 163 KB | 5.2s | Re-HEADs all blobs |

### BatchGetImage for warm sync

ECR `BatchGetImage` SDK API checks up to 100 manifests per call.
Could replace per-manifest HEAD checks in warm sync (81 → ~1-2
calls). Returns full manifest bodies, not just existence.

---

## 2026-04-17 - Leader-follower mount optimization

### Observation

Leader-follower election ensures images with the most shared blobs execute
first (leaders), committing manifests before followers start. Followers then
mount shared blobs from the leader's repo instead of re-uploading. Tested
on c6in.large, Jupyter corpus (5 images, 1 tag each), cold sync to ECR
us-east-1. Branch: `feat/leader-follower-mount`.

| Metric | Before (streaming PUT, no L-F) | After (leader-follower + blob concurrency) |
|--------|-------------------------------|---------------------------------------------|
| Requests | 1,049 | **591** |
| Response bytes | 11.5 GB | **4.9 GB** |
| Wall clock | 217.9s | **56.8s** |
| Mount attempts | 0 (short-circuited) | 192 |
| Mount success | 0 | **192 (100%)** |

**591 requests vs 1,049** - 44% fewer API calls. **4.9 GB vs 11.5 GB** --
57% fewer bytes via cross-image staging dedup and 100% mount success.
**56.8s vs 217.9s** - 3.8x faster wall clock via 6-concurrent blob
transfers within each image (matching skopeo's default parallelism).

Preferred mount sources (leader repos with committed manifests) are tried
first, avoiding 202 rejections from follower repos whose manifests have
not yet been committed. This raised mount success from 40% to 100%.

### Bugs found and fixed

`ClaimAction::Wait` uses `tokio::sync::Notify::notify_waiters()` which does
not store permits. Three code paths transitioned blobs out of `InProgress`
without calling `notify_blob`:

1. Mount success: `set_blob_completed` + continue (no notify)
2. HEAD exists: `set_blob_exists` + continue (no notify)
3. Mount failure: `invalidate_blob` (no notify before fallback)

This caused deadlocks when two followers raced on a shared blob: one mounted
without notifying, the other waited forever. Latent before leader-follower
because concurrent promotion made the first claimer typically the uploader
(no mount source available in the early phase). Fixed by adding `notify_blob`
on all three paths.

### Action taken

- Greedy set-cover election (`elect_leaders`) reorders pending tasks so
  images with the most shared blobs transfer first
- Wave-based follower promotion: wave 1 (blobs covered by leaders) runs
  first, wave 2 (inter-follower deps) waits for wave 1 to commit
- `notify_blob` added to mount-success, HEAD-exists, and invalidate paths
- Skip HEAD after NotMounted (202 already confirms blob absent)
- Cross-image source blob dedup via staging (pull once, push from disk)

### To re-validate

```bash
cargo xtask bench --tools ocync,dregsy,regsync --iterations 1 --limit 5 cold
```

Compare mount success rate, total requests, and response bytes across
all three tools.

---

## Optimization backlog

Ranked by estimated impact. Updated as findings change our
understanding. Each entry ships as its own PR.

| # | Optimization | Impact | Complexity | Status |
|---|-------------|--------|------------|--------|
| 1 | ~~**ECR blob mounting**~~ | ~~Saves blob transfer for every shared layer~~ | ~~Medium~~ | **Done.** Leader-follower + blob concurrency: 192/192 mounts, 44% fewer requests, 57% fewer bytes, 3.8x faster. |
| 2 | **Cross-image blob download dedup** - download each unique blob from source once, push to N target repos | ~168 source GETs, ~5.6 GB on Jupyter corpus | High | Not started |
| 3 | **Docker Hub authentication** - always authenticate pulls, add credential support for source registries | 10× more pull quota (100/hr vs 10/hr) | Low | Not started |
| 4 | **ACR streaming PUT fallback** - `ProviderKind::Acr` with chunked PATCH for blobs > 20 MB | Correctness on ACR (currently broken for large blobs) | Low | Not started |
| 5 | **BatchGetImage for warm sync** - replace per-manifest HEAD with SDK batch call | ~79 requests → ~1 call on warm sync | Medium | Not started |
| 6 | **Multi-scope Docker Hub tokens** - batch N repo scopes into 1 token exchange | 5 auth exchanges → 1 (Docker Hub only, cgr.dev incompatible) | Low | Not started |
| 7 | **Rate limit header parsing** - read `ratelimit-remaining` from Docker Hub and throttle proactively | Avoid 429s before they happen | Low | Not started |
| 8 | **Verify HTTP/2 on ECR** - check ALPN negotiation, enable multiplexing | Connection reuse for 50 concurrent requests | Trivial | Not started |

### Completed optimizations

| Optimization | Requests saved | Status |
|-------------|---------------|--------|
| Streaming PUT upload (eliminate chunked PATCH + monolithic buffer) | ~2,200 (3,249 to 1,049) | Done (PR #26) |
| Auth cache fix (EARLY_REFRESH_WINDOW 15 min to 30s) | ~267 | Done (PR #26) |
| Batch-check HEAD skip (cold sync) | ~247 | Done (PR #26) |
| ECR mount short-circuit (PR #25) | ~148 | Done (PR #25) |
| Leader-follower mount + blob concurrency + staging dedup | ~458 (1,049 to 591), 6.6 GB bytes saved, 3.8x faster | Done (feat/leader-follower-mount) |

---

<!--
Entry template for future findings - copy and fill in.

## YYYY-MM-DD - Short summary of the observation

### Observation

- What was measured, against which registry / corpus / build
- Concrete numbers in a table, not prose
- Link to the commit or artifact that produced the data

### Implication

- Which design assumption this affects (with a file:line pointer into
  docs/specs/ if applicable)
- Effect on bytes transferred
- Effect on API call count
- Effect on wall-clock (if meaningful and separable from the above)

### Action taken

- What changed in the code, the design doc, or the test harness
- Link to the PR

### To re-validate

- How a future maintainer can reproduce the observation
- What evidence would overturn this finding
-->
