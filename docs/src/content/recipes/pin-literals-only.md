---
title: Pin literals only
description: Mirror a small set of named tags without listing the source registry - the rate-limit-friendly pattern for floating tags and explicit version pins.
order: 3
---

When you already know the exact tag names you want, list them as literals. `ocync` skips the `list-tags` API call entirely and HEAD-checks each pinned tag directly. This is the rate-limit-friendly pattern for sources like Docker Hub.

## When to use

- Floating tags (`latest`, `stable`, `nightly`)
- Explicit pinned releases when you do not need automatic discovery
- Mirrors of low-tag-volume repos where one HEAD per tag beats listing

## Config

```yaml
registries:
  source:
    url: ghcr.io
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: source
  targets: ecr

mappings:
  - from: argoproj/argocd
    to: argocd
    tags:
      glob: ["v2.13.0", "v2.13.1", "stable"]
```

## Fields

When every entry in `glob:` is wildcard-free (no `*`, `?`, `[`) and no other tag fields are set (`semver`, `sort`, `latest`, `exclude`, `min_tags`), `ocync` recognizes the entries as literals and skips the source `list-tags` walk entirely. Each pinned tag becomes one HEAD against the source instead of a paginated listing of every tag in the repository.

For sources with thousands of tags this matters even when listing is not separately rate-limited: several round trips of pagination turn into zero. For Docker Hub specifically, the cap that bites is the manifest GET budget (10 per hour anonymous), and the HEAD checks issued for pinned tags do count against it - the saving here is the avoided listing cost, not a different rate-limit bucket.

## Variations

Multiple literal pins OR-match within a single `glob:` list, and the all-literal optimization still applies as long as every entry is wildcard-free:

```yaml
tags:
  glob: ["latest", "v2.13.0", "v2.13.1"]
```

Adding any wildcard entry to the same list (e.g. `"v2.*"`) disables the optimization for the whole list - the listing is then performed and the filter narrows in memory.

Mixing literal pins with the minimum-bytes pattern:

```yaml
defaults:
  artifacts:
    enabled: false
  tags:
    glob: ["latest"]
```

## Caveats

- "Latest five versions" needs ordering, not pinning - use `semver` + `sort` + `latest` instead.
- A tag pattern (`v2.*`) is a glob, not a literal - either accept the listing cost or list each version explicitly.
- Pinned tags and a filtered range cannot share a single mapping today; split into two mappings (one literal-only, one filtered) targeting the same `to:` repository.

## When to use `glob:` vs `include:`

`glob:` is the *filter* mechanism: it narrows the candidate pool. Use it when you want only specific tags, with no `semver:` range driving the pipeline.

`include:` is the *augment* mechanism: it always-adds tags alongside a `semver:`-driven pipeline. Use it when you want literal pins (`latest`, `latest-dev`) plus a version range in the same mapping.

For pin-only use cases (this recipe), `glob:` is the right field. For pin-plus-range, see [semver tracking](/recipes/semver-tracking).

## Related

- [Minimum bytes](/recipes/minimum-bytes) for the broader cost-sensitive pattern
- [Configuration reference](/configuration#tag-filtering) for the tag pipeline
