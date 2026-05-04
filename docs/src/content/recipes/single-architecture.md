---
title: Single architecture
description: Mirror one architecture only - the only ocync recipe that opts out of bit-for-bit content integrity. Read the warnings before adopting.
order: 7
---

<div class="callout callout-warning">

<span class="callout-title">Warning</span>

This recipe rewrites multi-arch index manifests at the target. It opts out of `ocync`'s bit-for-bit content integrity for affected mappings. Read the caveats below before using.

</div>

The single-architecture recipe trades content integrity for storage and egress savings. This is the only place in the engine where pushed content does not match source content byte-for-byte.

## When to use

- Every consumer of the target uses `linux/amd64` exclusively (no arm64 nodes, no Mac developer pulls, no future migration planned)
- You are not running `cosign verify` against the source's signatures
- You are not pinning by the source's index digest
- Egress and storage savings are large enough to justify the integrity loss

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
  platforms: ["linux/amd64"]
  artifacts:
    enabled: false
  tags:
    glob: ["latest"]

mappings:
  - from: chainguard/curl
    to: curl
```

## Fields

When `platforms:` is set, `ocync`:

1. Pulls the multi-arch index from the source.
2. Drops child descriptors for non-matching platforms.
3. Re-serializes the index with only the kept descriptors.
4. Pushes the rewritten index, which has a different SHA-256 than the source.

`artifacts.enabled: false` and `tags.glob: ["latest"]` are not required for platform filtering, but they are usually paired with it: a recipe willing to give up bit-for-bit integrity is usually also willing to drop referrers and narrow the tag set.

## Caveats

A typical multi-arch image has 2x to 4x layer surface across architectures. Skipping artifacts and narrowing tags typically saves 10x to 100x. Platform filtering is the smallest of the byte-saving levers and the only one with correctness consequences - if every other lever is already pulled and platform filtering is the next one, this recipe is for you. Most mirrors should not need it.

The integrity tradeoffs:

- `cosign verify` against the source index digest fails because the target index has a different digest, and cosign signatures sign the source's index digest.
- Pin-by-digest workflows that reference the source's index break for the same reason.
- Consumers on the excluded architecture pull the tag, get an index that says "no manifest for your platform," and fail.
- OCI 1.1 referrers attached to the original index reference the source digest, so they no longer apply to the rewritten artifact.

If any of those matters - any arm64 consumers (developers on Apple Silicon, Graviton nodes, Raspberry Pi), any `cosign verify` against source signatures, any pipeline pinning by source-index digest, or simply uncertainty - use [minimum bytes](/recipes/minimum-bytes) instead. It preserves multi-arch and gives up only the supply-chain referrers.

## Related

- [Minimum bytes](/recipes/minimum-bytes) for the multi-arch-preserving alternative
- [Production mirror](/recipes/production-mirror) for full bit-for-bit fidelity
- [Configuration reference](/configuration#defaults) for `platforms:` syntax
