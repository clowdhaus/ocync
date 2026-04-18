---
title: Helm chart
description: Deploy ocync on Kubernetes with Deployment, CronJob, or Job mode using the official Helm chart.
order: 4
---

The ocync Helm chart supports three deployment modes, selected via the `mode` value.

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

## CronJob mode (recommended)

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
    cpu: 100m
    memory: 128Mi
  limits:
    memory: 512Mi

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

The single-threaded runtime maps directly to `cpu: 100m` because the process is I/O-bound, not compute-bound.

## Watch mode (Deployment)

```yaml
mode: watch
watch:
  interval: 300
  healthPort: 8080
```

Exposes `/healthz` (liveness) and `/readyz` (readiness) endpoints. See [observability](../observability) for logging configuration. Supports SIGHUP for config reload without restart.

## Job mode

```yaml
mode: job
job:
  backoffLimit: 3
```

Runs a single sync and exits. Useful for CI pipelines or initial registry seeding.

## Auth via IRSA

For EKS, use IAM Roles for Service Accounts (IRSA) to grant ECR access:

```yaml
serviceAccount:
  create: true
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/ocync
```

The ocync container uses ambient AWS credentials, so there are no secrets to manage.

## Values reference

See the chart's [`values.yaml`](https://github.com/clowdhaus/ocync/blob/main/charts/ocync/values.yaml) for the full set of configurable values.
