---
title: Azure Container Registry
description: Using ocync with ACR via the AAD credential chain and proprietary OAuth2 exchange.
order: 4
---

## Auth

ocync auto-detects ACR (`*.azurecr.io`, `*.azurecr.cn`, `*.azurecr.us`) and authenticates against ACR's proprietary OAuth2 endpoints (not standard OCI Bearer exchange) via a four-source Azure AD credential chain:

1. Client secret (`AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID`)
2. Workload Identity (`AZURE_FEDERATED_TOKEN_FILE`; AKS Workload Identity or any OIDC federation)
3. Managed Identity (asks the instance metadata service at `169.254.169.254` directly, never through a proxy; an Azure VM or node with a system-assigned identity or a single user-assigned one, since no client ID is sent)
4. Azure CLI (`az login`; developer machines)

See the [Azure DefaultAzureCredential docs](https://learn.microsoft.com/en-us/dotnet/azure/sdk/authentication/credential-chains) for credential precedence.

Notable behaviors:

- Two-step OAuth2 exchange: AAD access token -> `POST /oauth2/exchange` -> ACR refresh token (~3h) -> `POST /oauth2/token` with scope -> ACR access token (~75min). TTLs are driven by the JWT `exp` claim.
- Sovereign cloud routing: `*.azurecr.cn` uses `login.chinacloudapi.cn`; `*.azurecr.us` uses `login.microsoftonline.us`. The hostname suffix selects both the AAD authority and the ACR resource endpoint.
- **Chunked uploads.** ACR was seen rejecting streaming uploads above ~20 MB with a connection reset or 413. That did not reproduce on 2026-11-03 (see [Checked on a live registry](#checked-on-a-live-registry)). ocync detects `*.azurecr.io` / `*.azurecr.cn` / `*.azurecr.us` hosts and routes blob pushes through a chunked-PATCH fallback (`blob_push_stream_acr`): each 16 MiB PATCH is sent as its chunk is read (Content-Range in the OCI `{start}-{end}` form, not RFC 7233), and the digest is verified once the stream ends and before the finalising PUT. A digest mismatch or a stream error sends no PUT, so nothing is committed, and the registry expires the abandoned session. While a PATCH is in flight the next chunk is read from the source, which keeps a network source from sitting unread for the length of a PATCH. A blob staged to disk is read from local disk at once, so its PATCHes run one after another. Each PATCH response's `Location` header is checked against the registry's full origin (scheme, host and port) so a compromised proxy cannot redirect later chunks (and the bearer-bearing finalize PUT) to an attacker. Measured on 2026-11-02, syncing 3 images (2.6 GB, 740 requests) into ACR from the AWS bench instance, median of 3 rounds: buffering each whole blob first peaked at 545 MB RSS and took 1m 2s; sending each chunk as it is read peaked at 183 MB and took 34s, with the same requests and bytes.
- Two rate-limit windows: ACR has separate ReadOps and WriteOps quotas, and ocync tracks them as two AIMD windows. Microsoft no longer publishes the per-tier numbers, so ocync starts both windows at the last published Premium-tier rates and halves on the first 429 from a lower tier. No live run has reached either quota.

## Checked on a live registry

On 2026-11-03, against a Standard-tier registry in eastus2, from an Azure VM in the same region:

- **Managed identity, end to end.** With no Azure environment variables set on the VM, `ocync auth check` and `ocync sync` signed in through the VM's system-assigned identity, which holds `AcrPush`. A second sync of `chainguard/nginx:latest`, run on its own into two new repositories that both mounted from the first sync's repositories, made 8 sign-in requests to Azure: 1 to the instance metadata service, 1 `POST /oauth2/exchange` for the refresh token, and 6 `POST /oauth2/token` for access tokens. Per target repository those were one `pull` token for the manifest check, one `pull,push` token, and one with `pull` on the mount source added.
- **Cross-repository mounts.** The second repository received all 24 blobs by mount, with no bytes uploaded. A second sync into two new repositories mounted every blob from the first two and uploaded nothing.
- **Request body limits.** None reached. Every size tried was accepted: a single `PUT` with a declared length at 1 to 256 MiB, one with a streamed body (`Transfer-Encoding: chunked`, over HTTP/1.1) at 8 to 64 MiB, and a `PATCH` chunk at 16 to 32 MiB. Larger sizes were not tried. The registry accepted only HTTP/1.1 when offered HTTP/2, so every request to ACR uses HTTP/1.1.
- **Request quotas and 429s** were not reached and remain unchecked.

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

On AKS, use Azure AD Workload Identity. The chart's `workloadIdentity.azure` block sets both the ServiceAccount annotations (`azure.workload.identity/client-id` and optionally `tenant-id`) AND the critical pod label `azure.workload.identity/use: "true"` -- without the pod label, the AAD webhook does not inject the projected SA token, and the credential chain falls through to managed identity. With no client ID sent, the metadata service answers for the node's system-assigned identity, or its only user-assigned one. On AKS that identity typically holds only `AcrPull`, so pushes are refused. A node with several user-assigned identities and no system-assigned one gets no token, and with no Azure CLI in the pod the sign-in fails.

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

For other secret-injection patterns, see [Kubernetes secret patterns](/registries/secrets).
