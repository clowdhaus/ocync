# bench/

Benchmark infrastructure for comparing ocync against dregsy and regsync.

## Prerequisites

- AWS credentials with ECR access + `ec2:DescribeInstanceTypes`
- `session-manager-plugin` installed locally (for `bench-remote`)
- SSM parameters populated:
  - `/ocync/bench/github-token` -- GitHub PAT for git fetch (private repo)
  - `/ocync/bench/dockerhub-access-token` -- Docker Hub PAT for authenticated pulls
  - `/ocync/bench/dockerhub-username` -- Docker Hub account name

## Quick start

```bash
# 1. Spin up the bench instance (if not already running)
cd bench/terraform && terraform init && terraform apply

# 2. Get the instance ID
INSTANCE_ID=$(cd bench/terraform && terraform output -raw instance_id)

# 3. Run benchmarks remotely (pushes code, builds, runs, streams live output)
cargo xtask bench-remote --instance-id $INSTANCE_ID --scenario cold

# 4. Fetch results locally (automatic after bench-remote, or manually)
cargo xtask bench-remote --fetch --instance-id $INSTANCE_ID

# 5. Tear down when done
cd bench/terraform && terraform destroy
```

## bench-remote

Single command for the full benchmark cycle. Requires `session-manager-plugin`.

```bash
# Full 3-tool cold comparison (3 iterations, picks median)
cargo xtask bench-remote --instance-id $INSTANCE_ID --scenario cold

# Quick smoke test
cargo xtask bench-remote --instance-id $INSTANCE_ID --scenario cold \
  --tools ocync --iterations 1 --limit 3

# All scenarios (cold + warm + partial + scale)
cargo xtask bench-remote --instance-id $INSTANCE_ID --scenario all

# Fetch results from the last run without re-running
cargo xtask bench-remote --fetch --instance-id $INSTANCE_ID
```

The `--instance-id` can also be set via `BENCH_INSTANCE_ID` env var.

**What it does:**
1. `git push` the current branch
2. Uploads a build+bench script to the instance via SSM send-command
3. Opens an interactive SSM session (`start-session`) with live stdout
4. On completion, fetches `summary.md` and the historical JSON archive

**Output:**
- `bench-results/<timestamp>/summary.md` -- human-readable comparison table with winners bolded
- `bench-results/runs/<timestamp>.json` -- full machine-readable archive for historical comparison

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

Instance metadata (type, CPU, memory, network, region) is captured from `ec2:DescribeInstanceTypes` (authoritative, not /proc or sysfs).

## Running on the instance directly

If you prefer an interactive session instead of `bench-remote`:

```bash
# Connect
aws ssm start-session --target $INSTANCE_ID --region us-east-1

# On the instance:
cd ~/ocync
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
export BENCH_TARGET_REGISTRY=${ACCOUNT}.dkr.ecr.us-east-1.amazonaws.com
cargo xtask bench --tools ocync,dregsy,regsync --iterations 3 cold
```

## Instance details

| What | Path |
|------|------|
| Source code | `/home/ec2-user/ocync/` |
| Cargo binaries | `/home/ec2-user/.cargo/bin/` |
| Bench-proxy CA | `/etc/bench-proxy/ca.pem`, `/etc/bench-proxy/ca-key.pem` |
| Competitor tools | `/usr/local/bin/{dregsy,regsync,skopeo}` |
| Results | `/home/ec2-user/ocync/bench-results/<timestamp>/` |
| Historical archive | `/home/ec2-user/ocync/bench-results/runs/<timestamp>.json` |

`BENCH_TARGET_REGISTRY` is set automatically by `bench-remote`. When running manually, derive it from `aws sts get-caller-identity`.

## Updating source code manually

```bash
cd /home/ec2-user/ocync
GH_TOKEN=$(aws ssm get-parameter --name /ocync/bench/github-token --with-decryption --query Parameter.Value --output text)
git remote set-url origin "https://${GH_TOKEN}@github.com/clowdhaus/ocync.git"
git fetch origin && git checkout <branch> && git reset --hard origin/<branch>
git remote set-url origin https://github.com/clowdhaus/ocync.git
cargo build --release --package ocync --package bench-proxy
```

## Instance lifecycle

Managed by Terraform in `bench/terraform/`. `user_data_replace_on_change = true` -- bootstrap changes recreate the instance.

```bash
cd bench/terraform && terraform init && terraform apply   # create
cd bench/terraform && terraform destroy                    # destroy
```

The IAM role includes: `AmazonEC2ContainerRegistryFullAccess`, `AmazonSSMManagedInstanceCore`, SSM parameter read, and `ec2:DescribeInstanceTypes`.

## Analyzing proxy logs

Raw per-request data lives in `<tool>-proxy.jsonl` files in the results directory.

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
