---
title: Amazon ECR
description: Using ocync with private Amazon ECR via IAM credentials, IRSA, and EKS Pod Identity.
order: 1
---

## Auth

ocync auto-detects ECR private from the hostname (`*.dkr.ecr.*.amazonaws.com`) and uses ambient AWS credentials via the AWS SDK. The credential chain follows AWS conventions:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`)
- Shared credentials file (`~/.aws/credentials`)
- Container or instance role (ECS task role, EC2 instance profile, Lambda execution role)
- IRSA or EKS Pod Identity (cluster-side identity binding)

No Docker config or static credentials are needed. See the [AWS credential precedence docs](https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html) for the full chain.

Notable behaviors:

- HTTP Basic, not Bearer exchange. SDK `GetAuthorizationToken` returns a base64-encoded `AWS:<password>` blob that the registry accepts directly; ocync sends `Authorization: Basic <token>` and skips the `/v2/token` round-trip.
- FIPS endpoints: set `AWS_USE_FIPS_ENDPOINT=true` to route SDK calls through `*.dkr.ecr-fips.<region>.amazonaws.com`. See the [FIPS guide](../../fips) for crypto provider details.
- Cross-repo blob mounting requires `BLOB_MOUNTING=ENABLED` on the account *and* a committed manifest in the source repo that references the specific blob. Enable per-account: `aws ecr put-account-setting --name BLOB_MOUNTING --value ENABLED`. Mount POSTs returning 202 (not yet fulfilled) fall back to a normal upload.
- Batch existence checks use `BatchCheckLayerAvailability` (up to 100 digests per call) instead of per-blob HEADs, reducing API volume on cold syncs.
- Per-action rate limits: ECR enforces independent quotas per API action (`UploadLayerPart` 500 TPS, `InitiateLayerUpload` / `CompleteLayerUpload` 100 TPS, `PutImage` 10 TPS). ocync tracks 9 separate AIMD windows so a 429 on uploads does not throttle manifest reads.

## CLI example

```bash
# Pull from Chainguard (anonymous), push to your private ECR. Uses
# whatever AWS credentials are in scope; no docker login required.
ocync copy \
  cgr.dev/chainguard/static:latest \
  123456789012.dkr.ecr.us-east-1.amazonaws.com/static:latest
```

For sync-mode (multi-image, config-driven):

```yaml
# ocync.yaml
registries:
  src:
    url: cgr.dev
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: src
  targets: ecr

mappings:
  - from: chainguard/static
    to: static
```

```bash
ocync sync --config ocync.yaml
```

## Kubernetes deployment

Two AWS-IAM paths are supported on EKS. Prefer EKS Pod Identity for new clusters; fall back to IRSA on clusters that have not adopted the Pod Identity Agent.

### EKS Pod Identity

The successor to IRSA. Configured cluster-side via a `PodIdentityAssociation` resource that binds the chart's ServiceAccount to an IAM role; no pod-spec annotation, no chart change. Requires the [`eks-pod-identity-agent`](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html) addon on the cluster, and a trust policy on the IAM role allowing `pods.eks.amazonaws.com`.

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev }
    ecr: { url: 123456789012.dkr.ecr.us-east-1.amazonaws.com }
  defaults:
    source: src
    targets: ecr
  mappings:
    - from: chainguard/static
      to: static
```

Create the association out-of-band (the chart's default ServiceAccount is `<release-name>-ocync`):

```bash
aws eks create-pod-identity-association \
  --cluster-name my-cluster \
  --namespace ocync \
  --service-account my-release-ocync \
  --role-arn arn:aws:iam::123456789012:role/ocync-pod-identity
```

### IAM Roles for Service Accounts (IRSA)

The original EKS workload-identity mechanism. The chart's `workloadIdentity.aws.roleArn` adds the `eks.amazonaws.com/role-arn` annotation to the ServiceAccount; the AWS SDK uses the projected OIDC token to assume the role.

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev }
    ecr: { url: 123456789012.dkr.ecr.us-east-1.amazonaws.com }
  defaults:
    source: src
    targets: ecr
  mappings:
    - from: chainguard/static
      to: static

workloadIdentity:
  provider: aws
  aws:
    roleArn: arn:aws:iam::123456789012:role/ocync-irsa
```

The IAM role's trust policy must allow the cluster's OIDC provider to federate the ServiceAccount.

### FIPS endpoints

Set `AWS_USE_FIPS_ENDPOINT=true` to route ECR calls through `*.dkr.ecr-fips.<region>.amazonaws.com`:

```yaml
env:
  - name: AWS_USE_FIPS_ENDPOINT
    value: "true"
```

For other secret-injection patterns (External Secrets Operator, CSI Secrets Store), see [Kubernetes secrets](./secrets).
