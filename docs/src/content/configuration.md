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
    semver_prerelease: exclude
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

See the [registry guides](../registries/ecr) for provider-specific auth details.

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
  platforms:                   # Platform filter
    - linux/amd64
    - linux/arm64
  tags:
    glob: "*"                  # Glob pattern
    semver: ">=1.0"            # Semver range
    semver_prerelease: exclude # Pre-release handling
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
| `platforms` | All platforms | Platform filter applied to all mappings. Each entry must be `os/arch` or `os/arch/variant` (e.g., `linux/amd64`, `linux/arm/v7`). List must not be empty |
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

## Tag filtering

Tags are filtered through a pipeline in order:

1. **glob**: include tags matching the glob pattern (string or list)
2. **semver**: include tags satisfying the semver range
3. **semver_prerelease**: control pre-release tag handling when `semver` is set
4. **exclude**: remove tags matching any exclude pattern (string or list)
5. **sort**: order remaining tags (`semver`, `alpha`)
6. **latest**: keep only the N most recent after sorting
7. **min_tags**: validate at least N tags survived the pipeline (error if fewer)

All filters are optional. Without any filters, all tags are synced.

| Field | Type | Description |
|---|---|---|
| `glob` | string or list | Include tags matching glob pattern(s). A single string or a list of patterns |
| `semver` | string | Include tags satisfying a semver range (e.g., `">=1.0"`, `"^2"`, `"1.x"`) |
| `semver_prerelease` | string | How to handle pre-release tags when `semver` is set. Values: `include`, `exclude`, `only`. Default: `exclude`. Requires `semver` to be set |
| `exclude` | string or list | Remove tags matching these glob pattern(s) |
| `sort` | string | Sort order for remaining tags: `semver` or `alpha` |
| `latest` | integer | Keep only the N most recent tags after sorting |
| `min_tags` | integer | Minimum number of tags that must survive the filtering pipeline. If fewer tags remain, the sync for this mapping fails with an error |

**Validation constraints:**
- `semver_prerelease` requires `semver` to be set
- `latest` requires `sort` to be set

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

Single region. Sync a curated set of Chainguard base images to ECR, keeping only the latest 5 semver-stable tags for `linux/amd64` and `linux/arm64`:

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
      semver: ">=15"
      sort: semver
      latest: 3
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

A [JSON schema](/ocync/config.schema.json) is available for editor autocompletion and validation. Add the schema comment to the top of your config file:

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

The schema is generated from the Rust config types and verified in CI to stay in sync. View the full schema at [`/ocync/config.schema.json`](/ocync/config.schema.json).

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
