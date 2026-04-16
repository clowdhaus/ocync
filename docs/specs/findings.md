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
| `xtask probe` 0s wait (commit `c2d33d6`) | synthetic 1 KiB blob | 5 | 0 | 5 | 0 |
| `xtask probe` 10s wait | same | 5 | 0 | 5 | 0 |
| `xtask probe` 60s wait | same | 5 | 0 | 5 | 0 |
| **Total** | | **193** | **0** | **193** | **0** |

The probe deliberately varied the interval between source blob commit and
mount attempt (0s / 10s / 60s) to test the hypothesis that ECR requires
settling time before fulfilling mount. It does not — all three delays
produced the same result.

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

### Action taken (PR #25)

- `MountResult::NotSupported` variant added; `blob_mount` returns it
  without a network request when the target is ECR.
- `supports_cross_repo_mount(Option<ProviderKind>)` helper in
  `ocync-distribution::blob` is the single point where registry mount
  capability is recorded.
- `blob_mount_unchecked` exposes the unconditional POST for diagnostic
  tooling (`xtask probe`) so future observations can bypass the
  short-circuit.
- Protocol tests pair a positive case (`registry:2` returns 201,
  `MountResult::Mounted`) with a negative case (real ECR returns 202,
  `MountResult::FallbackUpload` via unchecked; `MountResult::NotSupported`
  via `blob_mount`).

### To re-validate

If AWS behavior changes, flip `supports_cross_repo_mount` to allow ECR
and update both `tests/ecr_mount.rs::ecr_mount_returns_not_supported`
and `tests/ecr_mount.rs::ecr_mount_unchecked_returns_fallback_upload`
together. Re-confirm with:

```
cargo xtask probe \
    --registry <account>.dkr.ecr.<region>.amazonaws.com \
    --iterations 5
```

One 201 in the probe output is sufficient grounds to re-evaluate; AWS
may fulfill mount only under specific conditions (IAM scope, same
account vs. cross-account, specific regions), in which case the probe's
output should be analyzed before flipping the short-circuit.

### Cost of this particular finding

Approximately 30 seconds of round-trip latency per 178-blob sync —
measurable but not the dominant efficiency lever on the primary use
case. The larger significance: the mount optimization shipped without a
wire-level test against real ECR, so the silent no-op went undetected
for months. See `feedback_optimization_protocol_test.md` for the rule
that now prevents this class of bug.

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
