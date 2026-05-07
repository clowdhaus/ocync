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

> **Always set `latest:` for long-running mirrors.** If you omit `latest:`, every tag in the version range is synced -- popular images publish hundreds of variant tags (`-alpine`, `-bullseye-slim`, `-r0`, `-debian-12-r3`, etc.), and a `semver: ">=1.0"` mirror without a cap can sync hundreds of tags on the first run. Caps prevent the mirror from blowing up under variant proliferation.

`min_tags: 1` is a safety net for long-running mirrors. Counts the union of include and pipeline.

## Variations

The default already drops `*-rc*`, `*-alpha*`, `*-beta*`, `*-pre*`, `*-snapshot*`, `*-nightly*` via the system-exclude -- no extra config needed for stable-only:

```yaml
tags:
  semver: ">=2.0"
  sort: semver
  latest: 10
```

To opt back into prereleases for in-range versions, add the patterns to `include:`. As glob patterns, they rescue tags from the system default and then run through `semver:` like any other pipeline tag, so prereleases below the range still drop:

```yaml
tags:
  include: ["*-rc*", "*-alpha*", "*-beta*", "*-pre*", "*-snapshot*", "*-nightly*"]
  semver: ">=2.0"
  sort: semver
  latest: 10
```

To pin a specific RC alongside stable releases, write the exact tag as a literal in `include:`. Literal patterns bypass the pipeline entirely, so the literal is kept even if it would be rejected by `semver:`:

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

### Build + runtime variants

Some images publish a build variant alongside each release -- for example `3.12.5` and `3.12.5-dev`. To mirror both within a version range, add `include: ["*-dev"]` to the mapping:

```yaml
defaults:
  tags:
    exclude: ["*-dev", "*-r[0-9]*"]

mappings:
  - from: chainguard/python
    to: python
    tags:
      semver: ">=3.12, <3.13"
      include: ["*-dev"]
      sort: semver
      latest: 10
```

The `*-dev` glob overrides the project-wide deny, but the version range still applies, so 3.12 dev tags sync and 3.13 dev tags drop. `latest: 10` counts stable and dev tags together. Mappings without their own `include:` inherit the deny and stay stable-only.

## Related

The default-`exclude` choice plus the `include`-with-deny pattern addresses long-running issues elsewhere:

- ArgoCD [#353](https://github.com/argoproj/argo-cd/issues/353), [#426](https://github.com/argoproj/argo-cd/issues/426) - prerelease handling
- Harbor [#16131](https://github.com/goharbor/harbor/issues/16131), [#19328](https://github.com/goharbor/harbor/issues/19328) - prerelease drops

See also:

- [Variant filtering](/recipes/variant-filtering) for variant-specific narrowing
- [Configuration reference](/configuration#tag-filtering) for the full pipeline
