# ocync-distribution

OCI Distribution Specification client library - registry auth, blob/manifest transfer, and provider-specific protocol handling.

## Auth protocol

- ECR private uses HTTP Basic auth (not Bearer token exchange). ECR's `GetAuthorizationToken` (called through the owned client in `src/aws/`) returns a pre-encoded base64 token used directly.
- ECR Public uses its `GetAuthorizationToken` -> decode base64 -> OCI Bearer token exchange with those credentials. Those tokens are NOT valid as direct Bearer tokens; they must drive standard `/v2/token` exchange.
- Both ECR providers, the Azure and Google providers, and the AWS API client cache credentials via `CredentialCache<T>` in `credential_cache.rs` (generic read-lock fast path / write-lock + double-check). New ECR-style providers must use this cache, not hand-roll the RwLock pattern.
- GAR/GCR resolves Application Default Credentials with the owned resolver in `src/google/` (`devstorage.read_write` scope) -> `oauth2accesstoken:<token>` Basic creds -> `token_exchange::exchange()` -> Bearer token. Same flow as ECR Public. Auto-detected via `ProviderKind::Gar`/`Gcr`, but only when the run has Google credentials: a credentials file, else the metadata server if the machine is on Google Cloud (`Residency`, asked once per process). With neither, `GcpAuth::detected` returns `None` and the host is read as any other, with docker config, else anonymously. A source that exists but fails is an error, never an anonymous read. `auth_type: gar` or `gcr` skips the Google Cloud question (`GcpAuth::new`: file, else metadata server). The Google token is kept for its real lifetime from `expires_in` or `expireTime`, one per run shared by every GAR and GCR host through `SharedGoogleToken`. Implementation in `auth/gcp.rs`.
- ACR uses a proprietary OAuth2 flow, NOT standard OCI token exchange. Azure AD credential chain (client secret, workload identity, managed identity, Azure CLI -- extracted from `azure_identity` patterns, zero deps) -> `POST /oauth2/exchange` (AAD token -> ACR refresh token, ~3h TTL from JWT `exp` claim) -> `POST /oauth2/token` (refresh token + scope -> ACR access token, ~75min TTL from JWT `exp` claim). Exchange POSTs use a no-redirect HTTP client (`no_redirect_http_client()`) matching upstream `azure_identity` which disables redirects globally. Credential chain caches winning source index, reset when an exchange refuses the AAD token or the kept refresh token (exceeds upstream). Error bodies truncated to 200 chars (exceeds upstream). Auto-detected via `ProviderKind::Acr`. Implementation in `auth/acr.rs`.
- New cloud auth providers follow the ECR Public pattern: SDK credential -> Basic creds -> `token_exchange::exchange()` -> Bearer token.
- Challenge caching: `ChallengeCache` in `auth/token_exchange.rs` stores the parsed `WWW-Authenticate` realm+service so subsequent token exchanges skip the `/v2/` ping. Every Bearer provider (`configured::Auth`, ECR Public, Google) uses it through `TokenCache`. It is cleared only when a registry refusal matches a cached token.
- Realm URL validation: `validate_realm_url()` in `token_exchange.rs` validates realm URLs before sending credentials. Four layers: structural (scheme, userinfo, host), IP denylist (link-local, cloud metadata, unspecified, localhost, conditional loopback, IPv4-translated/NAT64), no-redirect client, domain binding (realm host must match or share parent domain with registry). Runs on both fresh and cached challenges.
- Parse `WWW-Authenticate` header dynamically; never hardcode token exchange endpoints.
- Token caching: `EARLY_REFRESH_WINDOW` = 30s. Docker Hub issues 300s tokens; a 15m window was a bug that bypassed the cache entirely.
- Per-scope tokens: format `repository:<name>:<actions>` where actions = `pull`, `push`, or `pull,push`. Per-scope caching (`scopes_cache_key` in `auth/mod.rs`) is universal across every Bearer-issuing provider (anonymous, basic, docker-config, ecr-public, gcp, acr). It is NOT a Chainguard-specific feature.
- cgr.dev's specific quirk is *enforcement*: it returns 403 on cross-scope token reuse where some registries silently accept it. The scope-keyed cache is what makes ocync correct against this enforcement -- but the cache itself exists for every Bearer flow.
- Per-scope coalescing: every Bearer provider routes its cache through `TokenCache` in `auth/token_cache.rs`. The helper holds the cache mutex only for brief reads and writes; the exchange runs as one flight per scope (`Flights` in `flight.rs`), so concurrent fetches for the same scope coalesce to one exchange while distinct scopes proceed in parallel. The helper owns the contract; per-provider tests verify wiring, not the contract.

## Provider dispatch (auto-detection)

With `auth_type` unset, `cgr.dev`, `ghcr.io`, `docker.io`, and any unrecognized hostname all share one default path: docker-config first, anonymous fallback. `docker login <host>` (or `chainctl auth login` for cgr.dev paid tags) is the supported way to supply credentials when `auth_type` is not set explicitly. A Google host (`*-docker.pkg.dev`, `gcr.io` and its mirrors) takes the same default path when the run has no Google credentials file and the machine is not on Google Cloud; with either, it gets the Google provider (see Auth protocol). The dispatch is in `src/cli/mod.rs`.

## Registry detection

- Always use `detect_provider_kind()` + `ProviderKind` enum.
- Never match raw hostnames; detection logic is centralized in `auth/detect.rs`.

## AIMD concurrency controller

- Per-(registry, action) AIMD windows, not per-host. `WindowKey` is a typed enum -- one variant per (provider, action-group) pair.
- AIMD congestion epochs: 100ms epoch prevents cascade collapse from burst 429s. Halving is only half the 429 response; see "Retry" below for the other half.
- ECR Public's shared read window includes `TagList`, and its bucket value (8 TPS) is derived from AWS's 10 TPS authenticated *pull* quota. AWS publishes no TPS quota for tag listing, and a 2026-08-28 probe drew repeated 429s on `TagList` at that pacing with credentials loaded. Treat that value as measured-and-insufficient, not documented. See `docs/src/content/registries/ecr-public.md`.
- Token-bucket layer (`TokenBucket` in `aimd.rs`) sits in front of the AIMD windows for registries with documented per-account TPS caps. Configured per `WindowKey` via `bucket_config_for_window()` returning a `BucketConfig { rate_per_sec, burst }`; ECR / ECR Public / GHCR / GAR / ACR get buckets, others fall back to AIMD-only. Bucket pacing happens BEFORE concurrency permits so a paced action does not occupy a slot another window could service.
- A window keeps one semaphore for its lifetime. A halving shrinks it in place (idle permits retired at once, the rest owed and retired as their requests finish), because replacing it strands requests already queued on the old one. Halving leaves the bucket alone: rate-cap state is independent of concurrency state, and restoring burst tokens during throttle would defeat the purpose.
- Cap values were verified against AWS service quotas, Google Artifact Registry quotas, and Microsoft historical SKU defaults on 2026-04-26. GHCR's value is community-measured against the visible 2000 RPM enforcement.

## Retry

`src/retry.rs` owns the transient-failure contract for this crate, and `retrying()` is the one shape every caller uses.

- The client surfaces failures to its caller; `ocync-sync`'s engine retries the ones it drives via `with_retry`. The **prepare phase runs before the engine exists**, so anything reachable from it has to retry itself or a single throttle fails a whole mapping.
- Retrying in the prepare phase: `list_tags` (body read included, so a mid-page reset is re-sent), `token_exchange::exchange` (both the `/v2/` ping and the realm token request), `acr::exchange_post`, and each request of a Google credential load, the AWS region lookup included (`send` in `google/mod.rs`). `analyze`'s manifest walk retries at its call site in `src/cli/commands/analyze.rs` using `ocync_sync::retry::with_retry`, because `manifest_pull` is shared with the engine and must not double-retry.
- `is_transient_transport` is `pub` and is the **single** definition of the transient-transport predicate for the workspace. `ocync_sync::retry::should_retry_transport` delegates to it. Do not add a second copy: `is_request()` already covers connect failures and timeouts on the async hyper path, which is easy to get wrong from first principles.
- Backoff is jittered. Up to 16 mappings hit one registry concurrently, so an unjittered schedule sends every throttled retry back in lockstep.
- Don't add retry inside `send_with_aimd` or `manifest_pull`. Those are on the engine's path, which already retries; `tests/client_integration.rs::get_429_retries_and_succeeds` pins that layering.

## Upload protocol quirks

- Default: POST + streaming PUT with `Transfer-Encoding: chunked` (2 requests/blob). Streaming uploads are gated by a per-`RegistryClient` semaphore (`blob_upload_sem`, default cap `DEFAULT_BLOB_UPLOAD_CONCURRENCY` = 64) to stay under the per-h2-connection `SETTINGS_MAX_CONCURRENT_STREAMS` budget (100-128 across major registries probed 2026-06-01). The caller holds one slot per copy, an `UploadSlot` from `reserve_upload_slot`, and passes it to `blob_push_stream`, which takes no count permit itself. The engine reserves it before opening the source download, so the lock order is always target upload slot, then source download permit. The GHCR and ACR fallbacks take only the byte-budget permit.
- GHCR: multi-PATCH chunked broken (last PATCH overwrites previous). Client falls back to POST + single PATCH + PUT (3 requests/blob), `blob_push_stream_ghcr`.
- GAR: the default streaming path. A second PATCH in one session answers 405 and a monolithic POST answers 502, so neither is used.
- ACR: ~20 MB streaming PUT body limit. Client buffers the full blob, verifies digest, then uploads in 16 MB PATCH chunks (OCI `{start}-{end}` Content-Range, NOT RFC 7233) followed by a finalize PUT, `blob_push_stream_acr`. Zero-byte blobs (e.g. signature empty-config) skip the PATCH loop and go straight to finalize PUT.
- Every upload `Location` a registry returns (the initiate POST's and each PATCH's) must share the registry's origin (scheme, host and port), so its credentials never reach another origin: `extract_location` in `blob.rs` refuses any other. Each PATCH and the finalize PUT go to a `Location` already checked.

## Cross-repo mount

- ECR fulfills mount when `BLOB_MOUNTING=ENABLED` account setting + a committed manifest in the source repo *references the specific blob being mounted*. "Source repo has *some* committed manifest" is insufficient — the committed manifest must include the blob in its config or layer set.
- The engine offers a repo as a mount source for a blob only once a committed manifest in it references that blob; see the commit rule in `crates/ocync-sync/CLAUDE.md`.
- Mount POST returns 201 (success) or 202 (not fulfilled, upload session started).
- Mount is attempted on all providers unconditionally; the 202 fallback is cheap (~100ms).

## Testing

- Network mocking: `wiremock`. Every optimization needs `.expect(0)` on the slow path AND `.expect(1)` on the fast path.
- Wiremock constraint: wiremock binds to `127.0.0.1` (IP host), so domain binding validation is skipped. Tests exercising domain binding must use cached challenges with a domain-based `base_url` string, not `mock.uri()`.
- Protocol correctness: `testcontainers` against `registry:2` in `tests/registry2_*.rs`.
- Mock trait impls must honor the real contract - filter inputs, assert context params (repo, registry) match expected values.
- Realm validation tests: realm and registry must use the same scheme (both `https://` or both `http://`) unless the test specifically targets the scheme check. Mismatched schemes cause the scheme check to fire first, masking the intended denylist rule. Always assert on the error message substring, not just `is_err()`.
- Per-provider auth coverage convention: in-file `#[cfg(test)] mod tests` next to the provider, using `wiremock::MockServer` against `configured::Auth::anonymous` / `basic` / `docker_config`, `AcrAuth::with_api` or `GcpAuth::with_api`. New behavioral tests for an existing provider are added to that in-file module, not a parallel `tests/auth_*.rs`.
- Shared provider mocks live in `auth::testing`: `challenge` (the `/v2/` Bearer challenge for a service) and `token` (a token endpoint answering one value).
- Concurrent-coalescing tests use `#[tokio::test(flavor = "current_thread")]` (as in `auth/token_cache.rs`) -- the production binary uses single-threaded tokio, so coalescing under that runtime model is the contract that matters.

## Provider name surface (testability)

`RegistryClient::auth_name()` (`client.rs`) returns the configured provider's `AuthProvider::name()` or `None`. Used by `src/cli/auth_dispatch_tests.rs` to assert that each `auth_type` config value (and each auto-detected hostname) routes to the expected provider. Provider-name strings are the public testability surface; keep them stable -- changing one from `"docker-config"` to `"docker_config"` would silently break every dispatch test row.

## Perf-harness test hooks on `RegistryClientBuilder`

Three `#[doc(hidden)] pub` builder methods exist on `RegistryClientBuilder` solely for in-repo perf and A/B harnesses (primarily `crates/ocync-sync/tests/perf_profile.rs`):

- `allow_invalid_certs(bool)` -- terminates TLS at a `testcontainers` `registry:2` with a self-signed cert.
- `force_http1(bool)` -- compares HTTP/2 vs HTTP/1.1 throughput.
- `http2_adaptive_window(bool)` -- A/Bs HTTP/2 adaptive flow-control window sizing.

These are **intentionally retained**. They look unused from a production-code grep because production code never reaches them (no CLI flag, no env var, no config setting). Removing them as "dead code" deletes load-bearing diagnostic infrastructure: any future perf investigation that needs to reproduce the HTTP/2 stall investigation (or any successor) would have to re-introduce the same plumbing from scratch.

If you're tempted to delete them, search `tests/perf_profile.rs` first -- it references all three by name.
