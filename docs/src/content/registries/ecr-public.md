---
title: Amazon ECR Public
description: Using ocync with public.ecr.aws via SDK auth or anonymous pulls.
order: 2
---

## Auth

ECR Public (`public.ecr.aws`) is a separate registry from ECR private with a different auth flow. ocync auto-detects it via the canonical hostname.

Authenticated path:

- ocync calls `ecr-public:GetAuthorizationToken` (always against `us-east-1`; ECR Public is single-region).
- The returned token is base64-decoded to extract the password half (`AWS:<password>`).
- That password is used as HTTP Basic credentials in the standard OCI `/v2/token` Bearer exchange.

Anonymous pulls work without AWS credentials but have lower per-IP rate limits: AWS documents unauthenticated pulls at 1 per second and not adjustable, against 10 per second authenticated. Use authenticated access for any non-trivial sync workload.

Authenticating does not buy an exemption on tag listing, though. See below.

Notable behaviors:

- No `auth_type` value exists for ECR Public; it is reachable only via auto-detection on `public.ecr.aws`. Setting `auth_type: ecr_public` is a parse error.
- No `BatchCheckLayerAvailability`. ECR Public uses per-blob HEADs via the OCI Distribution path, not the ECR SDK batch API.
- Lower rate limits than ECR private. Read paths share a single window; write window caps are 10x lower than ECR private.
- **Tag listing throttles below the documented pull rate.** Measured 2026-08-28: a 95-mapping `--dry-run` against `public.ecr.aws` with AWS credentials loaded drew repeated 429s on `TagList`, halving the AIMD window from 11 to 6 to 3, while ocync's token bucket for ECR Public reads was pacing at 8 requests per second. AWS publishes no TPS quota for tag listing at all; the 8 is derived from the 10 TPS authenticated *pull* quota, and the read window groups tag listing in with pulls. Treat the read pacing for this registry as measured rather than documented.
- Because of the above, `list_tags` retries a 429 rather than surfacing it. Before that, each throttle failed a whole mapping: the same run lost 5 mappings and exited 1. With the retry it exits 0 and loses none.

## CLI example

```bash
# Anonymous pull (works without AWS credentials but low rate limit).
ocync copy \
  public.ecr.aws/docker/library/alpine:latest \
  123456789012.dkr.ecr.us-east-1.amazonaws.com/alpine:latest
```

For authenticated pulls, ensure ambient AWS credentials are present (env vars, shared credentials, IRSA, etc.); ocync will pick them up automatically.

## Kubernetes deployment

Anonymous pulls require no AWS identity, so the simplest pod has no `workloadIdentity` block at all -- accept the lower per-IP rate limit. For authenticated reads (higher rate limit), the SDK uses whatever AWS identity the workload has, exactly as for ECR private. See [ECR Kubernetes deployment](/registries/ecr#kubernetes-deployment) for the EKS Pod Identity and IRSA setups; both apply unchanged.
