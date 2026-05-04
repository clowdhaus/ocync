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
      semver: ">=2.0"
      semver_prerelease: exclude
      sort: semver
      latest: 10
      min_tags: 1
```

## Fields

`semver: ">=2.0"` drops everything below 2.0. Non-semver tags like `latest` and `nightly` are also dropped at this stage, since they cannot be parsed as semver.

`semver_prerelease: exclude` drops `2.0.0-rc.1`, `2.0.0-alpha`, and any other prerelease identifier. This is the default - it is spelled out here so the intent is obvious in the config.

`sort: semver` orders the surviving tags by semantic version, highest first.

`latest: 10` keeps only the top 10 after sorting. This requires `sort:` to be set.

`min_tags: 1` is a safety net for long-running mirrors. If upstream renames their tag scheme or drops below your range, the sync fails loudly rather than silently producing an empty mirror.

## Variations

The default `exclude` matches npm, cargo, and Masterminds semver. To include pre-releases:

```yaml
tags:
  semver: ">=2.0"
  semver_prerelease: include    # 2.0.0-rc.1 matches >=2.0.0
  sort: semver
  latest: 10
```

`include` mode compares the base version (`2.0.0` for `2.0.0-rc.1`) against the range. So `2.0.0-rc.1` matches `>=2.0.0` but not `<2.0.0`.

To mirror only pre-releases:

```yaml
tags:
  semver: ">=2.0"
  semver_prerelease: only
```

Chainguard and Wolfi tags use a `-rN` build-revision suffix (`1.25.5-r0`, `1.25.5-r1`). Per the SemVer spec, anything after `-` is a prerelease identifier, so the default `exclude` drops them. To keep the `-rN` revisions while still rejecting the actual release-candidate prereleases:

```yaml
tags:
  semver: ">=1.25.0"
  semver_prerelease: include
  exclude: ["*-rc*", "*-beta*", "*-alpha*"]
  sort: semver
  latest: 10
```

`semver_prerelease: include` keeps the `-rN` tags, and the `exclude:` patterns drop the named prerelease channels. Most Chainguard mirrors converge on this shape.

## Related

The default-`exclude` choice plus the `include`-with-deny pattern addresses long-running issues elsewhere:

- ArgoCD [#353](https://github.com/argoproj/argo-cd/issues/353), [#426](https://github.com/argoproj/argo-cd/issues/426) - prerelease handling
- Harbor [#16131](https://github.com/goharbor/harbor/issues/16131), [#19328](https://github.com/goharbor/harbor/issues/19328) - prerelease drops

See also:

- [Variant filtering](/recipes/variant-filtering) for variant-specific narrowing
- [Configuration reference](/configuration#tag-filtering) for the full pipeline
