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

Abbreviated example:

```json
{
  "run_id": "019713a2-...",
  "images": [
    {
      "image_id": "019713a2-...",
      "source": "cgr.dev/chainguard/nginx:latest",
      "target": "123456789012.dkr.ecr.us-east-1.amazonaws.com/nginx:latest",
      "status": "synced",
      "bytes_transferred": 31457280,
      "blob_stats": { "transferred": 3, "skipped": 1, "mounted": 2 },
      "duration": { "secs": 4, "nanos": 210000000 }
    }
  ],
  "stats": {
    "images_synced": 1,
    "images_skipped": 0,
    "images_failed": 0,
    "blobs_transferred": 3,
    "blobs_skipped": 1,
    "blobs_mounted": 2,
    "bytes_transferred": 31457280,
    "discovery_cache_hits": 0,
    "discovery_cache_misses": 1,
    "discovery_head_failures": 0,
    "discovery_target_stale": 0
  },
  "duration": { "secs": 4, "nanos": 210000000 }
}
```

## Progress indicators

`ocync` auto-detects the output environment:

- **TTY**: real-time progress bars with per-image and aggregate stats
- **Non-TTY / CI**: periodic heartbeat lines with summary counts

Disable all progress output with `--quiet`.

## Logging

Control verbosity with `-v` flags:

| Level | Flag | Output |
|---|---|---|
| Info | (default) | Sync progress and results |
| Debug | `-v` | Auth events, cache decisions, per-image detail |
| Trace | `-vv` | HTTP requests, detailed internals |
| Error | `-q` / `--quiet` | Errors only |

### Log format

```bash
# Human-readable (default everywhere, including Kubernetes)
ocync sync -c config.yaml -vv

# JSON for log-aggregation pipelines that parse structured fields
ocync sync -c config.yaml -vv --log-format json
```

When deployed via the chart, set `logging.format: json` in helm values to opt into JSON output.

Override with the `RUST_LOG` environment variable for fine-grained filter directives.

## Health endpoints

In watch mode, `ocync` exposes HTTP health endpoints:

| Endpoint | Purpose | Healthy |
|---|---|---|
| `/healthz` | Liveness probe | Process is running |
| `/readyz` | Readiness probe | At least one successful sync completed |

Configure the port via `--health-port` (default 8080) or in [Helm values](/helm):

```yaml
mode: watch
watch:
  healthPort: 8080
```

See [CLI reference](/cli-reference#watch) for all `watch` flags including `--interval` and `--json`.
