# ocync-distribution

OCI Distribution Specification client library - registry auth, blob/manifest transfer, and provider-specific protocol handling.

## Auth protocol

- ECR private uses HTTP Basic auth (not Bearer token exchange). AWS SDK `GetAuthorizationToken` returns a pre-encoded base64 token used directly.
- `EcrAuth::new(hostname, profile)` accepts an optional named AWS profile. When `Some(p)`, the SDK builder calls `.profile_name(p)`, scoping credential resolution to that profile in the shared credentials/config file. When `None`, the ambient default credential chain is used. Per-registry isolation is structural — each `EcrAuth` instance holds its own `SdkConfig` — so a profile override on one registry does not affect any other registry's credential resolution. `EcrPublicAuth` is unchanged (out of scope).
- ECR Public uses SDK `GetAuthorizationToken` -> decode base64 -> OCI Bearer token exchange with those credentials. SDK tokens are NOT valid as direct Bearer tokens; they must drive standard `/v2/token` exchange.
- Both ECR providers cache SDK credentials via `SdkCredentialCache<T>` in `auth/ecr.rs` (generic read-lock fast path / write-lock + double-check). New ECR-style providers must use this cache, not hand-roll the RwLock pattern.
- GAR/GCR uses `google-cloud-auth` ADC (`devstorage.read_write` scope) -> `oauth2accesstoken:<token>` Basic creds -> `token_exchange::exchange()` -> Bearer token. Same flow as ECR Public. Auto-detected via `ProviderKind::Gar`/`Gcr`. SDK credential TTL is 600s (conservative: `google-cloud-auth` does not expose `expires_in`). Implementation in `auth/gcp.rs`.
- ACR uses a proprietary OAuth2 flow, NOT standard OCI token exchange. Azure AD credential chain (client secret, workload identity, managed identity, Azure CLI -- extracted from `azure_identity` patterns, zero deps) -> `POST /oauth2/exchange` (AAD token -> ACR refresh token, ~3h TTL from JWT `exp` claim) -> `POST /oauth2/token` (refresh token + scope -> ACR access token, ~75min TTL from JWT `exp` claim). Exchange POSTs use a no-redirect HTTP client (`no_redirect_http_client()`) matching upstream `azure_identity` which disables redirects globally. Credential chain caches winning source index, reset on `invalidate()` (exceeds upstream). Error bodies truncated to 200 chars (exceeds upstream). Auto-detected via `ProviderKind::Acr`. Implementation in `auth/acr.rs`.
- New cloud auth providers follow the ECR Public pattern: SDK credential -> Basic creds -> `token_exchange::exchange()` -> Bearer token.
- Challenge caching: `ChallengeCache` in `auth/token_exchange.rs` stores the parsed `WWW-Authenticate` realm+service so subsequent token exchanges skip the `/v2/` ping. All Bearer-based providers (anonymous, basic, docker-config, ecr-public) use it. Clear on invalidate.
- Realm URL validation: `validate_realm_url()` in `token_exchange.rs` validates realm URLs before sending credentials. Four layers: structural (scheme, userinfo, host), IP denylist (link-local, cloud metadata, unspecified, localhost, conditional loopback, IPv4-translated/NAT64), no-redirect client, domain binding (realm host must match or share parent domain with registry). Runs on both fresh and cached challenges.
- Parse `WWW-Authenticate` header dynamically; never hardcode token exchange endpoints.
- Token caching: `EARLY_REFRESH_WINDOW` = 30s. Docker Hub issues 300s tokens; a 15m window was a bug that bypassed the cache entirely.
- Per-scope tokens: format `repository:<name>:<actions>` where actions = `pull`, `push`, or `pull,push`. Per-scope caching (`scopes_cache_key` in `auth/mod.rs`) is universal across every Bearer-issuing provider (anonymous, basic, docker-config, ecr-public, gcp, acr). It is NOT a Chainguard-specific feature.
- cgr.dev's specific quirk is *enforcement*: it returns 403 on cross-scope token reuse where some registries silently accept it. The scope-keyed cache is what makes ocync correct against this enforcement -- but the cache itself exists for every Bearer flow.
- Per-scope coalescing: every Bearer provider routes its cache through `TokenCache` in `auth/token_cache.rs`. The helper holds the cache mutex only for brief reads and writes; the actual token exchange runs under an `Arc<Mutex<()>>` keyed by scope, so concurrent fetches for the same scope coalesce to one exchange while distinct scopes proceed in parallel. The helper owns the contract; per-provider tests verify wiring, not the contract.

## Provider dispatch (auto-detection)

When `auth_type` is unset in registry config, `src/cli/mod.rs` selects the auth provider by `detect_provider_kind(hostname)`:

| Detected `ProviderKind` | Auth provider |
| --- | --- |
| `Ecr` | `EcrAuth` |
| `EcrPublic` | `EcrPublicAuth` |
| `Gar` / `Gcr` | `GcpAuth` |
| `Acr` | `AcrAuth` |
| `Ghcr` / `DockerHub` / `Chainguard` / unknown | `DockerConfigAuth` (try `~/.docker/config.json`); falls back to `AnonymousAuth` if no config or no entry for the host |

This means `cgr.dev`, `ghcr.io`, `docker.io`, and any unrecognized hostname all share the same default path: docker-config first, anonymous fallback. `docker login <host>` (or `chainctl auth login` for cgr.dev paid tags) is the supported way to supply credentials when `auth_type` is not set explicitly.

When `auth_type` IS set in config, it overrides detection. Valid values: `ecr`, `gar`, `gcr`, `acr`, `basic`, `static_token`, `ghcr`, `docker_config`, `anonymous`. (`ghcr` and `docker_config` are equivalent.)

## Registry detection

- Always use `detect_provider_kind()` + `ProviderKind` enum.
- Never match raw hostnames; detection logic is centralized in `auth/detect.rs`.

## AIMD concurrency controller

- Per-(registry, action) AIMD windows, not per-host. `WindowKey` is a typed enum -- one variant per (provider, action-group) pair.
- ECR private: 9 windows (each API action has an independent per-region TPS cap).
- ECR Public: 5 windows (read paths share, plus 4 write windows; caps are 10x lower than private).
- Docker Hub: 3 windows (HEADs unmetered/shared, manifest-read separate, rest shared).
- GAR / GCR: 1 window (per-project quota is shared across all actions).
- GHCR: 1 window. GitHub enforces a single 2000 RPM aggregate cap per authenticated principal across reads and writes; separate read/write windows would silently exceed the cap.
- ACR: 2 windows (separate ReadOps and WriteOps quotas).
- Unknown (Chainguard, Quay, generic): 5-window coarse grouping (heads, reads, uploads, manifest-write, tag-list).
- AIMD congestion epochs: 100ms epoch prevents cascade collapse from burst 429s. Halving is only half the 429 response; see "Retry" below for the other half.
- ECR Public's shared read window includes `TagList`, and its bucket value (8 TPS) is derived from AWS's 10 TPS authenticated *pull* quota. AWS publishes no TPS quota for tag listing, and a 2026-08-28 probe drew repeated 429s on `TagList` at that pacing with credentials loaded. Treat that value as measured-and-insufficient, not documented. See `docs/src/content/registries/ecr-public.md`.
- Token-bucket layer (`TokenBucket` in `aimd.rs`) sits in front of the AIMD windows for registries with documented per-account TPS caps. Configured per `WindowKey` via `bucket_config_for_window()` returning a `BucketConfig { rate_per_sec, burst }`; ECR / ECR Public / GHCR / GAR / ACR get buckets, others fall back to AIMD-only. Bucket pacing happens BEFORE concurrency permits so a paced action does not occupy a slot another window could service.
- AIMD halving rebuilds the per-action semaphore but preserves the bucket: rate-cap state is independent of concurrency state. Restoring burst tokens during throttle would defeat the purpose.
- Cap values were verified against AWS service quotas, Google Artifact Registry quotas, and Microsoft historical SKU defaults on 2026-04-26. GHCR's value is community-measured against the visible 2000 RPM enforcement.

## Retry

`src/retry.rs` owns the transient-failure contract for this crate, and `retrying()` is the one shape every caller uses.

- The client surfaces failures to its caller; `ocync-sync`'s engine retries the ones it drives via `with_retry`. The **prepare phase runs before the engine exists**, so anything reachable from it has to retry itself or a single throttle fails a whole mapping.
- Retrying in the prepare phase: `list_tags` (body read included, so a mid-page reset is re-sent), `token_exchange::exchange` (both the `/v2/` ping and the realm token request), and `acr::exchange_post`. `analyze`'s manifest walk retries at its call site in `src/cli/commands/analyze.rs` using `ocync_sync::retry::with_retry`, because `manifest_pull` is shared with the engine and must not double-retry.
- `is_transient_transport` is `pub` and is the **single** definition of the transient-transport predicate for the workspace. `ocync_sync::retry::should_retry_transport` delegates to it. Do not add a second copy: `is_request()` already covers connect failures and timeouts on the async hyper path, which is easy to get wrong from first principles.
- Backoff is jittered. Up to 16 mappings hit one registry concurrently, so an unjittered schedule sends every throttled retry back in lockstep.
- Do NOT add retry inside `send_with_aimd` or `manifest_pull`. Those are on the engine's path, which already retries; `tests/client_integration.rs::get_429_retries_and_succeeds` pins that layering.

## Upload protocol quirks

- Default: POST + streaming PUT with `Transfer-Encoding: chunked` (2 requests/blob). Streaming PUT body is gated by a per-`RegistryClient` semaphore (`streaming_blob_sem`, default cap 64) to stay under the per-h2-connection `SETTINGS_MAX_CONCURRENT_STREAMS` budget (100-128 across major registries probed 2026-06-01).
- GHCR: multi-PATCH chunked broken (last PATCH overwrites previous). Client falls back to POST + single PATCH + PUT (3 requests/blob), `blob_push_stream_ghcr`.
- GAR: no chunked uploads. Client buffers full blob, monolithic PUT, `blob_push_stream_gar`.
- ACR: ~20 MB streaming PUT body limit. Client buffers the full blob, verifies digest, then uploads in 16 MB PATCH chunks (OCI `{start}-{end}` Content-Range, NOT RFC 7233) followed by a finalize PUT, `blob_push_stream_acr`. Zero-byte blobs (e.g. signature empty-config) skip the PATCH loop and go straight to finalize PUT. Each PATCH response's `Location` header is checked against the initiate host to prevent cross-host credential forwarding via a compromised proxy.

## Cross-repo mount

- ECR fulfills mount when `BLOB_MOUNTING=ENABLED` account setting + a committed manifest in the source repo *references the specific blob being mounted*. "Source repo has *some* committed manifest" is insufficient — the committed manifest must include the blob in its config or layer set.
- Multi-tag image gotcha: when a source repo has tag1 committed and tag2's blobs still uploading, a follower mounting one of tag2's blobs sees the source repo's commit watch as `true` (tag1 satisfied it) and attempts the mount. ECR returns 202 because that specific blob isn't yet referenced by any committed manifest. The engine's `mark_blob_repo_stale` + push fallback handles it correctly; ~200ms wasted per occurrence.
- Mount POST returns 201 (success) or 202 (not fulfilled, upload session started).
- Mount is attempted on all providers unconditionally; the 202 fallback is cheap (~100ms).

## Testing

- Network mocking: `wiremock`. Every optimization needs `.expect(0)` on the slow path AND `.expect(1)` on the fast path.
- Wiremock constraint: wiremock binds to `127.0.0.1` (IP host), so domain binding validation is skipped. Tests exercising domain binding must use cached challenges with a domain-based `base_url` string, not `mock.uri()`.
- Protocol correctness: `testcontainers` against `registry:2` in `tests/registry2_*.rs`.
- Mock trait impls must honor the real contract - filter inputs, assert context params (repo, registry) match expected values.
- Realm validation tests: realm and registry must use the same scheme (both `https://` or both `http://`) unless the test specifically targets the scheme check. Mismatched schemes cause the scheme check to fire first, masking the intended denylist rule. Always assert on the error message substring, not just `is_err()`.
- Per-provider auth coverage convention: in-file `#[cfg(test)] mod tests` next to the provider, using `wiremock::MockServer` against `BasicAuth::with_base_url` / `AcrAuth::with_api` / `GcpAuth::with_api` constructors. New behavioral tests for an existing provider are added to that in-file module, not a parallel `tests/auth_*.rs`. The pre-existing `tests/auth_anonymous.rs` predates this convention; do not migrate it but do not pattern-match on it either.
- BasicAuth helpers reusable for new tests: `mount_v2_challenge` (`auth/basic.rs:160`), `mount_token_endpoint` / `mount_token_endpoint_for_scope` (`:175` / `:189`; the latter filters by `scope` query param for multi-scope assertions), `test_credentials` (`:148`).
- Concurrent-coalescing tests use `#[tokio::test(flavor = "current_thread")]` (matching `tests/auth_anonymous.rs:165`) -- the production binary uses single-threaded tokio, so coalescing under that runtime model is the contract that matters.

## Provider name surface (testability)

`RegistryClient::auth_name()` (`client.rs`) returns the configured provider's `AuthProvider::name()` or `None`. Used by `src/cli/auth_dispatch_tests.rs` to assert that each `auth_type` config value (and each auto-detected hostname) routes to the expected provider. Provider-name strings are the public testability surface; keep them stable -- changing one from `"docker-config"` to `"docker_config"` would silently break every dispatch test row.

Provider names:
- `EcrAuth` -> `"ecr"`
- `EcrPublicAuth` -> `"ecr-public"`
- `GcpAuth` -> `"gcp"` (covers both `Gar` and `Gcr` `ProviderKind`s)
- `AcrAuth` -> `"acr"`
- `BasicAuth` -> `"basic"`
- `StaticTokenAuth` -> `"static-token"`
- `DockerConfigAuth` -> `"docker-config"` (covers `auth_type: docker_config` and the `auth_type: ghcr` alias)
- `AnonymousAuth` -> `"anonymous"`

`build_registry_client` (`src/cli/mod.rs`) calls `ocync_distribution::install_crypto_provider()` at the top -- production main does this too, but the dispatch entry point is also reached from tests that bypass main, so the install must be idempotent there.

## Perf-harness test hooks on `RegistryClientBuilder`

Three `#[doc(hidden)] pub` builder methods exist on `RegistryClientBuilder` solely for in-repo perf and A/B harnesses (primarily `crates/ocync-sync/tests/perf_profile.rs`):

- `allow_invalid_certs(bool)` -- terminates TLS at a `testcontainers` `registry:2` with a self-signed cert.
- `force_http1(bool)` -- compares HTTP/2 vs HTTP/1.1 throughput.
- `http2_adaptive_window(bool)` -- A/Bs HTTP/2 adaptive flow-control window sizing.

These are **intentionally retained**. They look unused from a production-code grep because production code never reaches them (no CLI flag, no env var, no config setting). Removing them as "dead code" deletes load-bearing diagnostic infrastructure: any future perf investigation that needs to reproduce the HTTP/2 stall investigation (or any successor) would have to re-introduce the same plumbing from scratch.

If you're tempted to delete them, search `tests/perf_profile.rs` first -- it references all three by name.

## Commands

```bash
# Unit + wiremock tests
cargo test --package ocync-distribution

# Integration tests against local registry (requires Docker)
cargo test --package ocync-distribution --test registry2_client
cargo test --package ocync-distribution --test registry2_mount
```
