---
title: Production mirror
description: Full-fidelity OCI mirror with multi-arch indexes, signatures, and SBOMs preserved bit-for-bit.
order: 1
---

A production mirror keeps every byte the upstream published: multi-architecture indexes, image manifests, blobs, and any OCI 1.1 referrers (signatures, SBOMs, attestations) attached to those images. This is what `ocync` does by default. The config below is the minimum you need to write down to get there.

## When to use

- Compliance or regulated environments where signed-image provenance matters
- Air-gapped clusters that need to run `cosign verify` against the source's signature
- Any mirror where consumers may pin by digest

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
      semver: ">=1.0"
      sort: semver
      latest: 5
```

## Fields

`artifacts.enabled` defaults to `true`, so cosign signatures, SBOMs, and attestations transfer alongside their parent images. With no `platforms:` filter, multi-arch indexes flow verbatim and the index digest at the target matches the source's index digest. The `semver` + `sort` + `latest` block lists every tag from the source and then narrows in memory to the five newest semver-stable releases; the listing cost is paid once per sync, after which only the surviving tags drive HEAD checks and pulls.

Once a sync completes, `cosign verify --certificate-identity-regexp ... <target>/my-app@sha256:<index-digest>` works against the same digest the source published, because nothing was rewritten on the way through.

## Related

- [Minimum bytes](/recipes/minimum-bytes) when you do not need the supply-chain artifacts
- [Helm chart and images](/recipes/helm-chart-and-images) for operator-shaped deployments
- [Configuration reference](/configuration#artifacts) for `artifacts.enabled`, `include`, `exclude`, `require_artifacts`
