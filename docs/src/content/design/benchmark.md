---
title: Benchmark design
description: Layered benchmark plan covering protocol, throughput, and cross-tool comparison
order: 4
---

## Context

The v1 benchmark was built to answer "is ocync faster than dregsy/regsync on real
registries?" and acquired additional responsibilities along the way --
visibility via HTTP proxy capture, competitor-config generation,
optimization firing-rate checks, CI regression detection.

Putting all of those on a single code path produced a benchmark that:

- **Measured infrastructure more than tools.** mitmproxy's single-core
  Python TLS capped measurable throughput at ~250 Mbps regardless of
  instance size.
- **Couldn't distinguish optimization firing rate from optimization
  effectiveness.** "Did ocync's cross-repo mount save bytes in this
  run?" was not directly measurable. We only discovered that ECR
  returns 202 to every mount attempt *after* the corpus was large
  enough to force the path and the metric existed to count it.
- **Papered over competitor-tool bugs.** regsync's per-scope token
  failures and dregsy's exit-1-on-partial behavior both became
  bench-suite configuration code rather than reported findings. This
  made ocync look better than it should have because competitors were
  silently partially-failing.
- **Grew ad-hoc.** Scenarios, metrics, and corpus entries were added in
  response to whatever question came up. There is no acceptance
  criterion for what a scenario must establish before it's useful.

## Goal

A benchmark that can be trusted, in the sense that anyone reading a
number knows what it means, what it excludes, and how to reproduce it
-- and that keeps producing trustworthy numbers as ocync and the
surrounding ecosystem evolve.

## Design

Three separate things, formerly conflated, now strictly separated.

### Layer 1: protocol tests

**Question it answers:** does the wire protocol actually do what we
claim? For ocync: does each optimization take the designed fast path
against the registry we target?

**Where it lives:** alongside the existing integration tests in
`crates/ocync-distribution/tests/`.

- `registry2_*.rs` suites run mount/client/push fast-path assertions
  against the reference `registry:2` image via testcontainers. Cheap,
  deterministic, runs in CI on every PR.
- Registry-specific quirks that cannot be exercised against
  `registry:2` (e.g. ECR's "never fulfills mount" behavior) are
  captured as evidence in the findings log and pinned by engine-level
  integration tests asserting the adapted code path.

**Hard rule:** every ocync optimization that claims bytes/requests
savings ships with a matching Layer 1 test that asserts the fast path
was taken at the wire level, such as `mounts > 0 && status == 201` for
cross-repo mount, `PATCH 202/201` counts for chunked upload, and `HEAD
skipped` counts for cache hits. An optimization without a
protocol test is considered unshipped; the PR is blocked.

**Output format:** standard Rust `#[test]` pass/fail. No aggregation,
no dashboards. Either the protocol is correct or the build fails.

### Layer 2: throughput benchmark

**Question it answers:** how fast is ocync in realistic conditions,
and when it slows down, why?

**Where it lives:** `xtask/src/bench/`.

**Scope:** ocync only. No competitor tools. No MITM proxy. One
scenario, one question, one headline number per run.

**Measurement primitives:**

- Wall clock: `std::time::Instant` around the ocync invocation.
- Egress bytes: diff of `/proc/net/dev` `ens5` tx_bytes before and
  after the run. Captures actual network effort, immune to
  implementation-level request counting.
- CPU and memory: `pidstat -u -r -p <ocync pid> 1` streamed to a file;
  post-processed for p50/p95/max of each.
- Per-image completion timestamps: parsed from ocync's `--json` stdout.
- API-level counts (request method histogram, HTTP status distribution,
  bytes split): **not part of Layer 2**. Those belong in Layer 1
  protocol tests or in ad-hoc capture runs.

**Scenarios** (designed to isolate one question each):

- `cold-throughput`: fresh ECR target, first-time sync of a
  representative corpus. Measures "how fast can ocync actually push
  bytes?"
- `warm-dedup`: re-sync of an already-synced corpus. Measures "how
  cheap is the no-op path?"
- `incremental`: re-sync after ~5% of tags changed at source.
  Measures "does ocync reliably skip unchanged blobs while pushing
  changes?"
- `scale`: cold throughput across corpus sizes (10, 25, 50, full).
  Measures "does ocync's throughput scale linearly with corpus size?"

**Rules:**

- **Pre-warm is mandatory.** Before the timed window, ocync makes a
  dummy HEAD/auth request to every registry in the corpus. Auth token
  fetches, DNS, TCP setup, and initial AIMD ramp-up must not show up
  in the cold-throughput number.
- **Iterations >= 3, default 5, median reported, p10/p90 published.**
  Single-run variance hides real regressions.
- **Results are versioned artifacts.** Output lands at
  `bench-results/{git_sha}/{instance_type}/{corpus_sha}/{timestamp}/`.
  Nothing is overwritten. Regression detection compares runs at the
  same `(instance_type, corpus_sha)` coordinate.
- **Failures are loud.** A scenario that can't complete (source
  registry 403, ECR rate limit, partial tool failure) fails the run,
  not the tool. The report says "incomplete" and explains why.

**Headline number per scenario:** one sentence. "ocync syncs 15 GB of
images in X seconds (p50, N=5) on c6in.4xlarge, Docker Hub to
us-east-1 ECR, from commit `<sha>`." If a scenario cannot be
summarized in one sentence, it's measuring too many things and needs
to be split.

### Layer 3: cross-tool comparison

**Question it answers:** positioning. "For users considering dregsy
or regsync, what tradeoff are they making?"

**Where it lives:** `bench/competitors/`, a separate directory with
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
  partial success, the run is reported as partial. No more config
  generation knowing every quirk.

## What this avoids

The wasted cycles in the original benchmark came from four questions
being tangled into one number:

1. "Is ocync slow?" (Layer 2 question)
2. "Is the proxy slow?" (Layer 2 infrastructure question)
3. "Does ECR honor mount?" (Layer 1 question)
4. "Is docker.io misconfigured?" (Layer 1 question)

A byte count cannot answer those. A layered split maps each question
to its answerable home. The rule "every optimization ships with a
protocol test" means this kind of silent failure can't recur. If the
mount optimization had shipped with a wire-level test asserting
`mounts > 0 && status == 201`, CI would have failed on the original PR
and the short-circuit would have shipped up front.

## What we do NOT do

- **Multi-tool parallel benchmarking on shared infrastructure.**
  Contention masks signal.
- **Time-series resource capture as default.** Adds scope for
  diminishing return. Enable when we have a concrete question.
- **"Ocync wins" framing in any published numbers.** Numbers are
  directional, not endorsement. Positioning goes in marketing, not
  in design docs.
- **Performance regression as a PR-blocking check.** Too flaky at
  the scale we operate. Post-merge notification only.
