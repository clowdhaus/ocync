---
title: Google Artifact Registry (GAR)
description: Using ocync with Google Artifact Registry, covering auth, monolithic upload, and configuration.
order: 4
---

## Auth

GAR uses credentials from your Docker config file, typically configured via `gcloud auth configure-docker`.

## Upload behavior

GAR does not support chunked uploads. `ocync` automatically buffers the full blob and performs a monolithic PUT upload.

## Blob mounting

GAR has not been observed to fulfill cross-repo blob mounts. `ocync` still attempts mounts and falls back to normal upload when the registry returns 202 instead of 201.

## Rate limits

GAR uses a shared per-project quota across all operation types. `ocync` uses a single shared AIMD window for GAR with an initial concurrency of 5 that grows adaptively up to `max_concurrent` (default 50) based on 429 feedback.

## Example config

```yaml
registries:
  chainguard:
    url: cgr.dev
  gar:
    url: us-docker.pkg.dev

defaults:
  source: chainguard
  targets: gar

mappings:
  - from: chainguard/nginx
    to: my-project/my-repo/nginx
```
