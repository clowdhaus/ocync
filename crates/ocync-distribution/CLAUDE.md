# ocync-distribution

OCI Distribution Specification client library - registry auth, blob/manifest transfer, and provider-specific protocol handling.

## Auth protocol

- ECR private uses HTTP Basic auth (not Bearer token exchange). AWS SDK `GetAuthorizationToken` returns a pre-encoded base64 token used directly.
- ECR Public uses SDK `GetAuthorizationToken` -> decode base64 -> OCI Bearer token exchange with those credentials. SDK tokens are NOT valid as direct Bearer tokens; they must drive standard `/v2/token` exchange.
- Both ECR providers cache SDK credentials via `SdkCredentialCache<T>` in `auth/ecr.rs` (generic read-lock fast path / write-lock + double-check). New ECR-style providers must use this cache, not hand-roll the RwLock pattern.
- GAR/GCR uses `google-cloud-auth` ADC -> `oauth2accesstoken:<token>` Basic creds -> `token_exchange::exchange()` -> Bearer token. Same flow as ECR Public. Auto-detected via `ProviderKind::Gar`/`Gcr`. Implementation in `auth/gcp.rs`.
- New cloud auth providers follow the ECR Public pattern: SDK credential -> Basic creds -> `token_exchange::exchange()` -> Bearer token. See `docs/superpowers/specs/2026-04-23-native-auth-and-realm-validation-design.md` for the full spec.
- Challenge caching: `ChallengeCache` in `auth/token_exchange.rs` stores the parsed `WWW-Authenticate` realm+service so subsequent token exchanges skip the `/v2/` ping. All Bearer-based providers (anonymous, basic, docker-config, ecr-public) use it. Clear on invalidate.
- Realm URL validation: `validate_realm_url()` in `token_exchange.rs` validates realm URLs before sending credentials. Four layers: structural (scheme, userinfo, host), IP denylist (link-local, cloud metadata, unspecified, localhost, conditional loopback, IPv4-translated/NAT64), no-redirect client, domain binding (realm host must match or share parent domain with registry). Runs on both fresh and cached challenges.
- Parse `WWW-Authenticate` header dynamically; never hardcode token exchange endpoints.
- Token caching: `EARLY_REFRESH_WINDOW` = 30s. Docker Hub issues 300s tokens; a 15m window was a bug that bypassed the cache entirely.
- Per-scope tokens: format `repository:<name>:<actions>` where actions = `pull`, `push`, or `pull,push`.
- cgr.dev uses per-repo tokens that 403 on wrong scope; scope-keyed cache handles it.

## Registry detection

- Always use `detect_provider_kind()` + `ProviderKind` enum.
- Never match raw hostnames; detection logic is centralized in `auth/detect.rs`.

## AIMD concurrency controller

- Per-(registry, action) AIMD windows, not per-host.
- ECR: 9 independent windows (each API action has different TPS limits).
- Docker Hub: HEAD unmetered, manifest-read separate, others shared.
- GAR: all actions share a single key (per-project quota).
- ACR (and other unknown registries): coarse grouping with 5 distinct windows (heads, reads, uploads, manifest-write, tag-list).
- AIMD congestion epochs: 100ms epoch prevents cascade collapse from burst 429s.

## Upload protocol quirks

- Default: POST + streaming PUT with `Transfer-Encoding: chunked` (2 requests/blob).
- GHCR: multi-PATCH chunked broken (last PATCH overwrites previous). Client falls back to POST + single PATCH + PUT (3 requests/blob).
- GAR: no chunked uploads. Client buffers full blob, monolithic PUT.
- ACR: known ~20 MB streaming PUT body limit. Chunked PATCH fallback not yet implemented.

## Cross-repo mount

- ECR fulfills mount when `BLOB_MOUNTING=ENABLED` account setting + source blob has a committed manifest.
- Mount POST returns 201 (success) or 202 (not fulfilled, upload session started).
- Mount is attempted on all providers unconditionally; the 202 fallback is cheap (~100ms).

## Testing

- Network mocking: `wiremock`. Every optimization needs `.expect(0)` on the slow path AND `.expect(1)` on the fast path.
- Wiremock constraint: wiremock binds to `127.0.0.1` (IP host), so domain binding validation is skipped. Tests exercising domain binding must use cached challenges with a domain-based `base_url` string, not `mock.uri()`.
- Protocol correctness: `testcontainers` against `registry:2` in `tests/registry2_*.rs`.
- Mock trait impls must honor the real contract - filter inputs, assert context params (repo, registry) match expected values.
- Realm validation tests: realm and registry must use the same scheme (both `https://` or both `http://`) unless the test specifically targets the scheme check. Mismatched schemes cause the scheme check to fire first, masking the intended denylist rule. Always assert on the error message substring, not just `is_err()`.

## Commands

```bash
# Unit + wiremock tests
cargo test --package ocync-distribution

# Integration tests against local registry (requires Docker)
cargo test --package ocync-distribution --test registry2_client
cargo test --package ocync-distribution --test registry2_mount
```
