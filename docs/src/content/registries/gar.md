---
title: Google Artifact Registry (GAR)
description: Using ocync with Google Artifact Registry, covering auth, monolithic upload, and configuration.
order: 4
---

## Auth

GAR uses credentials from your Docker config file, typically configured via `gcloud auth configure-docker`.

## Upload behavior

GAR does not support chunked uploads. ocync automatically buffers the full blob and performs a monolithic PUT upload.

## Blob mounting

GAR does not support cross-repo blob mounting.

## Example config

```yaml
registries:
  gar:
    url: us-docker.pkg.dev

mappings:
  - from: source/myapp
    to: gar/my-project/my-repo/myapp
```
