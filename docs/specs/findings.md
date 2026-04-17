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
`docs/specs/transfer-optimization-design.md §Same-Registry Optimization`,
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

**Cold sync pre-optimization (historical — see "Action taken" for current):**

| Tool | Platforms synced | Wall clock | Requests | Response bytes | Upload strategy |
|------|-----------------|-----------|----------|----------------|----------------|
| ocync (pre-opt) | 2 (multi-arch) | 189.6s | 3,249 | 11.5 GB | Chunked POST+PATCH+PUT (8 MB) |
| regsync v0.11.3 | 2 (multi-arch) | 172.3s | 1,302 | 11.5 GB | Monolithic POST+PUT |
| dregsy (skopeo) | 1 (single tag) | 92.8s | 1,538 | 5.9 GB | Single PATCH |

**dregsy's 5.9 GB is not an efficiency advantage** — it syncs only one
platform per tag (5 manifest PUTs) while ocync and regsync sync both
platforms in the manifest list (15 manifest PUTs each). The byte
difference is half-the-work, not better dedup.

**ocync vs regsync (same work, same bytes):** ocync uses **2.5× more
requests** (3,249 vs 1,302) for the same 11.5 GB transfer. The
difference is entirely in upload strategy:

| Tool | POST (init) | PATCH (chunks) | PUT (finalize) | HEAD | GET |
|------|------------|----------------|----------------|------|-----|
| ocync | 253 | 1,419 | 262 | 257 | 1,058 |
| regsync | 248 | 0 | 262 | 278 | 514 |

regsync uses monolithic upload (POST init + PUT full blob in one
request, no PATCH). ocync's chunked upload adds ~1,419 PATCH requests
(8 MB chunks × 247 blobs = ~1,419 PATCHes) and ~500 additional source
GETs (ocync re-downloads shared blobs per-mapping; regsync deduplicates
within a run).

**Unique blob digests downloaded from Docker Hub:**

| Tool | Unique digests | S3 GETs (actual downloads) | Re-downloads |
|------|---------------|---------------------------|-------------|
| ocync | 79 | 247 | 168 (2.1× per unique blob) |
| regsync | 79 | 247 | 168 |
| dregsy | 40 | 126 | 86 |

Both multi-arch tools download the same 79 unique blobs and make the
same 247 S3 GET requests — neither deduplicates cross-image blob
downloads within a single run.

**Warm sync (no-op, prime + measured pass, same corpus):**

| Tool | Wall clock | Requests | Response bytes |
|------|-----------|----------|----------------|
| ocync | **2.5s** | 81 | 371 KB |
| regsync | 4s | 27 | 27 KB |
| dregsy | 5.2s | 200 | 163 KB |

ocync's persistent TransferStateCache gives the fastest wall-clock on
warm sync. regsync uses fewer requests (manifest-digest-only comparison)
but is slower (sequential). dregsy re-HEADs all blobs (154 HEADs).

**Chunk size experiment (ocync-only, same corpus):**

| Chunk size | PATCH count | Total requests | Wall clock |
|-----------|-------------|----------------|------------|
| 8 MB (default) | 1,419 | 3,249 | 197.5s |
| 32 MB | 384 | 2,214 | 163.3s |

32 MB chunks reduce PATCHes by 3.7× and total requests by 32%.

**Blob HEAD elimination (cold sync):**

247 blob HEAD requests on the target returned 404 (100%). On cold
sync, ECR batch-check already confirms all blobs are absent. These
HEADs add 7.6% to the total request count with zero value.

### Implication

1. **Upload strategy is the #1 request-count lever.** Switching to
   monolithic upload for blobs below a threshold (e.g. 100 MB) would
   eliminate ~1,100 PATCH requests on this corpus. Alternatively,
   increasing chunk size to 32 MB saves ~1,035 requests with a trivial
   constant change.

2. **No cross-image blob dedup exists in any tool.** All three tools
   re-download (from source) and re-upload (to target) shared blobs
   when they appear in different source images mapped to different
   target repos. A session-scoped blob cache (download once, push to
   N targets) would save 168 source downloads (68%) on this corpus.

3. **Warm sync is ocync's strongest competitive feature.** 2.5s vs
   4–5s, and the gap widens with larger corpora (persistent cache
   skips all blob I/O). However, regsync's 27-request approach
   (compare manifest digests only) is more request-efficient than
   ocync's 81-request approach.

4. **dregsy's wall-clock advantage is illusory.** It syncs fewer
   platforms → fewer bytes → faster. Not a valid efficiency comparison.

### Action taken

Three optimizations implemented (branch `benchmark-comparison`):

1. **MONOLITHIC_THRESHOLD raised from 1 MB to 256 MB.** Most blobs now
   use POST+PUT (2 requests) instead of POST+PATCH×N+PUT. Blobs above
   256 MB still use chunked upload with DEFAULT_CHUNK_SIZE of 32 MB.

2. **EARLY_REFRESH_WINDOW lowered from 15 min to 30 sec.** Docker Hub
   tokens have 300s TTL. The prior 15-minute window caused
   `should_refresh()` to always return true, completely bypassing the
   token cache (272 redundant auth exchanges → 5).

3. **Batch-check HEAD skip.** When ECR batch API confirms blobs are
   absent, the per-blob HEAD is skipped. A `HashSet<Digest>` of
   batch-checked digests is passed to the per-blob loop; blobs in the
   set skip Step 3 (HEAD). TOCTOU-safe: if a blob appears between
   batch-check and push, the redundant push succeeds (registry
   deduplicates on content-addressable digest).

Also: `bench/CLAUDE.md` added for future session onboarding.

**Post-optimization result (same corpus, same instance):**

| Metric | Before | After | Change |
|--------|--------|-------|--------|
| Total requests | 3,249 | 1,225 | -62% |
| PATCH | 1,419 | 176 | -88% |
| HEAD (blob) | 247 | 0 | -100% |
| Auth tokens | 272 | 5 | -98% |
| GET | 1,058 | 524 | -50% |
| Wall clock | 189.6s | 162.3s | -14% |
| Response bytes | 11.5 GB | 11.5 GB | 0% |

### Current competitive position (cold sync, Jupyter 5-image corpus)

| Tool | Platforms | Requests | Response bytes | Wall clock |
|------|----------|---------|----------------|------------|
| **ocync** | 2 (multi-arch) | **1,225** | 11.5 GB | **162.3s** |
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

### Remaining optimization opportunities

| # | Optimization | Requests saved | Bytes saved | Complexity |
|---|-------------|---------------|-------------|------------|
| 1 | Cross-image blob download dedup | ~168 | ~5.6 GB | High |
| 2 | Optimize warm-sync manifest comparison | ~54 | ~344 KB | Low |

### To re-validate

```bash
cd /home/ec2-user/ocync
export BENCH_TARGET_REGISTRY=660548353186.dkr.ecr.us-east-1.amazonaws.com
# Cold comparison
cargo xtask bench --tools ocync,dregsy,regsync --iterations 1 --limit 5 cold
# Warm comparison
cargo xtask bench --tools ocync,dregsy,regsync --limit 5 warm
```

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
