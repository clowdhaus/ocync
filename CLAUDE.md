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
- No speculative derives — every `derive` trait (`Hash`, `Eq`, `Serialize`, etc.) must have a live caller; remove unused derives during pre-commit audit
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
2. Execution: for each (tag, target): optional batch existence check (ECR `BatchBlobChecker`, pre-populates cache) → per-blob: cache check → mount/HEAD → pull+push → push manifests

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
- **Types**: prefer enums/newtypes over `String`/`u16` for domain concepts (media types, artifact types, status codes, repository names, registry identifiers). String newtypes use `Deref<Target=str>` for ergonomic borrowing; rely on implicit deref for `&str` params, use `.as_str()` only for generic bounds (`impl Into<String>`) and tracing fields. Only derive traits with live callers — no speculative `Hash`, `Eq`, or `Serialize` "just in case"
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
- **Configurable timeouts**: hardcoded timeout values (drain deadlines, retry caps, etc.) must be configurable via builder methods with sensible defaults. Document the default in the builder method's doc comment, not just in the code that uses it.
- **Registry module centralization**: all utilities for a specific registry provider (hostname parsing, SDK config loading, batch operations) live in one module (e.g., `ecr.rs`). Auth implementations import shared helpers from the provider module, not the other way around. Never scatter provider-specific code across auth, batch, and CLI layers. After centralizing, audit `pub use` re-exports in `lib.rs` — consumers may import via the submodule path, leaving the top-level re-export dead.
- **Constructor encapsulation**: public constructors must not leak internal dependencies into caller signatures. If a struct needs an AWS SDK config internally, accept a hostname string and build the config inside — don't force callers to import `aws-config`. This keeps dependency boundaries clean: library crates own their deps, CLI crates just pass domain values.
- **Avoid tuple type aliases for struct-like data**: when a tuple alias mirrors an existing struct's fields, add `Clone` to the struct instead. Tuples add destructuring noise at every use site and diverge from the struct over time. Cheap clones (`Arc`, `Rc`, `String`) make struct Clone zero-cost.
- **stdout contention**: when a component writes to stdout (progress summary, status lines) and the command also has `--json` output on stdout, the component must suppress its stdout writes. Use a behavioral flag (`suppress_summary: bool`), not a format flag (`json: bool`) — the behavior is "don't write to stdout", not "the output is JSON". This applies to any stdout writer that coexists with structured output modes.
- **Boolean parameters named for behavior**: name boolean constructor/method params for the behavior they control, not the trigger. `suppress_summary` is reusable (JSON mode, single-image copy, future formats); `json` is tied to one trigger and misleads when reused for other purposes.
- **Trait in library, impl in consumer**: when a library crate defines a trait (e.g., `ProgressReporter`), keep the default/no-op impl (`NullProgress`) in the library but put presentation impls (`TextProgress`) in the consumer (CLI) crate. The library defines the contract; the consumer decides how to present. This prevents formatting code from leaking into library APIs.

## Testing standards

### Test what can break, not what works

Tests must verify **behavior under failure and concurrency**, not just happy-path output. A test suite where everything passes with `max_concurrent=1` and no error injection is testing a `for` loop, not a concurrent pipeline. Per scope discipline above: if you cannot write a test that exercises a code path, that code path does not belong in the PR.

### Assert request counts, not just results

Every multi-target engine test must use wiremock `.expect(N)` on source endpoints to verify the pull-once fan-out invariant. A test that passes when the source manifest is pulled 3 times instead of 1 does not protect the architecture. Assert exact byte counts, blob counts, and request counts — never `> 0` or `status == Synced` alone. This applies to ALL endpoints in the test, not just source manifests — child manifest GETs, target manifest PUTs, blob pushes, and blob pulls each need `.expect(N)` when the count is architecturally significant. Helper functions like `mount_blob_pull` and `mount_manifest_push` don't set expectations; use inline `Mock::given()...expect(N)` for any endpoint where the count matters.

### Verify optimizations take the designed path

A correct outcome does not prove the optimization worked. Images can sync successfully via the slow path (per-blob HEAD, redundant pulls, no mounts) and still report `Synced`. Tests for optimization features must prove the **intended fast path was taken**, not just that the end state is correct:

- When a batch API replaces per-blob HEAD: assert `.expect(0)` on the HEAD endpoint AND `.expect(1)` on the batch endpoint. The test must **fail** if the slow path is used.
- When cache pre-population skips transfers: assert `blob_stats.skipped == N` for the exact count of blobs the batch check reported as existing. If `skipped == 0` and `transferred == total`, the optimization was wired but never activated.
- When cross-repo mount succeeds: assert `blob_stats.mounted == N` and `.expect(0)` on the pull endpoint for that blob.
- When auto-create triggers: assert `.expect(1)` on the create endpoint AND that the manifest push was retried after creation, not just that the image eventually synced.

The pattern: assert the **negative** (the slow path was NOT taken) alongside the **positive** (the fast path produced correct results). An optimization that silently falls back to the slow path on every call is a bug, not a feature — and only negative assertions catch it.

Optimization tests must cover both single-image manifests and index manifests (multi-platform). The blob collection path differs: single images have `config + layers`, index manifests flatten across children. An optimization tested only with single images may silently fail for the index path — and multi-arch ECR sync is the primary use case.

### Test the bridges between layers

Unit-test leaves (AIMD math, staging filesystem, cache serialization). Integration-test the top (engine end-to-end). But also test the **bridges** between layers — these are where real bugs live:
- HTTP 429 response → AIMD permit throttling → window shrinkage
- Engine → staging pull-once-push-N-from-disk path
- Engine → shutdown drain state machine
- Client → auth invalidation → retry sequence
- Cache hit → target HEAD re-verification → stale entry eviction
- Index manifest → child manifest pull failure → image-level failure propagation
- Batch checker failure → per-blob HEAD fallback → correct transfer completion

If a code path is fully wired end-to-end, it needs an integration test that exercises it end-to-end. Unit tests on the leaf types are necessary but not sufficient.

### Test concurrent properties at real concurrency

At least some tests must run with `max_concurrent > 1` and multiple tags/targets executing simultaneously. Assert that dedup, caching, and AIMD still work under contention. If concurrent tests are flaky, that is a signal of a real race — investigate, don't serialize.

### Test configurable behavior at boundary values

Tests for configurable parameters (timeouts, concurrency caps, thresholds) must use values that **differentiate** the custom setting from the default. A drain deadline test where blob delays exceed both the custom 2s and default 25s proves nothing about the custom deadline — both would abandon. Use delays **between** the custom and default values so the test only passes if the custom value is actually used.

### Assert aggregate and per-image stats together

Engine integration tests must assert both per-image stats (`report.images[N].blob_stats`) and aggregate stats (`report.stats`). The aggregation path in `compute_stats` has its own logic — a bug there would be missed by per-image assertions alone.

### Test helpers for struct defaults

When adding a new field to a widely-used struct (especially one constructed in many tests), immediately add a test helper function with the default value. This prevents N boilerplate additions across existing tests and keeps the diff focused on new behavior. The helper should accept only the fields that vary between tests; default fields go inside the helper.

### Mock contract fidelity

Test mocks (trait implementations, not just wiremock) must honor the same input/output contract as the real implementation. A mock that ignores its input parameters (`_digests`, `_repo`) can't catch wiring bugs where the caller passes wrong values:
- If the real implementation filters by input, the mock must filter the same way
- If the real implementation returns only requested items, the mock must not return unrequested items
- Use named parameters (not `_`-prefixed) in mock impls that use the parameter, to signal the mock respects the contract
- A mock that returns a static response regardless of input is testing that the caller handles the response, not that the caller sends the right request
- **Context parameters need `assert_eq!`**: store expected values (repo name, registry ID) in the mock struct and assert they match on every call. A `_repo` parameter in a mock hides bugs where the engine passes the source repo instead of the target repo — a silent correctness failure. Only `_`-prefix parameters in always-error mocks where input genuinely doesn't matter
- Cross-check mock expected values against test setup: every `MockFoo::new("repo", ...)` must match the test's `ResolvedMapping.target_repo`
- **Non-tautological assertions**: when a mock asserts `assert_eq!(repo, self.expected_repo)`, the test must use different values for source and target (e.g., `source_repo: "src/nginx"`, `target_repo: "tgt/nginx"`) so the assertion actually differentiates. A test where `source_repo == target_repo == "repo"` makes every repo assertion pass regardless of which value the engine passes — the mock check exists but catches nothing

### Batched operation resilience

Multi-batch operations (chunked API calls, paginated requests) must handle mid-batch failures gracefully:
- Preserve results from successful batches before the failure (partial success)
- Propagate the error only if no results were obtained (total failure on first batch)
- Let the caller decide what to do with the unchecked remainder (fall back to per-item checks, retry, etc.)
- Test both total failure (first batch fails) and partial failure (Nth batch fails with N>1) — they exercise different code paths

### Fan-out feature testing

For 1:N fan-out features (e.g., sync to multiple targets), at least one test must exercise N>1 with **different state per target**. A feature tested only at N=1 doesn't verify target independence — shared state bugs, cross-target contamination, and per-target stat tracking only surface at N>=2. Each target should have its own mock server and its own assertions.

### Output format tests use exact assertions

CLI output format tests must use exact string assertions or line-prefix matching, not substring searches. A test that uses `.contains("synced")` passes even if the format is completely wrong — the word just has to appear somewhere. For stable output formats (summary lines, per-image status):
- At least one test should `assert_eq!` the exact output string
- Per-line assertions should use `line.starts_with("synced  ")` (with correct padding), not `output.matches("synced").count()`
- Tests for both stdout and stderr: verify content goes to the correct stream AND does not appear on the wrong stream (cross-stream negative assertions)

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

- **Design spec**: `docs/specs/2026-04-10-ocync-design.md` — full design document
- **Transfer optimization design**: `docs/specs/2026-04-12-transfer-optimization-design.md` — pipeline architecture, transfer state cache, adaptive concurrency, multi-target blob reuse
- **Implementation plan**: `docs/superpowers/plans/` (gitignored) — remaining v1 work is `2026-04-12-remaining-v1-implementation.md` (remaining: auth providers, progress/health/metrics, FIPS/packaging)

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
