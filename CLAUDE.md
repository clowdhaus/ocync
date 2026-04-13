# ocync

OCI registry sync tool. Rust workspace with 3 crates: `ocync` (CLI binary), `ocync-distribution` (OCI registry client), `ocync-sync` (sync engine).

## Design priorities

1. **Performance and efficiency** — this is the most important property of the tool. Every design decision must prioritize throughput, minimal API calls, low memory overhead, and wall-clock speed. Concurrent transfers, global blob deduplication, HEAD-before-pull skip checks, cross-repo mounts, streaming (no local disk), and chunked uploads exist to minimize time and bandwidth. Never trade performance for convenience. Never add unnecessary allocations, clones, or API round-trips. Measure before assuming something is fast enough.
2. **User experience** — a very close second. Clear error messages with actionable context, structured output for CI/CD, progress reporting (TTY-aware), dry-run validation, and sensible defaults. Users should never have to guess what went wrong or what the tool is doing. But UX features must not compromise transfer performance — e.g., progress reporting must be zero-cost when disabled, output formatting must not block the transfer pipeline.

When these conflict, performance wins — but look hard for solutions that satisfy both before accepting the tradeoff.

## Scope discipline

Every PR must be self-contained. Code in the diff must be called, tested, and integrated within that same diff.

- No forward declarations, placeholder types, or stub implementations
- No `pub` items without a caller in the diff (or existing code)
- No error variants that are never constructed
- No struct fields that are never read
- No enum variants for "future use"
- No Cargo feature flags anywhere — single binary, all registries, always; this is a CLI tool, not a library
- If you can't write a test that exercises a code path in this PR, it doesn't belong in this PR

## Pre-commit audit

Before marking work complete, mechanically verify:

1. **Dead code**: every `pub` item, error variant, struct field, and enum variant has a live caller/reader
2. **Wiring**: trace data flows end-to-end, not just "does this type-check" — follow the value from entry to exit
3. **Pattern breadth**: when fixing something, `grep` the full codebase for the same anti-pattern
4. **Visibility**: default to private; `pub(crate)` only when needed within the crate; `pub` only when consumed by another crate in this diff
5. **CI gates**: `cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo deny check` must pass locally before push

## Engine architecture

The sync engine uses a **pipelined pull-once, fan-out** pattern for 1:N mappings. Discovery and execution overlap via `tokio::select!` over two `FuturesUnordered` pools with a `VecDeque` pending queue between them. The plan phase is eliminated — progressive cache population replaces upfront batch HEAD checks.

1. Discovery: pull source manifest once per tag (`PulledManifest`), HEAD-check each target, produce `TransferTask` entries
2. Execution: for each (tag, target): cache check → mount/HEAD → pull+push blobs → push manifests

Source-side work (manifest pulls) must NEVER be repeated per target. Blobs are inherently target-specific (dedup/mount decisions depend on target state). Index manifests are fully resolved (all children pulled) during discovery before entering execution.

### Pipeline select! loop discipline

The pipeline `select!` must use `biased;` (prefer execution completions to free permits) and emptiness guards (`if !pool.is_empty()`) on every branch. Optional branches (shutdown signal, drain deadline) must use guard conditions (`if shutdown.is_some()`) — never `match` with `std::future::pending()` inside the async block, as this creates a permanently-pending future that prevents the `else` arm from firing when both pools drain. The drain deadline branch must also guard on `!execution_futures.is_empty()` so the engine exits immediately when all work completes rather than waiting for the full deadline.

OCI blobs are repo-scoped: a blob pushed to `registry/repo-a` is NOT accessible from `registry/repo-b` without a cross-repo mount or separate push. The blob dedup map must check `known_repos` for the current repo before skipping — never skip based on status alone.

### Cache staleness discipline

Any persistent cache that skips verification must analyze staleness on **both** source and target sides. Source-side staleness (image changed between syncs) is a missed optimization. Target-side staleness (image deleted/overwritten at target by lifecycle policy, manual push, or GC) is a **correctness bug** — the tool thinks it synced but the target has wrong/missing content. Every cache-skip path must have a verification or reconciliation mechanism for both sides.

### Rate limit controller granularity

Rate limit handling (AIMD, token buckets, etc.) must match the **actual API action granularity** of the target registry. ECR rate limits vary 5x within the same logical category (e.g., InitiateLayerUpload at 100 TPS vs UploadLayerPart at 500 TPS are both "blob write" but need independent windows). A 429 on one action must not throttle a different action with a higher limit. The controller key must be (registry, specific_api_action), not (registry, coarse_category).

### AIMD congestion epoch

AIMD window decreases must use a congestion epoch (100ms). Multiple 429s within the same epoch are a single congestion event — halve the window once, not once per 429. Without this, N simultaneous 429s collapse the window from 50 to 1. Recovery from 1 takes ~1,200 successes (~60 seconds). This is TCP Reno's core correctness property.

When the window shrinks, the per-action `Arc<Semaphore>` must be **replaced** (not shrunk by forgetting one permit). Tokio has no `remove_permits` API, so forgetting one permit only reduces the count by 1 regardless of the window delta. Outstanding `OwnedSemaphorePermit`s hold their own `Arc` reference and drain naturally. The `Drop` impl must mirror the `success()` path — if `on_success()` can grow the limit, Drop must also call `add_permits()`.

### Three-level concurrency control

Concurrency is three independent layers: (1) global image semaphore (`max_concurrent_transfers`, default 50) caps in-flight `(tag, target)` pairs, (2) per-registry aggregate semaphore (`max_concurrent`, default 50) caps total HTTP requests to one host, (3) per-(registry, action) AIMD windows adapt independently within the aggregate ceiling. Every request acquires permits from levels 2 and 3. Level 1 is engine-level, levels 2-3 are client-level.

### Optimization acceptance criteria

New optimization layers must:
1. **Quantify** wall-clock benefit for the primary use case (Chainguard → multi-region ECR) with concrete numbers
2. **Prove** the common case pays zero overhead (single-target, steady-state sync)
3. **Degrade gracefully** when assumptions break (stale cache, partial data, rate-limited source)
4. **Specify failure interaction** with other optimizations (see failure interaction matrix in design specs)

## Code standards

- **Naming**: stutter-free types (`Error` not `DistributionError`), spec terminology verbatim for OCI types, semantic field names
- **Imports**: `use` statements, never inline paths; group std > external > crate; direct deps, not re-exports
- **Docs**: every `.rs` file gets a `//!` module doc comment; all `pub` items get `///` doc comments
- **Errors**: invalid user config must return `Result` errors, never silently degrade; users can't distinguish "nothing matched" from "config broken"
- **Types**: prefer enums/newtypes over `String`/`u16` for domain concepts (media types, artifact types, status codes)
- **Dependencies**: `default-features = false` on everything; justify every new dep; prefer hand-written code under ~100 lines over a crate; use `regex-lite` for ASCII patterns; all shared deps must use `[workspace.dependencies]` — never add direct version references to crate-level Cargo.toml when the dep exists elsewhere in the workspace
- **Security**: manual `Debug` impls use `&"[REDACTED]"` for secrets; tracing HTTP crate caps via `add_directive()` after `EnvFilter`, never in base filter string
- **Auth**: expose both `get_token()` and `invalidate()`; never hand-roll 401 retry — use shared `invalidate_auth()` + retry helpers; use API-provided expiry over constants
- **Process control**: return `ExitCode` via `Termination` trait, never call `process::exit()`
- **Config parsing**: env var expansion on raw YAML before serde deserialization, not round-trip after; parse functions return `Option`/`Result`, never silently fall back to defaults; validators and parsers must accept the same inputs
- **Units**: parse and display functions for byte sizes use SI decimal prefixes (1 KB = 1,000) consistently; never mix SI parsing with binary display
- **Testing**: see Testing standards section below
- **Classifiers**: response classifier functions that don't use `self` should be free functions
- **Registry detection**: use `detect_provider_kind()` and `ProviderKind` enum for registry-specific behavior (GHCR fallback, GAR fallback, ECR rate limits) — never match on raw hostname strings; the canonical detector handles case-insensitivity, ports, and trailing dots
- **Ceremony consolidation**: when a multi-step protocol pattern (acquire permit → send → report → check status) repeats more than twice, extract a helper immediately; the helper becomes the single point where the protocol is correct, and callers become declarative
- **Visibility after consolidation**: after wrapping internal methods behind a new helper, audit whether the original methods still have external callers; tighten `pub(crate)` to private when the helper is the only remaining caller
- **Formatting**: no special symbols (`§`, etc.) in docs or code — use plain text references; prefer heading hierarchy over excessive bold
- **Best-effort I/O**: when an I/O result is intentionally not propagated (e.g., directory fsync after a successful atomic rename), log with `tracing::warn!` including path and error — never `let _ = io_op()`. Silent drops mask production issues.
- **SAFETY comments**: `// SAFETY:` is reserved for explaining why `unsafe` code is sound. For logic correctness assertions (e.g., "guard ensures Option is Some"), use plain comments.
- **Cleanup loop resilience**: loops that delete multiple files (cleanup, eviction, tmp removal) must log and continue on individual failures, never abort the whole operation with `?`. One stuck file should not prevent cleaning up the rest.
- **Configurable timeouts**: hardcoded timeout values (drain deadlines, retry caps, etc.) should be configurable via builder methods with sensible defaults. Document the default in the builder method's doc comment, not just in the code that uses it.

## Testing standards

### Test what can break, not what works

Tests must verify **behavior under failure and concurrency**, not just happy-path output. A test suite where everything passes with `max_concurrent=1` and no error injection is testing a `for` loop, not a concurrent pipeline. Per scope discipline above: if you cannot write a test that exercises a code path, that code path does not belong in the PR.

### Assert request counts, not just results

Every multi-target engine test must use wiremock `.expect(N)` on source endpoints to verify the pull-once fan-out invariant. A test that passes when the source manifest is pulled 3 times instead of 1 does not protect the architecture. Assert exact byte counts, blob counts, and request counts — never `> 0` or `status == Synced` alone.

### Test the bridges between layers

Unit-test leaves (AIMD math, staging filesystem, cache serialization). Integration-test the top (engine end-to-end). But also test the **bridges** between layers — these are where real bugs live:
- HTTP 429 response → AIMD permit throttling → window shrinkage
- Engine → staging pull-once-push-N-from-disk path
- Engine → shutdown drain state machine
- Client → auth invalidation → retry sequence
- Cache hit → target HEAD re-verification → stale entry eviction
- Index manifest → child manifest pull failure → image-level failure propagation

If a code path is fully wired end-to-end, it needs an integration test that exercises it end-to-end. Unit tests on the leaf types are necessary but not sufficient.

### Test concurrent properties at real concurrency

At least some tests must run with `max_concurrent > 1` and multiple tags/targets executing simultaneously. Assert that dedup, caching, and AIMD still work under contention. If concurrent tests are flaky, that is a signal of a real race — investigate, don't serialize.

### Test configurable behavior at boundary values

Tests for configurable parameters (timeouts, concurrency caps, thresholds) must use values that **differentiate** the custom setting from the default. A drain deadline test where blob delays exceed both the custom 2s and default 25s proves nothing about the custom deadline — both would abandon. Use delays **between** the custom and default values so the test only passes if the custom value is actually used.

### Assert aggregate and per-image stats together

Engine integration tests must assert both per-image stats (`report.images[N].blob_stats`) and aggregate stats (`report.stats`). The aggregation path in `compute_stats` has its own logic — a bug there would be missed by per-image assertions alone.

### wiremock for all network code

Network code requires `wiremock` tests verifying actual HTTP request sequences, not just unit tests on types. Tests must cover:
- Error paths by status code (401, 403, 404, 429, 500), not just success
- HTTP header assertions (Content-Type, Content-Range, Authorization format)
- Edge cases: empty streams, exact chunk boundaries, Location header chaining
- Stream error propagation (error mid-transfer, not just at start/end)
- Auth sequences: scope params, token rotation, 401 retry with invalidation

## Review protocol

Before claiming work is complete, dispatch independent review agents with specific questions:

- "Find any `pub` items with no callers"
- "Find any error variants never constructed"
- "Trace [specific data flow] end-to-end — does it actually work?"
- "Are there other instances of [the pattern just fixed] in the codebase?"

Never trust the implementer's self-report alone.

## Git workflow

- One PR at a time, merge to main, then next — no stacked/chained PRs, ever
- Never include `Co-Authored-By: Claude` or Anthropic attribution in commits or PRs
- Run CI checks locally before pushing
- Regenerate `Cargo.lock` during rebase conflicts (`git checkout --theirs Cargo.lock && cargo generate-lockfile`), never manually resolve
- Clean stale worktrees before switching to branches from past sessions

## Plans and specs

- **Design spec**: `docs/specs/2026-04-10-ocync-design.md` — full design document (1,752 lines)
- **Transfer optimization design**: `docs/specs/2026-04-12-transfer-optimization-design.md` — pipeline architecture, transfer state cache, adaptive concurrency, multi-target blob reuse (under review)
- **Implementation plans**: `docs/superpowers/plans/` (gitignored) — current v1 implementation plan is `2026-04-12-ocync-v1-implementation.md`

## Commands

```bash
# CI gate (run before every push)
cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo deny check

# Run all tests
cargo test

# Check formatting
cargo fmt --check

# Lint
cargo clippy -- -D warnings

# License/advisory check
cargo deny check
```
