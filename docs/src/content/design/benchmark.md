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

`bench-remote` drives the AWS instance, which syncs into ECR, Artifact Registry or ACR (ACR through Entra federation with an STS-signed token).

> **Status: Planned.** The Google Cloud and Azure virtual machines in `bench/terraform/` are for measuring sign-in by hand until `bench-remote` can use them as run hosts (#168).

## Accuracy rules

The harness is to enforce each rule. A rule marked Planned is not built yet. A run's summary flags a skipped or incomplete CDN pre-warm and nothing else yet.

### A fixed corpus

Every image in the corpus resolves to a manifest digest once and is recorded in `bench/corpus.lock.json` (`cargo xtask bench-lock`). Every tool syncs each corpus tag from its locked digest and writes it under the corpus tag, so a source tag that moves after the lock, such as `latest`, changes nothing: two runs on different dates sync the same content, and no round is refused because a tag moved. Before the build, the harness checks that the source still serves every locked digest the run needs, and stops, naming the digest, if one is gone. A new lock, made with `cargo xtask bench-lock`, starts a new comparison series. Every tool is given the locked digest for each tag. Given a digest, ocync skips the source when the target already holds it. With the lock's digests as pins, ocync's warm pass made 40 requests in each round of run `2026-11-01-154015`; configured with tags under the same lock, it made 108 in each round of run `2026-11-01-042432`, the 68 more being the source lookups a tag needs. The cold pass differs little: 5,784 requests against 5,824 to 5,825. A user who configures tags gets the second figures (both runs are recorded in `bench/results/ecr.json`).

Two further corpora, each with its own lock and baseline, measure shapes the main one does not: `bench/corpus-small.yaml` has 53 images of about 20 MB or less, so cost is per request rather than per byte, and `bench/corpus-large.yaml` has 6 multi-arch images of 9 to 25 GB. Their records carry the corpus name and they write nothing to the performance page.

### Content verified, not only tags

After every tool's pass, every target tag must resolve to the same manifest digest as the locked source, and each index must hold the same child manifests. This holds every tool to ocync's own invariant that content is copied bit for bit. A tool that converts formats or drops platforms fails the run rather than being credited with a fast time. dregsy runs skopeo with `--preserve-digests`, through a wrapper the harness writes for each run, because without it skopeo compresses uncompressed layers (`application/vnd.docker.image.rootfs.diff.tar`) when it pushes, and the copy is not the source content. With the flag skopeo fails a copy that cannot keep the digest.

### Repeated, interleaved rounds

A run of the sync and partial scenarios has at least 3 rounds, 5 by default. Each round runs every tool once, cold then warm, in a fresh random order. The order and seed are recorded. Scale and prepare run ocync alone, so they have no ratio to take and run one pass.

- Each tool's metric is reported as its median, with the minimum and maximum.
- A speed comparison is the median of the per-round ratios between two tools. Pairing within a round cancels drift that affects every tool in that round.
- The interval reported with it is the lowest to the highest per-round ratio. Without assuming any distribution, that range holds the true median with 93.75% confidence at 5 rounds and 75% at 3, and the confidence is stated with it. A verdict is given only from 5 rounds, with the chance that identical builds would show it printed beside it. A resampling (bootstrap) interval is not used: with this few rounds it reports more confidence than it has.
- A difference is claimed only from 5 rounds, and only when every round's ratio falls on the same side of 1. Otherwise it is reported as no measured difference, or, below 5 rounds, not at all.
- Wall clock, requests and response bytes are compared this way. Every round's results are kept in the run record, so the medians can be recomputed.

A baseline build of ocync, from a commit given with `--baseline-ref`, takes part in every round as one more tool, run with the config generated for ocync. Its verdict against the current build is the regression check: old and new ocync measured in the same rounds, not compared across months.

### Proxy overhead measured before it is trusted

Every request goes through `bench-proxy` unless the run passes `--no-proxy`, which records times but no traffic counts. The proxy terminates and re-encrypts TLS so it can count requests, bytes, mounts and 429s per tool. That work could slow tools by different amounts. The harness therefore measures it: `--measure-proxy-overhead` runs each tool's sync twice in every round, through the proxy and directly, back to back, and reports the per-tool overhead.

```bash
cargo xtask bench-remote --provider aws --scenario sync --tools ocync,regsync --measure-proxy-overhead
```

- It applies to the sync scenario, whose cold sync and warm re-sync are each run both ways, and is refused for the others and beside `--no-proxy`. Each leg starts from an emptied target with its own config, so the second leg does not run against the first's cache. That needs ECR: Artifact Registry and ACR keep a round's blobs until the round ends, so the option is refused for them.
- Each proxied run meets twice the usual load on the registries, so a paired run writes nothing to the performance page and is refused beside `--regression`. Its record still goes to the archive, where `direct_wall_clock_secs` marks it.
- The proxied leg is first in odd rounds and the direct leg in even ones, so drift across a round is charged to neither route.
- The direct leg records time only. Every count in the summary and the run record comes from the proxied legs, and the record stores the direct leg's wall clock beside the proxied run it was paired with (`direct_wall_clock_secs`), so the overhead can be recomputed from it.
- The summary gives, per tool and per scenario, the median and range of the per-round ratio of the proxied to the direct wall clock, and whether the overhead is measured: every round on the same side of 1. This is the rule a speed comparison between tools uses, and it needs 5 rounds, where identical routes would put every round on one side one time in sixteen. Below 5 rounds the summary says too few rounds. A small overhead seen in every round counts as measured, and the median shows how large it is.
- It doubles the run time of the sync scenario, and takes the usual rounds (at least 3, 5 by default).

The rule that follows from the result:

- If no tool shows a measured overhead, wall clock is taken with the proxy in the same pass as the counts.
- If not, wall clock is taken in a pass without the proxy, and counts in a separate pass with it.

> **Status: Planned.** The harness reports whether each tool shows a measured overhead, but a run does not yet apply the rule: today wall clock is always taken with the proxy, in the same pass as the counts, and the result of a paired run is read by a person.

A first check by hand, on 2026-11-03, could not resolve the overhead. It ran ocync (built from `main`, reporting 0.8.2) and regsync v0.11.7 into ECR on the c6in.4xlarge bench instance, over the first 5 corpus images, all from quay.io (corpus locked 2026-11-01, sources pre-warmed). Four passes alternated with and without the proxy, each 3 rounds of a cold then a warm sync, so 6 rounds per tool each way. The first pass, with the proxy, ran at 11bae49 and the rest at f7d0cf2, which changes only how the harness records regsync's version. The passes are run records `2026-11-03-203607`, `-205529`, `-211254` and `-212951` in `bench/results/ecr.json`.

| Median (range), cold sync | With proxy | Without proxy |
|---|---:|---:|
| ocync wall clock | 44.6s (41s..1m 3s) | 51.5s (38s..1m 22s) |
| regsync wall clock | 3m 6s (2m 21s..3m 38s) | 3m 8s (2m 23s..3m 48s) |

With the proxy, a cold sync took ocync a median 738 requests for 6.2 GB of responses and regsync 1,571 requests for 13.2 GB. A warm sync took ocync 0.3s with the proxy and 0.2s without (6 requests, 4 KB), and regsync 0.8s and 0.7s (11 requests, 0 B). No pass saw a 429. Without the proxy nothing is counted.

ocync's cold median came out faster with the proxy, which the proxy cannot cause: drift between unpaired passes is larger than any proxy effect, and the ranges are wide enough to hide a sizeable one. The warm passes are slower with the proxy for both tools, the direction a fixed per-request cost would show. Resolving it needs the paired, in-round measurement above. The check also leaves out the other tools, the other sources, the small corpus, and the Artifact Registry and ACR targets.

### Controlled and real-world sources

> **Status: Planned.** Today every run pulls from the public registries.

- **Controlled.** The locked corpus is copied once into a registry the bench owns, in the instance's region, and synced from there into the target. The public internet is out of the path, so speed ratios measure the tools. This is where speed claims come from.
- **Real-world.** The same corpus is pulled from its public sources. This is where rate-limit behavior, 429 counts and source request counts come from, since how a tool treats Docker Hub and the other public registries is part of what it claims.

Each published number names the source kind it came from.

### Competitors at their best

> **Status: Planned.** Today every competitor runs at its default concurrency, and the generated regsync configuration sets `repoAuth: true` on every source without the results saying so.

The defaults the sweep starts from, which the harness documents and never sets:

| Tool | Inner concurrency default |
|---|---|
| skopeo | 6 parallel downloads (containers/image `maxParallelDownloads`) |
| crane | `-j` effectively 4 (pflag overwrites the GOMAXPROCS default with 0, which crane ignores) |
| regctl | 3 requests per host (`reqConcurrent`) |
| oras | `--concurrency` 3 |

Before a published comparison, each tool's concurrency is swept once over a few values on the controlled source, and its fastest setting is used and recorded. A competitor's failure or quirk is reported in the results, not worked around silently in the generated configuration.

### Full provenance

Each run record carries the instance type, region and image ID, every tool's version and every relay's version (dregsy moves its bytes through skopeo). It also records the order and seed of every round.

Published runs are taken only from a commit on `main`, recorded by its full hash, so a run can be rebuilt later. Most commits behind the April 2026 runs are gone from the repository's history.

## Fairness steps already enforced

- **CDN pre-warm**, once per run before the first tool (`--skip-prewarm` skips it): every blob the corpus lock records for the run's tags is fetched once per repository, a fixed number at a time per registry, within 8 minutes, then a 6 minute wait if a response reports a CDN cache miss (a response with no cache header does not count). A short list of registries measured to have no blob cache (quay.io and cgr.dev, which serve blobs from object storage) is probed with one blob each: with no cache header, the rest of that registry's blobs are not requested and the run record names it under `no_cache`; with one, the registry is warmed in full and the run warns that the list is out of date. Manifests are not warmed, because no registry measured caches them. A skipped pre-warm, or a registry not fully warmed and how many of its blobs, is named in the run's summary and record.
- Before each tool's cold pass:
  - **Drop the OS page cache**, so a tool does not read the previous tool's blobs from memory.
  - **Clean the staging directory** and **delete and recreate the target repositories**, so the cold pass starts empty.
- The warm pass follows its cold pass directly, against the content that pass synced.
- **A 10 s pause** between tools for process cleanup.

`bench/CLAUDE.md` lists the operational details.

## Per-tag tools

skopeo, crane, regctl and oras are compared too. Each copies one tag per process, driven by the harness, because `skopeo sync` cannot copy `source@digest` to `target:tag`. Every copy keeps the source digest:

- **skopeo** runs `copy --preserve-digests --multi-arch all`: without the first flag it recompresses an uncompressed layer on push and the digest changes, and with it a copy that cannot keep the digest fails instead.
- **crane** `copy` pushes the manifest and blobs as fetched, every platform of an index.
- **regctl** `image copy` does the same, with no platform, referrer or digest-tag option given.
- **oras** `cp` copies the whole index and every blob unchanged, without `--recursive`, which would add referrers.

Fairness to ocync is not automatic for them. They run one copy at a time, as a user's loop over the tags would, and the run record names that (`per_tag_jobs`). Each process pays its own ping and token per registry and a warm pass re-fetches each source manifest, so their request counts include that per-process cost. The target's ECR token is not paid per process, since `docker-credential-ecr-login` caches it in `~/.ecr/cache.json` for 12 hours across processes, tools and rounds, so only the run's first copy fetches it. Their peak RSS is the largest single process, not a total, and the reports say so. The loop stops at the first copy that fails, naming it as `n/total` and the target repository and tag, never the registry host. Tuning their parallelism is a later item, under [Competitors at their best](#competitors-at-their-best).

## Referrers

ocync syncs OCI 1.1 referrer artifacts (SBOMs, attestations) by default, and the other tools copy none as configured here: regctl and oras support referrers and run with them off, so no competitor does extra work that ocync's numbers are compared against. Its source bytes therefore include content the others never fetch: about 117 MB on the main corpus. The harness counts `referrers` requests per tool, and the summary and the performance page carry a footnote whenever a count is non-zero, so the byte columns are read with that difference in mind.

## Published runs

Every public number comes from one run, called the published run, and from nothing else. Each surface that shows a number names that run by its timestamp.

The archive shows what happens without this rule. The cold-sync figures published since April came from a 39-image corpus (`2026-04-26-131910`), a 40-image one (`2026-10-31-035712`) and a 25-image one (`2026-10-31-211851`), over 1, 4 and 5 rounds. The tools compared changed from run to run: dregsy left, ocync's own April build joined, and the per-tag tools never appeared. The banner, the README and the performance page each quote a different one of those runs.

### What a run must be to be published

A run is published only through `--publish`, on `cargo xtask bench-remote`, or on `cargo xtask bench` for a run on the instance itself. The flag refuses to start unless the arguments fit the list below, and `bench-remote`, which owns the build, also refuses a commit that is not on `origin/main`, before it pushes. A direct `bench --publish` run checks the recorded hash's form and nothing more, so the place of its commit is on the operator. After the run, the record is checked against the list again. A run that fails the second check is archived as an experiment and writes no surface, and a warning says which requirement it missed.

- **Corpus:** the whole main corpus at its current lock. A new lock starts a new series, and the next published run must use it.
- **Tools:** ocync, regsync, dregsy, skopeo, crane, regctl and oras, every one of them, each with a run in all 5 rounds of the cold and the warm sync. The page keeps a column for each of the seven, so a tool with no run reads `--` there and never leaves the table. Today a tool that fails the content check stops the whole run, which then has fewer than 5 rounds and is not published.
- **Rounds:** 5 requested and 5 finished. A run that loses a round is archived and not published.
- **Settings:** every tool at the defaults it ships with, or at the setting the competitor sweep chose. ocync runs at its own defaults, with the pre-warm run and the proxy on. `--max-concurrent-transfers`, `--skip-prewarm`, `--no-proxy` and `--measure-proxy-overhead` are refused.
- **Build:** a commit on `main`, recorded by its full hash.
- **Place:** the standard instance in `bench/terraform/aws`, syncing into ECR in the same region. The scenario is `sync` or `all`, so the cold and the warm sync both run.
- **No baseline:** no baseline build. The regression check against an older ocync is its own run and is never published. `--limit` and `--skip-registries` are refused with it.

> **Status: Planned.** A tool that fails the content check, or fails to finish, stays in the page as a column that names the failure, and the run goes on to the rest. Today the first failure stops the run.

### What it writes

From the one run record, `--publish` writes:

- **Performance page tables:** one per scenario, cold and warm, between the existing markers, with a column for each of the seven tools, ocync first. Each cell is the median over the 5 rounds, the best value in each row is bold, and one line above the table gives the date, the run timestamp, the short commit, the instance, the corpus size, the target and the round count. Ranges, per-round ratios and confidence are not on the page. They stay in the run record, and the run's `summary.md` carries them.
- **Page description:** the `description` front matter of `docs/src/content/performance.md`.
- **README:** the summary sentence in `README.md`, between a new pair of markers.
- **Homepage and banners:** the speed and request figures in `docs/src/pages/index.astro`, in `docs/public/ecr-banner.svg` and `docs/public/og-image.svg`, and in the README banner's alt text. The PNG copies are rendered from the SVGs with `sharp`, which the docs site already installs.

Every surface states its numbers the same way, computed once from the record. The speed is the median over the rounds of regsync's wall clock divided by ocync's, round by round, to one decimal (`2.9x` in a banner, `2.9 times faster than regsync` in prose). The request saving is the percent fewer requests than regsync from the medians, in whole percent. The byte saving is the gigabytes fewer response bytes than regsync from the medians, to one decimal, in prose only. All of it is the cold sync. A run where regsync has no value in every round, or where ocync is not ahead of it, publishes nothing, since the surfaces word the figures as gains.

Each generated region sits between a pair of marker comments (`<!-- BENCH-README:START -->` and `<!-- BENCH-README:END -->` in Markdown and SVG, `// BENCH-STATS:START` in the homepage script, `# BENCH-DESCRIPTION:START` in the page's front matter) and names the run by its timestamp, in its text or in a comment line inside the markers. The banner SVGs carry it in their `<desc>`. A run without `--publish` writes none of these surfaces. The surfaces are all computed and checked before any is written, so a marker that has gone missing stops the publish before it changes a file. Fetching an older published run after a newer one leaves the surfaces on the newer one.

### Telling a published record from an experiment

The archive keeps every run, experiments included. `--publish` sets `published: true` on its record. Every surface cites the newest published record. Every other record is an experiment, whatever its corpus or rounds.

### The check that keeps them in line

A test reads the newest published record from `bench/results/ecr.json`, computes every generated region from it, and fails when any surface above, other than the PNG copies, differs from the result or cites another run. A hand edit to any of them fails CI, and `cargo xtask bench-publish-surfaces` rewrites them from the newest published record alone, without a run. So does a publish that updates one surface and not the others. The PNG copies are the one thing it cannot check, so the publish step renders them and fails when it cannot. Until the first published run, the archive holds no published record and the surfaces keep the text they have. Other tests fit a stand-in run to every tracked file, so a deleted marker fails CI before the first publish does.

## What we do not do

- **Run several tools at once on the same instance.** They would compete for the network and disk.
- **Block a pull request on a performance regression.** Runs are too costly and noisy for a merge gate. A regression is reported after merge.
- **Frame published numbers as wins.** They describe measured trade-offs under stated conditions.
