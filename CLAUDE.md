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

The sync engine uses a **pull-once, fan-out** pattern for 1:N mappings:

1. Pull source manifest once per tag (including child manifests for indexes)
2. For each target: HEAD check, transfer blobs, push manifests

Source-side work (manifest pulls) must NEVER be repeated per target. The loop structure must be `tag → pull_source → for target → push_to_target`, never `tag → for target → (pull + push)`. Blobs are inherently target-specific (dedup/mount decisions depend on target state) so blob pulls may repeat across targets, but manifest pulls are pure source-side work that is identical across all targets.

OCI blobs are repo-scoped: a blob pushed to `registry/repo-a` is NOT accessible from `registry/repo-b` without a cross-repo mount or separate push. The blob dedup map must check `known_repos` for the current repo before skipping — never skip based on status alone.

## Code standards

- **Naming**: stutter-free types (`Error` not `DistributionError`), spec terminology verbatim for OCI types, semantic field names
- **Imports**: `use` statements, never inline paths; group std > external > crate; direct deps, not re-exports
- **Docs**: every `.rs` file gets a `//!` module doc comment; all `pub` items get `///` doc comments
- **Errors**: invalid user config must return `Result` errors, never silently degrade; users can't distinguish "nothing matched" from "config broken"
- **Types**: prefer enums/newtypes over `String`/`u16` for domain concepts (media types, artifact types, status codes)
- **Dependencies**: `default-features = false` on everything; justify every new dep; prefer hand-written code under ~100 lines over a crate; use `regex-lite` for ASCII patterns
- **Security**: manual `Debug` impls use `&"[REDACTED]"` for secrets; tracing HTTP crate caps via `add_directive()` after `EnvFilter`, never in base filter string
- **Auth**: expose both `get_token()` and `invalidate()`; never hand-roll 401 retry — use shared `invalidate_auth()` + retry helpers; use API-provided expiry over constants
- **Process control**: return `ExitCode` via `Termination` trait, never call `process::exit()`
- **Config parsing**: env var expansion on raw YAML before serde deserialization, not round-trip after
- **Testing**: network code requires `wiremock` tests verifying actual HTTP request sequences, not just unit tests on types
- **Classifiers**: response classifier functions that don't use `self` should be free functions

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
