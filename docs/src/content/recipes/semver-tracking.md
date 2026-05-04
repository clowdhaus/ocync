---
title: Semver release tracking
description: Track the latest N versions of an upstream release stream while controlling pre-release inclusion - the long-running mirror pattern.
order: 6
---

For a mirror that follows an upstream's active release stream, you typically want the most recent N stable versions and explicit control over pre-releases. Three fields work together: `semver` for the version range, `sort` for ordering, and `latest` for the cap.

## When to use

- Long-running mirror that should pick up new releases automatically
- Want a hard cap on the number of versions kept (storage and egress control)
- Need explicit control over whether pre-release tags are included

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
  - from: my-org/my-app
    to: my-app
    tags:
      include: ["latest", "latest-dev"]
      semver: ">=2.0"
      sort: semver
      latest: 10
      min_tags: 1
```

## Fields

`include: ["latest", "latest-dev"]` always pins these literal floats. They survive the `semver:` filter and the system-exclude defaults.

`semver: ">=2.0"` drops everything below 2.0 from the candidate pool. Common prerelease patterns (`*-rc*`, `*-alpha*`, `*-beta*`, `*-pre*`, `*-snapshot*`, `*-nightly*`) drop by default via the system-exclude. Variant suffixes like `-alpine` and `-r0` are admitted (suffix is opaque).

`sort: semver` orders the surviving pipeline tags by version, highest first.

`latest: 10` keeps only the top 10 of the pipeline side. Include-pinned floats pass through uncapped.

`min_tags: 1` is a safety net for long-running mirrors. Counts the union of include and pipeline.

## Variations

The default already drops `*-rc*`, `*-alpha*`, `*-beta*`, `*-pre*`, `*-snapshot*`, `*-nightly*` via the system-exclude -- no extra config needed for stable-only:

```yaml
tags:
  semver: ">=2.0"
  sort: semver
  latest: 10
```

To opt back into all prereleases, add the patterns to `include:` (which overrides the system default):

```yaml
tags:
  include: ["*-rc*", "*-alpha*", "*-beta*", "*-pre*", "*-snapshot*", "*-nightly*"]
  semver: ">=2.0"
  sort: semver
  latest: 10
```

To pin a specific RC alongside stable releases:

```yaml
tags:
  include: ["1.25.0-rc1"]
  semver: ">=1.25.0"
  sort: semver
  latest: 10
```

Chainguard `-rN` build revisions admit by default; no special configuration:

```yaml
tags:
  include: ["latest", "latest-dev"]
  semver: ">=1.25.0"
  sort: semver
  latest: 10
```

## Related

The default-`exclude` choice plus the `include`-with-deny pattern addresses long-running issues elsewhere:

- ArgoCD [#353](https://github.com/argoproj/argo-cd/issues/353), [#426](https://github.com/argoproj/argo-cd/issues/426) - prerelease handling
- Harbor [#16131](https://github.com/goharbor/harbor/issues/16131), [#19328](https://github.com/goharbor/harbor/issues/19328) - prerelease drops

See also:

- [Variant filtering](/recipes/variant-filtering) for variant-specific narrowing
- [Configuration reference](/configuration#tag-filtering) for the full pipeline
