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
  so adding a new provider forces a compile-time decision. `Ecr` returns
  `false`; all other known providers return `true`.
- `blob_mount` short-circuits when the target hostname resolves to a
  provider that does not fulfill mount — returns `MountResult::Skipped`
  without issuing the POST, saving one round-trip per shared blob per
  target.
- `MountResult` has three variants to express what the engine needs to
  know:
  - `Mounted` — registry fulfilled the mount (201).
  - `NotFulfilled` — registry returned 202; the cached mount-source hint
    is probably stale, engine invalidates.
  - `Skipped` — client short-circuited without a request; the hint was
    never tested, engine does NOT invalidate. This preserves the
    in-progress claim so concurrent tasks transferring the same blob
    to a different repo at this target wait for the push and reuse the
    staged data (pull-once fan-out).
- Protocol coverage: `tests/registry2_mount.rs` pins `Mounted` and
  `NotFulfilled` against the OCI reference `registry:2` container.
  `engine_integration::sync_warm_cache_ecr_target_short_circuits_mount`
  pins the engine-level wire behavior (0 mount POSTs on ECR) and the
  cache-preservation property (Skipped must not invalidate).

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

### Cost of this particular finding

Approximately one round-trip per shared blob per target on ECR — the
fallback path (HEAD + push) already transferred the blob correctly, so
this is a wall-clock saving, not a bytes or rate-limit saving. The
larger significance is procedural: the original mount optimization
shipped without a wire-level test against real ECR, so the silent no-op
went undetected until a proxy log made it visible.

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
