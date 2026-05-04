---
title: Minimum bytes
description: Mirror just the container images you need - skip signatures, SBOMs, and historical tags - while preserving multi-arch indexes bit-for-bit.
order: 2
---

There are two levers for cutting bytes without sacrificing content integrity: skip the supply-chain referrers and tighten the tag set. Multi-arch stays intact.

## When to use

- CI mirrors that do not need supply-chain provenance
- Air-gapped fleets that do not run `cosign verify`
- Egress-sensitive environments where every megabyte counts

## Config

```yaml
registries:
  source:
    url: cgr.dev
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: source
  targets: ecr
  artifacts:
    enabled: false       # skip referrers (signatures, SBOMs, attestations)
  tags:
    glob: ["latest"]     # one specific tag (or use a tight semver range)

mappings:
  - from: chainguard/curl
    to: curl
```

## Fields

`artifacts.enabled: false` skips the OCI 1.1 referrer discovery and transfer that runs after each parent manifest, dropping cosign signatures, SBOMs, and attestations from the mirror.

`tags.glob` (or `tags.semver`) limits the candidate set. The single cheapest case is an all-literal `glob` (no `*`, `?`, `[`): `ocync` recognizes it and skips the source `list-tags` API call entirely, going straight to one HEAD per pinned tag. Any non-literal pattern, any `semver:` constraint, any `sort`/`latest`/`exclude`/`min_tags` field forces the full tag listing; the filter then narrows in memory.

There is deliberately no `platforms:` filter. Multi-arch indexes flow verbatim and the target index digest matches the source - filtering platforms would rewrite the index, change its digest, break `cosign verify`, break pin-by-digest workflows, and fail pulls from excluded architectures. Dropping referrers and tightening tags typically saves one to two orders of magnitude in bytes; dropping platforms saves another 2x to 4x but at the cost of bit-for-bit divergence. See [single architecture](/recipes/single-architecture) if that tradeoff is on the table.

## Variations

Pinning multiple floating tags as literals (preserves the listing-skip optimization):

```yaml
tags:
  glob: ["latest", "latest-dev"]
  # See pin-literals-only for the full discussion of the literal-pin pattern.
```

Tracking the latest five releases of a major version. Chainguard tags carry a `-rN` build-revision suffix (`1.25.5-r0`, `1.25.5-r1`) which the SemVer spec treats as a prerelease, so `semver_prerelease: include` is required against `cgr.dev` for any tag to survive the range filter:

```yaml
tags:
  semver: ">=1.0.0, <2.0.0"
  semver_prerelease: include
  sort: semver
  latest: 5
```

## Related

- [Production mirror](/recipes/production-mirror) when you need full provenance
- [Pin literals only](/recipes/pin-literals-only) for the rate-limit-friendly literal-tag pattern
- [Configuration reference](/configuration#tag-filtering) for the full tag pipeline
