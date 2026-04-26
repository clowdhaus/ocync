---
title: Amazon ECR
description: Using ocync with Amazon ECR for IAM auth, FIPS endpoints, cross-repo blob mounting, and batch optimizations.
order: 1
---

## Auth

ECR uses IAM credentials via the AWS SDK. `ocync` auto-detects ECR from the hostname pattern (`*.dkr.ecr.*.amazonaws.com`) and uses your ambient AWS credentials:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- AWS config file (`~/.aws/config`)
- Instance/task role (EC2, ECS, Lambda)
- EKS Pod Identity or IRSA (EKS service account)

No Docker config or static credentials needed.

ECR private uses HTTP Basic auth (not Bearer token exchange). The AWS SDK manages credentials directly.

## FIPS endpoints

Set `AWS_USE_FIPS_ENDPOINT=true` to use FIPS endpoints:

```
123456789012.dkr.ecr.us-east-1.amazonaws.com
  -> 123456789012.dkr.ecr-fips.us-east-1.amazonaws.com
```

See the [FIPS guide](../../fips) for crypto details.

## Cross-repo blob mounting

ECR supports cross-repo blob mounting when `BLOB_MOUNTING=ENABLED` on the account. When a blob exists in another repo on the same registry with a committed manifest, `ocync` mounts it server-side instead of uploading.

To enable:

```bash
aws ecr put-account-setting --name BLOB_MOUNTING --value ENABLED
```

`ocync`'s leader-follower algorithm ensures shared blobs are uploaded exactly once. Images are elected as leaders using a greedy set-cover algorithm; each leader uploads all its blobs and commits its manifest. Followers mount shared blobs from leader repositories, with mount sources restricted to repos that have committed manifests.

## Batch blob existence checks

`ocync` uses ECR's `BatchCheckLayerAvailability` API to check multiple blobs in a single request (up to 100 digests per call), reducing API call volume.

## Rate limits

ECR has per-action rate limits (e.g., `PutImage` at ~10 TPS vs `UploadLayerPart` at ~500 TPS). `ocync` tracks 9 independent concurrency windows -- one per API action -- so a 429 on blob uploads does not throttle manifest reads. These per-action limits are enforced pre-emptively by ocync's token-bucket layer; specific rate values are catalogued in the [rate-bucket spec](../../../superpowers/specs/2026-04-26-aimd-rate-bucket-design.md).

## ECR Public

ECR Public (`public.ecr.aws`) is detected separately via `ProviderKind::EcrPublic`. Anonymous pulls work but have lower rate limits. For authenticated access, configure credentials via your Docker config file (e.g., using `docker-credential-ecr-login`) or provide a static token. `ocync` uses the standard OCI token exchange flow for ECR Public, not the IAM-based Basic auth used by ECR private.

## Example config

```yaml
registries:
  chainguard:
    url: cgr.dev
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: chainguard
  targets: ecr

mappings:
  - from: chainguard/nginx
    to: nginx
```
