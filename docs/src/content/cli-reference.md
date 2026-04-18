---
title: CLI reference
description: Complete reference for ocync commands, flags, exit codes, and structured output.
order: 3
---

## Commands

```
ocync sync -c config.yaml               Sync images from config
ocync sync -c config.yaml --dry-run     Preview what would sync
ocync sync -c config.yaml --json        Output sync report as JSON
ocync copy <source> <destination>       Copy a single image
ocync tags <repository>                 List and filter tags
ocync watch -c config.yaml              Continuous sync on a schedule
ocync analyze -c config.yaml            Analyze blob sharing potential
ocync auth check -c config.yaml         Verify registry credentials
ocync validate config.yaml              Validate config without connecting
ocync expand config.yaml                Show config with env vars resolved
ocync version                           Print version and build info
```

## Global options

| Flag | Description |
|---|---|
| `-v`, `-vv`, `-vvv` | Increase log verbosity |
| `-q`, `--quiet` | Suppress all output except errors |
| `--log-format` | Set log format: `text` (default) or `json` (auto-detected in Kubernetes) |

## tags

List and filter tags for a repository:

```bash
ocync tags docker.io/library/nginx
ocync tags cgr.dev/chainguard/nginx --semver ">=1.0" --latest 10
```

| Flag | Description |
|---|---|
| `-c`, `--config` | Config file for registry credentials (optional) |
| `--glob` | Include tags matching a glob pattern (repeatable) |
| `--semver` | Include tags matching a semver range (e.g., `>=1.0, <2.0`) |
| `--exclude` | Exclude tags matching a pattern (repeatable) |
| `--sort` | Sort order: `semver` or `alpha` |
| `--latest` | Show only the N most recent tags |

## watch

Continuous sync on a schedule with health endpoints:

```bash
ocync watch -c config.yaml --interval 600 --health-port 8080
```

| Flag | Default | Description |
|---|---|---|
| `-c`, `--config` | (required) | Path to sync config file |
| `--interval` | `300` | Seconds between sync runs (minimum: 1) |
| `--health-port` | `8080` | Port for `/healthz` and `/readyz` endpoints |
| `--json` | | Output sync reports as JSON |

See [observability](../observability) for health endpoint details.

## analyze

Analyze blob sharing and cross-repo mount potential without performing a sync. Pulls source manifests only (no blobs transferred) and reports total unique blobs, shared blobs across images, deduplicated bytes saved, and per-target mount opportunities.

```bash
ocync analyze -c config.yaml
ocync analyze -c config.yaml --json
```

| Flag | Description |
|---|---|
| `-c`, `--config` | Path to sync config file |
| `--json` | Emit a JSON report instead of text summary |

Use `analyze` to estimate transfer savings before running a full sync, or to verify that blob deduplication and mounting are configured correctly.

## expand

Show config with all environment variables resolved:

```bash
ocync expand config.yaml
ocync expand config.yaml --show-secrets
```

| Flag | Description |
|---|---|
| `--show-secrets` | Show credential values instead of redacting them. Do not use when stdout is piped to a file or logging system |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | All images synced or skipped |
| `1` | Partial failure (some images failed) |
| `2` | All images failed or unclassified error |
| `3` | Invalid configuration |
| `4` | Authentication or authorization failure |

## Structured output

Use `--json` to get machine-readable sync reports for CI/CD pipelines:

```bash
ocync sync -c config.yaml --json
```

The JSON output includes per-image results, aggregate statistics (blobs transferred, bytes, mounts, cache hits), and any errors encountered.

## Environment variables

| Variable | Description |
|---|---|
| `AWS_REGION` | AWS region for ECR auth |
| `AWS_USE_FIPS_ENDPOINT` | Use FIPS endpoints for ECR |
| `DOCKER_CONFIG` | Docker config directory (default: `~/.docker`) |
| `OCYNC_LOG` | Log filter directive (overrides `-v` flags) |
| `NO_COLOR` | Disable colored output |
