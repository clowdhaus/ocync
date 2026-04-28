---
title: Chainguard Registry (cgr.dev)
description: "Using ocync with cgr.dev via anonymous pulls of `:latest` / `:latest-dev` or pull-tokens for paid-tier tags. Recommended path for headless / CI use is `auth_type: basic` with a chainctl pull-token."
order: 7
---

## Auth

Chainguard's `cgr.dev` uses the standard OCI Bearer token exchange. There are three credential surfaces, in the order you'll most likely use them:

- **`auth_type: basic` with a chainctl pull-token (recommended for headless / CI / Kubernetes).** `chainctl auth pull-token` issues a long-lived `(username, password)` pair scoped to an organization or library. Drop those values directly into `credentials.username` / `credentials.password` -- no docker config file required. This is the natural fit for ocync's primary use case (sync running unattended in CI or in a Kubernetes pod).
- **`auth_type: docker_config` (or unset auto-detect) with `chainctl auth login` (recommended for developer machines).** `chainctl auth login` opens an OIDC browser flow and `chainctl auth configure-docker` writes the resulting credentials into `~/.docker/config.json`. ocync auto-detects the docker config path for `cgr.dev`.
- **Anonymous (free tier).** No credentials required. Limited to `:latest` and `:latest-dev` tags on the public image catalog. Version-pinned tags (e.g., `python:3.12`), historical revisions, and the full FIPS catalog all require a pull-token.

See the [Chainguard auth docs](https://edu.chainguard.dev/chainguard/chainguard-registry/authenticating/) for issuing pull-tokens and configuring chainctl.

ocync's resolution order when a request hits `cgr.dev`:

1. Explicit `auth_type: basic` with `credentials:` -- HTTP Basic in the `/v2/token` exchange.
2. Explicit `auth_type: docker_config` (or unset, which auto-detects to docker-config for `cgr.dev`) -- read `~/.docker/config.json` (or `$DOCKER_CONFIG/config.json`); HTTP Basic with the matching entry's username/password.
3. Anonymous fallback when no docker config entry exists for `cgr.dev`.

ocync-specific behaviors:

- **Per-scope token cache (universal, not Chainguard-specific).** Every Bearer-issuing provider in ocync caches tokens per repository scope, keyed by `repository:<name>:<actions>`. cgr.dev's notable quirk is *enforcement*: it returns **403** on cross-scope token reuse, where some registries silently accept cross-scope tokens. The per-scope cache avoids this 403 by issuing a fresh exchange for each scope.
- **Anonymous tag visibility is restricted.** Only `:latest` and `:latest-dev` are listed/resolvable anonymously. Pulling other tags requires a pull-token even for images in the public catalog.

## CLI example

### Anonymous (free tier, `:latest` only)

```bash
ocync copy \
  cgr.dev/chainguard/static:latest \
  ghcr.io/myorg/static:latest
```

### Static credentials via `chainctl auth pull-token` (recommended for CI)

Generate a pull-token once (output includes a username and password; capture both into your secret store):

```bash
chainctl auth pull-token --library-paid <your-org-slug>
# Username: <pull-token-id>
# Password: <pull-token-secret>
```

Then either use the credentials directly on the CLI:

```bash
CHAINGUARD_USERNAME=<pull-token-id> \
CHAINGUARD_PASSWORD=<pull-token-secret> \
ocync sync --config ocync.yaml
```

with `ocync.yaml`:

```yaml
registries:
  src:
    url: cgr.dev
    auth_type: basic
    credentials:
      username: ${CHAINGUARD_USERNAME}
      password: ${CHAINGUARD_PASSWORD}
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

### Developer machine via `chainctl auth login` + docker config

```bash
chainctl auth login
chainctl auth configure-docker

# ocync picks up the credentials automatically:
ocync copy \
  cgr.dev/chainguard/python:3.12 \
  ghcr.io/myorg/python:3.12
```

## Kubernetes deployment

The recommended K8s path is the same `auth_type: basic` flow as CI, with the pull-token's username/password injected via `envFrom`:

```yaml
# values.yaml
config:
  registries:
    src:
      url: cgr.dev
      auth_type: basic
      credentials:
        username: ${CHAINGUARD_USERNAME}
        password: ${CHAINGUARD_PASSWORD}
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

Create the Secret out-of-band, capturing the pull-token output:

```bash
chainctl auth pull-token --library-paid <your-org-slug>
# Username: <pull-token-id>
# Password: <pull-token-secret>

kubectl create secret generic ocync-credentials \
  --from-literal=CHAINGUARD_USERNAME=<pull-token-id> \
  --from-literal=CHAINGUARD_PASSWORD=<pull-token-secret> \
  --from-literal=GITHUB_TOKEN="$(gh auth token)"
```

For credential rotation -- pulling the pull-token out of AWS Secrets Manager / GCP Secret Manager / Azure Key Vault / Vault and refreshing it on a schedule -- use the External Secrets Operator or CSI Secrets Store patterns documented in [Kubernetes secret patterns](./secrets); the `auth_type: basic` configuration above stays unchanged, only the source of `CHAINGUARD_USERNAME` / `CHAINGUARD_PASSWORD` changes.

### Alternative: docker-config-volume approach

When you have an existing `chainctl auth configure-docker` flow (e.g., a machine that already has `~/.docker/config.json` populated and you want to lift-and-shift), mount a Secret-backed docker config and use `auth_type: docker_config`:

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev, auth_type: docker_config }
    # ... targets ...

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

```bash
chainctl auth login
chainctl auth configure-docker

kubectl create secret generic chainguard-docker-config \
  --from-file=config.json=$HOME/.docker/config.json
```

This works but ties the pod to a specific `~/.docker/config.json` snapshot; rotating the credentials means re-running `chainctl auth configure-docker` and re-creating the Secret. The `auth_type: basic` + `envFrom` path is more declarative and rotates cleanly via ESO / CSI.
