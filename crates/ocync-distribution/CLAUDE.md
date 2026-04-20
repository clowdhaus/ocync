# ocync-distribution

OCI Distribution Specification client library - registry auth, blob/manifest transfer, and provider-specific protocol handling.

## Auth protocol

- ECR private uses HTTP Basic auth (not Bearer token exchange). AWS SDK manages credentials directly.
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
- GAR/ACR: all actions share a single key.
- TCP Reno congestion epochs: 100ms epoch prevents cascade collapse from burst 429s.

## Upload protocol quirks

- Default: POST + streaming PUT with `Transfer-Encoding: chunked` (2 requests/blob).
- GHCR: multi-PATCH chunked broken (last PATCH overwrites previous). Client falls back to single PATCH + PUT.
- GAR: no chunked uploads. Client buffers full blob, monolithic PUT.
- ACR: known ~20 MB streaming PUT body limit. Chunked PATCH fallback not yet implemented.

## Cross-repo mount

- ECR fulfills mount when `BLOB_MOUNTING=ENABLED` account setting + source blob has a committed manifest.
- Mount POST returns 201 (success) or 202 (not fulfilled, upload session started).
- Mount is attempted on all providers unconditionally. `ProviderKind::fulfills_cross_repo_mount()` exists but is currently dead code (returns `true` for all variants, zero callers).

## Dependencies

- `default-features = false` everywhere; justify every new dep.
- `reqwest` needs `system-proxy` + `rustls-tls-native-roots-no-provider` or proxy/trust-store is silently disabled.
- Crypto backend: `fips` vs `non-fips` feature flags (unavoidable platform linking).

## Testing

- Network mocking: `wiremock`. Every optimization needs `.expect(0)` on the slow path AND `.expect(1)` on the fast path.
- Protocol correctness: `testcontainers` against `registry:2` in `tests/registry2_*.rs`.
- Mock trait impls must honor the real contract - filter inputs, assert context params (repo, registry) match expected values.

## Commands

```bash
# Unit + wiremock tests
cargo test --package ocync-distribution

# Integration tests against local registry (requires Docker)
cargo test --package ocync-distribution --test registry2_client
cargo test --package ocync-distribution --test registry2_mount
```
