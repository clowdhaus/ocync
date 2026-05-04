---
title: Helm chart and images
description: Mirror an operator's Helm chart together with its dependent container images. ArgoCD as the canonical example.
order: 4
---

Operators are typically distributed as a Helm chart plus several container images that the chart references. To run the operator from your mirror, both have to land at the target. List the chart and each image as separate mappings.

## When to use

- Mirroring a Helm-distributed operator whose chart is published as an OCI artifact
- Air-gapped Kubernetes clusters that need both chart and images locally
- Multi-source mirrors where the chart and images live in different upstream registries

## Config

ArgoCD as the canonical example - the chart on `ghcr.io`, the controller image on `quay.io`, both fanning out to ECR:

```yaml
registries:
  ghcr:
    url: ghcr.io
  quay:
    url: quay.io
  ecr:
    url: ${AWS_ACCOUNT_ID}.dkr.ecr.us-east-1.amazonaws.com

defaults:
  targets: ecr

mappings:
  # Chart artifact (single OCI artifact, not multi-arch)
  - from: argoproj/argo-helm/argo-cd
    source: ghcr
    to: argo/argo-cd
    tags:
      glob: ["7.6.0"]

  # Controller image (multi-arch index, preserved bit-for-bit).
  # ApplicationSet, the Notifications controller, and the Repo Server
  # are all bundled into this single image since ArgoCD v2.5; older
  # split-image charts referenced them separately.
  - from: argoproj/argocd
    source: quay
    to: argo/argocd
    tags:
      glob: ["v2.13.0"]
```

## Variations

The same shape applies to any operator that publishes its Helm chart as an OCI artifact alongside its container images. A few examples:

- **cert-manager**: chart `quay.io/jetstack/charts/cert-manager` plus images `quay.io/jetstack/cert-manager-controller`, `quay.io/jetstack/cert-manager-webhook`, `quay.io/jetstack/cert-manager-cainjector`
- **Karpenter**: chart `public.ecr.aws/karpenter/karpenter` plus image `public.ecr.aws/karpenter/controller`

Operators whose Helm charts are still distributed only via classic Helm repositories (HTTP `index.yaml`, e.g. AWS Load Balancer Controller and the AWS EBS CSI driver from `aws.github.io/eks-charts`) are out of scope for `ocync` - their container images can be mirrored, but the charts need a Helm-repo mirror. Mirror the images with `ocync` and serve the chart from a separate Helm repository in those cases.

## Caveats

`ocync` mirrors what you tell it to mirror. It does not parse Helm chart values to discover and follow image references; you list the chart and the images yourself.

That means the mirrored chart's `values.yaml` still references the upstream image registry after sync. Consumers have to override `image.repository` at install time:

```bash
helm install argocd <your-mirror>/argo/argo-cd \
  --set global.image.repository=<your-mirror>/argo/argocd
```

This is the bit-for-bit promise in action. The chart at your mirror is byte-identical to the source; auto-rewriting registry references would change the chart digest and break signature verification. The design tradeoff is tracked in [issue #80](https://github.com/clowdhaus/ocync/issues/80).

## Related

- [Production mirror](/recipes/production-mirror) for the full-fidelity defaults
- [Issue #80](https://github.com/clowdhaus/ocync/issues/80) for the registry URL rewriting investigation
