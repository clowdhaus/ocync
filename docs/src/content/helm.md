---
title: Helm chart
description: Deploy ocync on Kubernetes with Deployment, CronJob, or Job mode using the official Helm chart.
order: 4
---

The `ocync` Helm chart supports three deployment modes, selected via the `mode` value.

## Installation

```bash
helm install ocync oci://public.ecr.aws/clowdhaus/ocync --version 0.1.0
```

## Deployment modes

| Mode | K8s resource | Use case |
|---|---|---|
| `watch` (default) | Deployment | Continuous sync with health endpoints |
| `cronjob` | CronJob | Scheduled sync every N minutes |
| `job` | Job | One-shot sync for CI or seeding |

## CronJob mode

```yaml
# values.yaml
mode: cronjob
cronjob:
  schedule: "*/15 * * * *"
  concurrencyPolicy: Forbid

image:
  repository: public.ecr.aws/clowdhaus/ocync
  tag: latest-fips

serviceAccount:
  create: true
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/ocync

resources:
  requests:
    cpu: 500m
    memory: 128Mi
    ephemeral-storage: 1Gi
  limits:
    memory: 256Mi
    ephemeral-storage: 2Gi

config:
  registries:
    chainguard:
      url: cgr.dev
    ecr:
      url: 123456789012.dkr.ecr.us-east-1.amazonaws.com
  target_groups:
    default: [ecr]
  defaults:
    source: chainguard
    targets: default
    tags:
      glob: "*"
      latest: 20
      sort: semver
  mappings:
    - from: chainguard/nginx
      to: nginx
```

The process is I/O-bound (single-threaded tokio runtime), not compute-bound. The `cpu: 500m` request gives the pod enough scheduling weight for Karpenter to steer toward network-optimized instances.

## Watch mode

```yaml
mode: watch
watch:
  interval: 300
  healthPort: 8080
```

Exposes `/healthz` (liveness) and `/readyz` (readiness) endpoints. See [observability](../observability) for logging configuration.

## Job mode

```yaml
mode: job
```

Runs a single sync and exits. Useful for CI pipelines or initial registry seeding.

## Authentication

The `ocync` container uses ambient credentials from the pod's environment, so there are no secrets to manage. The method depends on the Kubernetes platform.

### Amazon EKS

EKS supports two mechanisms for granting IAM credentials to pods. Both work with `ocync` -- choose based on your cluster's configuration.

**EKS Pod Identity** (recommended for new clusters):

```yaml
serviceAccount:
  create: true
```

Associate the service account with an IAM role using the [EKS Pod Identity Agent](https://docs.aws.amazon.com/eks/latest/userguide/pod-id-agent-setup.html):

```bash
aws eks create-pod-identity-association \
  --cluster-name my-cluster \
  --namespace default \
  --service-account ocync \
  --role-arn arn:aws:iam::123456789012:role/ocync
```

**IAM Roles for Service Accounts (IRSA)**:

```yaml
serviceAccount:
  create: true
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/ocync
```

In both cases, the IAM role needs permissions to pull from source and push to target repositories:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "EcrAuth",
      "Effect": "Allow",
      "Action": "ecr:GetAuthorizationToken",
      "Resource": "*"
    },
    {
      "Sid": "EcrPull",
      "Effect": "Allow",
      "Action": [
        "ecr:BatchGetImage",
        "ecr:GetDownloadUrlForLayer",
        "ecr:BatchCheckLayerAvailability"
      ],
      "Resource": "arn:aws:ecr:*:123456789012:repository/*"
    },
    {
      "Sid": "EcrPush",
      "Effect": "Allow",
      "Action": [
        "ecr:PutImage",
        "ecr:InitiateLayerUpload",
        "ecr:UploadLayerPart",
        "ecr:CompleteLayerUpload"
      ],
      "Resource": "arn:aws:ecr:*:123456789012:repository/*"
    }
  ]
}
```

Scope the `Resource` ARNs to specific repositories in production.

### Google GKE

GKE uses [Workload Identity Federation](https://cloud.google.com/kubernetes-engine/docs/how-to/workload-identity) to bind a Kubernetes service account to a Google Cloud service account:

```yaml
serviceAccount:
  create: true
  annotations:
    iam.gke.io/gcp-service-account: ocync@my-project.iam.gserviceaccount.com
```

Bind the Kubernetes service account to the GCP service account:

```bash
gcloud iam service-accounts add-iam-policy-binding \
  ocync@my-project.iam.gserviceaccount.com \
  --role roles/iam.workloadIdentityUser \
  --member "serviceAccount:my-project.svc.id.goog[default/ocync]"
```

The GCP service account needs `roles/artifactregistry.reader` on source repositories and `roles/artifactregistry.writer` on targets.

### Azure AKS

AKS uses [Workload Identity](https://learn.microsoft.com/en-us/azure/aks/workload-identity-overview) to federate a Kubernetes service account with an Azure managed identity:

```yaml
serviceAccount:
  create: true
  annotations:
    azure.workload.identity/client-id: <managed-identity-client-id>
  labels:
    azure.workload.identity/use: "true"
```

Create the federated credential:

```bash
az identity federated-credential create \
  --name ocync-federated \
  --identity-name ocync-identity \
  --resource-group my-rg \
  --issuer "$(az aks show -n my-cluster -g my-rg --query oidcIssuerProfile.issuerUrl -o tsv)" \
  --subject system:serviceaccount:default:ocync
```

Grant `AcrPush` on the target ACR and `AcrPull` on source ACR instances.

## Values reference

See the chart's [`values.yaml`](https://github.com/clowdhaus/ocync/blob/main/charts/ocync/values.yaml) for the full set of configurable values.
