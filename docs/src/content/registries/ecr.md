---
title: Amazon ECR (private)
description: Using ocync with private Amazon ECR via IAM auth, IRSA / EKS Pod Identity, and the BatchCheckLayerAvailability batch path.
order: 1
---

## Auth

ocync auto-detects ECR private from the hostname (`*.dkr.ecr.*.amazonaws.com`) and uses ambient AWS credentials via the AWS SDK. The credential chain follows AWS conventions:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`)
- Shared credentials file (`~/.aws/credentials`)
- Container/instance role (ECS task role, EC2 instance profile, Lambda execution role)
- IRSA or EKS Pod Identity (cluster-side identity binding)

No Docker config or static credentials are needed. See the [AWS credential precedence docs](https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html) for the full chain.

ocync-specific behaviors:

- **HTTP Basic, no Bearer exchange.** ECR private's SDK `GetAuthorizationToken` returns a base64-encoded `AWS:<password>` blob that the registry accepts directly as HTTP Basic credentials. ocync sends `Authorization: Basic <token>` rather than performing a `/v2/token` exchange.
- **FIPS endpoints.** Set `AWS_USE_FIPS_ENDPOINT=true` in the environment to route SDK calls through `*.dkr.ecr-fips.<region>.amazonaws.com`. See the [FIPS guide](../../fips) for crypto provider details.
- **Cross-repo blob mounting.** ocync mounts blobs server-side when `BLOB_MOUNTING=ENABLED` is set on the account *and* the source repo has a committed manifest that references the specific blob. Enable per-account: `aws ecr put-account-setting --name BLOB_MOUNTING --value ENABLED`. Mount POSTs that return 202 (not yet fulfilled) fall back to a normal upload.
- **Batch existence checks.** ocync uses `BatchCheckLayerAvailability` (up to 100 digests per call) instead of per-blob HEAD requests, reducing API call volume on cold syncs.
- **Per-action rate limits.** ECR enforces independent quotas per API action (`UploadLayerPart` 500 TPS, `InitiateLayerUpload` / `CompleteLayerUpload` 100 TPS, `PutImage` 10 TPS). ocync tracks 9 separate concurrency windows so a 429 on uploads does not throttle manifest reads.

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

ECR private is the canonical IRSA / EKS Pod Identity registry. The chart's `workloadIdentity.aws` block sets the IRSA annotation on the ServiceAccount:

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

For EKS Pod Identity, no chart change is required -- create the `PodIdentityAssociation` cluster-side and link it to the chart's ServiceAccount (`{{ release-name }}-ocync` by default).

Set `AWS_USE_FIPS_ENDPOINT=true` to route ECR calls through FIPS endpoints:

```yaml
env:
  - name: AWS_USE_FIPS_ENDPOINT
    value: "true"
```

For other secret-injection patterns (External Secrets Operator, CSI Secrets Store), see [Kubernetes secret patterns](./secrets).
