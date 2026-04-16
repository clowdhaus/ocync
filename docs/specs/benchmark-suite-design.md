# Benchmark suite design

Comparative benchmark harness that measures ocync against dregsy and regsync on real registries, producing publishable numbers and regression detection.

## Goals

1. **Publishable comparison** — headline table showing wall-clock time, bytes transferred, API request counts, and rate-limit (429) responses across all three tools
2. **Regression detection** — CI-compatible mode that alerts when ocync performance degrades beyond a threshold
3. **Optimization validation** — prove that skip, dedup, mount, and AIMD optimizations take the fast path, not just that the end result is correct

## Image corpus

A curated set of public images from three sources, defined in a single YAML config file.

### Sources

- **Chainguard free images** (cgr.dev/chainguard/) — all available free-tier images. Primary source. Multi-arch, minimal layers.
- **AWS Public ECR** (public.ecr.aws) — amazonlinux, amazoncorretto, ubuntu, aws-cli, lambda base images, EKS distro images. Bulk source, no rate limits for authenticated AWS users.
- **Docker Hub** — 3-5 popular images only (alpine, nginx, redis). Proves cross-registry pulls. Kept minimal to stay within unauthenticated rate limits.

### Corpus config format

```yaml
corpus:
  target_registry: "${BENCH_TARGET_REGISTRY}"
  target_prefix: "bench"

images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]

  - source: "cgr.dev/chainguard/python"
    tags: ["latest", "3.12"]

  - source: "public.ecr.aws/amazonlinux/amazonlinux"
    tags: ["2023", "2"]

  - source: "docker.io/library/alpine"
    tags: ["3.20", "latest"]
```

Environment variables (`${VAR}` syntax) are expanded before parsing. The `BENCH_TARGET_REGISTRY` variable must be set to the target ECR registry hostname.

A separate overrides file (`bench/corpus-partial-overrides.yaml`) defines tag replacements for the partial sync scenario, avoiding full corpus duplication.

### Scaling curve

No separate tiers. The harness runs progressively larger subsets of the full corpus (10, 25, 50, all) to produce a scaling curve. The curve shows how each tool's wall-clock time grows with image count.

## Target environment

- **Registry**: ECR in a single fixed region (us-east-1)
- **Clean state per run**: all `bench/*` target repos are deleted before each tool runs, ensuring no tool benefits from a previous tool's pushes
- **Repo pre-creation**: all target repos are created before each tool's run, since ocync does not auto-create repos and the comparison must be fair
- **Cooldown**: 30-second pause between tool runs to prevent one tool's rate-limit penalties from affecting the next

## Benchmark runner

EC2 instance in the same region as the target ECR. The benchmark runs on multiple instance sizes to profile ocync's resource requirements — this data directly informs Kubernetes resource requests/limits and Helm chart defaults.

### Instance matrix

The matrix spans instance families and generations to determine whether ocync is CPU-bound, memory-bound, or network-bound, and to validate that performance holds on current-generation hardware.

| Instance | Family | vCPU | Memory | Network | Purpose |
|----------|--------|------|--------|---------|---------|
| c6in.large | Compute (6th gen) | 2 | 4 GB | 25 Gbps | Baseline — compute-optimized, high network |
| c7g.large | Compute (Graviton3) | 2 | 4 GB | 12.5 Gbps | ARM comparison — cost efficiency test |
| m7i.large | General (7th gen) | 2 | 8 GB | 12.5 Gbps | Memory headroom — does 2x RAM help? |
| m7g.large | General (Graviton3) | 2 | 8 GB | 12.5 Gbps | ARM + memory — best cost/perf candidate |
| c6in.xlarge | Compute (6th gen) | 4 | 8 GB | 30 Gbps | vCPU scaling — does more CPU help? |

Selection criteria:
- **No burstable instances** (t-series): baseline networking is ~1.5 Gbps, a sustained benchmark would exhaust network credits mid-run.
- **Both x86 and ARM**: Graviton instances are 20-40% cheaper. If ocync performs equally well, that's a strong Helm chart recommendation.
- **Compute vs general purpose**: determines whether the bottleneck is CPU (compute wins) or memory (general purpose wins). For an I/O-heavy tool like ocync, neither may matter — network and registry latency likely dominate.
- **Same vCPU count across families** (2 vCPU for most): isolates the family/generation variable rather than conflating it with core count. c6in.xlarge (4 vCPU) tests whether more cores help concurrency.

The tool comparison (ocync vs dregsy vs regsync) runs on a single fixed instance (c6in.large). The resource profiling runs ocync-only across all instance sizes to find the sweet spot.

### EBS volume

All instances use a gp3 volume with provisioned performance to ensure disk is never the bottleneck:

- **Volume type**: gp3
- **Size**: 100 GB (sufficient for staging, tool binaries, and results)
- **IOPS**: 6,000 (2x default 3,000 — headroom for concurrent blob staging writes)
- **Throughput**: 400 MB/s (3.2x default 125 MB/s — must exceed network throughput to disk)

The harness records EBS CloudWatch metrics (VolumeReadOps, VolumeWriteOps, VolumeThroughputPercentage) to verify the volume was not the bottleneck. If any run shows throughput at >80% of provisioned, the report flags it.

Cost: ~$12/month for the volume, but it's only alive during benchmark runs.

### Resource profiling

During each ocync run, the harness captures:

- **CPU utilization** — sampled via `/proc/stat` or `sysinfo` crate at 1-second intervals
- **RSS memory** — peak and average, sampled from `/proc/<pid>/status`
- **Network throughput** — bytes/sec from `/proc/net/dev` at 1-second intervals
- **Disk I/O** — read/write bytes/sec and IOPS from `/proc/diskstats` at 1-second intervals

These time-series go into the report alongside the benchmark results. The Helm chart defaults come from the smallest instance size that completes within 10% of the largest instance's wall-clock time.

### Cost

Instance costs range $0.07–$0.36/hr. The full matrix (5 instance sizes x benchmark suite) completes in under 4 hours, costing roughly $3-6 total including EBS. Graviton instances (c7g, m7g) are ~20% cheaper than their x86 equivalents.

### Reproducibility

Instance type, region, AMI, kernel version, EBS volume config (type/IOPS/throughput), and ENA driver version recorded in report metadata.

### Orchestration

Two modes: interactive (primary), CI (future).

**Interactive mode (what we build first):**

1. `terraform apply` in `bench/terraform/` provisions the environment:
   - VPC with private subnets, public subnets, NAT gateway, and ECR VPC endpoints
   - EC2 instance with user data that installs: Rust (rustup), Go (for dregsy/regsync), mitmproxy (pip), git
   - IAM instance profile with ECR full access + SSM access
   - gp3 volume (100 GB, 6,000 IOPS, 400 MB/s)
2. `aws ssm start-session --target <instance-id>` to connect (no SSH keys)
3. Clone the repo, `cargo build --release`, install dregsy/regsync via `go install`
4. `cargo xtask bench all` runs the full suite
5. Pull results off the instance via SSM or `scp`
6. `terraform destroy` tears everything down

Building on the instance avoids cross-compilation from macOS to Linux. SSM avoids SSH key management — authentication is IAM-based.

**CI mode (future addition):**

GitHub Actions workflow that automates the same flow: `terraform apply` → SSM Run Command to execute the benchmark → pull results → `terraform destroy`. Triggered manually or on a schedule (weekly). Not in scope for the initial implementation.

### Terraform resources

All Terraform config lives in `bench/terraform/`, using `terraform-aws-modules`:

- `clowdhaus/tags/aws` — organization-standard tags (environment, repository, created_by, deployed_by)
- `terraform-aws-modules/vpc/aws` — VPC with private subnets, public subnets, single NAT gateway, and VPC endpoints for ECR (`ecr.api`, `ecr.dkr` interface endpoints, `s3` gateway for ECR's blob storage). NAT gateway provides internet access for pulling from public registries (cgr.dev, docker.io, public.ecr.aws). VPC endpoints give private-network access to the target ECR registry.
- `terraform-aws-modules/vpc/aws//modules/vpc-endpoints` — ECR and S3 VPC endpoints with module-managed security group
- `terraform-aws-modules/ec2-instance/aws` — benchmark instance with user data, gp3 volume config, security group, IAM role + instance profile (ECR full access and SSM managed policy) — all created by the module

Files:
- `main.tf` — locals, module calls, data sources
- `outputs.tf` — instance ID (for SSM)
- `user-data.sh` — bootstrap script (install Rust, Go, mitmproxy, system deps)

## Metrics collection

### mitmproxy

All three tools route traffic through mitmproxy (mitmdump CLI mode) via `HTTPS_PROXY` environment variable with mitmproxy's CA cert installed. A small Python addon script (unavoidable — mitmproxy addons are Python) writes structured JSON logs.

Each log entry:

```json
{
  "timestamp": "2026-04-15T10:30:01.123Z",
  "method": "HEAD",
  "url": "/v2/nginx/manifests/latest",
  "request_bytes": 245,
  "response_bytes": 0,
  "status": 200,
  "duration_ms": 47
}
```

### Metrics derived from proxy logs

| Metric | What it proves |
|--------|---------------|
| Total requests by method (HEAD/GET/PUT/POST/PATCH) | API efficiency — fewer calls = smarter tool |
| GET requests for blobs already at target | Dedup failure — ocync should have near-zero |
| 429 response count | Rate limit handling — ocync AIMD should minimize these |
| Duplicate blob GETs (same digest, multiple requests) | Pull-once fan-out — ocync should pull each blob exactly once |
| Total bytes in + out | Network efficiency |
| Request timing distribution | Concurrency behavior — parallel tools show overlapping requests |

### ocync-specific metrics

ocync's `--json` output provides blob stats (transferred/skipped/mounted), discovery cache hits/misses, per-image durations, and aggregate stats. These go into the report as "ocync self-reported" alongside the proxy-measured numbers. The proxy numbers are the apples-to-apples comparison; `--json` numbers are bonus detail.

### dregsy/regsync metrics

Wall-clock time plus whatever they print to stdout/stderr. The proxy log is the primary data source for these tools since their output is unstructured.

## Benchmark harness

Rust xtask crate at `xtask/` in the workspace root. Uses `aws-sdk-ecr` for repo creation/deletion (no `aws` CLI dependency).

### Usage

```bash
cargo xtask bench cold
cargo xtask bench warm
cargo xtask bench partial
cargo xtask bench scale
cargo xtask bench all
cargo xtask bench --tools ocync,regsync    # subset of tools
cargo xtask bench --limit 10              # first 10 images only
cargo xtask bench --regression             # ocync-only, compare against baseline
```

### Tool prerequisites

The harness validates before starting:

- **ocync**: built from the workspace (`cargo build --release`)
- **dregsy**: Go binary on `$PATH` (installed via `go install` or GitHub release)
- **regsync**: Go binary on `$PATH` (installed via `go install` or GitHub release)
- **mitmdump**: mitmproxy CLI on `$PATH`
- **AWS credentials**: valid, ECR accessible in target region

Tool versions are recorded in the report metadata.

### Config generation

The harness reads the corpus YAML and generates equivalent config files for each tool:

- `ocync.yaml` — ocync native config
- `dregsy.yaml` — dregsy task-based config
- `regsync.yaml` — regsync sync config

Same images, same source, same target, generated from one source of truth.

## Benchmark scenarios

### Cold sync

Clean target, full sync. Measures raw transfer performance.

**Per tool:**
1. Create all `bench/*` ECR repos
2. Start mitmdump
3. Run tool, capture wall-clock time + stdout/stderr
4. Stop mitmdump, collect log
5. Delete all `bench/*` repos
6. 30-second cooldown

Run 3 times per tool, report median.

### Warm sync

Target already has everything, nothing changed. Measures skip optimization.

**Per tool:**
1. Create repos, run tool once (prime the target)
2. Reset mitmproxy log
3. Run tool again, capture second-run metrics
4. Delete repos + cooldown

Single iteration (variance is in the cold run, not the skip logic).

### Partial sync

5% of tags changed between runs. Measures incremental sync.

**Per tool:**
1. Create repos, run full sync with the base corpus
2. Swap to a second corpus config that replaces ~5% of tags with different tags from the same images (e.g., `python:3.12` becomes `python:3.11`). The harness ships both configs; the "changed" config is static, not randomly generated.
3. Reset mitmproxy log
4. Run tool with the changed corpus, capture metrics
5. Delete repos + cooldown

### Scale benchmark

ocync only by default (competitors optional). Measures how wall-clock time and request count grow with image count.

Run cold sync at 10, 25, 50, all images. Collect data points for the scaling curve.

### Regression mode

ocync only. Compares current run against a stored baseline. Exits non-zero if wall-clock time or request count regresses by more than a configurable threshold (default 20%). Designed for CI.

## Report output

### Summary table (publishable)

```
| Metric                   | ocync  | regsync | dregsy  |
|--------------------------|--------|---------|---------|
| Cold sync (median)       | 47s    | 3m 12s  | 8m 41s  |
| Warm sync (no changes)   | 3s     | 38s     | 1m 22s  |
| Partial sync (5% changed)| 12s    | 52s     | 2m 10s  |
| Total bytes transferred  | 2.1 GB | 4.3 GB  | 8.4 GB  |
| API requests (cold)      | 847    | 2,100   | 4,200   |
| API requests (warm)      | 203    | 890     | 4,200   |
| 429 responses            | 12     | 87      | 340     |
| Duplicate blob fetches   | 0      | 34      | 189     |
```

Numbers above are illustrative, not predictions.

### Detailed output

Written to `bench-results/YYYY-MM-DD-HHMMSS/`:

- `summary.md` — comparison table plus environment metadata (region, instance type, corpus size, tool versions)
- `cold.json`, `warm.json`, `partial.json` — raw run data per tool (wall-clock, proxy metrics, ocync JSON report)
- `scale.json` — scaling curve data points (image count to wall-clock per tool)
- `proxy-logs/` — raw mitmproxy JSON logs per tool per run (for auditing)

### Scaling curve data

```json
[
  {"images": 10, "results": {"ocync": 8.0}},
  {"images": 25, "results": {"ocync": 15.0}}
]
```

No chart rendering in the xtask (no plotting dependency). The structured JSON is designed for a gnuplot one-liner, matplotlib script, or GitHub Actions rendering step.

### Resource profile (Kubernetes sizing)

Per-instance-size summary for ocync runs:

```
| Instance       | Wall-clock | Peak CPU | Peak RSS | Avg Net MB/s | Recommended |
|----------------|------------|----------|----------|--------------|-------------|
| c6in.large     |            |          |          |              |             |
| c7g.large      |            |          |          |              |             |
| m7i.large      |            |          |          |              |             |
| m7g.large      |            |          |          |              |             |
| c6in.xlarge    |            |          |          |              |             |
```

Table populated after first benchmark run. The "recommended" marker goes on the smallest instance where adding more resources yields less than 10% improvement — this becomes the Helm chart default for `resources.requests`.

Detailed time-series data (CPU, memory, network at 1-second intervals) written to `bench-results/YYYY-MM-DD-HHMMSS/resource-profiles/` as JSON per instance size.

## ECR management

All ECR operations use `aws-sdk-ecr` from the xtask crate.

- **Pre-create repos**: `CreateRepository` for each `bench/*` target before each tool run
- **Delete repos**: `DeleteRepository` with `force: true` for all `bench/*` targets after each tool run
- **Credential validation**: `GetAuthorizationToken` at startup to fail fast on bad credentials
- **No other AWS API usage**: all measurement comes from mitmproxy and tool output
