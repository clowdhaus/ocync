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
| `-v` / `--verbose` | Increase log verbosity (`-v` debug, `-vv` or higher trace) |
| `-q`, `--quiet` | Suppress all output except errors |
| `--log-format` | Set log format: `text` (default) or `json` |

## sync

Sync images defined in a config file:

```bash
ocync sync -c config.yaml
ocync sync -c config.yaml --dry-run
ocync sync -c config.yaml --json
```

| Flag | Description |
|---|---|
| `-c`, `--config` | Path to sync config file (required) |
| `--dry-run` | Preview what would sync without making changes |
| `--json` | Output sync report as JSON to stdout |

### Dry-run output

`--dry-run` runs the full filter pipeline against each mapping's source tags and prints, per mapping:

- **`source candidates: N`** -- the number of tags fetched from the source.
- **`include path:`** -- tags rescued via `include:` (bypasses `glob:`/`semver:` and the system-exclude defaults). Default cap is 5 names; `-v` removes the cap.
- **`pipeline:`** -- per-stage attrition (`glob`, `semver`, `exclude`, `latest`). Each row shows count_in -> count_out and the drop count.
- **`kept (N):`** -- the final tags. When `include:` is used, rescued tags are listed first and tagged `[via include]` so the rescue path is visible.
- **`dropped N:`** -- Pareto-sorted drop attribution (largest cause first), with sample tag names per reason. Default cap is 5 names per reason; `-v` removes the cap.
- **`min_tags: N`** -- when `min_tags:` is configured, the line prints `kept M, satisfied` or `kept M, real sync will FAIL with BelowMinTags`. Real-sync (no `--dry-run`) errors out below `min_tags`; dry-run shows the report and surfaces the gap so the configuration can be fixed before running.

## copy

Copy a single image between registries:

```bash
ocync copy cgr.dev/chainguard/nginx:latest \
    123456789012.dkr.ecr.us-east-1.amazonaws.com/nginx:latest
```

| Argument | Description |
|---|---|
| `<source>` | Source image reference with tag (required) |
| `<destination>` | Destination image reference (required) |

The source reference must include a tag. The destination tag defaults to the source tag if omitted.

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
| `--health-bind` | `127.0.0.1` | IP for the health endpoint to bind on. Set to `0.0.0.0` for container hosts where probes originate externally |
| `--json` | | Output sync reports as JSON |

See [observability](/observability) for health endpoint details.

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

## validate

Validate a config file without connecting to registries:

```bash
ocync validate config.yaml
```

| Argument | Description |
|---|---|
| `<config>` | Path to the config file to validate (required) |

Checks config syntax, structure, and references (registry names, target groups) without making any network requests. Catches errors before attempting a sync. Exits with code `0` on success or `3` on invalid configuration.

## expand

Show config with all environment variables resolved:

```bash
ocync expand config.yaml
ocync expand config.yaml --show-secrets
```

| Flag | Description |
|---|---|
| `--show-secrets` | Show credential values instead of redacting them. Do not use when stdout is piped to a file or logging system |

## auth check

Verify registry credentials for all registries in a config:

```bash
ocync auth check -c config.yaml
ocync auth check -c config.yaml -c config2.yaml
```

| Flag | Description |
|---|---|
| `-c`, `--config` | Path to config file (required, repeatable for multiple configs) |

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
| `RUST_LOG` | Log filter directive (overrides `-v` flags) |
| `NO_COLOR` | Disable colored output |
