---
title: Azure Container Registry (ACR)
description: Using ocync with Azure Container Registry, covering auth, chunked upload for large blobs, and blob mounting.
order: 5
---

## Auth

ACR uses credentials from your Docker config file, typically configured via `az acr login`.

## Upload behavior

ACR requires chunked PATCH upload for blobs larger than ~20 MB (streaming PUT body limit). ocync automatically switches to chunked upload with `Content-Range` headers for large blobs.

## Blob mounting

ACR supports cross-repo blob mounting within the same registry.

## Example config

```yaml
registries:
  acr:
    url: myregistry.azurecr.io

mappings:
  - from: source/myapp
    to: acr/myapp
```
