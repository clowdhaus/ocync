---
title: Azure Container Registry (ACR)
description: Using ocync with Azure Container Registry via the AAD credential chain, sovereign clouds, and the AAD-to-ACR-refresh-to-ACR-access exchange.
order: 4
---

## Auth

ocync auto-detects ACR (`*.azurecr.io`, `*.azurecr.cn`, `*.azurecr.us`) and authenticates against ACR's proprietary OAuth2 endpoints (not standard OCI Bearer exchange) via a four-source Azure AD credential chain:

1. Client secret (`AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID`)
2. Workload Identity (the `AZURE_FEDERATED_TOKEN_FILE` projected token; AKS Workload Identity or any equivalent OIDC federation)
3. Managed Identity (`IDENTITY_ENDPOINT` / `IDENTITY_HEADER`; AKS pod-managed identity, App Service, Functions)
4. Azure CLI (`az login` -- developer machines)

See the [Azure DefaultAzureCredential docs](https://learn.microsoft.com/azure/developer/intro/authentication-overview#defaultazurecredential) for credential precedence; ocync's chain is extracted from `azure_identity` patterns with zero external dependencies.

ocync-specific behaviors:

- **Two-step OAuth2 exchange.** AAD access token -> `POST /oauth2/exchange` -> ACR refresh token (~3h JWT `exp`). Refresh token + scope -> `POST /oauth2/token` -> ACR access token (~75min JWT `exp`). Token TTLs are driven by the JWT `exp` claim, not a static default.
- **Sovereign cloud routing.** `*.azurecr.cn` uses `login.chinacloudapi.cn` and `containerregistry.azure.cn`; `*.azurecr.us` uses `login.microsoftonline.us` and `containerregistry.azure.us`. The hostname suffix determines both the AAD authority and the ACR resource endpoint.
- **No-redirect HTTP client.** ACR's `/oauth2/exchange` and `/oauth2/token` POSTs use a no-redirect reqwest client, matching upstream `azure_identity`'s global redirect-disable. This prevents accidental credential exposure to a redirected host.
- **Credential chain caches the winning source.** Once one of the four AAD credential sources succeeds, ocync caches its index and uses it directly for refresh. `invalidate()` resets the cached index, allowing a different source to win after credential rotation.
- **~20 MB streaming PUT body limit.** ACR rejects streaming uploads larger than ~20 MB with a connection reset or HTTP 413. Chunked PATCH fallback is not yet implemented; blobs exceeding ~20 MB cannot be pushed to ACR with the current ocync release.
- **Two rate-limit windows.** ACR enforces separate ReadOps and WriteOps quotas; ocync tracks them as two AIMD windows.

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
