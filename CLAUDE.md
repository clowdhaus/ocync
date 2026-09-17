# ocync

OCI registry sync tool. Crates: `ocync` (CLI), `ocync-distribution` (registry client), `ocync-sync` (sync engine), plus `bench/proxy` and `xtask`.

Read the nested file for the area you touch:

- `crates/ocync-distribution/CLAUDE.md`: auth, AIMD, registry detection, upload quirks, testing
- `crates/ocync-sync/CLAUDE.md`: concurrency and `RefCell` rules, notify contracts, leader-follower, engine
- `bench/CLAUDE.md`: benchmark infrastructure, bench-proxy, competitor config, instance ops
- `docs/CLAUDE.md`: link convention, link plugin, GitHub Pages trailing-slash bugs

## Priorities, in order

1. Bytes transferred and rate-limit friendliness. Wall-clock follows from this.
2. Correctness. An optimization must degrade safely and visibly.
3. Wall-clock. Never report it without byte and request counts.
4. UX, at zero cost when disabled.

## Invariants

- **Content is synced bit-for-bit.** Never convert, transform or rewrite manifest or blob bytes, including Docker v2 to OCI. Digests are identity, and every skip optimization depends on them.
- **One failure never cancels the run.** `SyncReport` is the contract. No `?` on a per-mapping or per-image path in `synchronize::run`, `resolve_all` or `analyze::run`. A per-registry client or batch checker that fails to construct fails only the mappings using it, storing the classified error against the alias. Isolated failures still reach the exit code, the cycle tail and the `--json` document. Optional optimizations degrade with a WARN, required work does not.
- **Single-threaded tokio** (`current_thread`): shared state is `Rc<RefCell<>>`, never `Arc<Mutex<>>`.
- **Crypto**: `aws-lc-rs` is the only TLS provider. `ring` and `native-tls` are banned in `deny.toml`, and `ocync_distribution::install_crypto_provider()` installs the provider at startup.
- The only Cargo features are `fips` and `non-fips`. Dependencies use `default-features = false`.
- Return `ExitCode` via `Termination`, never `process::exit()`.
- Every `.rs` file has `//!` and every `pub` item has `///`. Types do not stutter (`Error`, not `DistributionError`).

## Tests

Every change carries a negative assertion: a test that fails if the intended path is NOT taken. Unit-test leaves and integration-test the bridges (client to engine, HTTP to AIMD, cache to target HEAD).

## Workflow

- Gate before push: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && cargo deny check`.
- One pull request at a time, merged before the next. Never stack.
- On a `Cargo.lock` rebase conflict: `git checkout --theirs Cargo.lock && cargo generate-lockfile`.
- Design lives in `docs/src/content/design/`. Unbuilt features carry `> **Status: Planned.**`, removed when implemented.
- A run that changes what we know about a registry updates its page in `docs/src/content/registries/` in the same pull request.
