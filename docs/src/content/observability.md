---
title: Observability
description: Structured JSON output, progress indicators, health endpoints, and logging configuration for ocync.
order: 5
---

## Structured output

Use `--json` for machine-readable sync reports:

```bash
ocync sync -c config.yaml --json
```

Reports include per-image results and aggregate statistics: blobs transferred, bytes moved, mounts performed, cache hits, and errors.

## Progress indicators

`ocync` auto-detects the output environment:

- **TTY**: real-time progress bars with per-image and aggregate stats
- **Non-TTY / CI**: periodic heartbeat lines with summary counts

Disable all progress output with `--quiet`.

## Logging

Control verbosity with `-v` flags:

| Level | Flag | Output |
|---|---|---|
| Error | (default) | Errors only |
| Warn | `-v` | Warnings and errors |
| Info | `-vv` | Sync progress, auth events |
| Debug | `-vvv` | HTTP requests, cache decisions |

### Log format

```bash
# Human-readable (default)
ocync sync -c config.yaml -vv

# JSON (auto-detected in Kubernetes, or explicit)
ocync sync -c config.yaml -vv --log-format json
```

JSON log format is auto-detected when running inside Kubernetes (via `KUBERNETES_SERVICE_HOST` environment variable).

Override with the `RUST_LOG` environment variable for fine-grained filter directives.

## Health endpoints (watch mode)

When running in watch mode, `ocync` exposes HTTP health endpoints:

| Endpoint | Purpose | Healthy |
|---|---|---|
| `/healthz` | Liveness probe | Process is running |
| `/readyz` | Readiness probe | At least one successful sync completed |

Configure the port via `--health-port` (default 8080) or in [Helm values](../helm):

```yaml
mode: watch
watch:
  healthPort: 8080
```

See [CLI reference](../cli-reference#watch) for all `watch` flags including `--interval` and `--json`.
