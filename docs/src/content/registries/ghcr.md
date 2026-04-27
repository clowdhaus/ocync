---
title: GitHub Container Registry (GHCR)
description: Using ocync with GHCR via PAT, GITHUB_TOKEN, or docker-config, with the multi-PATCH chunked-upload fallback and the 2000 RPM aggregate cap.
order: 6
---

## Auth

GHCR uses the standard OCI Bearer token exchange. Credentials are typically a GitHub personal access token (classic) with `read:packages` and `write:packages` scopes, or the `GITHUB_TOKEN` provided by GitHub Actions.

ocync resolves credentials in this order:

- Explicit `auth_type: static_token` with `token:` in your ocync config (most common in CI; pairs cleanly with `GITHUB_TOKEN`)
- Explicit `auth_type: ghcr` -- alias for `auth_type: docker_config`, used when credentials are written by `docker login ghcr.io`
- Explicit `auth_type: basic` with username + PAT (ergonomically equivalent to `static_token`)
- `~/.docker/config.json` from `docker login ghcr.io` (auto-detect path when `auth_type` is unset)

See the [GHCR auth docs](https://docs.github.com/packages/working-with-a-github-packages-registry/working-with-the-container-registry) for issuing PATs and granting package permissions.

ocync-specific behaviors:

- **`auth_type: ghcr` is a docker-config alias.** Setting `auth_type: ghcr` is identical to `auth_type: docker_config` apart from a `tracing::debug!` line noting the alias was used. The dispatch resolves both to the same `DockerConfigAuth` provider.
- **Multi-PATCH chunked upload is broken on GHCR.** GHCR's last-PATCH-overwrites-previous bug means ocync cannot use the default chunked PATCH path. ocync falls back to POST + single PATCH + PUT (3 requests/blob instead of 2) automatically.
- **Cross-repo blob mounting.** GHCR fulfills mounts within the same organization or user namespace. Mount POSTs that return 202 (not fulfilled) fall through to upload.
- **Single 2000 RPM aggregate cap.** GHCR enforces one rate-limit window across all reads and writes per authenticated principal. ocync uses a single AIMD window for `ghcr.io` rather than separate read/write windows so the token-bucket layer cannot accidentally exceed the cap by spending the read budget and write budget concurrently.

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

For PAT rotation via External Secrets Operator or CSI Secrets Store, see [Kubernetes secret patterns](./secrets).
