---
title: Google Artifact Registry (GAR / GCR)
description: Using ocync with Google Artifact Registry via Application Default Credentials, including legacy GCR hostnames and monolithic-upload behavior.
order: 3
---

## Auth

ocync auto-detects GAR (`*-docker.pkg.dev`) and the legacy GCR hostnames (`gcr.io`, `us.gcr.io`, `eu.gcr.io`, `asia.gcr.io`) and uses Google Application Default Credentials (ADC) via the `google-cloud-auth` library:

- `GOOGLE_APPLICATION_CREDENTIALS` env var pointing at a service account key file
- `gcloud auth application-default login` (developer machines)
- GKE Workload Identity (cluster-side identity binding)
- Compute Engine / Cloud Run / GCE metadata server (instance default service account)

See the [GCP ADC docs](https://cloud.google.com/docs/authentication/application-default-credentials) for the full credential precedence chain.

ocync-specific behaviors:

- **Bearer-via-Basic flow.** ocync exchanges the OAuth2 access token for an OCI Bearer token by sending the token as the password half of HTTP Basic credentials with the username `oauth2accesstoken`. This matches what `gcloud auth configure-docker` writes into the docker config.
- **Conservative SDK credential TTL (600s).** `google-cloud-auth` does not expose `expires_in`, so ocync re-fetches every 10 minutes regardless of the actual token lifetime. Workload Identity Federation tokens can be as short as 15 minutes; a 10-minute refresh window prevents the 401 / invalidate / re-fetch round-trip in long-running watch mode. ADC refreshes are sub-millisecond.
- **Monolithic uploads.** GAR does not support OCI chunked uploads. ocync buffers the full blob in memory and performs a single PUT. This means RSS scales with blob size during pushes; a 2GB layer will spike memory accordingly.
- **Mount-attempt-then-fallback.** GAR has not been observed to fulfill cross-repo blob mounts. ocync still attempts the mount POST (cheap, ~100ms) and falls through to a normal upload when GAR returns 202 instead of 201.
- **Per-project shared quota.** GAR enforces a single per-project quota across all operation types. ocync uses one AIMD window per project rather than per-action.

## CLI example

```bash
# Push from Chainguard to GAR. Works on a developer machine after
# `gcloud auth application-default login` or with GOOGLE_APPLICATION_CREDENTIALS set.
ocync copy \
  cgr.dev/chainguard/static:latest \
  us-docker.pkg.dev/my-project/my-repo/static:latest
```

Sync-mode example:

```yaml
# ocync.yaml
registries:
  src: { url: cgr.dev }
  gar: { url: us-docker.pkg.dev }

defaults:
  source: src
  targets: gar

mappings:
  - from: chainguard/static
    to: my-project/my-repo/static
```

```bash
ocync sync --config ocync.yaml
```

## Kubernetes deployment

On GKE, use Workload Identity. The chart's `workloadIdentity.gcp` block sets the `iam.gke.io/gcp-service-account` annotation:

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev }
    gar: { url: us-docker.pkg.dev }
  defaults:
    source: src
    targets: gar
  mappings:
    - from: chainguard/static
      to: my-project/my-repo/static

workloadIdentity:
  provider: gcp
  gcp:
    serviceAccount: ocync@my-project.iam.gserviceaccount.com
```

Off-GKE (e.g., running on EKS but pushing to GAR), mount a service account key file via `extraVolumes` and set `GOOGLE_APPLICATION_CREDENTIALS`:

```yaml
env:
  - name: GOOGLE_APPLICATION_CREDENTIALS
    value: /var/secrets/gcp/key.json

extraVolumes:
  - name: gcp-key
    secret:
      secretName: gcp-service-account-key

extraVolumeMounts:
  - name: gcp-key
    mountPath: /var/secrets/gcp
    readOnly: true
```

For ExternalSecrets-managed key files or CSI Secrets Store, see [Kubernetes secret patterns](./secrets).
