# ocync

Fast, concurrent OCI registry sync — copies container images between registries with zero wasted bandwidth.

[![CI](https://github.com/clowdhaus/ocync/actions/workflows/ci.yml/badge.svg)](https://github.com/clowdhaus/ocync/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

## Features

**Available now:**

- **Pure OCI Distribution API** — no Docker daemon, no shelling out, direct HTTPS to registries
- **Additive sync only** — never deletes; registries handle lifecycle and retention
- **ECR-native with broad registry support** — automatic provider detection for registry-specific optimizations
- **FIPS 140-3 by default** — aws-lc-rs with NIST Certificate #4816
- **Cross-repo blob mounting** — zero data transfer for shared layers within a registry
- **Tag filtering** — glob patterns, semver ranges, exclude patterns, sort, latest-N
- **Platform filtering** — sync only the architectures you need (e.g., `linux/amd64`, `linux/arm64`)
- **Adaptive rate limiting** — per-action AIMD concurrency that adjusts to each registry's limits
- **Transfer state cache** — persistent cache skips already-synced images across runs
- **Graceful shutdown** — SIGINT/SIGTERM drains in-flight transfers before exiting

**Coming soon:**

- **Progress reporting** — TTY-aware progress bars with transfer rates
- **Structured JSON output** — machine-readable sync reports for CI/CD pipelines
- **Credential helpers** — native auth for GCR/GAR, ACR, GHCR, Docker Hub
- **Dry-run preview** — see what would sync without making changes

## Quick start

Install:

```bash
cargo install ocync
```

Copy a single image:

```
$ ocync copy cgr.dev/chainguard/nginx:latest \
    123456789012.dkr.ecr.us-east-1.amazonaws.com/nginx:latest

  nginx:latest ✓ synced (3 blobs, 1 mounted, 42 MB transferred) [1.8s]
```

Sync from a config file:

```
$ ocync sync -c sync.yaml

  nginx:1.27          ✓ synced → us-east-1, us-west-2 (6 blobs, 4 mounted) [2.1s]
  nginx:1.26          ✓ skipped (already synced)
  python:3.12-slim    ✓ synced → us-east-1, us-west-2 (8 blobs, 3 mounted) [3.4s]

  3 images, 2 synced, 1 skipped
  14 blobs (7 deduplicated, 7 mounted), 89 MB transferred [5.2s]
```

*Output format is under active development and may differ from what is shown above.*

## How it works

- **Pull once, push everywhere** — source images are pulled once per tag. Fan-out to N target registries happens simultaneously, never re-pulling the source.
- **Global blob deduplication** — shared layers across all images in a sync run are transferred only once, regardless of how many images reference them.
- **Cross-repo mounting** — when a blob already exists in another repository on the same target registry, it is mounted instead of transferred. Zero bytes over the wire.
- **Streaming transfers** — bytes flow directly from source registry to target registry. No intermediate disk storage required.
- **Adaptive concurrency** — each registry action type (manifest pull, blob push, etc.) has its own AIMD window that adjusts based on rate limit feedback. A throttled blob push does not slow down manifest pulls.

## Usage

```
ocync sync -c config.yaml              # Sync images from config
ocync sync -c config.yaml --dry-run    # Preview what would sync           (coming soon)
ocync sync -c config.yaml --json       # Output sync report as JSON        (coming soon)
ocync copy <source> <destination>      # Copy a single image
ocync tags <repository>                # List and filter tags
ocync watch -c config.yaml             # Continuous sync on a schedule
ocync auth check -c config.yaml        # Verify registry credentials
ocync validate config.yaml             # Validate config without connecting
ocync expand config.yaml               # Show config with env vars resolved
```

## Configuration

```yaml
registries:
  chainguard:
    url: cgr.dev
  ecr-east:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr # auto-detected from hostname when omitted
  ecr-west:
    url: 123456789012.dkr.ecr.us-west-2.amazonaws.com
    auth_type: ecr

target_groups:
  prod: # logical name for a set of target registries
    - ecr-east
    - ecr-west

defaults:
  source: chainguard
  targets: prod
  platforms:
    - linux/amd64
    - linux/arm64
  tags:
    semver: ">=1.0" # matches any semver tag >= 1.0
    sort: semver
    latest: 10 # keep only the 10 most recent after filtering

mappings:
  - from: chainguard/nginx # source repository path
    to: nginx # target repository path (same across all targets)
  - from: chainguard/python
    to: python
    tags:
      glob: "*-slim" # override default tag filter for this mapping
```

- **Registries** — named registry definitions with optional auth type (auto-detected from hostname when omitted)
- **Target groups** — logical groupings for 1:N fan-out
- **Defaults** — tags, platforms, and skip_existing applied to all mappings unless overridden
- **Mappings** — source-to-target image relationships with optional per-mapping overrides

## Supported registries

| Registry | Auth | Status |
|---|---|---|
| Amazon ECR (private) | Automatic (IAM) | Available |
| Amazon ECR Public | Automatic (IAM) | Available |
| Chainguard | Token exchange | Available |
| Docker Hub | Anonymous / docker config | Available |
| GitHub Container Registry | `GITHUB_TOKEN` | Coming soon |
| Google Artifact Registry | Application Default Credentials | Coming soon |
| Azure Container Registry | Service principal | Coming soon |
| Quay.io | Token exchange | Coming soon |
| Any OCI-compliant registry | Basic / token / anonymous | Available |

ECR includes provider-specific optimizations: batch blob existence checks, cross-repo mounts, and per-action rate limit adaptation. Auth type is auto-detected from the registry hostname when not explicitly configured.

## License

Apache-2.0
