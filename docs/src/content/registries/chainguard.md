---
title: Chainguard Registry (cgr.dev)
description: Using ocync with cgr.dev via anonymous pulls of `:latest` / `:latest-dev` or chainctl pull-tokens for paid-tier tags.
order: 7
---

## Auth

Chainguard's `cgr.dev` uses the standard OCI Bearer token exchange. Two distinct credential surfaces:

- **Anonymous (free tier).** No credentials required. Limited to `:latest` and `:latest-dev` tags on the public image catalog.
- **Paid tier (pull-tokens).** Issued via `chainctl auth login` or `chainctl auth pull-token` and written into the Docker config file. Required for version-pinned tags (e.g., `python:3.12`), historical revisions, and the full FIPS catalog.

See the [Chainguard auth docs](https://edu.chainguard.dev/chainguard/chainguard-registry/authenticating/) for issuing pull-tokens.

ocync resolves credentials in this order:

- Explicit `auth_type: basic` with `credentials:` (rare for cgr.dev; the docker-config flow is the standard path because `chainctl` writes there)
- Explicit `auth_type: docker_config` (or unset, which auto-detects to docker-config for `cgr.dev`)
- `~/.docker/config.json` populated by `chainctl auth login`
- Anonymous fallback when no docker config entry exists for `cgr.dev`

ocync-specific behaviors:

- **Per-scope token cache (universal, not Chainguard-specific).** Every Bearer-issuing provider in ocync caches tokens per repository scope, keyed by `repository:<name>:<actions>`. cgr.dev's notable quirk is *enforcement*: it returns **403** on cross-scope token reuse, where some registries silently accept cross-scope tokens. The per-scope cache avoids this 403 by issuing a fresh exchange for each scope.
- **Anonymous tag visibility is restricted.** Only `:latest` and `:latest-dev` are listed/resolvable anonymously. Pulling other tags requires a pull-token even for images in the public catalog.

## CLI example

```bash
# Anonymous pull of :latest, push to your registry.
ocync copy \
  cgr.dev/chainguard/static:latest \
  ghcr.io/myorg/static:latest
```

For paid-tier tags, log in once via chainctl (writes to `~/.docker/config.json`):

```bash
chainctl auth login
chainctl auth configure-docker

# Then ocync picks up the credentials automatically:
ocync copy \
  cgr.dev/chainguard/python:3.12 \
  ghcr.io/myorg/python:3.12
```

Sync-mode:

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
  - from: chainguard/python
    to: myorg/python
```

## Kubernetes deployment

For paid-tier tags, the pull-token (or service-account credentials) need to be available to the pod via a docker-config file. The chart's `extraVolumes` mounts a Secret-backed docker config:

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev, auth_type: docker_config }
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
      name: ocync-credentials  # provides GITHUB_TOKEN

extraVolumes:
  - name: docker-config
    secret:
      secretName: chainguard-docker-config
      items:
        - key: config.json
          path: config.json

extraVolumeMounts:
  - name: docker-config
    mountPath: /home/nonroot/.docker
    readOnly: true
```

Create both Secrets out-of-band:

```bash
chainctl auth login
chainctl auth configure-docker

kubectl create secret generic chainguard-docker-config \
  --from-file=config.json=$HOME/.docker/config.json

kubectl create secret generic ocync-credentials \
  --from-literal=GITHUB_TOKEN="$(gh auth token)"
```

For credential rotation via External Secrets or CSI Secrets Store, see [Kubernetes secret patterns](./secrets).
