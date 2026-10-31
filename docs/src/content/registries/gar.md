---
title: Google Artifact Registry
description: Using ocync with Artifact Registry, including gcr.io, via Application Default Credentials.
order: 3
---

## Auth

ocync auto-detects GAR (`*-docker.pkg.dev`) and GCR hostnames (`gcr.io` and any `*.gcr.io`, such as `us.gcr.io`, which Artifact Registry [serves](https://docs.cloud.google.com/artifact-registry/docs/transition/gcr-repositories)) and uses Google Application Default Credentials (ADC):

- `GOOGLE_APPLICATION_CREDENTIALS` pointing at a credentials file: a service account key, an authorized user, a Workload Identity Federation (`external_account`) configuration or an impersonated service account
- `gcloud auth application-default login` (developer machines)
- GKE Workload Identity (cluster-side identity binding)
- Compute Engine / Cloud Run / GCE metadata server (instance default service account)

Workload Identity Federation takes its subject token from a file (such as a projected Kubernetes service account token), a URL, or AWS. On EKS, the AWS source signs with the pod's AWS credentials (IRSA or Pod Identity), so `ocync` reaches Artifact Registry with no service account key. The executable source is not supported. See the [security design](/design/security#owned-google-authentication) for how this matches Google's own library.

With no Google credentials at all, ocync reads a Google registry it detects by hostname as it reads any other registry: with the credentials `docker login` or `gcloud auth configure-docker` put in the docker config, else anonymously, so public repositories work (`mirror.gcr.io`, `gcr.io/distroless` and public Artifact Registry repositories were read this way on 2026-10-31). "No Google credentials" means no `GOOGLE_APPLICATION_CREDENTIALS` file, no gcloud credentials file, no `GCE_METADATA_HOST` that is set and not empty, and a machine that is not on Google Cloud: its DMI product name is not Google's, and the metadata IP does not answer one probe of at most 2 seconds with `Metadata-Flavor: Google`, which is how Cloud Run and Cloud Functions are detected. The machine is asked once per process, so watch mode does not ask each cycle. Credentials that exist but fail to load or are refused are an error, never an anonymous read, and so is a metadata server that fails on Google Cloud. A registry set with `auth_type: gar` or `gcr` never reads anonymously and never waits for the probe. `ocync auth check` marks a registry anonymous when the docker config holds no credentials for it.

See the [GCP ADC docs](https://cloud.google.com/docs/authentication/application-default-credentials) for the full precedence chain.

Notable behaviors:

- The OAuth2 access token is sent as the password half of HTTP Basic credentials (username `oauth2accesstoken`) to drive a standard OCI `/v2/token` Bearer exchange. This matches what `gcloud auth configure-docker` writes into the docker config.
- Streaming uploads: ocync uploads each blob as a POST followed by one streaming PUT (`Transfer-Encoding: chunked`), so memory does not grow with blob size. On 2026-10-28 that path peaked at 21.6 MB while pushing 382,663,875 bytes across 8 blobs. Buffering each blob for a single PUT, the path ocync used before, peaked at 452.8 MB on a different set of 8 blobs totalling 224,330,551 bytes, so the two are not a like-for-like comparison: the buffered peak follows the largest blobs held at once. A second PATCH in one upload session answers 405, and a monolithic POST answers 502, so ocync uses neither.
- Cross-repo blob mounts between two images in the same Artifact Registry repository are fulfilled: on 2026-10-28 a sync into `<repo>/main/static` mounted all 4 blobs an earlier sync had pushed to `<repo>/branch/static` in the same repository, transferring 0 bytes. Mounts between repositories are fulfilled too: on 2026-10-30 one run syncing a two-platform index into two Artifact Registry repositories uploaded its 4 blobs once and mounted all 4 into the second. A mount answered 202 falls through to upload.
- GAR enforces one quota per project and region across all operation types. ocync paces each registry host in one window at 240 requests per second, so hosts sharing a project and region, such as `gcr.io` and `us-docker.pkg.dev`, are paced separately against one quota.
- The access token is kept for its real lifetime, from the token endpoint's `expires_in` or impersonation's `expireTime`, and fetched again 30 seconds before it expires. It lasted 3,600 seconds on 2026-10-30, and the registry token exchanged for it 43,200.

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
