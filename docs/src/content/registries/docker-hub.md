---
title: Docker Hub
description: Using ocync with Docker Hub, covering rate limits, auth, and what counts against your quota.
order: 2
---

## Auth

Docker Hub uses credentials from your Docker config file (`~/.docker/config.json`), or you can provide static credentials in the config file. Set the `DOCKER_CONFIG` environment variable to use an alternative config directory.

Anonymous access works but has severely restricted rate limits.

## Rate limits

As of 2025, Docker Hub enforces these limits:

| Access | Limit |
|---|---|
| Anonymous | 10 manifest pulls/hour |
| Authenticated (free) | 100 manifest pulls/hour |
| Paid plans | Higher limits |

**What counts:**
- Manifest GET requests count against the limit
- Blob GET requests do **not** count
- HEAD requests are **free**

ocync uses HEAD checks wherever possible to minimize rate-limit consumption.

## Blob mounting

Docker Hub supports cross-repo blob mounting. ocync automatically uses this when blobs exist in another repository under the same account.

## Example config

```yaml
registries:
  dockerhub:
    url: registry-1.docker.io
    credentials:
      username: ${DOCKER_USERNAME}
      password: ${DOCKER_PASSWORD}

mappings:
  - from: dockerhub/library/nginx
    to: ecr/nginx
```

For Docker config file auth (no explicit credentials needed):

```yaml
registries:
  dockerhub:
    url: registry-1.docker.io
    auth_type: docker_config
```
