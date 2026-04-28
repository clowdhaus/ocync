---
title: Azure Container Registry (ACR)
description: Using ocync with ACR via the AAD credential chain and proprietary OAuth2 exchange.
order: 4
---

## Auth

ocync auto-detects ACR (`*.azurecr.io`, `*.azurecr.cn`, `*.azurecr.us`) and authenticates against ACR's proprietary OAuth2 endpoints (not standard OCI Bearer exchange) via a four-source Azure AD credential chain:

1. Client secret (`AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID`)
2. Workload Identity (`AZURE_FEDERATED_TOKEN_FILE`; AKS Workload Identity or any OIDC federation)
3. Managed Identity (`IDENTITY_ENDPOINT` / `IDENTITY_HEADER`; AKS pod-managed identity, App Service, Functions)
4. Azure CLI (`az login`; developer machines)

See the [Azure DefaultAzureCredential docs](https://learn.microsoft.com/azure/developer/intro/authentication-overview#defaultazurecredential) for credential precedence.

Notable behaviors:

- Two-step OAuth2 exchange: AAD access token -> `POST /oauth2/exchange` -> ACR refresh token (~3h) -> `POST /oauth2/token` with scope -> ACR access token (~75min). TTLs are driven by the JWT `exp` claim.
- Sovereign cloud routing: `*.azurecr.cn` uses `login.chinacloudapi.cn`; `*.azurecr.us` uses `login.microsoftonline.us`. The hostname suffix selects both the AAD authority and the ACR resource endpoint.
- **~20 MB streaming PUT body limit.** ACR rejects streaming uploads above ~20 MB with a connection reset or 413; chunked PATCH fallback is not yet implemented. Blobs exceeding this limit cannot currently be pushed to ACR.
- Two rate-limit windows: ACR enforces separate ReadOps and WriteOps quotas; ocync tracks them as two AIMD windows.

## CLI example

```bash
# After `az login`, push from Chainguard to ACR. ocync uses the Azure CLI
# credential source from the chain.
ocync copy \
  cgr.dev/chainguard/static:latest \
  myregistry.azurecr.io/static:latest
```

Sync-mode:

```yaml
# ocync.yaml
registries:
  src: { url: cgr.dev }
  acr: { url: myregistry.azurecr.io }

defaults:
  source: src
  targets: acr

mappings:
  - from: chainguard/static
    to: static
```

```bash
ocync sync --config ocync.yaml
```

## Kubernetes deployment

On AKS, use Azure AD Workload Identity. The chart's `workloadIdentity.azure` block sets both the ServiceAccount annotations (`azure.workload.identity/client-id` and optionally `tenant-id`) AND the critical pod label `azure.workload.identity/use: "true"` -- without the pod label, the AAD webhook does not inject the projected SA token, and the credential chain falls through to managed identity or Azure CLI (which won't be available in a pod).

```yaml
# values.yaml
config:
  registries:
    src: { url: cgr.dev }
    acr: { url: myregistry.azurecr.io }
  defaults:
    source: src
    targets: acr
  mappings:
    - from: chainguard/static
      to: static

workloadIdentity:
  provider: azure
  azure:
    clientId: 00000000-0000-0000-0000-000000000000
    tenantId: 11111111-1111-1111-1111-111111111111  # optional
```

For sovereign clouds, the AAD authority is selected automatically from the registry hostname suffix; the chart values are unchanged. Configuring the AAD app, federated identity credential, and ACR `AcrPull` role assignment is the user's IAM/AAD work; see [Azure Workload Identity docs](https://azure.github.io/azure-workload-identity/docs/).

For other secret-injection patterns, see [Kubernetes secret patterns](./secrets).
