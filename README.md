# ocync

Sync OCI container images across registries - efficiently.

<p align="center">
  <img src="docs/public/ecr-banner.svg" alt="ocync - 4x faster, 40% fewer requests, adaptive rate control" width="900">
</p>

[![CI](https://github.com/clowdhaus/ocync/actions/workflows/ci.yml/badge.svg)](https://github.com/clowdhaus/ocync/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue.svg)](https://blog.rust-lang.org/2025/06/26/Rust-1.94.0.html)
[![ECR Public](https://img.shields.io/badge/ECR_Public-clowdhaus%2Focync-ff9900.svg?logo=amazonecs)](https://gallery.ecr.aws/clowdhaus/ocync)

ocync copies container images between OCI registries with blob deduplication, cross-repo mounting, and streaming transfers. On real-world workloads, ocync completes cold syncs 4x faster than comparable tools with up to 40% fewer API requests.

## Features

- **Pure OCI Distribution API** - no Docker daemon, no shelling out, direct HTTPS to registries
- **Additive sync only** - never deletes; registries handle lifecycle and retention
- **Pipelined architecture** - discovery and execution overlap; no idle time between phases
- **Global blob deduplication** - shared layers across all images are transferred once per sync run
- **Cross-repo blob mounting** - leader-follower election ensures shared layers mount instead of re-upload
- **Streaming transfers** - bytes flow source to target with no intermediate disk (single-target mode)
- **Adaptive rate limiting** - per-(registry, action) [AIMD](https://clowdhaus.github.io/ocync/design/overview#adaptive-concurrency-aimd) (additive increase, multiplicative decrease) concurrency adapts to each registry's observed throttling, complemented by a token-bucket layer that enforces documented per-account TPS ceilings where the registry publishes them
- **Transfer state cache** - persistent cache skips already-synced blobs across runs
- **FIPS 140-3 by default** - aws-lc-rs with NIST Certificate #4816
- **Tag filtering** - glob patterns, semver ranges, exclude patterns, sort, latest-N
- **Platform filtering** - sync only the architectures you need (e.g., `linux/amd64`, `linux/arm64`)
- **Structured JSON output** - machine-readable sync reports for CI/CD pipelines
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

<details>
<summary>Binary releases</summary>

Download from [GitHub Releases](https://github.com/clowdhaus/ocync/releases):

| Platform | Binary | FIPS |
|---|---|---|
| Linux x86_64 | `ocync-fips-linux-amd64` | Yes |
| Linux arm64 | `ocync-fips-linux-arm64` | Yes |
| macOS arm64 | `ocync-macos-arm64` | No |
| Windows x86_64 | `ocync-windows-amd64.exe` | No |

Linux binaries are statically linked with FIPS 140-3 validated cryptography.

</details>

<details>
<summary>Docker</summary>

```bash
docker pull public.ecr.aws/clowdhaus/ocync:latest-fips
```

Multi-arch image (`linux/amd64`, `linux/arm64`) based on `chainguard/static`.

</details>

<details>
<summary>Helm</summary>

```bash
helm install ocync oci://public.ecr.aws/clowdhaus/ocync --version 0.1.0
```

Supports Deployment (watch), CronJob, and Job modes. See the [Helm chart docs](https://clowdhaus.github.io/ocync/helm).

</details>

<details>
<summary>Build from source</summary>

```bash
# FIPS build (default, requires CMake + Go + Perl)
cargo install --locked ocync

# Non-FIPS build (no extra dependencies)
cargo install --locked ocync --no-default-features --features non-fips
```

Minimum Rust version: 1.94 (edition 2024).

</details>

## Documentation

Full documentation at [clowdhaus.github.io/ocync](https://clowdhaus.github.io/ocync):

- [Getting started](https://clowdhaus.github.io/ocync/getting-started) - installation, first sync, key concepts
- [Configuration](https://clowdhaus.github.io/ocync/configuration) - config file reference
- [CLI reference](https://clowdhaus.github.io/ocync/cli-reference) - commands, flags, exit codes
- [Registry guides](https://clowdhaus.github.io/ocync/registries/ecr) - ECR, Docker Hub, GHCR, GAR, ACR, Chainguard
- [Helm chart](https://clowdhaus.github.io/ocync/helm) - Kubernetes deployment
- [Performance](https://clowdhaus.github.io/ocync/performance) - benchmarks and architecture
- [FIPS 140-3](https://clowdhaus.github.io/ocync/fips) - compliance details

## Supported registries

| Registry | Auth | Blob mount | Notes |
|---|---|---|---|
| Amazon ECR (private) | IAM (automatic) | Yes (opt-in) | Batch APIs, per-action rate limits |
| Amazon ECR Public | IAM (automatic) | No | Separate auth from private ECR |
| Chainguard | Token exchange | N/A (source) | No rate limits |
| Docker Hub | Docker config / static | Yes | 100/6hr authenticated manifest GETs; HEADs free |
| GitHub Container Registry | Docker config | Yes | Single-PATCH upload fallback |
| Google Artifact Registry | Docker config | Yes | Monolithic upload only |
| Azure Container Registry | Docker config | Yes | Streaming PUT (chunked upload planned) |
| Any OCI-compliant registry | Basic / token / anonymous | Varies | Auto-detected |

## License

Apache-2.0
