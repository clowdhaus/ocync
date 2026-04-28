---
title: Kubernetes secrets
description: Four ways to plug registry credentials into the ocync Helm chart.
order: 8
---

ocync's Helm chart exposes four orthogonal patterns for getting registry credentials into the workload pod: `envFrom`, External Secrets Operator, CSI Secrets Store, and Workload Identity. Pick the one that matches how secrets are managed in your cluster; combinations are fine (e.g., Workload Identity for AWS plus `envFrom` for a Docker Hub PAT).

The chart values referenced below are documented inline in [`charts/ocync/values.yaml`](https://github.com/clowdhaus/ocync/blob/main/charts/ocync/values.yaml). Working CI fixtures live in [`charts/ocync/ci/`](https://github.com/clowdhaus/ocync/tree/main/charts/ocync/ci).

## envFrom

Simplest pattern. Create a `Secret` out-of-band and reference it via `envFrom`; the ocync config substitutes `${VAR_NAME}` from environment variables.

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
    # ... targets ...

envFrom:
  - secretRef:
      name: ocync-credentials
```

```bash
kubectl create secret generic ocync-credentials \
  --from-literal=DOCKER_USERNAME=youruser \
  --from-literal=DOCKER_PASSWORD="$(cat ~/dockerhub-pat)"
```

This pattern composes with everything below: Workload Identity covers cloud auth, `envFrom` covers token-based registries in the same pod.

## External Secrets Operator

When secrets live in AWS Secrets Manager, GCP Secret Manager, Azure Key Vault, HashiCorp Vault, or a similar external store, [External Secrets Operator (ESO)](https://external-secrets.io/) keeps a native `Secret` synchronized from the source of truth. The chart can render the `ExternalSecret` resource for you so the entire deployment is declarative:

```yaml
# values.yaml
externalSecrets:
  enabled: true
  refreshInterval: "1h"
  secretStoreRef:
    name: aws-secret-store
    kind: ClusterSecretStore
  target:
    name: ocync-credentials
    creationPolicy: Owner
  data:
    - secretKey: DOCKER_USERNAME
      remoteRef:
        key: prod/ocync/docker-username
    - secretKey: DOCKER_PASSWORD
      remoteRef:
        key: prod/ocync/docker-password

envFrom:
  - secretRef:
      name: ocync-credentials
```

The chart renders `ExternalSecret` (`external-secrets.io/v1`); ESO writes the `ocync-credentials` Secret; the workload consumes it via `envFrom`. ESO must already be installed in the cluster -- the chart does not pre-check CRD presence and `kubectl apply` will surface a `no matches for kind "ExternalSecret"` error if it is missing.

For the underlying `SecretStore` / `ClusterSecretStore` (which provider, how it authenticates), see the [ESO provider docs](https://external-secrets.io/latest/provider/aws-secrets-manager/) for your target store.

## CSI Secrets Store

When you want secrets mounted as files (not env vars) and synced into a native Secret as a side effect, the [Secrets Store CSI Driver](https://secrets-store-csi-driver.sigs.k8s.io/) is the standard pattern. The chart renders a `SecretProviderClass` (`secrets-store.csi.x-k8s.io/v1`) and you consume it via `extraVolumes`:

```yaml
# values.yaml
secretProviderClass:
  enabled: true
  provider: aws  # | azure | gcp | vault
  parameters:
    objects: |
      - objectName: prod/ocync/docker-username
        objectAlias: DOCKER_USERNAME
        objectType: secretsmanager
      - objectName: prod/ocync/docker-password
        objectAlias: DOCKER_PASSWORD
        objectType: secretsmanager
  secretObjects:
    - secretName: ocync-credentials
      type: Opaque
      data:
        - objectName: DOCKER_USERNAME
          key: DOCKER_USERNAME
        - objectName: DOCKER_PASSWORD
          key: DOCKER_PASSWORD

extraVolumes:
  - name: secrets-store
    csi:
      driver: secrets-store.csi.k8s.io
      readOnly: true
      volumeAttributes:
        secretProviderClass: ocync

extraVolumeMounts:
  - name: secrets-store
    mountPath: /mnt/secrets-store
    readOnly: true

envFrom:
  - secretRef:
      name: ocync-credentials
```

Both the CSI driver and the cloud-specific provider plugin (AWS, Azure, GCP, or Vault) must be installed. As with ExternalSecrets, the chart does not pre-check CRD presence.

**First-pod startup race.** The CSI driver populates the synced `Secret` (the one referenced by `envFrom`) only after the volume has mounted. `envFrom` resolves at container start, so on a brand-new pod the `Secret` may not exist yet and Kubernetes surfaces `CreateContainerConfigError`. The pod recovers automatically once the driver finishes the first sync (typically within seconds). This is expected behavior of the CSI Secrets Store Driver, not a chart bug; if you need deterministic ordering, populate the `Secret` out-of-band (or via ExternalSecrets) and have the driver merely refresh it.

## Workload Identity

For cloud registries (ECR, ECR Public, GAR, ACR), the cleanest path is **no Secret at all** -- bind the workload to a cloud IAM identity and let the SDK resolve credentials on each pod. The chart's `workloadIdentity` block sets the right ServiceAccount annotation (and pod label, for Azure) for your provider:

```yaml
# values.yaml -- AWS IRSA
workloadIdentity:
  provider: aws
  aws:
    roleArn: arn:aws:iam::123456789012:role/ocync-irsa
```

```yaml
# values.yaml -- GKE Workload Identity
workloadIdentity:
  provider: gcp
  gcp:
    serviceAccount: ocync@my-project.iam.gserviceaccount.com
```

```yaml
# values.yaml -- Azure AD Workload Identity (AKS)
workloadIdentity:
  provider: azure
  azure:
    clientId: 00000000-0000-0000-0000-000000000000
    tenantId: 11111111-1111-1111-1111-111111111111  # optional
```

The Azure provider sets both the SA annotation (`azure.workload.identity/client-id`) AND the pod label (`azure.workload.identity/use: "true"`). Without the pod label, the AAD mutating webhook does not inject the projected SA token and the credential chain falls through to managed identity / Azure CLI.

EKS Pod Identity is *not* represented in `workloadIdentity` because it is configured cluster-side via `PodIdentityAssociation` (not via the pod spec). Set up the association out-of-band and link it to the chart's ServiceAccount; no chart values are needed.

## AWS shared-config files

Use this pattern when one ECR registry needs credentials distinct from the ambient chain (see [ECR per-registry profile](./ecr#per-registry-static-credentials-third-party-access)). The `aws_profile` config field reads from a credentials file mounted at the path in `AWS_SHARED_CREDENTIALS_FILE`; this section covers two production-grade ways to populate that file.

The recommended secret-store layout is to store the entire INI blob -- including the `[profile-name]` header -- as a single string value at one key. This keeps the chart values minimal and avoids per-field templating.

### External Secrets Operator (AWS shared-config)

The third party's credentials are mirrored into AWS Secrets Manager, Vault, Azure Key Vault, GCP Secret Manager, or another store. ESO syncs to a native `Secret` that the chart mounts as a file:

```yaml
# values.yaml
externalSecrets:
  enabled: true
  refreshInterval: "1h"
  secretStoreRef:
    name: cluster-secret-store
    kind: ClusterSecretStore
  data:
    - secretKey: credentials
      remoteRef:
        key: vendor/aws-creds   # value: full INI blob, one string

extraVolumes:
  - name: aws-creds
    secret:
      secretName: my-release-ocync   # default ExternalSecret target
extraVolumeMounts:
  - name: aws-creds
    mountPath: /etc/aws
    readOnly: true
env:
  - name: AWS_SHARED_CREDENTIALS_FILE
    value: /etc/aws/credentials
```

Rotation handled upstream; audit trail in the source store; nothing plaintext in your repos.

If your secret store holds the access key and secret key as separate fields, flatten them upstream into a single INI blob value. The chart does not currently expose ESO's `target.template:` for in-cluster assembly; if you have a hard requirement to keep them separate at rest, open an issue describing the constraint.

### CSI Secrets Store driver (AWS shared-config)

For clusters where policy mandates the CSI driver and forbids long-lived `Secret` resources. Same source-of-truth as the ESO path, but the driver mounts directly from the cloud secret store:

```yaml
# values.yaml
secretProviderClass:
  enabled: true
  provider: aws  # | azure | gcp | vault
  parameters:
    objects: |
      - objectName: vendor/aws-creds
        objectType: secretsmanager
        objectAlias: credentials

extraVolumes:
  - name: aws-creds
    csi:
      driver: secrets-store.csi.k8s.io
      readOnly: true
      volumeAttributes:
        secretProviderClass: ocync   # release name (matches the existing CSI section above)
extraVolumeMounts:
  - name: aws-creds
    mountPath: /etc/aws
    readOnly: true
env:
  - name: AWS_SHARED_CREDENTIALS_FILE
    value: /etc/aws/credentials
```

The CSI race-on-startup behavior described in the [CSI Secrets Store](#csi-secrets-store) section above applies here too: the file may not exist for the first few seconds after a brand-new pod starts.

## Combining patterns

A typical mixed deployment (mirror from Docker Hub to ECR using IRSA + envFrom):

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

# IRSA for ECR (no Secret needed)
workloadIdentity:
  provider: aws
  aws:
    roleArn: arn:aws:iam::123456789012:role/ocync-irsa

# envFrom for Docker Hub PAT (created out-of-band or via ExternalSecrets)
envFrom:
  - secretRef:
      name: ocync-credentials
```

## What the chart does *not* render

- Native `Secret` resources. Encouraging secrets in `values.yaml` is an anti-pattern; the chart consumes Secrets but never creates them.
- SealedSecrets templates. SealedSecrets produces a normal `Secret`, which is picked up by `envFrom` from the first pattern.
- Vault Agent Injector annotations. Driven entirely by `podAnnotations` and `serviceAccount.annotations`; no chart-specific gating needed.
