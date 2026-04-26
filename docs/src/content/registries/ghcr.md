---
title: GitHub Container Registry (GHCR)
description: Using ocync with GHCR, covering token auth, upload quirks, and blob mounting.
order: 3
---

## Auth

GHCR uses token-based auth via your Docker config file or a GitHub personal access token (PAT) with `read:packages` and `write:packages` scopes.

To use a PAT directly in the config:

```yaml
registries:
  ghcr:
    url: ghcr.io
    auth_type: static_token
    token: ${GITHUB_TOKEN}
```

## Upload behavior

GHCR has a known issue with multi-PATCH chunked uploads: the last PATCH overwrites previous chunks. `ocync` automatically falls back to single-PATCH upload for GHCR.

## Blob mounting

GHCR supports cross-repo blob mounting within the same organization/user namespace.

## Rate limits

GHCR enforces a single aggregate cap of 2000 requests per minute per authenticated principal, shared across read and write actions. `ocync` models this with one shared AIMD window for `ghcr.io` (rather than separate read/write windows) so the token-bucket layer cannot exceed the documented cap by spending the read budget and write budget concurrently.

## Example config

```yaml
registries:
  ghcr:
    url: ghcr.io
    auth_type: static_token
    token: ${GITHUB_TOKEN}
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: ghcr
  targets: ecr

mappings:
  - from: my-org/myapp
    to: myapp
```
