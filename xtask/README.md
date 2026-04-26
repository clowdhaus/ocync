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
cargo xtask bench-remote --provider aws --scenario cold
```

Pushes the current branch, SSHs into the bench instance, builds, runs benchmarks with live streamed output, then SCPs results back.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `--provider` | string | (required) | Cloud provider; reads `bench/terraform/<provider>/bench.json` |
| `--scenario` | string | `all` | Scenario to run (passed through to `bench`) |
| `--tools` | string | `ocync,dregsy,regsync` | Tools to benchmark |
| `--iterations` | string | `1` | Cold-sync iterations (picks median) |
| `--limit` | integer | all | Use first N corpus images only |
| `--fetch` | flag | | Fetch results from last run without re-running |
| `--force` | flag | | Kill any running benchmark and start fresh |
| `--git-ref` | string | current branch | Git ref to checkout on the instance |
| `--output` | path | `bench/results` | Local directory to save fetched results |

### `cargo xtask bench`

Runs the benchmark suite directly on the current machine (intended for execution on the bench instance itself, not locally).

```bash
cargo xtask bench [OPTIONS] <SCENARIO>
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `<SCENARIO>` | subcommand | (required) | `cold`, `warm`, `partial`, `scale`, or `all` |
| `--tools` | comma-separated | `ocync,dregsy,regsync` | Tools to benchmark |
| `--corpus` | path | `bench/corpus.yaml` | Image corpus YAML |
| `--partial-overrides` | path | `bench/corpus-partial-overrides.yaml` | Partial sync tag overrides |
| `--limit` | integer | all | Use first N corpus images only |
| `--iterations` | integer | `1` | Cold scenario iterations (picks median by wall-clock) |
| `--proxy-port` | u16 | `8080` | Bench-proxy listen port |
| `--proxy-binary` | path | `target/release/bench-proxy` | Path to bench-proxy binary |
| `--proxy-ca` | path | `/etc/bench-proxy/ca.pem` | Bench-proxy CA certificate |
| `--proxy-ca-key` | path | `/etc/bench-proxy/ca-key.pem` | Bench-proxy CA private key |
| `--output` | path | `bench/results/<timestamp>` | Output directory for results |
| `--regression` | flag | | Compare against baseline, exit non-zero on regression |
| `--threshold-pct` | u32 | `20` | Regression threshold percentage |

## Scenarios

| Scenario | Description |
|----------|-------------|
| `cold` | Clean target, full sync -- measures raw transfer performance |
| `warm` | Re-sync with nothing changed -- measures skip optimization |
| `partial` | Re-sync after ~5% of tags changed -- measures incremental sync |
| `scale` | Increasing corpus sizes (10, 25, 50, all) -- ocync only |
| `all` | Run cold, warm, partial, scale in sequence |

## Usage examples

```bash
# Spin up infrastructure
cd bench/terraform/aws && terraform init && terraform apply

# Quick smoke test (1 tool, 3 images, 1 iteration)
cargo xtask bench-remote --provider aws --tools ocync --limit 3 --scenario cold

# Full 3-tool cold comparison (3 iterations, picks median)
cargo xtask bench-remote --provider aws --iterations 3 --scenario cold

# All scenarios
cargo xtask bench-remote --provider aws --scenario all

# Fetch results from last run without re-running
cargo xtask bench-remote --provider aws --fetch

# Kill stuck benchmark and start fresh
cargo xtask bench-remote --provider aws --force --scenario cold

# Regression check against baseline (on instance)
cargo xtask bench --tools ocync --regression --threshold-pct 15 cold

# Tear down
cd bench/terraform/aws && terraform destroy
```

## Output

| File | Description |
|------|-------------|
| `bench/results/<timestamp>/summary.md` | Comparison table with per-scenario metrics |
| `bench/results/<timestamp>/<tool>-proxy.jsonl` | Raw per-request proxy log per tool |
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
