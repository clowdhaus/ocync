---
title: GitHub Container Registry
description: Using ocync with GHCR via PAT, GITHUB_TOKEN, or docker-config.
order: 6
---

## Auth

GHCR uses the standard OCI Bearer token exchange. Credentials are typically a GitHub personal access token (classic) with `read:packages` / `write:packages` scopes, or the `GITHUB_TOKEN` from GitHub Actions.

ocync resolves credentials in this order:

- `auth_type: static_token` with `token:` (recommended for CI; pairs cleanly with `GITHUB_TOKEN`).
- `auth_type: basic` with username + PAT (ergonomically equivalent to `static_token`).
- `auth_type: docker_config` or unset, which auto-detects `~/.docker/config.json` from `docker login ghcr.io`.

`auth_type: ghcr` is a deprecated alias for `docker_config` and still works; new configs should use `static_token` or `docker_config` directly.

See the [GHCR auth docs](https://docs.github.com/packages/working-with-a-github-packages-registry/working-with-the-container-registry) for issuing PATs and granting package permissions.

Notable behaviors:

- Multi-PATCH chunked upload is broken on GHCR (last PATCH overwrites previous). ocync falls back to POST + single PATCH + PUT (3 requests/blob instead of 2) automatically.
- Cross-repo blob mounting is fulfilled within the same org or user namespace. Mount POSTs returning 202 (not fulfilled) fall through to upload.
- Single 2000 RPM aggregate cap across all reads and writes per authenticated principal. ocync uses one AIMD window so the token-bucket layer cannot exceed the cap by spending read and write budgets concurrently.

## CLI example

```bash
# Push from Chainguard to a private GHCR repo using a PAT.
GITHUB_TOKEN=$(gh auth token) \
ocync copy \
  cgr.dev/chainguard/static:latest \
  ghcr.io/myorg/static:latest
```

(With `auth_type` unset and `docker login ghcr.io` already run, the docker config path picks up the credentials automatically.)

Sync-mode example with explicit token auth:

```yaml
# ocync.yaml
registries:
  src: { url: cgr.dev }
  ghcr:
    url: ghcr.io
    auth_type: static_token
    token: ${GITHUB_TOKEN}

defaults:
  source: src
  targets: ghcr

mappings:
  - from: chainguard/static
    to: myorg/static
```

```bash
GITHUB_TOKEN=$(gh auth token) ocync sync --config ocync.yaml
```

## Kubernetes deployment

GHCR is token-based; the chart pulls the PAT from a Secret via `envFrom`:

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev }
    ghcr:
      url: ghcr.io
      auth_type: static_token
      token: ${GITHUB_TOKEN}
  defaults:
    source: src
    targets: ghcr
  mappings:
    - from: chainguard/static
      to: myorg/static

envFrom:
  - secretRef:
      name: ocync-credentials
```

Create the Secret out-of-band:

```bash
kubectl create secret generic ocync-credentials \
  --from-literal=GITHUB_TOKEN="$(gh auth token)"
```

For PAT rotation via External Secrets Operator or CSI Secrets Store, see [Kubernetes secret patterns](/registries/secrets).
