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

GAR uses a shared per-project quota across all operation types. `ocync` uses a single shared AIMD window for GAR, capped at 5 concurrent requests, to avoid exhausting the project quota.

## Example config

```yaml
registries:
  gar:
    url: us-docker.pkg.dev

mappings:
  - from: source/myapp
    to: gar/my-project/my-repo/myapp
```
