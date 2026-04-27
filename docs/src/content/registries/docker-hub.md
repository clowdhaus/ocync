---
title: Docker Hub
description: Using ocync with Docker Hub via docker config or static credentials, including rate-limit specifics and the docker.io / registry-1.docker.io endpoint split.
order: 5
---

## Auth

Docker Hub uses the standard OCI Bearer token exchange. ocync resolves credentials in this order:

- Explicit `auth_type: basic` with `credentials:` in your ocync config (env-substituted `${DOCKER_USERNAME}` / `${DOCKER_PASSWORD}`)
- Explicit `auth_type: docker_config` (or unset, which falls back to docker-config auto-detection)
- `~/.docker/config.json` from `docker login` (override the path with `DOCKER_CONFIG`)
- Anonymous (severely rate-limited)

Personal access tokens (created at <https://app.docker.com/settings/personal-access-tokens>) are recommended over passwords. The token replaces the password field; the username is your Docker Hub username.

ocync-specific behaviors:

- **Endpoint split: `docker.io` vs `registry-1.docker.io`.** The canonical identifier (image references, token scopes, `/v2/token` `service` parameter) is `docker.io`. The HTTP `/v2/` API is served from `registry-1.docker.io`; `docker.io` itself 302s to Docker's marketing site. ocync rewrites the endpoint host while keeping the auth scope as `docker.io`. Every other tool (docker, skopeo, crane, regclient) does the same rewrite.
- **Rate-limit accounting.** Manifest GET requests count against the documented limit (10 anonymous / hour, 100 authenticated free / 6h). Blob GETs do **not** count. HEAD requests are free. ocync issues HEAD checks before GETs wherever possible to minimize quota burn.
- **Three concurrency windows.** HEADs share an unmetered window; manifest reads have their own (rate-limited) window; remaining write paths share a third window. This isolates a manifest-read 429 from blocking blob HEADs.
- **Cross-repo blob mounting.** Docker Hub fulfills mounts within the same account. ocync attempts mount POST first; on 202 (not fulfilled) falls back to upload.
- **Aliases.** ocync recognizes `docker.io`, `index.docker.io`, and `registry-1.docker.io` as the same registry. Use `docker.io` in new configs.

## CLI example

```bash
# Pull from Docker Hub library, push to your private ECR.
ocync copy \
  docker.io/library/nginx:latest \
  123456789012.dkr.ecr.us-east-1.amazonaws.com/nginx:latest
```

Sync-mode with credentials from environment:

```yaml
# ocync.yaml
registries:
  hub:
    url: docker.io
    auth_type: basic
    credentials:
      username: ${DOCKER_USERNAME}
      password: ${DOCKER_PASSWORD}
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: hub
  targets: ecr

mappings:
  - from: library/nginx
    to: nginx
```

```bash
DOCKER_USERNAME=youruser DOCKER_PASSWORD=$(cat ~/dockerhub-pat) \
  ocync sync --config ocync.yaml
```

For docker-config-based auth (no explicit credentials in the ocync config):

```yaml
registries:
  hub:
    url: docker.io
    auth_type: docker_config
```

## Kubernetes deployment

Docker Hub is token-based, not cloud-IAM. The chart pulls credentials from a Kubernetes Secret via `envFrom`:

```yaml
# values.yaml
config:
  registries:
    hub:
      url: docker.io
      auth_type: basic
      credentials:
        username: ${DOCKER_USERNAME}
        password: ${DOCKER_PASSWORD}
    ecr: { url: 123456789012.dkr.ecr.us-east-1.amazonaws.com }
  defaults:
    source: hub
    targets: ecr
  mappings:
    - from: library/nginx
      to: nginx

envFrom:
  - secretRef:
      name: ocync-credentials
```

Create the Secret out-of-band:

```bash
kubectl create secret generic ocync-credentials \
  --from-literal=DOCKER_USERNAME=youruser \
  --from-literal=DOCKER_PASSWORD="$(cat ~/dockerhub-pat)"
```

For ExternalSecrets-managed credentials or CSI-mounted secret stores, see [Kubernetes secret patterns](./secrets).
