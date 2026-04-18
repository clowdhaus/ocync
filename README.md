# ocync

Sync OCI container images across registries - efficiently.

[![CI](https://github.com/clowdhaus/ocync/actions/workflows/ci.yml/badge.svg)](https://github.com/clowdhaus/ocync/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

ocync copies container images between OCI registries with blob deduplication, cross-repo mounting, and streaming transfers. It is 8x faster than dregsy and 3.5x faster than regsync on real-world workloads while using 2-2.6x fewer bytes and zero rate-limit 429s.

## Features

- **Pure OCI Distribution API** - no Docker daemon, no shelling out, direct HTTPS to registries
- **Additive sync only** - never deletes; registries handle lifecycle and retention
- **Pipelined architecture** - discovery and execution overlap; no idle time between phases
- **Global blob deduplication** - shared layers across all images are transferred once per sync run
- **Cross-repo blob mounting** - leader-follower election ensures shared layers mount instead of re-upload
- **Streaming transfers** - bytes flow source to target with no intermediate disk (single-target mode)
- **Adaptive rate limiting** - per-(registry, action) AIMD concurrency adapts to each registry's limits
- **Transfer state cache** - persistent cache skips already-synced blobs across runs
- **FIPS 140-3 by default** - aws-lc-rs with NIST Certificate #4816
- **Tag filtering** - glob patterns, semver ranges, exclude patterns, sort, latest-N
- **Platform filtering** - sync only the architectures you need (e.g., `linux/amd64`, `linux/arm64`)
- **Dry-run preview** - see what would sync without making changes
- **Structured JSON output** - machine-readable sync reports for CI/CD pipelines
- **TTY-aware progress** - real-time progress bars in terminals, heartbeats in CI
- **Graceful shutdown** - SIGINT/SIGTERM drains in-flight transfers within K8s termination window

## Quick start

Copy a single image:

```bash
ocync copy cgr.dev/chainguard/nginx:latest \
    123456789012.dkr.ecr.us-east-1.amazonaws.com/nginx:latest
```

Sync from a config file:

```bash
ocync sync -c config.yaml
```

Preview what would sync:

```bash
ocync sync -c config.yaml --dry-run
```

## Installation

### Binary releases

Download from [GitHub Releases](https://github.com/clowdhaus/ocync/releases):

| Platform | Binary | FIPS |
|---|---|---|
| Linux x86_64 | `ocync-fips-linux-amd64` | Yes |
| Linux arm64 | `ocync-fips-linux-arm64` | Yes |
| macOS arm64 | `ocync-macos-arm64` | No |
| Windows x86_64 | `ocync-windows-amd64.exe` | No |

Linux binaries are statically linked with FIPS 140-3 validated cryptography. macOS and Windows use aws-lc-rs without FIPS mode.

### Docker

```bash
docker pull public.ecr.aws/clowdhaus/ocync:latest-fips
```

Multi-arch image (`linux/amd64`, `linux/arm64`) based on `chainguard/static` - zero CVEs, no shell, no package manager.

### Helm

```bash
helm install ocync oci://public.ecr.aws/clowdhaus/ocync --version 0.1.0
```

Supports three deployment modes via `mode` value:

| Mode | K8s resource | Use case |
|---|---|---|
| `watch` (default) | Deployment | Continuous sync with health endpoints |
| `cronjob` | CronJob | Scheduled sync every N minutes |
| `job` | Job | One-shot sync for CI or seeding |

### Build from source

```bash
# FIPS build (default, requires CMake + Go + Perl)
cargo install --locked ocync

# Non-FIPS build (no extra dependencies)
cargo install --locked ocync --no-default-features --features aws-lc
```

Minimum Rust version: 1.85 (edition 2024).

## Usage

```
ocync sync -c config.yaml                  # Sync images from config
ocync sync -c config.yaml --dry-run        # Preview what would sync
ocync sync -c config.yaml --json           # Output sync report as JSON
ocync copy <source> <destination>          # Copy a single image
ocync tags <repository>                    # List and filter tags
ocync watch -c config.yaml                 # Continuous sync on a schedule
ocync analyze -c config.yaml              # Analyze blob sharing potential
ocync auth check -c config.yaml           # Verify registry credentials
ocync validate config.yaml                # Validate config without connecting
ocync expand config.yaml                  # Show config with env vars resolved
ocync version                             # Print version and build info
```

### Global options

```
-v, -vv, -vvv     Increase log verbosity
-q, --quiet        Suppress all output except errors
--log-format       Set log format (text | json, auto-detected in Kubernetes)
```

## Configuration

```yaml
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

**Registries** - named registry definitions. Auth type is auto-detected from hostname (ECR, Docker Hub, GHCR, GAR, ACR, Chainguard) or set explicitly via `auth_type`.

**Target groups** - logical groupings for fan-out to multiple registries.

**Defaults** - tags, platforms, and skip_existing applied to all mappings unless overridden.

**Mappings** - source-to-target image relationships. Per-mapping overrides for tags, platforms, and targets.

Environment variables are supported: `${VAR}`, `${VAR:-default}`, `${VAR:?error}`.

## Supported registries

| Registry | Auth | Blob mount | Notes |
|---|---|---|---|
| Amazon ECR (private) | IAM (automatic) | Yes (opt-in) | Batch APIs, per-action rate limits |
| Amazon ECR Public | IAM (automatic) | No | Separate from private ECR |
| Chainguard | Token exchange | N/A (source) | No rate limits |
| Docker Hub | Docker config / static | Yes | 100/hr authenticated manifest GETs; HEADs free |
| GitHub Container Registry | Docker config | Yes | Single-PATCH upload fallback |
| Google Artifact Registry | Docker config | No | Monolithic upload only |
| Azure Container Registry | Docker config | Yes | Chunked upload for blobs >20 MB |
| Quay.io | Token exchange | Unconfirmed | Standard OCI token exchange |
| Any OCI-compliant registry | Basic / token / anonymous | Varies | Auto-detected from `WWW-Authenticate` |

ECR includes provider-specific optimizations: batch blob existence checks, adaptive per-action rate limiting, and leader-follower cross-repo mounting. Auth type is auto-detected from the registry hostname.

## Performance

On a 42-image / 55-tag cold sync to ECR (c6in.4xlarge), ocync completed in **4m 33s** using **4,131 requests** and **16.9 GB** transferred - 8x faster than dregsy (36m 22s), 3.5x faster than regsync (16m 6s), with 2-2.6x fewer bytes and zero rate-limit 429s.

See [docs/design.md](docs/design.md) for the full benchmark table and how ocync achieves these results.

## Kubernetes deployment

### CronJob (recommended)

```yaml
# values.yaml
mode: cronjob
cronjob:
  schedule: "*/15 * * * *"
  concurrencyPolicy: Forbid

image:
  repository: public.ecr.aws/clowdhaus/ocync
  tag: latest-fips

serviceAccount:
  create: true
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/ocync

resources:
  requests:
    cpu: 100m
    memory: 128Mi
  limits:
    memory: 512Mi

config:
  registries:
    chainguard:
      url: cgr.dev
    ecr:
      url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
  target_groups:
    default: [ecr]
  defaults:
    source: chainguard
    targets: default
    tags:
      glob: "*"
      latest: 20
      sort: semver
  mappings:
    - from: chainguard/nginx
      to: nginx
```

The single-threaded runtime maps directly to `cpu: 100m` - the process is I/O-bound, not compute-bound.

### Watch mode (Deployment)

```yaml
mode: watch
watch:
  interval: 300
  healthPort: 8080
```

Exposes `/healthz` (liveness) and `/readyz` (readiness) endpoints. Supports SIGHUP for config reload without restart.

## Architecture

ocync uses a pipelined sync engine where discovery and execution overlap, AIMD adaptive concurrency that discovers actual registry capacity through feedback, leader-follower blob mounting that ensures shared layers are uploaded once and mounted everywhere else, and a persistent transfer state cache that eliminates redundant API calls across runs.

See [docs/design.md](docs/design.md) for the full technical design.

## License

Apache-2.0
