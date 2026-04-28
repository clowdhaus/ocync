---
title: Chainguard
description: "Using ocync with cgr.dev: anonymous for :latest, chainctl pull-token + auth_type basic for paid tags."
order: 7
---

## Auth

`cgr.dev` uses the standard OCI Bearer token exchange. Three credential paths, in order of likely use:

- `auth_type: basic` with a chainctl pull-token (recommended for CI / Kubernetes). `chainctl auth pull-token` issues a long-lived `(username, password)` pair scoped to an organization or library. Drop those values into `credentials.username` / `credentials.password`; no docker config file required.
- `auth_type: docker_config` (or unset, which auto-detects) with `chainctl auth login` (recommended for developer machines). `chainctl auth configure-docker` writes credentials into `~/.docker/config.json`.
- Anonymous (free tier). Limited to `:latest` and `:latest-dev` on the public catalog. Version-pinned tags, historical revisions, and the full FIPS catalog require a pull-token.

See the [Chainguard auth docs](https://edu.chainguard.dev/chainguard/chainguard-registry/authenticating/) for issuing pull-tokens and configuring chainctl.

Notable behaviors:

- cgr.dev returns **403** on cross-scope token reuse where most registries silently accept it. ocync's per-scope token cache (universal, not Chainguard-specific) issues a fresh exchange per `repository:<name>:<actions>` scope and avoids this.
- Anonymous tag visibility is restricted: only `:latest` and `:latest-dev` are listable / resolvable. Other tags need a pull-token even for public-catalog images.

## CLI examples

### Anonymous

Free tier, limited to `:latest` and `:latest-dev`.

```bash
ocync copy \
  cgr.dev/chainguard/static:latest \
  ghcr.io/myorg/static:latest
```

### Pull-token

Recommended for CI. Generate a pull-token once; the output includes a username and password to capture into your secret store.

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

### Developer machine

`chainctl auth login` + `chainctl auth configure-docker` writes credentials to `~/.docker/config.json`; ocync picks them up automatically.

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

### Docker config volume

For a lift-and-shift from a machine that already has `~/.docker/config.json` populated by `chainctl auth configure-docker`, mount a Secret-backed docker config and use `auth_type: docker_config`:

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
