# Benchmark Work — Handoff

## Current state (at handoff, 2026-04-16)

- **PR #23 is merged to `main`** as squash commit `b469a7c`. Everything
  below ("what's now on main") is live.
- **Live benchmark instance**: `i-042193c339708c702` (c6in.4xlarge,
  us-east-1, private subnet). Bootstrap is complete. It's running the
  code that shipped in `b469a7c`; the `.cargo/bin/ocync` and
  `.cargo/bin/bench-proxy` binaries reflect that.
- **The instance is intentionally NOT torn down.** We want to keep
  iterating. If you need to destroy it, `cd bench/terraform &&
  terraform destroy`.
- **Target state for the benchmark harness is documented** in
  `docs/specs/benchmark-design-v2.md` — read that before continuing.

## What's now on main (shipped in PR #23)

### Ocync bug fixes (user-facing)

- `docker.io` → `registry-1.docker.io` endpoint rewrite in
  `src/cli/mod.rs::endpoint_host`. All CLI commands (sync, copy,
  analyze) auto-translate the canonical hostname to the actual v2 API
  endpoint, while keeping `docker.io` as the auth scope/service name
  (which is what Docker Hub's token server expects). Previously
  `url: docker.io` in a config silently 302'd to `www.docker.com` and
  failed with an HTML-decode error.

### Benchmark infrastructure

- **Pure-Rust `bench-proxy` replaces mitmproxy.** New workspace crate
  at `bench/proxy/` — tokio + rustls/aws-lc-rs + hyper + rcgen. mitmdump
  capped at ~250 Mbps on single-threaded Python TLS; bench-proxy scales
  with cores. Two subcommands: `ca-init` (run once in bootstrap) and
  `serve`. See `bench/proxy/src/{main,ca,proxy}.rs`.
- **Corpus expanded** with Jupyter family (cross-repo mount candidates
  with GB-sized shared layers), `docker.io/nvidia/cuda 12.6.2`, and
  `docker.io/tensorflow/tensorflow *-gpu`. Mount-exercising entries are
  ordered first so `--limit N` hits them.
- **Mount activity is measured.** `ProxyMetrics` now tracks
  `mount_attempts` and `mount_successes`, surfaced as
  `mounts=<succ>/<attempt>` in the per-tool stderr summary.
- **Proxy logs live in the output directory** (`bench-results/<run>/
  {tool}-proxy.jsonl`), not a tempdir that gets auto-deleted.
- **Instance is c6in.4xlarge** (was c6in.large — compile was the
  bottleneck on the smaller size; iteration was painful).

### Design doc

- `docs/specs/benchmark-design-v2.md` — full proposal for the
  layered restructure. Read this first.

## Headline finding from the first instrumented run

**ECR returned 202 Upload Session Started to all 178 cross-repo mount
attempts** in a 6-image Jupyter corpus. Zero 201 successes. The mount
code path is firing correctly (the `from=` parameters point at sibling
bench repos that hold the blob); ECR just isn't fulfilling the mount.
Details: `memory/project_ecr_mount_behavior.md`.

This is the open question for Follow-up PR #1.

## Follow-up work (sequenced PRs per design v2)

### PR #1 — Layer 1 protocol tests + resolve the ECR mount question

**Goal:** Answer definitively whether ECR ever honors OCI cross-repo
mount. If no, ship the short-circuit. Establish the
"every-optimization-has-a-protocol-test" enforcement point.

**Deliverables** (from `docs/specs/benchmark-design-v2.md` §PR #1):

- `xtask probe --registry <host>` subcommand. Creates two ephemeral
  repos, pushes a blob, waits `--delay-secs N`, mounts, prints the
  observed status code, cleans up. There's a scaffold from this session
  that got reverted — reconstruct from the design doc.
- `crates/ocync-distribution/tests/registry2_mount.rs` — testcontainer
  mount assertions against the reference OCI registry. Pins the
  protocol-compliant baseline.
- `crates/ocync-distribution/tests/ecr_mount.rs` — real-ECR
  assertions gated behind `cargo test --features ecr-integration`.
- Short-circuit (conditional on probe result): if ECR always returns
  202, skip the mount attempt entirely for ECR targets in
  `crates/ocync-sync/src/engine.rs::transfer_image_blobs`. Unit test
  that pins the skip.

**Acceptance criteria:** see design doc.

**Starting point on the instance:** the instance already has `xtask`
and `bench-proxy` built. To run the probe (once it exists):
`cargo xtask probe --registry 660548353186.dkr.ecr.us-east-1.amazonaws.com`.

### PR #2 — Layer 2 throughput restructure

**Goal:** remove MITM proxy from measurement path; add pre-warm,
versioned outputs, mandatory ≥3 iterations; ocync-only. Details in
design doc §PR #2.

### PR #3 — Cross-tool comparison extracted

**Goal:** move dregsy/regsync harness into `bench/competitors/`, out
of CI, out of `xtask bench`. Details in design doc §PR #3.

## Things to know before touching the instance

- **Auth to pull the repo on instance is scrubbed by bootstrap.** The
  bootstrap clones with the SSM token, then `git remote set-url origin`
  strips it. To pull updates manually:
  ```bash
  GH=$(aws ssm get-parameter --name /ocync/bench/github-token --with-decryption --query Parameter.Value --output text --region us-east-1)
  cd /home/ec2-user/ocync
  sudo -u ec2-user git -c credential.helper="!f(){ echo username=x; echo password=$GH; };f" fetch origin main
  sudo -u ec2-user git reset --hard origin/main
  ```
- **The instance is on `benchmark-suite`** (the branch we just merged
  from). Before running anything new, reset it to `main`.
- **`/etc/bench-proxy/ca.pem` is in the system trust store.** Don't
  regenerate it unless the proxy binary changes incompatibly.
- **SSM send-command is finicky with shell quoting.** For anything
  non-trivial, write the script to a file, base64-encode, and pipe
  through `base64 -d` on the instance side. Inline bash heredocs in
  JSON parameters hang.

## Memory updates made this session

New entries in `~/.claude/projects/.../memory/`:

- `project_bench_proxy.md` — why and how bench-proxy replaces mitmproxy
- `project_ecr_mount_behavior.md` — the 202-for-all-mounts finding
- `feedback_docker_hub_s3_redirects.md` — proxies must not follow 3xx
- `feedback_rustls_resolver_sync.md` — `ResolvesServerCert::resolve` is
  sync, must use `std::sync::RwLock` for the leaf cert cache
- `feedback_registry_normalization.md` — audit
  docker/skopeo/crane/regclient before shipping a registry client

`CLAUDE.md` updated with bench-proxy rules (forward 3xx don't follow;
leaf cert cache sync; proxy logs in output dir) and the open ECR
mount finding.

## Process notes for the next session

- **Start by reading `docs/specs/benchmark-design-v2.md`.** The plan
  is written; the goal of the next session is to execute PR #1, not
  redesign.
- **Don't rebuild the instance.** Bootstrap takes ~2.5 min on
  c6in.4xlarge; not worth doing unless terraform config changes.
- **PR #1 scope is deliberately narrow.** Probe + 2 test suites +
  (conditional) short-circuit. That's the whole PR. Do not accept
  scope creep into the benchmark restructure.
- **If the probe reveals something unexpected** (e.g., ECR DOES
  return 201 under some conditions), update
  `memory/project_ecr_mount_behavior.md` with the new findings
  before writing any code.

## Things that went well

- Splitting rustfmt drift into a separate commit (`20bea61`) kept the
  actual fix commits (docker.io, bench-proxy, redirect handling,
  sync leaf cache) reviewable.
- The mount-metrics addition was ~10 LoC but produced the single most
  useful result of the session (`mounts=0/178`) — small, focused
  instrumentation >> big frameworks.
- Writing the design doc BEFORE starting the restructure (rather
  than after, post-facto) forced clarity about what each layer is for.

## Things to avoid next time

- Don't chase SSM quoting bugs with inline heredocs; write to a file,
  base64-encode, ship.
- Don't accept a "wall clock improved" signal as proof anything
  else worked — the first successful Jupyter run was 223 s because
  the proxy got out of the way, not because mount started firing.
- Don't reach for `rt.block_on(...)` inside a sync callback. If the
  caller is sync, the state has to be sync too.
