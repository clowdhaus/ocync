---
title: Azure Container Registry (ACR)
description: Using ocync with Azure Container Registry, covering auth, chunked upload for large blobs, and blob mounting.
order: 5
---

## Auth

ACR uses credentials from your Docker config file, typically configured via `az acr login`.

## Upload behavior

ACR uses the default streaming PUT upload path. ACR has a known ~20 MB body limit on streaming PUT; blobs larger than this may require chunked PATCH upload, which is not yet implemented. Large blob uploads to ACR may fail for blobs exceeding this limit.

## Blob mounting

ACR supports cross-repo blob mounting within the same registry.

## Rate limits

ACR rate limits are per-registry and vary by SKU (Basic/Standard/Premium). `ocync` uses a single shared AIMD window for ACR.

## Example config

```yaml
registries:
  acr:
    url: myregistry.azurecr.io

mappings:
  - from: source/myapp
    to: acr/myapp
```
