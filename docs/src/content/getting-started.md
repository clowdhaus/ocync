---
title: Getting started
description: Install ocync, copy your first image, and set up config-driven sync across OCI registries.
order: 1
---

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

### Homebrew

```bash
brew install clowdhaus/taps/ocync
```

macOS (arm64) and Linux (amd64, arm64). Linux builds use FIPS-validated AWS-LC.

### Docker

```bash
docker pull public.ecr.aws/clowdhaus/ocync:latest-fips
```

Multi-arch image (`linux/amd64`, `linux/arm64`) based on `chainguard/static` with zero CVEs, no shell, and no package manager.

### Helm

```bash
helm install ocync oci://public.ecr.aws/clowdhaus/ocync --version 0.5.0
```

See the [Helm chart guide](/helm) for deployment modes and configuration.

### Build from source

```bash
# FIPS build (default, requires CMake + Go + Perl)
cargo install --locked ocync

# Non-FIPS build (no extra dependencies)
cargo install --locked ocync --no-default-features --features non-fips
```

Minimum Rust version: 1.94 (edition 2024).

### Verify installation

```bash
ocync version
```

## Copy your first image

```bash
ocync copy cgr.dev/chainguard/nginx:latest \
    123456789012.dkr.ecr.us-east-1.amazonaws.com/nginx:latest
```

`ocync` auto-detects the registry type from the hostname and handles authentication automatically. For ECR, it uses your ambient AWS credentials (environment variables, config file, or instance role).

## Config-driven sync

For syncing multiple images, create a config file:

```yaml
registries:
  chainguard:
    url: cgr.dev
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

target_groups:
  default:
    - ecr

defaults:
  source: chainguard
  targets: default
  tags:
    glob: "*"
    latest: 10
    sort: semver

mappings:
  - from: chainguard/nginx
    to: nginx
  - from: chainguard/python
    to: python
```

Run the sync:

```bash
ocync sync -c config.yaml
```

Preview what would sync without making changes:

```bash
ocync sync -c config.yaml --dry-run
```

## Key concepts

**Additive sync**: `ocync` never deletes images or tags from the target registry. Registries handle lifecycle and retention through their own policies.

**Blob deduplication**: container images share layers. `ocync` tracks every blob it has seen in a sync run and transfers each unique blob exactly once, regardless of how many images reference it.

**Cross-repo mounting**: when a blob already exists in another repository on the same registry, `ocync` mounts it instead of uploading again. This is a registry-side operation with zero data transfer.

**Transfer state cache**: a persistent cache records which blobs already exist at each target. Subsequent runs skip HEAD checks for known-good blobs, reducing API calls.

## Next steps

- [Recipes](/recipes/production-mirror) for common mirror patterns: production fidelity, minimum bytes, helm + images, variant filtering, semver tracking
- [Configuration reference](/configuration) for full config file syntax and options
- [CLI reference](/cli-reference) for all commands, flags, and exit codes
- [Helm chart](/helm) for Kubernetes deployment (CronJob, Deployment, or Job)
- [Observability](/observability) for logging, JSON output, and health endpoints
- Registry guides: [Amazon ECR](/registries/ecr), [Amazon ECR Public](/registries/ecr-public), [Docker Hub](/registries/docker-hub), [GHCR](/registries/ghcr), [GAR](/registries/gar), [ACR](/registries/acr), [Chainguard](/registries/chainguard), [Kubernetes secrets](/registries/secrets)
