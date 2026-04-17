# bench/

Benchmark infrastructure for comparing ocync against dregsy and regsync.

## Instance access

- **Instance ID**: `i-042193c339708c702` (us-east-1) — check Terraform state if stale
- **Access**: SSM only (no public IP, no SSH key)
- **Commands run as root** by default — always use `sudo -u ec2-user bash -lc "..."` for build/bench commands
- **SSM send-command** truncates output at 2500 chars. For long output, redirect to a file and read it back.

```bash
# Quick command
aws ssm send-command \
  --instance-ids i-042193c339708c702 \
  --document-name AWS-RunShellScript \
  --parameters 'commands=["sudo -u ec2-user bash -lc \"cd ~/ocync && cargo xtask bench --help\""]' \
  --region us-east-1 --output text --query 'Command.CommandId'

# Get output (wait a few seconds)
aws ssm get-command-invocation \
  --command-id <ID> --instance-id i-042193c339708c702 \
  --region us-east-1 --query '[Status,StandardOutputContent,StandardErrorContent]' --output text
```

## Paths on instance

| What | Path |
|------|------|
| Source code | `/home/ec2-user/ocync/` |
| Cargo binaries | `/home/ec2-user/.cargo/bin/` (ocync, bench-proxy) |
| Bench-proxy CA | `/etc/bench-proxy/ca.pem`, `/etc/bench-proxy/ca-key.pem` |
| Competitor tools | `/usr/local/bin/dregsy`, `/usr/local/bin/regsync`, `/usr/local/bin/skopeo` |
| Docker config | `/home/ec2-user/.docker/config.json` (ECR cred helper) |
| Results | `/home/ec2-user/ocync/bench-results/<timestamp>/` |

## Environment variables

`BENCH_TARGET_REGISTRY` must be set before running benchmarks. It is **not** baked into user-data.

```bash
export BENCH_TARGET_REGISTRY=660548353186.dkr.ecr.us-east-1.amazonaws.com
```

AWS credentials come from the instance profile (IAM role) — no manual config needed.

## Running benchmarks

```bash
cd /home/ec2-user/ocync
export BENCH_TARGET_REGISTRY=660548353186.dkr.ecr.us-east-1.amazonaws.com

# Quick smoke test (3 images, 1 iteration, ocync only)
cargo xtask bench --tools ocync --iterations 1 --limit 3 cold

# Full 3-tool comparison
cargo xtask bench --tools ocync,dregsy,regsync --iterations 3 cold

# Warm sync (cold prime + measured warm pass)
cargo xtask bench --tools ocync,dregsy,regsync warm

# All scenarios
cargo xtask bench --tools ocync,dregsy,regsync all
```

The xtask harness handles: building ocync from source, starting bench-proxy (MITM), creating/deleting ECR repos, running tools, capturing proxy JSONL.

## Updating source code on the instance

```bash
cd /home/ec2-user/ocync
git fetch origin
git checkout <branch>
cargo build --release --package ocync --package bench-proxy
cp target/release/ocync ~/.cargo/bin/ocync
cp target/release/bench-proxy ~/.cargo/bin/bench-proxy
```

Note: `user-data.sh` hardcodes `benchmark-suite` branch for fresh bootstraps. Change line 157 to match if recreating the instance.

## Analyzing proxy logs

```bash
# Count requests by method
jq -r '.method' <tool>-proxy.jsonl | sort | uniq -c | sort -rn

# Count blob HEAD 404s (wasted on cold sync)
jq -r 'select(.method=="HEAD" and (.url | contains("/blobs/sha256:")) and .status==404)' <tool>-proxy.jsonl | wc -l

# Mount attempts
grep '"mount"' <tool>-proxy.jsonl | wc -l

# Total response bytes
jq '[.response_bytes] | add' <tool>-proxy.jsonl
```

## Instance lifecycle

Managed by Terraform in `bench/terraform/`. `user_data_replace_on_change = true` — bootstrap changes recreate the instance.

```bash
cd bench/terraform && terraform init && terraform apply   # create
cd bench/terraform && terraform destroy                    # destroy
```

Instance type: c6in.4xlarge, 100GB gp3 (6000 IOPS, 400 MB/s throughput).
