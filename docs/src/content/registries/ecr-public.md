---
title: Amazon ECR Public
description: Using ocync with public.ecr.aws via the SDK GetAuthorizationToken to OCI Bearer exchange, or anonymous pulls.
order: 2
---

## Auth

ECR Public (`public.ecr.aws`) is a separate registry from ECR private with a different auth flow. ocync auto-detects it via the canonical hostname.

Authenticated path:

- ocync calls `ecr-public:GetAuthorizationToken` (always against `us-east-1`, ECR Public is single-region).
- The returned token is base64-decoded to extract the password half (`AWS:<password>`).
- That password is used as HTTP Basic credentials in the standard OCI `/v2/token` Bearer exchange.

Anonymous pulls work without any AWS credentials but have lower per-IP rate limits. Use authenticated access for any non-trivial sync workload.

ocync-specific behaviors:

- **Distinct from ECR private.** No `AuthType::EcrPublic` config variant exists; ECR Public is reachable only via auto-detection on the `public.ecr.aws` hostname. Setting `auth_type: ecr_public` in config is a parse error.
- **No `BatchCheckLayerAvailability`.** ECR Public uses per-blob HEAD requests via the OCI Distribution path, not the ECR SDK batch API. The ECR-private optimization at `synchronize.rs:257` does not apply here.
- **Standard Bearer flow.** Once the SDK-derived Basic credentials are exchanged for a Bearer token, ocync uses the same per-scope token cache and challenge cache as every other Bearer-issuing provider.
- **Lower rate limits than ECR private.** Read paths share a single window; write windows are separate but their caps are 10x lower than ECR private.

## CLI example

```bash
# Anonymous pull (works without AWS credentials but low rate limit).
ocync copy \
  public.ecr.aws/docker/library/alpine:latest \
  123456789012.dkr.ecr.us-east-1.amazonaws.com/alpine:latest
```

For authenticated pulls, ensure ambient AWS credentials are present (env vars, shared credentials, IRSA, etc.); ocync will pick them up automatically.

## Kubernetes deployment

ECR Public reuses the same AWS IRSA / Pod Identity surface as private ECR -- the SDK uses whatever AWS identity the workload has:

```yaml
# values.yaml
config:
  registries:
    src: { url: public.ecr.aws }
    dst: { url: 123456789012.dkr.ecr.us-east-1.amazonaws.com }
  defaults:
    source: src
    targets: dst
  mappings:
    - from: docker/library/alpine
      to: alpine

workloadIdentity:
  provider: aws
  aws:
    roleArn: arn:aws:iam::123456789012:role/ocync-irsa
```

For pure anonymous pulls (no AWS identity needed), omit `workloadIdentity` entirely. The pod will still be able to pull from `public.ecr.aws` but at the lower anonymous rate limit.

For other secret-injection patterns, see [Kubernetes secret patterns](./secrets).
