# Benchmark design v2

Status: proposal. Supersedes the implicit design currently encoded in
`xtask/src/bench/`.

## Context

The v1 benchmark (commits up to `b7c2170` on `benchmark-suite`) was
built to answer "is ocync faster than dregsy/regsync on real
registries?" and acquired additional responsibilities along the way —
visibility via HTTP proxy capture, competitor-config generation,
optimization firing-rate checks, CI regression detection.

Putting all of those on a single code path produced a benchmark that:

- **Measured infrastructure more than tools.** mitmproxy's single-core
  Python TLS capped measurable throughput at ~250 Mbps regardless of
  instance size. The published baseline (`ocync 659 MB / 243 s`, five
  Chainguard images, c6in.large) is proxy-limited for all three tools.
- **Couldn't distinguish optimization firing rate from optimization
  effectiveness.** "Did ocync's cross-repo mount save bytes in this
  run?" was not directly measurable. We only discovered that ECR
  returns 202 to every mount attempt (`mounts=0/178` in the Jupyter
  benchmark after instrumentation landed) *after* the corpus was large
  enough to force the path and the metric existed to count it.
- **Papered over competitor-tool bugs.** regsync's per-scope token
  failures on cgr.dev / gcr.io / nvcr.io and dregsy's exit-1-on-partial
  behavior both became bench-suite configuration code rather than
  reported findings. This made ocync look better than it should have
  because competitors were silently partially-failing.
- **Grew ad-hoc.** Scenarios (cold/warm/partial/scale), metrics
  (request method histogram, 429 count, duplicate GETs, mount
  attempts), and corpus entries were added in response to whatever
  question came up. There is no acceptance criterion for what a
  scenario must establish before it's useful.

## Goal

A benchmark that can be trusted — in the sense that anyone reading a
number knows what it means, what it excludes, and how to reproduce it
— and that keeps producing trustworthy numbers as ocync and the
surrounding ecosystem evolve.

## Design

Three separate things, formerly conflated, now strictly separated.

### Layer 1 — Protocol tests

**Question it answers:** does the wire protocol actually do what we
claim? For ocync: does each optimization take the designed fast path
against the registry we target?

**Where it lives:** alongside the existing integration tests in
`crates/ocync-distribution/tests/`. New suites:

- `registry2_mount.rs` — mount fast-path assertions against the
  reference `registry:2` image via testcontainers. Cheap, deterministic,
  runs in CI on every PR.
- `ecr_mount.rs` — equivalent assertions against real ECR, gated by
  `cargo test --features ecr-integration` (needs AWS credentials and
  costs ~$0.01 per run). Runs nightly in CI from an IAM role with
  scoped ECR permissions.
- `xtask probe --registry <host>` — ad-hoc developer tool that
  performs a single mount/push/mount sequence against any registry
  and prints the observed protocol behavior. Use when filing an AWS
  support ticket or diagnosing a new registry provider.

**Hard rule (enforced at code-review time):** every ocync optimization
that claims bytes/requests savings ships with a matching Layer-1 test
that asserts the fast path was taken at the wire level — `mounts > 0
&& status == 201` for cross-repo mount, `PATCH 202/201` counts for
chunked upload, `HEAD skipped` counts for cache hits, etc. An
optimization without a protocol test is considered unshipped; the PR
is blocked. Today the testing standards in CLAUDE.md describe this
principle but do not enforce it — the mount optimization shipped
without an ECR test and we got burned.

**Output format:** standard Rust `#[test]` pass/fail. No aggregation,
no dashboards. Either the protocol is correct or the build fails.

### Layer 2 — Throughput benchmark

**Question it answers:** how fast is ocync in realistic conditions,
and when it slows down, why?

**Where it lives:** `xtask/src/bench/` (cleaned up, see migration
plan below).

**Scope:** ocync only. No competitor tools. No MITM proxy. One
scenario, one question, one headline number per run.

**Measurement primitives:**

- Wall clock: `std::time::Instant` around the ocync invocation.
- Egress bytes: diff of `/proc/net/dev` `ens5` tx_bytes before and
  after the run. Captures actual network effort, immune to
  implementation-level request counting.
- CPU and memory: `pidstat -u -r -p <ocync pid> 1` streamed to a file;
  post-processed for p50/p95/max of each.
- Per-image completion timestamps: parsed from ocync's `--json` stdout
  (already emitted; we currently discard most of it).
- API-level counts (request method histogram, HTTP status distribution,
  bytes split): **not part of Layer 2**. Those belong in Layer 1
  protocol tests or in ad-hoc capture runs. Layer 2 does not add the
  proxy to the measurement path.

**Scenarios** (designed to isolate one question each):

- `cold-throughput` — fresh ECR target, first-time sync of a
  representative corpus. Measures "how fast can ocync actually push
  bytes?" Answers "is our network ceiling the limit, or are we
  leaving throughput on the table?"
- `warm-dedup` — re-sync of an already-synced corpus. Measures "how
  cheap is the no-op path?" Answers "does our HEAD-skip + cache work?"
- `incremental` — re-sync after ~5% of tags changed at source.
  Measures "does ocync reliably skip unchanged blobs while pushing
  changes?" Answers "is our change detection correct and cheap?"
- `scale` — cold throughput across corpus sizes (10, 25, 50, full).
  Measures "does ocync's throughput scale with corpus size?" Answers
  "are we O(n) in images, or is there a latent O(n²)?"

**Rules:**

- **Pre-warm is mandatory.** Before the timed window, ocync makes a
  dummy HEAD/auth request to every registry in the corpus. Auth token
  fetches, DNS, TCP setup, and initial AIMD ramp-up must not show up
  in the cold-throughput number.
- **Iterations ≥ 3, default 5, median reported, p10/p90 published.**
  Single-run variance hides real regressions. The existing
  `--iterations` flag stays but the default moves from 1 back to 5.
- **Results are versioned artifacts.** Output lands at
  `bench-results/{git_sha}/{instance_type}/{corpus_sha}/{timestamp}/`.
  Nothing is overwritten. Regression detection compares runs at the
  same `(instance_type, corpus_sha)` coordinate.
- **Failures are loud.** A scenario that can't complete (source
  registry 403, ECR rate limit, partial tool failure) fails the run,
  not the tool. The report says "incomplete" and explains why.

**Headline number per scenario:** one sentence. "ocync syncs 15 GB of
Jupyter images in X seconds (p50, N=5) on c6in.4xlarge, Docker Hub →
us-east-1 ECR, from commit <sha>." If a scenario cannot be summarized
in one sentence, it's measuring too many things and needs to be split.

### Layer 3 — Cross-tool comparison (explicitly out-of-band)

**Question it answers:** positioning. "For users considering dregsy
or regsync, what tradeoff are they making?"

**Where it lives:** `bench/competitors/` — a separate directory with
its own docs, harness, and runbook. Not part of `xtask bench`. Not
part of CI. Runs are manually initiated.

**Rules:**

- **Explicit caveats.** Every output document leads with "dregsy and
  regsync are Go/skopeo-based, have different auth architectures,
  different concurrency defaults, different feature sets. Numbers are
  directional, not authoritative."
- **Only wall clock and egress bytes are compared.** Request counts
  are not fairly comparable (skopeo subprocess boundary hides request
  fan-out). Response bytes are not fairly comparable (mount success
  reduces bytes asymmetrically).
- **Separate EC2 instance per tool.** Eliminates NAT and endpoint
  contention between tools.
- **Competitor failures are reported, not compensated.** If regsync
  needs `repoAuth: true` on cgr.dev, that's a regsync configuration
  note in the report, not silent harness magic. If dregsy exits 1 on
  partial success, the run is reported as partial. No more
  `config_gen.rs` knowing every quirk.

## Migration plan

Three sequenced PRs on `main`, each independently mergeable.

### PR #1 — Layer 1 (Protocol tests + ECR mount resolution)

**Goal:** Before changing anything about Layer 2, answer the
outstanding ECR-mount question definitively and establish the
"optimization needs a protocol test" enforcement point.

**Deliverables:**

- `xtask probe --registry <host>` subcommand. Creates two ephemeral
  repos, pushes a blob to one, waits optionally, attempts mount to
  the other, prints observed status code. Cleans up.
- `crates/ocync-distribution/tests/registry2_mount.rs` — testcontainer
  suite asserting mount returns 201 with correct `Location` header
  against the OCI reference registry. Pins protocol-compliant
  baseline.
- `crates/ocync-distribution/tests/ecr_mount.rs` — real-ECR suite
  behind `--features ecr-integration`. Asserts actual ECR mount
  behavior on a fresh pair of repos with a known-committed source
  blob. This is what we should have had before merging the mount
  optimization.
- Based on probe result:
  - If ECR returns 202 even with a fully-committed source blob: add
    an ECR-target short-circuit so ocync skips mount attempts on ECR
    entirely (saves ~1 POST per shared blob). Document in CLAUDE.md.
  - If ECR does return 201 under some specific conditions: document
    the conditions as a CLAUDE.md memory entry and update the engine
    to meet them (likely a timing adjustment or `from=` encoding fix).

**Acceptance criteria:**

- `cargo test --package ocync-distribution` passes including the new
  registry:2 suite.
- `cargo test --features ecr-integration` passes against real ECR.
- `xtask probe` produces deterministic output against both registry:2
  and ECR.
- ECR mount behavior is documented in `CLAUDE.md` with a matching
  test — not "we observed 202 that one time" but "ECR returns {201
  when X | 202 always}" backed by the probe + test.

**Out of scope:** Layer 2 changes. Competitor-tool code. Performance
measurement.

### PR #2 — Layer 2 (Throughput benchmark)

**Goal:** Restructure `xtask/src/bench/` so ocync-only measurements
are trustworthy and reproducible.

**Deliverables:**

- Remove MITM proxy from the measurement path. Proxy crate
  (`bench/proxy/`) stays; it becomes a standalone developer tool
  invoked separately as `cargo run --package bench-proxy serve …`
  when visibility is needed, not part of `xtask bench`.
- Add `/proc/net/dev` diffing for egress bytes. This replaces the
  current `response_bytes` metric sourced from the proxy log.
- Add mandatory pre-warm step to each scenario. Implementation:
  before starting the timer, ocync does one `HEAD /v2/` per registry.
  AIMD and token caches are hot when the timer starts.
- Default `--iterations 5`. Delete any documented command that uses
  `--iterations 1` (CLAUDE.md, design spec).
- Add `pidstat` capture on the ocync process; output goes into the
  run's output directory as `pidstat.txt`.
- Parse ocync's `--json` stdout for per-image timestamps. Include in
  report as a per-image timing breakdown.
- Versioned output path: `bench-results/{git_sha}/{instance_type}/{corpus_sha}/{timestamp}/`.
  Regression detection keyed on `(instance_type, corpus_sha)`.
- Delete the `ocync vs dregsy vs regsync` baseline from CLAUDE.md.
  Replace with "first Layer-2 numbers TBD."

**Acceptance criteria:**

- `cargo xtask bench cold-throughput` runs without proxy, produces a
  report with wall-clock p50/p10/p90, egress MB, CPU/memory p50/max.
- Running the same scenario twice at the same git SHA produces
  results within ±5% of each other (establishes reproducibility).
- Every scenario's headline fits in one sentence.

**Out of scope:** Competitor-tool comparison. Multi-instance
parallelism.

### PR #3 — Layer 3 (Cross-tool comparison, extracted)

**Goal:** Move competitor-tool code out of the main benchmark path
and position it correctly.

**Deliverables:**

- New directory `bench/competitors/` containing:
  - `README.md` — explicit caveats, what's comparable and what's not.
  - Go-based or bash-based harness that runs each tool against its
    own dedicated target ECR namespace.
  - Separate Terraform module for per-tool instances.
  - Results format: narrative + caveats + numbers, not leaderboard.
- Remove `config_gen.rs` competitor logic (dregsy_config,
  regsync_config) from `xtask`. Move to `bench/competitors/`.
- Update `xtask bench --tools` to reject non-ocync values with a
  helpful error pointing at `bench/competitors/`.

**Acceptance criteria:**

- `xtask bench` no longer has any knowledge of dregsy or regsync.
- `bench/competitors/` harness produces a side-by-side report at any
  git SHA, run manually. Not in CI.

**Out of scope:** new optimization work. Layer 2 iteration.

## What this avoids

The wasted cycles in the current session came from four questions
being tangled into one number:

1. "Is ocync slow?" (Layer 2 question)
2. "Is the proxy slow?" (Layer 2 infrastructure question)
3. "Does ECR honor mount?" (Layer 1 question)
4. "Is docker.io misconfigured?" (Layer 1 question)

A byte count cannot answer those. A layered split maps each question
to its answerable home. The rule "every optimization ships with a
protocol test" means a future version of this session can't happen —
if the mount optimization had shipped with an ECR-integration test
asserting `mounts > 0 && status == 201`, CI would have failed on the
original PR and we would have shipped the short-circuit up front.

## Open questions (for PR #1 to resolve)

- Does ECR return 201 to any OCI mount under any conditions? The
  probe answers this in seconds. Informs whether the optimization is
  disabled on ECR or retained.
- Does registry:2 (the reference implementation) return 201 reliably?
  If yes, the protocol is fine and ECR is the outlier. If no, the
  optimization design has a protocol issue we need to address upstream.
- What's the right per-tool timeout for a cross-tool bench? Current
  harness doesn't have one; a hung dregsy can inflate "wall clock"
  into the hours. Resolved in Layer 3 work.

## What we do NOT do

- **Multi-tool parallel benchmarking on shared infrastructure.**
  Contention masks signal.
- **Time-series resource capture as default.** Adds scope for
  diminishing return. Enable when we have a concrete question.
- **"Ocync wins" framing in any published numbers.** Numbers are
  directional, not endorsement. Positioning goes in marketing, not
  in CLAUDE.md or the spec.
- **Performance regression as a PR-blocking check.** Too flaky at
  the scale we operate. Post-merge notification only.
