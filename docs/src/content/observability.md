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

A mapping that could not be resolved at all -- unreachable registry, denied tag listing, bad repository name -- never reaches the engine and so produces no per-image entry. Those mappings are listed separately under `unresolved_mappings`, which is omitted entirely when every mapping resolved. The rest of the run still proceeds; one failing mapping does not cancel the others.

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
  "duration": { "secs": 4, "nanos": 210000000 },
  "unresolved_mappings": [
    {
      "from": "cgr.dev/chainguard/private",
      "error": "mapping 'cgr.dev/chainguard/private': registry error: 403 Forbidden"
    }
  ]
}
```

## Progress indicators

`ocync` auto-detects the output environment:

- **TTY**: real-time progress bars with per-image and aggregate stats
- **Non-TTY / CI**: periodic heartbeat lines with summary counts

At the default verbosity, per-image lines are suppressed, so a long run would otherwise be silent from start to finish. Two periodic lines fill that gap:

- `resolving mappings` during mapping resolution, once the phase has run for more than five seconds
- `sync in progress` every 30 seconds while discovery or transfers are still in flight, carrying `discovering`, `pending`, `in_flight`, `completed`, and `elapsed_secs`

Both are timer-gated, so a run that finishes quickly emits neither.

Disable all progress output with `--quiet`.

## Logging

Control verbosity with `-v` flags:

| Level | Flag | Output |
|---|---|---|
| Info | (default) | Sync progress and results |
| Debug | `-v` | Auth events, cache decisions, per-image detail |
| Trace | `-vv` | HTTP requests, detailed internals |
| Error | `-q` / `--quiet` | Errors only |

`-v` also uncaps the per-reason sample list in `--dry-run` output (default cap: 5 tags per drop reason and 5 names in the literal include path).

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
