# ocync

OCI registry sync tool. Rust workspace with 3 crates: `ocync` (CLI binary), `ocync-distribution` (OCI registry client), `ocync-sync` (sync engine).

## Design priorities

Ranked by weight. These override local optimization instincts when they conflict.

1. **Efficiency — bytes transferred and rate-limit friendliness.** Every blob we avoid transferring, every API call we avoid issuing, is the real win. Wall-clock is downstream.
2. **Correctness** — staleness handling, auth invalidation, protocol conformance. Efficiency optimizations must degrade safely, not silently.
3. **Wall-clock speed** — a consequence of (1), not a goal separate from it. Reports that prioritize wall-clock without byte/request counts are misleading.
4. **UX** — clear errors, structured output, sensible defaults. Must be zero-cost when disabled so it never drags on (1).

## Scope discipline

Every PR ships the smallest correct change + one test that catches regression. Defer scaffolding to a follow-up PR justified by a second observation.

- No forward declarations, stub implementations, or placeholder types
- No `pub` items without a caller in the diff
- No struct fields or enum variants without a reader
- No Cargo feature flags except crypto backend (`fips` vs `non-fips`) — that one is unavoidable platform linking
- Test what can break: at least one test that would fail if the intended path is NOT taken (negative assertion)
- If the change is ~10 LOC of real intent, aim for ~100 LOC total diff. 10× is a smell worth justifying.

## Code standards

- **Naming**: stutter-free types (`Error` not `DistributionError`); OCI spec terminology verbatim for on-wire types
- **Imports**: `use` statements; group std > external > crate; no inline paths
- **Docs**: every `.rs` file gets `//!`; all `pub` items get `///`
- **Errors**: invalid user config returns `Result`, never silently degrades
- **Dependencies**: `default-features = false` everywhere; justify every new dep; prefer hand-written under ~100 lines over a crate. `reqwest` needs `system-proxy` + `rustls-tls-native-roots` or proxy/trust-store support is silently disabled.
- **Auth**: ECR private uses Basic auth (not Bearer token exchange); parse `WWW-Authenticate` dynamically, never hardcode
- **Registry detection**: use `detect_provider_kind()` + `ProviderKind` enum; never match raw hostnames
- **Process control**: return `ExitCode` via `Termination`; never `process::exit()`
- **Concurrency model**: single-threaded tokio (`current_thread`). All shared state uses `Rc<RefCell<>>`, never `Arc<Mutex<>>`. `RefCell` borrows must be dropped before any `.await` point. Intra-image blob concurrency uses `FuturesUnordered` + local `Semaphore(6)`.

## Testing

- Unit-test leaves, integration-test bridges (client → engine, HTTP → AIMD, cache → target HEAD). A bug between layers is the most common bug.
- Network code uses `wiremock`. Every optimization has at least one test with `.expect(0)` on the slow path AND `.expect(1)` on the fast path.
- Protocol correctness uses `testcontainers` against `registry:2`. Suites live in `crates/ocync-distribution/tests/registry2_*.rs`.
- Engine integration tests pass `Some(&shutdown)` (the production path) unless explicitly testing termination.
- Mock trait impls must honor the real contract — filter inputs, assert context params (repo, registry) match expected values.
- Concurrency tests run at `max_concurrent > 1`. Serializing a flaky concurrent test hides a race.
- Mount-related integration tests must use symmetric mocks (both repos accept upload AND mount) since leader-follower election can pick either image as leader. Assert on aggregate stats, not per-image ordering.
- `tokio::sync::Notify::notify_waiters()` does not store permits. Every code path that transitions a blob out of `InProgress` must call `notify_blob` or concurrent waiters deadlock. Same applies to `BlobStage::notify_staged`/`notify_failed` for source-pull dedup.
- The greedy set-cover election in `elect_leaders` provably covers every shared blob. There is no "uncovered follower" path -- all followers' shared blobs are in the leader blob union. Do not add wave partitioning among followers; it is dead code.

## Git workflow

- One PR at a time, merge to main, then next. No stacked PRs ever.
- Never include `Co-Authored-By: Claude` or Anthropic attribution
- Run the CI gate locally before push: `cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo deny check`
- During rebase conflicts on `Cargo.lock`, regenerate with `git checkout --theirs Cargo.lock && cargo generate-lockfile`

## Findings log

Empirical observations that inform design live in `docs/specs/findings.md`. When a benchmark or probe run changes our understanding of a registry's behavior, add an entry (observation → implication → action → re-validate) in the same PR as the behavior change.

## Plans and specs

- `docs/specs/ocync-design.md` — full design document (engine architecture, concurrency, cache)
- `docs/specs/transfer-optimization-design.md` — pipeline, transfer state cache, AIMD, multi-target reuse
- `docs/specs/benchmark-design-v2.md` — layered benchmark plan (protocol / throughput / cross-tool)
- `docs/specs/findings.md` — empirical evidence log
- `docs/superpowers/plans/` (gitignored) — in-flight implementation plans

## Commands

```bash
# CI gate (run before every push)
cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo deny check

# Run all tests
cargo test

# Integration tests against local registry (requires Docker)
cargo test --package ocync-distribution --test registry2_client
cargo test --package ocync-distribution --test registry2_mount
```

## Benchmarks

Prerequisites: Terraform, AWS credentials with ECR access, SSM parameter `/ocync/bench/github-token` populated.

```bash
cd bench/terraform && terraform init && terraform apply
cargo xtask bench --limit 3 --tools ocync --iterations 1 cold
cargo xtask bench --tools ocync,dregsy,regsync all
cd bench/terraform && terraform destroy
```

The bench instance bootstraps with: **ocync** (built from source, AWS SDK for ECR auth), **dregsy + skopeo** (skopeo built with `-tags "exclude_graphdriver_btrfs exclude_graphdriver_devicemapper containers_image_openpgp"` for AL2023), **regsync** (`go install`), **amazon-ecr-credential-helper** (Docker credential chain), **bench-proxy** (pure-Rust MITM at `bench/proxy/`, replaced mitmproxy which capped at ~250 Mbps).

`BENCH_TARGET_REGISTRY` must be set to the ECR hostname. `user_data_replace_on_change = true` recreates on bootstrap changes; `root_block_device` changes require explicit taint.

### Competitor config gotchas

Codified in `xtask/src/bench/config_gen.rs`:
- **regsync** requires `repoAuth: true` on source creds for per-repo-token registries (cgr.dev, gcr.io, nvcr.io). Without it, multi-image syncs fail with HTTP 403 on the second image. Do NOT set `credExpire` as a duration string — YAML parser fails; rely on 1h default.
- **dregsy** requires `auth-refresh: 12h` on ECR targets. Without it, dregsy skips the AWS SDK refresher and falls through to skopeo's fragile credential resolution.
- **dregsy** requires `platform: all` per mapping for multi-arch copy. Without it, skopeo copies only the native platform — the comparison is not apples-to-apples.
- **dregsy** exits 1 on any failed skopeo copy, even with 99% success. Parse per-image logs for real metrics, not exit code.
- **Docker Hub auth** is read from two SSM parameters: `/ocync/bench/dockerhub-access-token` (PAT) and `/ocync/bench/dockerhub-username` (account name). Both are injected into all three tool configs. Docker Hub PATs require the account username that created the token; a wrong username causes 401.

### Baseline and optimization backlog

See `docs/specs/findings.md` for:
- **-- Current competitive position** — comparison table (ocync vs regsync vs dregsy)
- **-- Optimization backlog** — ranked list of next optimizations with impact/complexity/status

Prior dregsy results were invalid (1 platform instead of 2). Re-run with `platform: all` needed for fair baseline.

### Bench-proxy

- **Forward 3xx, don't follow.** Docker Hub 302s to pre-signed S3 URLs whose SigV4 signatures bind to the origin host. Proxy must use `reqwest::Client::builder().redirect(Policy::none())`.
- **Leaf cert cache is sync.** `rustls::server::ResolvesServerCert::resolve` is called synchronously. Use `std::sync::RwLock`, not `tokio::sync::RwLock`.
- **Proxy logs live in the output dir**, not the tempdir, so post-run analysis survives.
- **Mount metrics** surface in `ProxyMetrics` as `mounts=<succ>/<attempt>` per tool for quick "is this code path cold" feedback.
- **Source-pull metrics** `source_blob_gets` and `source_blob_bytes` in `ProxyMetrics` track blob GETs to non-target (source) registries. `aggregate()` takes `target_registry` to split by direction.
- **Instance metadata** collected via IMDS (instance type, region) + `aws ec2 describe-instance-types` CLI (authoritative CPU/memory/network). Do NOT use `/proc/cpuinfo` (ARM lacks `model name`) or sysfs network speed (reports NIC link rate, not instance bandwidth). Do NOT add `aws-sdk-ec2` as a dependency (monolithic, 38G build artifacts).
- **Historical archive** -- full `BenchReport` (Serialize) written to `bench-results/runs/{timestamp}.json` for cross-run comparison. Markdown `summary.md` has per-scenario metric tables with winners bolded.

### Known registry-specific behavior

- **ECR fulfills cross-repo mount when BLOB_MOUNTING=ENABLED** and source blob has a committed manifest. The mount short-circuit (`fulfills_cross_repo_mount`) was removed; all providers now attempt mount. Leader-follower election + wave promotion ensures mount sources have committed manifests.
- **regsync cannot cross-repo mount in sync scenarios**: regclient only attempts `from=` when source and target share the same registry hostname (architectural limitation, not a config error).
- **GHCR multi-PATCH chunked upload is broken** (each PATCH overwrites previous chunks). Client falls back to single-PATCH + PUT.
- **GAR does not support chunked uploads.** Client buffers and uses monolithic upload.
