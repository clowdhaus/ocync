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
- Per-scope tokens: format `repository:<name>:<actions>` where actions = `pull`, `push`, or `pull,push`. Per-scope caching (`scopes_cache_key` in `auth/mod.rs`) is universal across every Bearer-issuing provider (anonymous, basic, docker-config, ecr-public, gcp). It is NOT a Chainguard-specific feature.
- cgr.dev's specific quirk is *enforcement*: it returns 403 on cross-scope token reuse where some registries silently accept it. The scope-keyed cache is what makes ocync correct against this enforcement -- but the cache itself exists for every Bearer flow.

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
- AIMD congestion epochs: 100ms epoch prevents cascade collapse from burst 429s.
- Token-bucket layer (`TokenBucket` in `aimd.rs`) sits in front of the AIMD windows for registries with documented per-account TPS caps. Configured per `WindowKey` via `bucket_config_for_window()` returning a `BucketConfig { rate_per_sec, burst }`; ECR / ECR Public / GHCR / GAR / ACR get buckets, others fall back to AIMD-only. Bucket pacing happens BEFORE concurrency permits so a paced action does not occupy a slot another window could service.
- AIMD halving rebuilds the per-action semaphore but preserves the bucket: rate-cap state is independent of concurrency state. Restoring burst tokens during throttle would defeat the purpose.
- Cap values were verified against AWS service quotas, Google Artifact Registry quotas, and Microsoft historical SKU defaults on 2026-04-26. GHCR's value is community-measured against the visible 2000 RPM enforcement.

## Upload protocol quirks

- Default: POST + streaming PUT with `Transfer-Encoding: chunked` (2 requests/blob).
- GHCR: multi-PATCH chunked broken (last PATCH overwrites previous). Client falls back to POST + single PATCH + PUT (3 requests/blob).
- GAR: no chunked uploads. Client buffers full blob, monolithic PUT.
- ACR: known ~20 MB streaming PUT body limit. Chunked PATCH fallback not yet implemented.

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

## Commands

```bash
# Unit + wiremock tests
cargo test --package ocync-distribution

# Integration tests against local registry (requires Docker)
cargo test --package ocync-distribution --test registry2_client
cargo test --package ocync-distribution --test registry2_mount
```
