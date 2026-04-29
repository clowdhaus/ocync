---
title: Google Artifact Registry
description: Using ocync with GAR and legacy GCR via Application Default Credentials.
order: 3
---

## Auth

ocync auto-detects GAR (`*-docker.pkg.dev`) and legacy GCR hostnames (`gcr.io`, `us.gcr.io`, `eu.gcr.io`, `asia.gcr.io`) and uses Google Application Default Credentials (ADC):

- `GOOGLE_APPLICATION_CREDENTIALS` pointing at a service account key file
- `gcloud auth application-default login` (developer machines)
- GKE Workload Identity (cluster-side identity binding)
- Compute Engine / Cloud Run / GCE metadata server (instance default service account)

See the [GCP ADC docs](https://cloud.google.com/docs/authentication/application-default-credentials) for the full precedence chain.

Notable behaviors:

- The OAuth2 access token is sent as the password half of HTTP Basic credentials (username `oauth2accesstoken`) to drive a standard OCI `/v2/token` Bearer exchange. This matches what `gcloud auth configure-docker` writes into the docker config.
- Monolithic uploads: GAR does not support OCI chunked uploads. ocync buffers the full blob in memory and performs a single PUT, so RSS scales with blob size during pushes; a 2 GB layer spikes memory accordingly.
- Cross-repo blob mounts have not been observed to be fulfilled by GAR. ocync attempts the mount POST anyway (cheap, ~100 ms) and falls through to upload on 202.
- GAR enforces a single per-project quota across all operation types; ocync uses one AIMD window per project rather than per-action.
- SDK credential TTL is a conservative 10 minutes (`google-cloud-auth` does not expose `expires_in`), so watch-mode refresh is decoupled from actual token lifetime. ADC refreshes are sub-millisecond.

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

For ExternalSecrets-managed key files or CSI Secrets Store, see [Kubernetes secret patterns](/registries/secrets).
