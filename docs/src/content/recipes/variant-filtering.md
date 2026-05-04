---
title: Variant filtering
description: Mirror only the alpine, slim, or distroless variants of an image using glob include or exclude patterns.
order: 5
---

Many upstream images publish multiple variants of the same version: `1.25.5`, `1.25.5-alpine`, `1.25.5-slim`, `1.25.5-debug`. Variant selection is a string-pattern question, so the lever is `glob` (include) or `exclude` (deny). Pick whichever expresses the intent more directly.

## When to use

- Mirror only one flavor of an image (alpine, slim, distroless) and drop the rest
- Mirror only the bare-version tags and drop every suffixed variant
- Combine a variant filter with the rest of the tag pipeline in the same mapping

## Config

To keep one variant and drop the rest, glob-include the suffix:

```yaml
registries:
  dockerhub:
    url: docker.io
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: dockerhub
  targets: ecr
  artifacts:
    enabled: false

mappings:
  - from: library/postgres
    to: postgres
    tags:
      glob: ["*-alpine"]
```

To keep the bare-version tags and drop the suffixed variants, invert the pattern with `exclude`:

```yaml
tags:
  exclude: ["*-alpine", "*-slim", "*-bookworm", "*-bullseye"]
```

## Fields

`glob:` is OR-include - a tag survives if it matches any pattern. `exclude:` is OR-deny - a tag is dropped if it matches any pattern. Both stages run in the same pipeline; combining them lets you express "variants of this flavor only, except a deny-listed sub-set."

## Variations

Combine a variant glob with a numeric range cutoff. The lenient parser admits `15.10-alpine` directly (prefix `[15, 10]`), so a `glob` plus `semver` bound work in the same mapping:

```yaml
tags:
  glob: ["*-alpine"]
  semver: ">=15.0"
  sort: semver
  latest: 5
```

### Match only base versions

To keep plain-numeric tags (`15.10`, `16.0`, `17.1.2`) and drop every dashed-suffix variant in one move, exclude any tag that contains a dash:

```yaml
tags:
  semver: ">=15.0"
  exclude: ["*-*"]
  sort: semver
```

The `semver:` stage continues to drop non-version tags like `latest`, `lts-iron`, and `nightly`. The `exclude` patterns drop `15.10-alpine`, `15.10-bullseye-slim`, and any other variant.

## Related

The variant-include and variant-exclude shapes here cover long-running feature requests in comparable tools:

- dregsy [#72](https://github.com/xelalexv/dregsy/issues/72) - "semver range AND substring filter"
- skopeo [#852](https://github.com/containers/skopeo/issues/852) - "deny-list on top of include"
- regclient [#1041](https://github.com/regclient/regclient/issues/1041) - `semverRange + allow + deny`

See also:

- [Semver tracking](/recipes/semver-tracking) for the latest-N release-window pattern
- [Configuration reference](/configuration#tag-filtering) for the full pipeline
