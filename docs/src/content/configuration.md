---
title: Configuration
description: Config file reference for ocync with registry definitions, target groups, tag filtering, and environment variable support.
order: 2
---

ocync uses a YAML config file to define registries, target groups, defaults, and image mappings.

## Minimal example

```yaml
registries:
  source:
    url: cgr.dev
  target:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

mappings:
  - from: source/chainguard/nginx
    to: target/nginx
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
| `max_concurrent_transfers` | `50` | Maximum number of images synced in parallel |
| `cache_dir` | Adjacent to config file | Directory for persistent cache and blob staging |
| `cache_ttl` | `"12h"` | How long cached blob existence checks are valid. `"0"` disables TTL expiry (lazy invalidation only) |
| `staging_size_limit` | Unlimited | Maximum disk space for blob staging. Uses SI decimal prefixes (1 GB = 1,000,000,000 bytes). `"0"` disables disk staging |

## Registries

Named registry definitions. Auth type is auto-detected from the hostname.

```yaml
registries:
  my-registry:
    url: registry.example.com         # Required
    auth_type: ecr                    # Optional (see below)
    max_concurrent: 50                # Optional per-registry concurrency cap
    credentials:                      # For auth_type: basic
      username: myuser
      password: ${REGISTRY_PASSWORD}
    token: ${GITHUB_TOKEN}            # For auth_type: static_token
```

### Auth types

| Value | Description |
|---|---|
| `ecr` | AWS ECR token exchange via IAM credentials |
| `gcr` | Google Cloud artifact/container registry |
| `acr` | Azure Container Registry |
| `ghcr` | GitHub Container Registry |
| `basic` | HTTP basic auth (requires `credentials`) |
| `static_token` or `token` | Pre-obtained bearer token (requires `token` field) |
| `docker_config` | Docker config.json credential store |
| `anonymous` | No authentication |

Auth type is auto-detected from the hostname for ECR, Docker Hub, GHCR, GAR, ACR, and Chainguard. Any OCI-compliant registry works with auto-detected or explicit auth.

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

When syncing to multiple targets, ocync pulls from source once and pushes to all targets in parallel.

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
    exclude:                   # Exclude patterns
      - "*-debug"
    sort: semver               # Sort order: semver, alphabetical, time
    latest: 10                 # Keep only N most recent after sort
```

## Mappings

Source-to-target image relationships:

```yaml
mappings:
  - from: chainguard/nginx     # Source repository
    to: nginx                  # Target repository name
    tags:                      # Per-mapping tag override
      glob: "1.*"
      latest: 5
    platforms:                 # Per-mapping platform override
      - linux/amd64
    targets: staging           # Per-mapping target group override
```

## Tag filtering

Tags are filtered through a pipeline in order:

1. **glob**: include tags matching the glob pattern
2. **semver**: include tags satisfying the semver range
3. **exclude**: remove tags matching any exclude pattern
4. **sort**: order remaining tags (`semver`, `alphabetical`, `time`)
5. **latest**: keep only the N most recent after sorting

All filters are optional. Without any filters, all tags are synced.

## Environment variables

Config values support environment variable substitution:

| Syntax | Behavior |
|---|---|
| `${VAR}` | Substitute value, error if unset |
| `${VAR:-default}` | Substitute value, use default if unset |
| `${VAR:?error message}` | Substitute value, error with message if unset |

```yaml
registries:
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com
```

The `DOCKER_CONFIG` environment variable controls the Docker config file location. If set, ocync reads `$DOCKER_CONFIG/config.json`. Otherwise, it defaults to `~/.docker/config.json`.

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
