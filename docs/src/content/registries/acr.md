---
title: Azure Container Registry (ACR)
description: Using ocync with Azure Container Registry, covering auth, chunked upload for large blobs, and blob mounting.
order: 5
---

## Auth

ACR uses credentials from your Docker config file, typically configured via `az acr login`.

## Upload behavior

ACR uses the default streaming PUT upload path. ACR has a known ~20 MB body limit on streaming PUT; blobs larger than this will fail with a connection reset or HTTP 413 error. Chunked PATCH upload fallback is not yet implemented, so blobs exceeding ~20 MB cannot be pushed to ACR.

## Blob mounting

ACR supports cross-repo blob mounting within the same registry.

## Rate limits

ACR rate limits are per-registry and vary by SKU (Basic/Standard/Premium).

## Example config

```yaml
registries:
  chainguard:
    url: cgr.dev
  acr:
    url: myregistry.azurecr.io

defaults:
  source: chainguard
  targets: acr

mappings:
  - from: chainguard/nginx
    to: nginx
```
