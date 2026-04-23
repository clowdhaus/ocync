# ocync

OCI registry sync tool. Rust workspace with 3 core crates: `ocync` (CLI binary), `ocync-distribution` (OCI registry client), `ocync-sync` (sync engine). The workspace also includes `bench/proxy` (benchmark MITM proxy) and `xtask` (build/bench automation).

## Crate-specific guidance

Each crate has its own CLAUDE.md with targeted context:

- `crates/ocync-distribution/CLAUDE.md` - auth protocols, AIMD, registry detection, upload quirks, testing (wiremock/testcontainers)
- `crates/ocync-sync/CLAUDE.md` - concurrency model, RefCell rules, notify contracts, leader-follower, engine architecture, testing
- `bench/CLAUDE.md` - benchmark infrastructure, bench-proxy, competitor config gotchas, instance ops

## Design priorities

Ranked by weight. These override local optimization instincts when they conflict.

1. **Efficiency - bytes transferred and rate-limit friendliness.** Every blob we avoid transferring, every API call we avoid issuing, is the real win. Wall-clock is downstream.
2. **Correctness** - staleness handling, auth invalidation, protocol conformance. Efficiency optimizations must degrade safely, not silently.
3. **Wall-clock speed** - a consequence of (1), not a goal separate from it. Reports that prioritize wall-clock without byte/request counts are misleading.
4. **UX** - clear errors, structured output, sensible defaults. Must be zero-cost when disabled so it never drags on (1).

## Content integrity

ocync syncs content bit-for-bit from source to target(s). We do NOT convert, transform, or rewrite manifest or blob content. Digests are identity -- changing bytes changes the digest, breaks signatures, pin-by-digest workflows, and the OCI content-addressable model.

- Manifest bytes are transferred verbatim. No format conversion (Docker v2 to OCI or vice versa).
- Blob bytes are streamed directly from source to target without modification.
- All registries in production accept both Docker v2 and OCI manifests. There is no real-world use case for format conversion, and it would break every digest-based optimization (skip detection, transfer state cache, immutable tag handling, head-first).

## Scope discipline

Every PR ships the smallest correct change + one test that catches regression. Defer scaffolding to a follow-up PR justified by a second observation.

- No forward declarations, stub implementations, or placeholder types
- No `pub` items without a caller in the diff
- No struct fields or enum variants without a reader
- No Cargo feature flags except crypto backend (`fips` vs `non-fips`) - unavoidable platform linking
- Test what can break: at least one test that would fail if the intended path is NOT taken (negative assertion)
- If the change is ~10 LOC of real intent, aim for ~100 LOC total diff. 10x is a smell worth justifying.
- Challenge the use case before building. If a feature breaks existing optimizations (skip detection, caching, digest comparison), the cost likely exceeds the benefit.

## Code standards

- **Naming**: stutter-free types (`Error` not `DistributionError`); OCI spec terminology verbatim for on-wire types
- **Imports**: `use` statements; group std > external > crate; no inline paths
- **Docs**: every `.rs` file gets `//!`; all `pub` items get `///`
- **Errors**: invalid user config returns `Result`, never silently degrades
- **Dependencies**: `default-features = false` everywhere; justify every new dep; prefer hand-written under ~100 lines over a crate
- **Crypto**: `aws-lc-rs` is the sole TLS crypto provider (FIPS and non-FIPS). `ring` and `native-tls` are banned in `deny.toml`. `reqwest` uses `rustls-no-provider`; the provider is installed via `ocync_distribution::install_crypto_provider()` at process startup.
- **Process control**: return `ExitCode` via `Termination`; never `process::exit()`
- **Concurrency model**: single-threaded tokio (`current_thread`). All shared state uses `Rc<RefCell<>>`, never `Arc<Mutex<>>`. See `crates/ocync-sync/CLAUDE.md` for full rules.

## Testing

- Unit-test leaves, integration-test bridges (client -> engine, HTTP -> AIMD, cache -> target HEAD). A bug between layers is the most common bug.
- See crate CLAUDE.md files for crate-specific testing guidance.

## Git workflow

- One PR at a time, merge to main, then next. No stacked PRs ever.
- When dispatching parallel worktree agents, each MUST create its own branch from `main` (`git checkout -b feat/xxx main`). Never push to an existing branch from a worktree agent.
- Never include `Co-Authored-By: Claude` or Anthropic attribution
- Run the CI gate locally before push: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test && cargo deny check`
- During rebase conflicts on `Cargo.lock`, regenerate with `git checkout --theirs Cargo.lock && cargo generate-lockfile`
- Never squash commits with `git reset --soft` when intermediate commits touch the same files -- content from middle commits is silently dropped. Use `git rebase -i` with fixup/squash instead.
- Never squash commits with `git reset --soft` when intermediate commits touch the same files -- content from middle commits is silently dropped. Use `git rebase -i` with fixup/squash instead.

## Plans and specs

- `docs/src/content/design/overview.md` - full design document (engine architecture, concurrency, cache)
- `docs/src/content/design/engine.md` - pipeline, transfer state cache, AIMD, multi-target reuse
- `docs/src/content/design/benchmark.md` - layered benchmark plan (protocol / throughput / cross-tool)
- `docs/src/content/design/watch-mode.md` - watch mode, discovery optimization, platform filtering
- `docs/superpowers/plans/` (gitignored) - in-flight implementation plans
- `docs/superpowers/specs/` (gitignored) - design specs; delete once fully implemented
- Unimplemented features are marked with `> **Status: Planned.**` in design docs. Remove the marker when implementing -- implemented features are self-evident from code.

When a benchmark or probe run changes our understanding of a registry's behavior, update the relevant per-registry doc in `docs/src/content/registries/` in the same PR as the behavior change.

## Commands

```bash
# CI gate (run before every push)
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test && cargo deny check

# Run all tests
cargo test
```

See crate CLAUDE.md files for crate-specific test commands. See `bench/CLAUDE.md` for benchmark infrastructure.
