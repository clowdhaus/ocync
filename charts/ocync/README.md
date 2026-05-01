# ocync

OCI container image sync tool

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square)  ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square)  ![AppVersion: 0.1.0](https://img.shields.io/badge/AppVersion-0.1.0-informational?style=flat-square)

```bash
helm install ocync oci://public.ecr.aws/clowdhaus/ocync --version 0.1.0
```

See the [Helm chart documentation](https://clowdhaus.github.io/ocync/helm) and [`docs/src/content/registries/secrets.md`](../../docs/src/content/registries/secrets.md) for configuration, deployment modes, and the four supported secret-injection patterns.

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| Bryant Biggs |  | <https://github.com/bryantbiggs> |

## Source Code

* <https://github.com/clowdhaus/ocync>

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| affinity | object | See [values.yaml](./values.yaml) for the full instance-type list. | Pod affinity / anti-affinity. Defaults to preferring EC2 network-optimized instance types (`c5n` / `c6in` / `m5n` / `m6in` / `r5n` / `r6in` families, 25-200 Gbps). Soft preference -- pods schedule on any node if these are unavailable. Override with `affinity: {}` to disable. |
| config | object | `{}` (you must supply registries, defaults, and mappings) | ocync configuration YAML, rendered into a ConfigMap mounted at `/etc/ocync/config.yaml`. `global.cache_dir` is auto-injected as `/tmp/ocync-cache` if not set, so the transfer state cache and blob staging area land in the writable `/tmp` emptyDir. |
| cronJobAnnotations | object | `{}` | Annotations applied to the CronJob's `metadata.annotations` when `mode: cronjob`. Common uses: Helm lifecycle hooks (`helm.sh/hook`), Argo CD sync-wave / hook annotations, policy-controller selectors (Kyverno, Gatekeeper), cost-allocation tags. Pod-template annotations belong under `podAnnotations`. |
| cronjob.concurrencyPolicy | string | `"Forbid"` | CronJob concurrency policy: `Allow` | `Forbid` | `Replace`. |
| cronjob.failedJobsHistoryLimit | int | `3` | Number of failed CronJob runs to retain. |
| cronjob.schedule | string | `"0 */6 * * *"` | Cron schedule expression (cronjob mode only). |
| cronjob.successfulJobsHistoryLimit | int | `3` | Number of successful CronJob runs to retain. |
| deploymentAnnotations | object | `{}` | Annotations applied to the Deployment's `metadata.annotations` when `mode: watch`. Use for controllers that key off workload-level metadata: Stakater Reloader (`reloader.stakater.com/auto`), Argo Rollouts notification subscriptions, etc. Pod-template annotations belong under `podAnnotations`. |
| env | list | `[]` | Additional environment variables for the container. Set `AWS_USE_FIPS_ENDPOINT=true` to route ECR API calls through FIPS endpoints. |
| envFrom | list | `[]` | envFrom sources (Secret/ConfigMap references). Use for token-based auth: DOCKER_PASSWORD, GITHUB_TOKEN, chainctl tokens. The referenced resource must already exist or be created alongside (e.g. via `externalSecrets`). |
| externalSecrets.data | list | `[]` | Mapping of secret keys to remote references. Shape: `[{ secretKey, remoteRef: { key, property? } }]`. |
| externalSecrets.enabled | bool | `false` | Render an ExternalSecret resource. |
| externalSecrets.refreshInterval | string | `"1h"` | ExternalSecret refresh interval. |
| externalSecrets.secretStoreRef | object | `{}` | Reference to the SecretStore / ClusterSecretStore. Shape: `{ name, kind: SecretStore | ClusterSecretStore }`. |
| externalSecrets.target | object | `{}` | Target K8s Secret to create. Shape: `{ name?: string, creationPolicy?: Owner | Orphan }`. Defaults to chart fullname / `Owner`. |
| extraArgs | list | `[]` | Additional CLI arguments appended to the container args after the mode-specific defaults. |
| extraVolumeMounts | list | `[]` | Additional container volume mounts appended to volumeMounts. Pair each entry with an `extraVolumes` entry of the same name. |
| extraVolumes | list | `[]` | Additional pod volumes appended to the workload's volumes list. Use for docker-config.json from a Secret, CA bundles, GCP SA key files, or CSI-driven secret stores. |
| fullnameOverride | string | `""` | Override the fully-qualified release name (overrides `<release>-<chartname>`). |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy. |
| image.repository | string | `"public.ecr.aws/clowdhaus/ocync"` | Container image repository. |
| image.tag | string | `<chart-app-version>-fips` | Container image tag. |
| imagePullSecrets | list | `[]` | Image pull secrets for the workload pods. |
| jobAnnotations | object | `{}` | Annotations applied to the Job's `metadata.annotations` when `mode: job`. Common uses: Helm lifecycle hooks (`helm.sh/hook: post-install` etc.), Argo CD sync-wave / hook annotations on one-shot Jobs, policy-controller selectors. Pod-template annotations belong under `podAnnotations`. |
| mode | string | `"watch"` | Deployment mode: `watch` (Deployment with sync interval), `cronjob` (CronJob), or `job` (one-shot Job). |
| nameOverride | string | `""` | Override the chart name used in resource names and labels. |
| nodeSelector | object | `{}` | Node selector for the pod. |
| podAnnotations | object | `{}` | Pod-level annotations. Required for Vault Agent Injector (`vault.hashicorp.com/agent-inject`) and service-mesh sidecars (`linkerd.io/inject`, `sidecar.istio.io/inject`). |
| podLabels | object | `{}` | Pod-level labels. Composed with `workloadIdentity.podLabels` (the chart sets `azure.workload.identity/use: "true"` automatically when `workloadIdentity.provider` is `azure`). |
| podSecurityContext | object | `{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod security context. |
| resources | object | `{"limits":{"ephemeral-storage":"2Gi","memory":"256Mi"},"requests":{"cpu":"500m","ephemeral-storage":"1Gi","memory":"128Mi"}}` | Resource requests and limits. ocync is single-threaded (tokio current_thread) but network-bound during sync. CPU request influences Karpenter instance-type selection; 500m ensures the workload has enough weight to steer toward network-optimized nodes. Peak RSS observed at ~58 MB during full cold sync (42 images). |
| secretProviderClass.enabled | bool | `false` | Render a SecretProviderClass resource. |
| secretProviderClass.parameters | object | `{}` | Provider-specific parameters (free-form map). |
| secretProviderClass.provider | string | `""` | Provider plugin: `aws` | `azure` | `gcp` | `vault`. |
| secretProviderClass.secretObjects | list | `[]` | Optional K8s Secret sync targets. Shape: `[{ secretName, type, data: [{ objectName, key }] }]`. |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container security context. |
| serviceAccount.annotations | object | `{}` | Annotations to add to the ServiceAccount. Composed with `workloadIdentity` (user-set keys win). |
| serviceAccount.create | bool | `true` | Whether the chart should create a ServiceAccount for the workload. |
| serviceAccount.name | string | `""` | Override the ServiceAccount name (defaults to the chart fullname). |
| tolerations | list | `[]` | Tolerations for the pod. |
| watch.healthPort | int | `8080` | Health check port (watch mode only). Exposed via Service and used by liveness/readiness probes. |
| watch.interval | int | `300` | Sync interval in seconds (watch mode only). |
| workloadIdentity.aws.roleArn | string | `""` | IAM role ARN to assume via IRSA. Sets `eks.amazonaws.com/role-arn` on the ServiceAccount. |
| workloadIdentity.azure.clientId | string | `""` | AAD app client ID. Sets `azure.workload.identity/client-id` on the ServiceAccount AND `azure.workload.identity/use: "true"` on the pod (the latter is the critical pod label without which the AAD webhook does not inject the projected SA token). |
| workloadIdentity.azure.tenantId | string | `""` | AAD tenant ID (optional). When set, adds `azure.workload.identity/tenant-id` to the ServiceAccount. |
| workloadIdentity.gcp.serviceAccount | string | `""` | GSA email for GKE Workload Identity. Sets `iam.gke.io/gcp-service-account` on the ServiceAccount. |
| workloadIdentity.provider | string | `"none"` | Workload identity provider: `none` | `aws` | `gcp` | `azure`. |

----------------------------------------------
Autogenerated from chart metadata using [helm-docs v1.14.2](https://github.com/norwoodj/helm-docs/releases/v1.14.2)
