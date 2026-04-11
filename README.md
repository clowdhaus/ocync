# ocync

OCI registry sync tool — copies container images between registries.

## Features

- **Library-first** — `ocync-distribution` is a standalone OCI Distribution client; `ocync-sync` provides sync orchestration
- **Pure OCI Distribution API** — no skopeo, no Docker daemon, direct HTTPS
- **Additive sync only** — never deletes; registries handle lifecycle
- **ECR-first** with broad registry support (GCR, ACR, GHCR, Docker Hub, Chainguard)
- **FIPS 140-3 ready** via `--features fips` (aws-lc-rs, NIST Certificate #4816)
- **Two-phase sync** — resolve all manifests, deduplicate blobs globally, then transfer
- **Cross-repo blob mounting** — zero data transfer for shared layers
- **Tag filtering** — glob, semver ranges, exclude patterns, sort + latest-N

## Installation

```bash
cargo install ocync
```

## Usage

```bash
# Sync from config
ocync sync --config config.yaml

# Copy a single image
ocync copy docker.io/library/nginx:1.27 ghcr.io/myorg/nginx:1.27

# List and filter tags
ocync tags docker.io/library/nginx --glob "1.*" --semver ">=1.26" --sort semver --latest 10

# Validate config
ocync validate config.yaml
```

## Configuration

See [docs/specs/2026-04-10-ocync-design.md](docs/specs/2026-04-10-ocync-design.md) for the full specification.

## License

Apache-2.0
