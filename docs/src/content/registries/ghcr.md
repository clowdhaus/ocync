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

GHCR rate limits are tied to your GitHub account's API rate limit and Actions minutes/storage quotas. `ocync` tracks GHCR rate limits via the `X-RateLimit-Remaining` response header.

## Example config

```yaml
registries:
  ghcr:
    url: ghcr.io

mappings:
  - from: source/myapp
    to: ghcr/my-org/myapp
```
