---
title: Google Artifact Registry (GAR)
description: Using ocync with Google Artifact Registry, covering auth, monolithic upload, and configuration.
order: 4
---

## Auth

GAR uses credentials from your Docker config file, typically configured via `gcloud auth configure-docker`.

Legacy GCR hostnames (`gcr.io`, `us.gcr.io`, `eu.gcr.io`, `asia.gcr.io`) are also detected and handled the same way.

## Upload behavior

GAR does not support chunked uploads. `ocync` automatically buffers the full blob and performs a monolithic PUT upload.

## Blob mounting

GAR has not been observed to fulfill cross-repo blob mounts. `ocync` still attempts mounts and falls back to normal upload when the registry returns 202 instead of 201.

## Rate limits

GAR uses a shared per-project quota across all operation types.

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
