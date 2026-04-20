# bench/

Benchmark infrastructure for comparing ocync against dregsy and regsync.

## Prerequisites

- `~/.ssh/id_ed25519` exists, public key added to GitHub
- AWS credentials with ECR access + SSM parameter read (Docker Hub creds)
- Terraform installed
- SSM parameters populated:
  - `/ocync/bench/dockerhub-access-token` - Docker Hub PAT for authenticated pulls
  - `/ocync/bench/dockerhub-username` - Docker Hub account name

## Quick start

```bash
# 1. Spin up the bench instance
cd bench/terraform/aws && terraform init && terraform apply

# 2. Run benchmarks (reads bench.json automatically)
cargo xtask bench-remote --provider aws --scenario sync

# 3. Tear down when done
cd bench/terraform/aws && terraform destroy
```

## bench-remote

Single command for the full benchmark cycle.

```bash
# Full 3-tool cold+warm comparison
cargo xtask bench-remote --provider aws --scenario sync

# Quick smoke test
cargo xtask bench-remote --provider aws --scenario sync \
  --tools ocync --limit 3

# All scenarios (cold+warm + partial + scale)
cargo xtask bench-remote --provider aws --scenario all

# Fetch results from the last run without re-running
cargo xtask bench-remote --provider aws --fetch
```

**What it does:**
1. `git push` the current branch
2. SSH into the instance (using `~/.ssh/id_ed25519`)
3. Pull code, build, run benchmarks with live streamed output
4. SCP results back: `summary.md` and historical JSON archive

**Output:**
- `bench/results/<timestamp>/summary.md` - human-readable comparison table with winners bolded
- `bench/results/{registry}.json` - compact per-registry run record archive (tracked in git)

## Metrics captured per tool per scenario

| Metric | Source | Winner = |
|--------|--------|----------|
| Wall clock | Process timing | lowest |
| Peak RSS | `/usr/bin/time -v` (kernel maxrss) | lowest |
| Requests | Bench-proxy JSONL | lowest |
| Response bytes | Bench-proxy JSONL | lowest |
| Source blob GETs | Bench-proxy (non-target host) | lowest |
| Source blob bytes | Bench-proxy (non-target host) | lowest |
| Mounts (success/attempt) | Bench-proxy (POST with mount=) | highest |
| Duplicate blob GETs | Bench-proxy (same URL fetched twice) | lowest |
| Rate-limit 429s | Bench-proxy (HTTP 429 responses) | lowest |

Instance metadata (type, CPU, memory, network, region) is captured from `ec2:DescribeInstanceTypes`.

## Running on the instance directly

```bash
# Connect
ssh ec2-user@<public-ip>

# On the instance:
cd ~/ocync
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
export BENCH_TARGET_REGISTRY=${ACCOUNT}.dkr.ecr.us-east-1.amazonaws.com
cargo xtask bench --tools ocync,dregsy,regsync sync
```

## Instance details

| What | Path |
|------|------|
| Source code | `/home/ec2-user/ocync/` |
| Cargo binaries | `/home/ec2-user/.cargo/bin/` |
| Bench-proxy CA | `/etc/bench-proxy/ca.pem`, `/etc/bench-proxy/ca-key.pem` |
| Competitor tools | `/usr/local/bin/{dregsy,regsync,skopeo}` |
| Results | `/home/ec2-user/ocync/bench/results/<timestamp>/` |
| Run records | `/home/ec2-user/ocync/bench/results/{registry}.json` |

`BENCH_TARGET_REGISTRY` is set automatically by `bench-remote`. When running manually, derive it from `aws sts get-caller-identity`.

## Instance lifecycle

Managed by Terraform in `bench/terraform/aws/`. `user_data_replace_on_change = true` -- bootstrap changes recreate the instance.

```bash
cd bench/terraform/aws && terraform init && terraform apply   # create
cd bench/terraform/aws && terraform destroy                    # destroy
```

The IAM role includes: `AmazonEC2ContainerRegistryFullAccess`, `ec2:DescribeInstanceTypes`, and ECR public auth.

## Security notes

- **SSH key in user-data**: The operator's `~/.ssh/id_ed25519` private key is templated into EC2 user-data via Terraform and stored in Terraform state (S3, encrypted). Acceptable for a single-operator throwaway instance. The same key grants write access to the repo via GitHub SSH -- use a dedicated deploy key if this infrastructure is ever extended to multiple operators.
- **IAM `AmazonEC2ContainerRegistryFullAccess`**: Intentionally broad because bench scenarios create and delete ECR repos (`ecr::create_repos` / `ecr::delete_repos`). Not scoped to specific repos because repo names are derived from the corpus at runtime.
- **Docker Hub token in `.bench-env`**: Credentials are written to `~/.bench-env` (chmod 600) and sourced from `.bashrc`. Visible to any process running as ec2-user. Acceptable for single-operator ephemeral instances.
- **SSH `StrictHostKeyChecking=accept-new`**: First-connect TOFU since the instance is recreated on every `terraform apply`. Host key is not verified against a known value.

## Terraform structure

Each cloud provider gets its own directory:

```
bench/terraform/
  aws/     # VPC, public subnet, VPC endpoints, EC2, SSH key pair
  gcp/     # (planned, does not exist yet) VPC, GCE, Private Google Access, GAR
  azure/   # (planned, does not exist yet) VNet, VM, ACR private endpoint
```

Terraform writes `bench.json` to the provider directory with connection details. The xtask reads this file -- no manual copy-paste of IPs.

## Analyzing proxy logs

Raw per-request data lives in `<tool>-proxy.jsonl` files in `bench/results/<timestamp>/`.

```bash
# Count requests by method
jq -r '.method' <tool>-proxy.jsonl | sort | uniq -c | sort -rn

# Source blob pulls only (non-ECR hosts)
jq -r 'select(.method=="GET" and (.url | contains("/blobs/sha256:")) and (.host | contains("ecr") | not))' <tool>-proxy.jsonl | wc -l

# Mount attempts and success rate
jq -r 'select(.url | contains("mount="))' <tool>-proxy.jsonl | jq -r '.status' | sort | uniq -c

# Total response bytes
jq '[.response_bytes] | add' <tool>-proxy.jsonl
```

## Run record archive

Each benchmark run appends a compact record to `bench/results/{registry}.json` (e.g. `ecr.json`). These files are tracked in git for longitudinal comparison across machines, cloud providers, and code versions.

The registry key is derived from the target registry hostname:
- `*.ecr.*.amazonaws.com` -> `ecr.json`
- `gcr.io` / `*.gcr.io` / `*-docker.pkg.dev` -> `gcr.json`
- `*.azurecr.io` -> `acr.json`
- Anything else -> `other.json`

Each record includes: timestamp, git ref, machine metadata (provider, instance type, arch, vCPUs, memory, network, region), corpus size, and all 9 per-tool metrics per scenario.

## Competitor config gotchas

- **regsync `type: image` resolves `:latest` first**: When the source has no tag (e.g. `docker.io/nvidia/cuda`), regsync does a manifest HEAD on `:latest` before checking the `tags.allow` list. Images without a `latest` tag (nvidia/cuda, pytorch/pytorch) produce 404 errors in the log. Non-fatal but adds wasted requests to the benchmark metrics.
- **regsync `repoAuth: true`**: Required for registries that issue per-scope tokens (cgr.dev, gcr.io, nvcr.io). Without it, regsync silently fails auth on the second repo from the same registry.
- **dregsy `auth-refresh: 12h`**: Required for ECR targets. Without it, dregsy's ECR token expires mid-run on large corpora. The 12h matches ECR token lifetime.
- **dregsy JSON parse errors**: dregsy occasionally logs `invalid character 'b' looking for beginning of value` when a registry returns a non-JSON error body (e.g. HTML rate limit page). Non-fatal; dregsy retries and continues.
