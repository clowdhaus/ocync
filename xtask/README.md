# xtask

Workspace automation via `cargo xtask`. Provides benchmark orchestration for comparing ocync against dregsy and regsync.

## Prerequisites

- Terraform installed
- Git push access to the repo (xtask pushes the current branch from the operator's machine)
- OpenSSH public key authorized for instance access, supplied to Terraform via `terraform.tfvars` or `TF_VAR_ssh_public_key`
- AWS credentials with ECR access + SSM parameter read
- SSM parameters populated:
  - `/ocync/bench/dockerhub-username` - Docker Hub account name
  - `/ocync/bench/dockerhub-access-token` - Docker Hub PAT for authenticated pulls

Docker Hub credentials can alternatively be provided via `DOCKERHUB_USERNAME` and `DOCKERHUB_ACCESS_TOKEN` environment variables.

## Commands

### `cargo xtask bench-remote`

Primary entry point -- orchestrates benchmarks on a remote cloud instance via SSH.

```bash
cargo xtask bench-remote --provider aws --scenario sync
```

Pushes the current branch, SSHs into the bench instance, builds, runs benchmarks with live streamed output, then SCPs results back.

A run already in progress on the instance, including one still checking out or building, is refused: the command prints that and fails, leaving the run alone. `--force` kills the run and everything it started, then begins fresh. `--fetch` copies the newest results on the instance, which are a live run's partial results only once its benchmark has started (while it checks out or builds, they are the previous run's). It cannot be combined with any option that shapes a run.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `--provider` | string | (required) | Cloud provider; reads `bench/terraform/<provider>/bench.json` |
| `--scenario` | string | `all` | Scenario to run (passed through to `bench`) |
| `--tools` | string | `ocync` | Tools to benchmark |
| `--rounds` | integer, at least 3 | `5` | Rounds of the sync and partial scenarios (see [Rounds](#rounds-and-the-baseline-build)) |
| `--baseline-ref` | commit | | Also run the ocync built from this commit (see [Rounds](#rounds-and-the-baseline-build)) |
| `--limit` | integer | all | Use first N corpus images only |
| `--skip-registries` | string | | Comma-separated source registries to skip |
| `--skip-prewarm` | flag | | Skip CDN pre-warming (it otherwise runs for at most 8 minutes, then waits 6 for the CDN after any miss) |
| `--no-proxy` | flag | | Run without bench-proxy on the instance |
| `--fetch` | flag | | Copy the newest results on the instance without running (a live run's partial results once its benchmark has started); conflicts with every run option |
| `--force` | flag | | Kill a running benchmark and start fresh; without it a live run is refused |
| `--git-ref` | string | current branch | Git ref to checkout on the instance |
| `--output` | path | `bench/results` | Local directory to save fetched results |

### `cargo xtask bench-lock`

Resolves every source tag in the corpus and the partial overrides to its manifest digest, the digests of the manifests an index lists, and the config and layer blobs of each, and writes the lock beside the corpus, `bench/corpus.lock.json` by default. `bench` refuses to run when a source no longer matches the lock, checks synced content against it by digest, and warms the CDN with the blobs the lock records. Needs the network (and optionally Docker Hub credentials); no cloud write.

```bash
cargo xtask bench-lock
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `--corpus` | path | `bench/corpus.yaml` | Image corpus YAML |
| `--partial-overrides` | path | `bench/corpus-partial-overrides.yaml` | Partial sync tag overrides, locked too |

Re-running over unchanged sources leaves the file as it is. A new lock starts a new comparison series: run records carry its `locked_at` as `corpus_locked_at`.

### `cargo xtask bench`

Runs the benchmark suite directly on the current machine (intended for execution on the bench instance itself, not locally).

```bash
cargo xtask bench [OPTIONS] <SCENARIO>
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `<SCENARIO>` | subcommand | (required) | `sync`, `partial`, `scale`, `prepare`, or `all` |
| `--tools` | comma-separated | `ocync` | Tools to benchmark: `ocync`, `ocync-baseline` (needs `--baseline-ref`), `dregsy`, `regsync` |
| `--rounds` | integer, at least 3 | `5` | Rounds of the sync and partial scenarios, each running every tool once in a fresh random order |
| `--baseline-ref` | hex commit id | | Also run the ocync built from this commit in every round, and compare it with the current one |
| `--corpus` | path | `bench/corpus.yaml` | Image corpus YAML |
| `--partial-overrides` | path | `bench/corpus-partial-overrides.yaml` | Partial sync tag overrides |
| `--limit` | integer | all | Use first N corpus images only |
| `--proxy-port` | u16 | `8080` | Bench-proxy listen port |
| `--proxy-binary` | path | `target/release/bench-proxy` | Path to bench-proxy binary |
| `--proxy-ca` | path | `/etc/bench-proxy/ca.pem` | Bench-proxy CA certificate |
| `--proxy-ca-key` | path | `/etc/bench-proxy/ca-key.pem` | Bench-proxy CA private key |
| `--output` | path | `bench/results/<timestamp>` | Output directory for results |
| `--regression` | flag | | Compare against baseline, exit non-zero on regression |
| `--threshold-pct` | u32 | `20` | Regression threshold percentage |

## Rounds and the baseline build

One run per tool cannot separate a difference between tools from run-to-run noise. The sync and partial scenarios therefore run in `--rounds` rounds (5 by default, never fewer than 3): each round runs every tool once, cold then warm, in a fresh random order. The source tags are checked against the lock again before every round after the first. The pre-warm runs once, before round 1. Scale and prepare run ocync alone and run one pass whatever `--rounds` says.

**Rounds multiply run time.** A full comparison at the default is five times one pass, and every round deletes and recreates the target repositories and waits 10 s between tools. Use `--rounds 3` for a smoke test.

`summary.md` and the performance page show each metric as its median with the lowest and highest round, and, for each other tool, the median and range of the per-round ratio of that tool to ocync. The range holds the true median ratio with a confidence of `1 - 2 * 0.5^n` (75% at 3 rounds, 93.75% at 5), stated beside it. A difference is claimed only when every round agrees; otherwise it reads "no measured difference". Wall clock, requests and response bytes are compared this way. The run record keeps every round's results with its index, order and seed.

`--baseline-ref <commit>` adds the tool `ocync-baseline` (shown as `ocync@<short id>`), the ocync built from that commit, to every round. A short hex id is resolved to the full one before anything runs, and a branch name is refused. The harness checks the commit out as a git worktree in `bench/.baseline/<id>`, builds it there with its own target directory, and reuses both on a later run at the same commit. It runs the baseline with the config it generates for ocync, so a commit that does not build, or whose `ocync validate` rejects that config, fails the run before the pre-warm. The comparison against `ocync` in the summary is the regression check; `--regression` and `bench/results/baseline.json` keep working and now compare ocync's median over the rounds. Only a commit with `ocync validate` and the `sync --json` flags can be a baseline, and on `bench-remote` the commit must be one the instance can fetch, such as an ancestor of the pushed branch.

## Scenarios

| Scenario | Description |
|----------|-------------|
| `sync` | Clean target, full sync (cold), then a re-sync with nothing changed (warm) |
| `partial` | Re-sync after ~5% of tags changed -- measures incremental sync |
| `scale` | Increasing corpus sizes (10, 25, 50, all) -- ocync only |
| `prepare` | Resolve every mapping and stop -- ocync only |
| `all` | Run every scenario in sequence |

## Usage examples

```bash
# Spin up infrastructure
cd bench/terraform/aws && terraform init && terraform apply

# Quick smoke test (1 tool, 3 images, the fewest rounds)
cargo xtask bench-remote --provider aws --tools ocync --limit 3 --rounds 3 --scenario sync

# Full 3-tool sync comparison (5 rounds)
cargo xtask bench-remote --provider aws --tools ocync,dregsy,regsync --scenario sync

# Has ocync regressed since a commit? Old and new ocync in the same rounds
cargo xtask bench-remote --provider aws --baseline-ref <commit> --scenario sync

# All scenarios
cargo xtask bench-remote --provider aws --scenario all

# Fetch results from last run without re-running
cargo xtask bench-remote --provider aws --fetch

# Kill a running benchmark and start fresh (without --force a live run is refused)
cargo xtask bench-remote --provider aws --force --scenario sync

# Regression check against baseline (on instance)
cargo xtask bench --tools ocync --regression --threshold-pct 15 sync

# Tear down
cd bench/terraform/aws && terraform destroy
```

## Output

| File | Description |
|------|-------------|
| `bench/results/<timestamp>/summary.md` | Comparison table with per-scenario metrics |
| `bench/results/<timestamp>/<tool>-<pass>-proxy.jsonl` | Raw per-request proxy log per tool and pass (e.g. `ocync-scale-10-proxy.jsonl`); for the sync and partial scenarios under `round-<n>/` |
| `bench/results/{registry}.json` | Per-registry run record archive (tracked in git) |
| `bench/results/baseline.json` | Regression baseline (created on first `--regression` run) |

## Metrics

| Metric | Winner = |
|--------|----------|
| Wall clock | lowest |
| Peak RSS | lowest |
| Requests | lowest |
| Response bytes | lowest |
| Source blob GETs | lowest |
| Source blob bytes | lowest |
| Mounts (success/attempt) | highest |
| Duplicate blob GETs | lowest |
| Rate-limit 429s | lowest |
