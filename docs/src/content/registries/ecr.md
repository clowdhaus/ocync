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
- IRSA (EKS service account)

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

`ocync`'s leader-follower algorithm ensures shared blobs are uploaded exactly once. One image is elected leader for each shared blob and performs the actual upload; all other images (followers) wait and then mount from the leader's repository.

## Batch blob existence checks

`ocync` uses ECR's `BatchCheckLayerAvailability` API to check multiple blobs in a single request (up to 100 digests per call), reducing API call volume.

## Rate limits

ECR has per-action rate limits. `ocync` tracks 9 independent [AIMD](../../design/overview#adaptive-concurrency-aimd) (additive increase, multiplicative decrease) concurrency windows, using the same algorithm TCP uses for congestion control:

- Manifest GET/PUT/HEAD
- Blob GET/HEAD/POST/PUT/PATCH
- Tag list

Each window adapts independently, so a 429 on blob uploads does not throttle manifest reads.

## ECR Public

ECR Public (`public.ecr.aws`) is detected separately via `ProviderKind::EcrPublic`. Anonymous pulls work, but authenticated pulls via IAM get higher rate limits. The `docker-credential-ecr-login` helper supports public ECR -- it calls `ecr-public:GetAuthorizationToken` + `sts:GetServiceBearerToken`. The managed policy `AmazonElasticContainerRegistryPublicReadOnly` grants these permissions.

## Example config

```yaml
registries:
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

mappings:
  - from: source/nginx
    to: ecr/nginx
```
