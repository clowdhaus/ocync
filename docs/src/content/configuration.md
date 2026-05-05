---
title: Configuration
description: Config file reference for ocync with registry definitions, target groups, tag filtering, and environment variable support.
order: 2
---

`ocync` uses a YAML config file to define registries, target groups, defaults, and image mappings. A [JSON schema](#json-schema) is available for editor autocompletion and validation.

## Minimal example

```yaml
registries:
  source:
    url: cgr.dev
  target:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: source
  targets: target
  tags:
    glob: "*"

mappings:
  - from: chainguard/nginx
    to: nginx
```

## Full example

```yaml
global:
  max_concurrent_transfers: 50
  cache_ttl: "12h"
  staging_size_limit: "2GB"

registries:
  chainguard:
    url: cgr.dev
  ecr-east:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
  ecr-west:
    url: 123456789012.dkr.ecr.us-west-2.amazonaws.com

target_groups:
  prod:
    - ecr-east
    - ecr-west

defaults:
  source: chainguard
  targets: prod
  platforms:
    - linux/amd64
    - linux/arm64
  tags:
    semver: ">=1.0"
    sort: semver
    latest: 10

mappings:
  - from: chainguard/nginx
    to: nginx
  - from: chainguard/python
    to: python
    tags:
      glob: "*-slim"
```

## Global settings

Top-level `global` section controls engine-wide behavior:

```yaml
global:
  max_concurrent_transfers: 50    # Maximum concurrent image syncs (default: 50)
  cache_dir: /var/cache/ocync     # Cache directory (default: next to config file)
  cache_ttl: "12h"                # Warm cache TTL (default: "12h", "0" disables)
  staging_size_limit: "2GB"       # Disk staging limit (SI prefixes, "0" disables)
```

| Field | Default | Description |
|---|---|---|
| `max_concurrent_transfers` | `50` | Maximum number of images synced in parallel. Must be >= 1 |
| `cache_dir` | Adjacent to config file | Directory for persistent cache and blob staging |
| `cache_ttl` | `"12h"` | How long cached blob existence checks are valid. Accepts bare integers (seconds) or integers with a suffix: `s`, `m`, `h`, `d`. `"0"` disables TTL expiry (lazy invalidation only) |
| `staging_size_limit` | Unlimited | Maximum disk space for blob staging. Accepts `"0"` (disabled) or an integer with a suffix: `B`, `KB`, `MB`, `GB`, `TB`. Uses SI decimal prefixes (1 GB = 1,000,000,000 bytes) |

## Registries

Named registry definitions. Auth type is auto-detected from the hostname.

```yaml
registries:
  my-registry:
    url: registry.example.com         # Required
    auth_type: ecr                    # Optional (see below)
    max_concurrent: 50                # Optional per-registry concurrency cap (>= 1)
    head_first: true                  # Optional: HEAD-check targets before source GET
    credentials:                      # For auth_type: basic
      username: myuser
      password: ${REGISTRY_PASSWORD}
    token: ${GITHUB_TOKEN}            # For auth_type: static_token
```

| Field | Required | Description |
|---|---|---|
| `url` | Yes | Registry hostname (e.g., `cgr.dev`, `123456789012.dkr.ecr.us-east-1.amazonaws.com`) |
| `auth_type` | No | Authentication method (see below). Auto-detected from hostname when omitted |
| `max_concurrent` | No | Per-registry aggregate concurrency cap. Limits total simultaneous HTTP requests to this registry across all mappings. Must be >= 1 |
| `head_first` | No | HEAD-check all targets against the source HEAD digest before pulling full source manifests on cache miss. Skips the expensive source GET when all targets already match. Useful for rate-limited sources (e.g., Docker Hub). Bypassed when platform filtering is active. Default: `false` |
| `credentials` | When `auth_type: basic` | Object with `username` and `password` fields. Both required |
| `token` | When `auth_type: static_token` | Pre-obtained bearer token string |

### Auth types

| Value | Description |
|---|---|
| `ecr` | AWS ECR private. HTTP Basic using an SDK-issued token; ambient AWS credentials |
| `gar` | Google Artifact Registry (`*-docker.pkg.dev`); Application Default Credentials |
| `gcr` | Legacy Google Container Registry (`*.gcr.io`); same provider as `gar` |
| `acr` | Azure Container Registry; AAD credential chain via proprietary OAuth2 exchange |
| `ghcr` | Alias for `docker_config`. Prefer `static_token` (e.g., `GITHUB_TOKEN`) or `docker_config` directly |
| `basic` | HTTP Basic. Requires `credentials` with `username` and `password` |
| `static_token` or `token` | Pre-obtained bearer token. Requires `token`. Does not refresh |
| `docker_config` | Reads `~/.docker/config.json` (or `$DOCKER_CONFIG/config.json`) |
| `anonymous` | No credentials; still performs the `/v2/token` Bearer exchange when challenged |

Auto-detected from hostname. ECR (private and Public), GAR, GCR, and ACR each route to a dedicated native-auth provider. GHCR, Docker Hub, Chainguard, and any unrecognized hostname share the same fallback path: `docker_config` first (using `~/.docker/config.json` or `$DOCKER_CONFIG/config.json`), then anonymous if no entry is found. Set `auth_type` explicitly to override detection.

See the [registry guides](/registries/ecr) for provider-specific auth details.

## Target groups

Logical groupings for fan-out to multiple registries:

```yaml
target_groups:
  prod:
    - ecr-east
    - ecr-west
  staging:
    - ecr-staging
```

When syncing to multiple targets, `ocync` pulls from source once and pushes to all targets in parallel.

## Defaults

Applied to all mappings unless overridden at the mapping level:

```yaml
defaults:
  source: chainguard           # Default source registry
  targets: prod                # Default target group
  artifacts:                   # OCI 1.1 referrer handling (default: enabled)
    enabled: true
  platforms:                   # Platform filter (omit to preserve multi-arch)
    - linux/amd64
    - linux/arm64
  tags:
    glob: "*"                  # Glob pattern
    semver: ">=1.0"            # Semver range
    exclude:                   # Exclude patterns
      - "*-debug"
    sort: semver               # Sort order: semver, alpha
    latest: 10                 # Keep only N most recent after sort
    min_tags: 1                # Minimum tags required
```

| Field | Default | Description |
|---|---|---|
| `source` | None | Default source registry name for all mappings |
| `targets` | None | Default target group name or inline list |
| `artifacts` | `enabled: true` | OCI 1.1 referrer handling (signatures, SBOMs, attestations). See [Artifacts](#artifacts) |
| `platforms` | All platforms | Platform filter. Each entry must be `os/arch` or `os/arch/variant` (e.g., `linux/amd64`, `linux/arm/v7`). When set, multi-arch indexes are rewritten at the target with a different digest than the source - bit-for-bit divergence. Omit to preserve multi-arch verbatim |
| `tags` | None | Tag filtering pipeline (see [Tag filtering](#tag-filtering)) |

## Mappings

Source-to-target image relationships:

```yaml
mappings:
  - from: chainguard/nginx     # Source repository (required)
    to: nginx                  # Target repository name
    source: chainguard         # Override source registry
    targets: staging           # Override target group
    tags:                      # Override tag filters
      glob: "1.*"
      sort: semver             # Sort order: semver, alpha
      latest: 5
    platforms:                 # Override platform filter
      - linux/amd64
```

| Field | Required | Description |
|---|---|---|
| `from` | Yes | Source repository path (e.g., `chainguard/nginx`) |
| `to` | No | Target repository name. When omitted, uses the full `from` path |
| `source` | No | Override source registry for this mapping |
| `targets` | No | Override target group or inline list for this mapping |
| `tags` | No | Override tag filtering for this mapping |
| `platforms` | No | Override platform filter for this mapping. List must not be empty if provided |
| `artifacts` | No | Override artifact handling for this mapping. See [Artifacts](#artifacts) |

## Artifacts

Controls whether OCI 1.1 referrers (cosign signatures, SBOMs, attestations) are discovered and transferred alongside their parent image manifests. Default: enabled.

```yaml
defaults:
  artifacts:
    enabled: true                  # Default; set to false to skip referrers
    include:                       # When non-empty, only artifacts whose
      - application/vnd.dev.cosign.simplesigning.v1+json   # artifact_type matches one of these
      - application/vnd.cyclonedx+json
    exclude:                       # Always-skip artifacts whose type matches
      - application/vnd.in-toto+json
    require_artifacts: false       # When true, fail the sync if a parent has zero referrers
```

| Field | Default | Description |
|---|---|---|
| `enabled` | `true` | When `true`, after each image manifest is synced, `ocync` queries the source's `/v2/<repo>/referrers/<digest>` endpoint and transfers each referrer manifest plus its blobs to the target. Set to `false` to skip referrer discovery entirely |
| `include` | All types | If non-empty, only referrers whose `artifact_type` matches one of these MIME types are transferred. Empty means all types pass |
| `exclude` | None | Referrers whose `artifact_type` matches one of these are skipped, even if `include` matches |
| `require_artifacts` | `false` | When `true`, an image with zero discovered referrers fails the sync with an error. Use to enforce that every mirrored image carries provenance |

### Why default-on

Defaulting `enabled: true` is the only setting consistent with `ocync`'s bit-for-bit promise. If `enabled: false` were the default, the mirror would look correct (every image present, every digest valid) but `cosign verify` against the source's signature would fail at deployment time, because the signature lives on a referrer that was never copied. The opt-out is there for users who deliberately want unsigned mirrors: testing setups, isolated networks, or hard byte budgets.

### Opt-out

```yaml
defaults:
  artifacts:
    enabled: false              # Skip all referrer discovery and transfer
```

### Type filtering

Mirror only SBOMs:

```yaml
defaults:
  artifacts:
    enabled: true
    include:
      - application/vnd.cyclonedx+json
      - application/spdx+json
```

### Hard-fail on missing signatures

```yaml
defaults:
  artifacts:
    enabled: true
    require_artifacts: true     # Fail sync if any image lacks referrers
```

## Tag filtering

Tags are filtered through a pipeline:

1. **glob + semver**: build the candidate pool by intersecting the glob match set (default `*`) with the version range
2. **exclude**: remove tags matching any user `exclude` pattern OR any default-exclude pattern (see below)
3. **sort**: order the pool (`semver` or `alpha`)
4. **latest**: keep only the N most recent of the pool
5. **include**: union always-include tag matches into the result (not subject to `glob`, `semver`, default-excludes, `sort`, or `latest`); still subject to user `exclude`
6. **min_tags**: validate the final union has at least N tags (error if fewer)

All filters are optional. Without any filters, all tags are synced.

| Field | Type | Description |
|---|---|---|
| `glob` | string or list | Include tags matching glob pattern(s). A single string or a list of patterns |
| `include` | string or list | Always-include glob pattern(s). Tags matching any pattern survive `glob:`/`semver:` filters and the system-exclude defaults. Same syntax as `exclude:` |
| `semver` | string | Include tags satisfying a version range. Operators: `>=`, `<=`, `>`, `<`, `=`. Comma-joined for AND-narrowing. Example: `">=1.0, <2.0"` |
| `exclude` | string or list | Remove tags matching these glob pattern(s) |
| `sort` | string | Sort order for remaining tags: `semver` or `alpha` |
| `latest` | integer | Keep only the N most recent tags after sorting |
| `min_tags` | integer | Minimum number of tags that must survive the filtering pipeline. If fewer tags remain, the sync for this mapping fails with an error |
| `immutable_tags` | string | Glob pattern marking tags that never change content (e.g. `"v?[0-9]*.[0-9]*.[0-9]*"`). When a tag matches AND already exists in **every** target's tag list, the sync skips it with zero source and target requests. Useful for long-running mirrors of registries that publish many semver-pinned tags |

**Validation constraints:**
- `latest` requires `sort` to be set

### Capping output size

`latest:` is optional, but mirrors with `semver:` and no `latest:` cap will sync every tag matching the version range. Under the lenient parser, popular images often publish hundreds of variant tags (`-alpine`, `-r0`, `-debian-12-rN`, `-bookworm-slim`, etc.). For long-running mirrors, set `latest: N` (with `sort: semver`) to cap output size. ocync emits a startup warning when `semver:` is set without `latest:`.

**Override semantics:** when a mapping defines `tags:`, the entire block replaces `defaults.tags` - fields are not merged. If you want a mapping to inherit some default fields and override others, repeat the inherited fields in the mapping's `tags:` block.

### Default-exclude patterns

ocync drops common prerelease-marker tag patterns by default to keep mirrors focused on stable releases. The default-exclude list (case-insensitive) is:

- `*-rc*`
- `*-alpha*`
- `*-beta*`
- `*-pre*`
- `*-snapshot*`
- `*-nightly*`

Patterns deliberately NOT in the default list (still admitted unless you exclude them yourself):

- `*-dev*` -- Chainguard publishes `latest-dev` and `1.25.5-dev` as stable variants
- `*-edge*` -- Alpine rolling stable channel
- `*-final*` -- Java stable marker
- `-r<N>` -- Chainguard/Bitnami build counters

To opt back into prereleases, add them to `include:` (which overrides the default-exclude). To pin a single prerelease tag for testing, add the exact tag string to `include:`. To add custom exclude patterns on top of the defaults, use `exclude:`.

## Environment variables

Config values support environment variable substitution:

| Syntax | Behavior |
|---|---|
| `${VAR}` | Substitute value, empty string if unset |
| `${VAR:-default}` | Substitute value, use default if unset |
| `${VAR:?error message}` | Substitute value, error with message if unset |

```yaml
registries:
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com
```

The `DOCKER_CONFIG` environment variable controls the Docker config file location. If set, `ocync` reads `$DOCKER_CONFIG/config.json`. Otherwise, it defaults to `~/.docker/config.json`.

## Example configurations

### Chainguard to ECR

Single region. Sync a curated set of Chainguard base images to ECR, keeping the latest 5 stable releases plus the `latest`/`latest-dev` floats for `linux/amd64` and `linux/arm64`. Chainguard's `-rN` build-revision suffix admits directly under the lenient parser:

```yaml
registries:
  chainguard:
    url: cgr.dev
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: chainguard
  targets: ecr
  platforms:
    - linux/amd64
    - linux/arm64
  tags:
    include: ["latest", "latest-dev"]
    semver: ">=1.0"
    sort: semver
    latest: 5

mappings:
  - from: chainguard/nginx
    to: nginx
  - from: chainguard/python
    to: python
  - from: chainguard/node
    to: node
```

### Docker Hub fan-out

Mirror Docker Hub images to multiple ECR regions with authenticated pulls and debug tag exclusion:

```yaml
registries:
  dockerhub:
    url: docker.io
    auth_type: basic
    credentials:
      username: ${DOCKERHUB_USERNAME}
      password: ${DOCKERHUB_ACCESS_TOKEN}
  ecr-east:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com
  ecr-west:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-west-2.amazonaws.com

target_groups:
  prod:
    - ecr-east
    - ecr-west

defaults:
  source: dockerhub
  targets: prod
  platforms:
    - linux/amd64
    - linux/arm64
  tags:
    exclude:
      - "*-debug"
      - "*-rc*"
    sort: semver
    latest: 10
    min_tags: 1

mappings:
  - from: library/nginx
    to: nginx
  - from: library/redis
    to: redis
  - from: library/postgres
    to: postgres
    tags:
      # Mapping tags: replaces defaults.tags entirely. Repeat the
      # inherited fields explicitly when a mapping needs both.
      semver: ">=15"
      exclude:
        - "*-debug"
        - "*-rc*"
      sort: semver
      latest: 3
      min_tags: 1
```

### GHCR to ECR with glob filtering

Sync specific tagged releases from GitHub Container Registry:

```yaml
registries:
  ghcr:
    url: ghcr.io
    auth_type: static_token
    token: ${GITHUB_TOKEN}
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com

mappings:
  - from: my-org/my-app
    to: my-app
    source: ghcr
    targets: ecr
    tags:
      glob: "v*"
      exclude: "*-alpha"
      sort: semver
      latest: 5
    platforms:
      - linux/amd64
```

## JSON schema

A [JSON schema](/config.schema.json) is available for editor autocompletion and validation. Add the schema comment to the top of your config file:

```yaml
# yaml-language-server: $schema=https://clowdhaus.github.io/ocync/config.schema.json

registries:
  source:
    url: cgr.dev
  target:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: source
  targets: target
  tags:
    glob: "*"

mappings:
  - from: chainguard/nginx
    to: nginx
```

This works with any editor that supports the [YAML Language Server](https://github.com/redhat-developer/yaml-language-server) (VS Code with the YAML extension, Neovim with `yaml-language-server`, JetBrains IDEs).

The schema is generated from the Rust config types and verified in CI to stay in sync. View the full schema at [`/ocync/config.schema.json`](/config.schema.json).

## Validation

Validate config syntax without connecting to registries:

```bash
ocync validate config.yaml
```

Show config with all environment variables resolved:

```bash
ocync expand config.yaml
```

Use `--show-secrets` to display credentials instead of redacting them (do not pipe to logs):

```bash
ocync expand config.yaml --show-secrets
```
