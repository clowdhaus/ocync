---
title: Benchmark design
description: How ocync is measured against other sync tools so that every published number is accurate and reproducible
order: 4
---

## Goal

Every published number says what it measured, what it excludes and how to reproduce it, and a difference between tools is published only when it is larger than the run-to-run noise.

The archive in `bench/results/ecr.json` shows why the noise comes first. regsync's code did not change across six cold syncs of the same corpus, yet its time ranged from 765 s to 1535 s, and dregsy's from 916 s to 1936 s. Most of that spread comes from the environment: public source registries, the time of day and the network. A single run per tool cannot tell a tool difference from that noise.

## Two kinds of test

### Protocol tests

**Question:** does each optimization take its fast path on the wire?

They live with the integration tests in `crates/ocync-distribution/tests/`. The `registry2_*.rs` suites assert mounts, pushes and client fast paths against the reference `registry:2` image through testcontainers, in CI on every pull request. A registry behavior that `registry:2` cannot reproduce, such as ECR never fulfilling a mount, is recorded on that registry's page and pinned by an engine-level test of the adapted path.

**Hard rule:** an optimization that claims to save bytes or requests ships with a protocol test that asserts its fast path was taken, such as a mount answered `201`. Without one, the optimization counts as unshipped.

### Comparison benchmark

**Question:** how does ocync compare with other sync tools on a realistic corpus, in time, bytes, requests and rate-limit responses?

It lives in `xtask/src/bench/` and runs on one instance per cloud through `cargo xtask bench-remote`. All tools run on the same instance, taking turns. One instance per tool would multiply the cost by the number of tools. Taking turns in a fresh random order every round, and comparing tools within the same round, removes the bias of a shared machine.

## Accuracy rules

The harness enforces each rule. Where a rule is not built yet, the run's output says so.

### A fixed corpus

> **Status: Planned.**

Every image in the corpus resolves to a manifest digest once and is recorded in a lock file beside the corpus. Before each round, the harness resolves every source tag again and refuses to run if any digest has changed. A floating tag such as `latest` is allowed in the corpus only through the lock. Today 29 of the corpus's 38 entries track `latest`, so two runs on different dates sync different content.

### Content verified, not only tags

> **Status: Planned.** Today the check lists the target's tags and confirms each expected tag exists.

After every tool's pass, every target tag must resolve to the same manifest digest as the locked source, and each index must hold the same child manifests. This holds every tool to ocync's own invariant that content is copied bit for bit. A tool that converts formats or drops platforms fails the run rather than being credited with a fast time.

### Repeated, interleaved rounds

> **Status: Planned.** Today each tool runs once per scenario, in one shuffled order.

A run has at least 3 rounds, 5 by default. Each round runs every tool once, cold then warm, in a fresh random order. The order and seed are recorded.

- Each tool's metric is reported as its median, with the minimum and maximum.
- A speed comparison is the median of the per-round ratios between two tools, with a 95% bootstrap confidence interval. Pairing within a round cancels drift that affects every tool in that round.
- A difference whose interval includes 1 is reported as no measured difference.

### Proxy overhead measured before it is trusted

> **Status: Planned.**

Every request goes through `bench-proxy`, which terminates and re-encrypts TLS so it can count requests, bytes, mounts and 429s per tool. That work could slow tools by different amounts. The harness therefore measures it: each tool runs several times with and without the proxy, interleaved, and the per-tool overhead is published.

- If every tool's overhead is within the run-to-run noise, wall clock is taken with the proxy in the same pass as the counts.
- If not, wall clock is taken in a pass without the proxy, and counts in a separate pass with it.

### Controlled and real-world sources

> **Status: Planned.** Today every run pulls from the public registries.

- **Controlled.** The locked corpus is copied once into a registry the bench owns, in the instance's region, and synced from there into the target. The public internet is out of the path, so speed ratios measure the tools. This is where speed claims come from.
- **Real-world.** The same corpus is pulled from its public sources. This is where rate-limit behavior, 429 counts and source request counts come from, since how a tool treats Docker Hub and the other public registries is part of what it claims.

Each published number names the source kind it came from.

### Competitors at their best

> **Status: Planned.** Today regsync and dregsy run at their default concurrency.

Before a published comparison, each tool's concurrency is swept once over a few values on the controlled source, and its fastest setting is used and recorded. A competitor's failure or quirk is reported in the results, not worked around silently in the generated configuration.

### Full provenance

Each run record carries the instance type, region and image ID, every tool's version and every relay's version (dregsy moves its bytes through skopeo). It also records the order and seed of every round.

> **Status: Planned.** Published runs are taken only from a commit on `main`, recorded by its full hash, so a run can be rebuilt later. Most commits behind the April 2026 runs are gone from the repository's history.

## Fairness steps already enforced

These run before every tool pass and stay in place:

- **CDN pre-warm** before the first tool: every source manifest and unique blob is fetched once, so the first tool does not meet a colder CDN than the last.
- **Drop the OS page cache**, so a tool does not read the previous tool's blobs from memory.
- **Clean the staging directory** and **delete and recreate the target repositories**, so a cold pass starts empty.
- **A 10 s pause** between tools for process cleanup.

`bench/CLAUDE.md` lists the operational details.

## Referrers

ocync syncs OCI 1.1 referrer artifacts (SBOMs, attestations) by default, and the compared tools do not. Its source bytes therefore include content the others never fetch: about 117 MB on the main corpus. The harness counts `referrers` requests per tool, and the summary and the performance page carry a footnote whenever a count is non-zero, so the byte columns are read with that difference in mind.

## What a published claim looks like

One sentence that names the comparison, the source kind, the statistic, the round count, the instance, the target and the commit. For example: "ocync finished a cold sync of the locked 55 GB corpus 3.4 times faster than regsync (median of 5 per-round ratios, 95% interval 3.1 to 3.7), controlled source, c6in.4xlarge, ECR us-east-2, commit `<hash>`."

## What we do not do

- **Run several tools at once on the same instance.** They would compete for the network and disk.
- **Block a pull request on a performance regression.** Runs are too costly and noisy for a merge gate. A regression is reported after merge.
- **Frame published numbers as wins.** They describe measured trade-offs under stated conditions.
