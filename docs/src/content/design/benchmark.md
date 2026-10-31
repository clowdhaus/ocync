---
title: Benchmark design
description: How ocync is measured against other sync tools so that every published number is accurate and reproducible
order: 4
---

## Goal

Every published number says what it measured, what it excludes and how to reproduce it, and a difference between tools is published only when it is larger than the run-to-run noise.

The archive in `bench/results/ecr.json` shows why the noise comes first. Over five cold syncs to ECR in us-east-2 between 22 and 26 April 2026, regsync took 765 s to 1509 s and dregsy 916 s to 1273 s, with neither tool changed. Part of that spread is the environment (public source registries, the time of day, the network), and part may be the content itself, since the corpus's floating tags can move between dates. A single run per tool cannot tell a tool difference from either.

## Two kinds of test

### Protocol tests

**Question:** does each optimization take its fast path on the wire?

They live with the integration tests in `crates/ocync-distribution/tests/`. The `registry2_*.rs` suites assert mounts, pushes and client fast paths against the reference `registry:2` image through testcontainers, in CI on every pull request. A registry behavior that `registry:2` cannot reproduce, such as ECR never fulfilling a mount, is recorded on that registry's page and pinned by an engine-level test of the adapted path.

**Hard rule:** an optimization that claims to save bytes or requests ships with a protocol test that asserts its fast path was taken, such as a mount answered `201`. Without one, the optimization counts as unshipped.

### Comparison benchmark

**Question:** how does ocync compare with other sync tools on a realistic corpus, in time, bytes, requests and rate-limit responses?

It lives in `xtask/src/bench/` and runs on one instance through `cargo xtask bench-remote`. All tools run on the same instance, taking turns. One instance per tool would multiply the cost by the number of tools; taking turns in a fresh random order every round, and comparing tools within the same round, is how the shared machine is kept fair.

> **Status: Planned.** Today each tool runs once per scenario in one shuffled order, and `bench-remote` drives only the AWS instance. The Google Cloud and Azure hosts in `bench/terraform/` are for measuring sign-in by hand until they can be driven (#168).

## Accuracy rules

The harness is to enforce each rule. A rule marked Planned is not built yet. A run's summary flags a skipped or incomplete CDN pre-warm and nothing else yet.

### A fixed corpus

Every image in the corpus resolves to a manifest digest once and is recorded in `bench/corpus.lock.json` (`cargo xtask bench-lock`). Before the run and again before every later scenario, the harness resolves every source tag again and stops if any digest has changed. A floating tag such as `latest` is allowed in the corpus only through the lock, so two runs on different dates sync the same content or do not run.

> **Status: Planned.** Re-resolving the tags before each round comes with repeated rounds.

### Content verified, not only tags

After every tool's pass, every target tag must resolve to the same manifest digest as the locked source, and each index must hold the same child manifests. This holds every tool to ocync's own invariant that content is copied bit for bit. A tool that converts formats or drops platforms fails the run rather than being credited with a fast time.

### Repeated, interleaved rounds

> **Status: Planned.** Today each tool runs once per scenario, in one shuffled order.

A run has at least 3 rounds, 5 by default. Each round runs every tool once, cold then warm, in a fresh random order. The order and seed are recorded.

- Each tool's metric is reported as its median, with the minimum and maximum.
- A speed comparison is the median of the per-round ratios between two tools. Pairing within a round cancels drift that affects every tool in that round.
- The interval reported with it is the lowest to the highest per-round ratio. Without assuming any distribution, that range holds the true median with 93.75% confidence at 5 rounds and 75% at 3, and the confidence is stated with it. A resampling (bootstrap) interval is not used: with this few rounds it reports more confidence than it has.
- A difference is claimed only when every round's ratio falls on the same side of 1. Otherwise it is reported as no measured difference.

### Proxy overhead measured before it is trusted

> **Status: Planned.**

Every request goes through `bench-proxy` unless the run passes `--no-proxy`, which records times but no traffic counts. The proxy terminates and re-encrypts TLS so it can count requests, bytes, mounts and 429s per tool. That work could slow tools by different amounts. The harness therefore measures it: each tool runs several times with and without the proxy, interleaved, and the per-tool overhead is published.

- If every tool's overhead is within the run-to-run noise, wall clock is taken with the proxy in the same pass as the counts.
- If not, wall clock is taken in a pass without the proxy, and counts in a separate pass with it.

### Controlled and real-world sources

> **Status: Planned.** Today every run pulls from the public registries.

- **Controlled.** The locked corpus is copied once into a registry the bench owns, in the instance's region, and synced from there into the target. The public internet is out of the path, so speed ratios measure the tools. This is where speed claims come from.
- **Real-world.** The same corpus is pulled from its public sources. This is where rate-limit behavior, 429 counts and source request counts come from, since how a tool treats Docker Hub and the other public registries is part of what it claims.

Each published number names the source kind it came from.

### Competitors at their best

> **Status: Planned.** Today regsync and dregsy run at their default concurrency, and the generated regsync configuration sets `repoAuth: true` on every source without the results saying so.

Before a published comparison, each tool's concurrency is swept once over a few values on the controlled source, and its fastest setting is used and recorded. A competitor's failure or quirk is reported in the results, not worked around silently in the generated configuration.

### Full provenance

Each run record carries the instance type, region and image ID, every tool's version and every relay's version (dregsy moves its bytes through skopeo). It also records the order and seed of every round.

> **Status: Planned.** Published runs are taken only from a commit on `main`, recorded by its full hash, so a run can be rebuilt later. Most commits behind the April 2026 runs are gone from the repository's history.

## Fairness steps already enforced

- **CDN pre-warm**, once per run before the first tool (`--skip-prewarm` skips it): every blob the corpus lock records for the run's tags is fetched once per repository, a fixed number at a time per registry, within 8 minutes. Manifests are not warmed. A skipped pre-warm, or a registry not fully warmed and how many of its blobs, is named in the run's summary and record.
- Before each tool's cold pass:
  - **Drop the OS page cache**, so a tool does not read the previous tool's blobs from memory.
  - **Clean the staging directory** and **delete and recreate the target repositories**, so the cold pass starts empty.
- The warm pass follows its cold pass directly, against the content that pass synced.
- **A 10 s pause** between tools for process cleanup.

`bench/CLAUDE.md` lists the operational details.

## Referrers

ocync syncs OCI 1.1 referrer artifacts (SBOMs, attestations) by default, and the compared tools do not. Its source bytes therefore include content the others never fetch: about 117 MB on the main corpus. The harness counts `referrers` requests per tool, and the summary and the performance page carry a footnote whenever a count is non-zero, so the byte columns are read with that difference in mind.

## What a published claim looks like

One sentence that names the comparison, the source kind, the statistic, the round count, the instance, the target and the commit, in this form: "ocync finished a cold sync of the locked corpus `<ratio>` times faster than `<tool>` (median of `<n>` per-round ratios, range `<low>` to `<high>`, `<confidence>`% confidence), `<source kind>` source, `<instance type>`, `<registry and region>`, commit `<hash>`."

## What we do not do

- **Run several tools at once on the same instance.** They would compete for the network and disk.
- **Block a pull request on a performance regression.** Runs are too costly and noisy for a merge gate. A regression is reported after merge.
- **Frame published numbers as wins.** They describe measured trade-offs under stated conditions.
