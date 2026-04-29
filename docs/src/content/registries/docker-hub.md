---
title: Docker Hub
description: Using ocync with Docker Hub via docker config or static credentials.
order: 5
---

## Auth

Docker Hub uses the standard OCI Bearer token exchange. ocync resolves credentials in this order:

- `auth_type: basic` with `credentials:` (env-substituted `${DOCKER_USERNAME}` / `${DOCKER_PASSWORD}`)
- `auth_type: docker_config` or unset (auto-detects `~/.docker/config.json`; override the path with `DOCKER_CONFIG`)
- Anonymous (severely rate-limited)

Personal access tokens (created at <https://app.docker.com/settings/personal-access-tokens>) are recommended over passwords; the token goes in the password field, with your Docker Hub username.

Notable behaviors:

- Endpoint split. The canonical identifier (image references, token scopes, `/v2/token` `service` parameter) is `docker.io`, but the `/v2/` API is served from `registry-1.docker.io` (`docker.io` itself 302s to Docker's marketing site). ocync rewrites the endpoint while keeping the auth scope as `docker.io`, matching docker / skopeo / crane / regclient. `docker.io`, `index.docker.io`, and `registry-1.docker.io` are recognized as aliases; use `docker.io` in new configs.
- Rate-limit accounting: manifest GETs count against the documented limit (10 anonymous / hour, 100 authenticated free / 6h); blob GETs do not, and HEADs are free. ocync issues HEAD checks before GETs wherever possible to minimize quota burn.
- Three concurrency windows: HEADs share an unmetered window, manifest reads have their own rate-limited window, and writes share a third. A manifest-read 429 does not block blob HEADs.
- Cross-repo blob mounting is fulfilled within the same account. Mount POSTs returning 202 (not fulfilled) fall back to upload.

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

For ExternalSecrets-managed credentials or CSI-mounted secret stores, see [Kubernetes secret patterns](/registries/secrets).
