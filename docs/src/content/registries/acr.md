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
- **Streaming uploads.** ocync sends each blob to ACR as a POST and one streaming PUT, 2 requests per blob, the same path as ECR. ACR was once seen rejecting streaming uploads above ~20 MB with a connection reset or 413, but that did not reproduce on 2026-11-03 (see [Checked on a live registry](#checked-on-a-live-registry)). The `Location` header of the POST response is checked against the registry's full origin (scheme, host and port) so a compromised proxy cannot redirect the PUT, which carries the bearer token, to an attacker. Syncing 3 images (2.6 GB) into ACR from the AWS bench instance, median of 3 rounds: the chunked upload this replaced took 740 requests, peaked at 183 MB RSS and took 34s (2026-11-02); the streaming upload took 519 requests, peaked at 20 MB and took 34s (2026-11-03, run record `2026-11-03-221403` in `bench/results/acr.json`).
- Four rate-limit windows: ACR meters four groups separately, per identity per registry, on Basic and Standard (Premium doubles each, and quadruples referrers; [Microsoft's SKU limits](https://learn.microsoft.com/en-us/azure/container-registry/container-registry-skus)): reads (GET, HEAD) at 5,000 requests a minute, tag listing at 2,000, referrers queries at 250, and writes (PUT, PATCH, POST) at 1,000. Tag listing and referrers also count as reads, so each draws from its own bucket and from the read bucket. A client cannot see the tier, so ocync paces every group 20% under the Basic and Standard limits, about 67 reads, 27 tag lists, 3 referrers queries and 13 writes a second, and halves a window on a 429. A run signs in as one identity per registry, so all of its clients of one ACR host share those buckets.

## Checked on a live registry

On 2026-11-03, against a Standard-tier registry in eastus2, from an Azure VM in the same region:

- **Managed identity, end to end.** With no Azure environment variables set on the VM, `ocync auth check` and `ocync sync` signed in through the VM's system-assigned identity, which holds `AcrPush`. A second sync of `chainguard/nginx:latest`, run on its own into two new repositories that both mounted from the first sync's repositories, made 8 sign-in requests to Azure: 1 to the instance metadata service, 1 `POST /oauth2/exchange` for the refresh token, and 6 `POST /oauth2/token` for access tokens. Per target repository those were one `pull` token for the manifest check, one `pull,push` token, and one with `pull` on the mount source added.
- **Cross-repository mounts.** The second repository received all 24 blobs by mount, with no bytes uploaded. A second sync into two new repositories mounted every blob from the first two and uploaded nothing.
- **Request body limits.** None reached, from the VM or from the AWS bench instance. Every size tried was accepted: a single `PUT` with a declared length at 1 to 256 MiB, one with a streamed body (`Transfer-Encoding: chunked`, over HTTP/1.1) empty and at 8 MiB to 2 GiB, and a `PATCH` chunk at 16 to 64 MiB. Larger sizes were not tried. The registry accepted only HTTP/1.1 when offered HTTP/2, so every request to ACR uses HTTP/1.1.
- **Request quotas and 429s.** From the AWS bench instance, signed in as one identity, 20,000 manifest GETs at 256 in flight drew 2,965 429s, and 3,000 writes at 64 in flight drew 1,314. A 429 carries `Retry-After` in seconds (60, counting down) and a body naming the limit, for example `exceeded the per-identity DataplaneWrite rate limit of 1000 requests in a 60 second window`. ocync waits out `Retry-After` before retrying a registry request, up to 120 seconds per operation; a sign-in request (OAuth exchange or token) reports its 429 at once.

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
