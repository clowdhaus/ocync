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
   misleading — a proxy-bound benchmark can't tell you whether your
   tool is actually efficient, only that *something* is slow.

# Findings

## 2026-04-16 — ECR does not fulfill OCI cross-repo blob mount

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
ECR requires settling time before fulfilling mount. It does not — all
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
  provider that does not fulfill mount — returns `MountResult::NotMounted`
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
account, specific regions, specific IAM scopes) — analyze the response
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
optimization — which matches the theoretical analysis.

The two-variant and three-variant fixes were observationally identical
on every axis (requests, bytes, mount counts), confirming the
three-variant distinction did not pay for its complexity.

The procedural significance: the original mount optimization shipped
without a wire-level test against real ECR, so the silent no-op went
undetected until a proxy log made it visible.

---

## 2026-04-16 — 3-tool cold/warm comparison and upload strategy analysis

### Observation

Fair 3-tool comparison on c6in.4xlarge, Jupyter corpus (5 images, 1 tag
each, `latest`), cold sync to ECR us-east-1, 1 iteration per tool.
All three tools exited 0 (no partial failures). Branch: `benchmark-comparison`.

Pre-optimization, ocync used 3,249 requests (chunked PATCH upload,
broken auth cache, redundant blob HEADs) vs regsync's 1,302 for the
same 11.5 GB multi-arch transfer. dregsy's 1,538 requests / 5.9 GB
were not comparable — it synced only 1 platform. No tool deduplicated
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

### Current competitive position (cold sync, Jupyter 5-image corpus)

| Tool | Platforms | Requests | Response bytes | Wall clock |
|------|----------|---------|----------------|------------|
| **ocync** (streaming PUT) | 2 (multi-arch) | **1,049** | 11.5 GB | **183.1s** |
| regsync v0.11.3 | 2 (multi-arch) | 1,302 | 11.5 GB | 172.3s |
| dregsy (skopeo) | 1 (single tag) | 1,538 | 5.9 GB | 92.8s |

ocync wins on requests and wall-clock vs regsync (same work). dregsy
comparison is invalid — it syncs 1 platform, not 2 (see Observation).

| Tool | Requests | Response bytes | Wall clock |
|------|---------|----------------|------------|
| **ocync** | **81** | 371 KB | **2.5s** |
| regsync | 27 | 27 KB | 4s |
| dregsy | 200 | 163 KB | 5.2s |

Warm sync: ocync wins wall-clock decisively. regsync uses fewer
requests (manifest-digest comparison only) but is sequential.

### To re-validate

Re-run after Docker Hub rate limit resets. dregsy now configured with
`platform: all` for fair multi-arch comparison.

```bash
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
export BENCH_TARGET_REGISTRY=${ACCOUNT}.dkr.ecr.us-east-1.amazonaws.com
cargo xtask bench --tools ocync,dregsy,regsync --iterations 1 --limit 5 cold
cargo xtask bench --tools ocync,dregsy,regsync --limit 5 warm
```

---

## 2026-04-16 — Research findings: ECR, OCI upload, Docker Hub

Three parallel research agents surveyed ECR optimization, OCI upload
best practices across tools, and Docker Hub rate limiting. Actionable
findings only — full reports in session artifacts.

### ECR blob mounting now available (January 2026)

AWS launched opt-in `BLOB_MOUNTING` account setting. When enabled,
cross-repo mount POSTs return 201 instead of 202. Our
`ProviderKind::fulfills_cross_repo_mount` returning `false` for ECR
is now conditionally wrong.

Enable: `aws ecr put-account-setting --name BLOB_MOUNTING --value ENABLED`

Requirements: same account + region, identical encryption config,
pusher needs `ecr:GetDownloadUrlForLayer` on source repo. Not
supported for pull-through cache repos.

ECR uses content-addressable storage at the S3 layer — blobs
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

### Upload strategy comparison across tools

| Tool | Strategy | Requests/blob | Cross-image dedup |
|------|---------|:---:|:---:|
| **ocync** | POST + streaming PUT | **2** | Yes (TransferStateCache) |
| containerd | POST + streaming PUT | 2 | Yes (StatusTracker) |
| regsync | POST + PUT (buffered) | 2 | No |
| crane | POST + streaming PATCH + PUT | 3 | Yes (sync.Map) |
| skopeo | POST + PATCH + PUT | 3 | No |

ocync and containerd are tied for most request-efficient. AIMD
adaptive concurrency is unique to ocync — no competitor implements
anything similar.

### BatchGetImage for warm sync

ECR `BatchGetImage` SDK API checks up to 100 manifests per call.
Could replace per-manifest HEAD checks in warm sync (81 → ~1-2
calls). Returns full manifest bodies, not just existence.

---

## Optimization backlog

Ranked by estimated impact. Updated as findings change our
understanding. Each entry ships as its own PR.

| # | Optimization | Impact | Complexity | Status |
|---|-------------|--------|------------|--------|
| 1 | **ECR blob mounting** — detect `BLOB_MOUNTING` setting, conditionally re-enable mount | Saves blob transfer for every shared layer on ECR targets | Medium | Blocked: tested 2026-04-17 with BLOB_MOUNTING=ENABLED, still returns 202/404. May need specific IAM perms or repo config. |
| 2 | **Cross-image blob download dedup** — download each unique blob from source once, push to N target repos | ~168 source GETs, ~5.6 GB on Jupyter corpus | High | Not started |
| 3 | **Docker Hub authentication** — always authenticate pulls, add credential support for source registries | 10× more pull quota (100/hr vs 10/hr) | Low | Not started |
| 4 | **ACR streaming PUT fallback** — `ProviderKind::Acr` with chunked PATCH for blobs > 20 MB | Correctness on ACR (currently broken for large blobs) | Low | Not started |
| 5 | **BatchGetImage for warm sync** — replace per-manifest HEAD with SDK batch call | ~79 requests → ~1 call on warm sync | Medium | Not started |
| 6 | **Multi-scope Docker Hub tokens** — batch N repo scopes into 1 token exchange | 5 auth exchanges → 1 (Docker Hub only, cgr.dev incompatible) | Low | Not started |
| 7 | **Rate limit header parsing** — read `ratelimit-remaining` from Docker Hub and throttle proactively | Avoid 429s before they happen | Low | Not started |
| 8 | **Verify HTTP/2 on ECR** — check ALPN negotiation, enable multiplexing | Connection reuse for 50 concurrent requests | Trivial | Not started |

### Completed optimizations (this branch)

| Optimization | Requests saved | Status |
|-------------|---------------|--------|
| Streaming PUT upload (eliminate chunked PATCH + monolithic buffer) | ~2,200 (3,249 to 1,049) | Done |
| Auth cache fix (EARLY_REFRESH_WINDOW 15 min → 30s) | ~267 | Done |
| Batch-check HEAD skip (cold sync) | ~247 | Done |
| ECR mount short-circuit (PR #25) | ~148 | Done |

---

<!--
Entry template for future findings — copy and fill in.

## YYYY-MM-DD — Short summary of the observation

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
