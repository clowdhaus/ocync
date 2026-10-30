# bench/

Benchmark infrastructure for comparing ocync against dregsy and regsync.

## Prerequisites

- Git push access to the repo (the `bench-remote` xtask pushes the current branch from the operator's machine)
- An OpenSSH public key authorized for instance access, supplied to Terraform via `terraform.tfvars` or `TF_VAR_ssh_public_key`
- AWS credentials with ECR access + SSM parameter read (Docker Hub creds)
- Terraform installed
- SSM parameters populated in us-east-2:
  - `/ocync/bench/dockerhub-access-token` - Docker Hub PAT for authenticated pulls
  - `/ocync/bench/dockerhub-username` - Docker Hub account name

## bench-remote

Single command for the full benchmark cycle.

```bash
# Full 3-tool cold+warm comparison (--tools defaults to ocync only)
cargo xtask bench-remote --provider aws --scenario sync --tools ocync,dregsy,regsync

# Quick smoke test
cargo xtask bench-remote --provider aws --scenario sync \
  --tools ocync --limit 3

# All scenarios (cold+warm + partial + scale)
cargo xtask bench-remote --provider aws --scenario all

# Fetch results from the last run without re-running
# (the newest results on the instance; a live run's partial ones once its benchmark has started)
cargo xtask bench-remote --provider aws --fetch

# Replace a run that is still going
cargo xtask bench-remote --provider aws --force --scenario sync
```

**A run in progress is refused unless `--force`.** A run lock on the instance (`bench/.bench-run.lock`) is held from the start of checkout until the benchmark and every tool it spawned have exited, so a second `bench-remote` cannot delete `target/` under a build. The remote script, the build and the benchmark runner share one process group, recorded in `bench/.bench-run.pgid` and never removed (the lock, not the file, says whether a run is live). `--force` kills that group, then takes the lock. Git runs with the lock's fd closed so a daemonized git cannot hold it from outside the group. Only `flock` exit 75 means held; any other `flock` failure aborts without the refusal. `--fetch` conflicts with every option that shapes a run, since it starts nothing. A run fetches results only after the script prints `benchmark started`; a failure before that (lock held after the wait, `flock` failed, build failed) returns an error and leaves local results alone.

**What it does:**
1. `git push` the current branch
2. SSH into the instance
3. Pull code, build, run benchmarks with live streamed output
4. SCP results back: `summary.md` and historical JSON archive

**Output:**
- `bench/results/<timestamp>/summary.md` - human-readable comparison table with winners bolded
- `bench/results/{registry}.json` - compact per-registry run record archive (tracked in git)

## The prepare scenario

`cargo xtask bench prepare` (or `bench-remote --scenario prepare`) measures the pre-engine phase on its own: build a client per registry, list every mapping's tags, filter them, and stop. It runs `ocync sync --dry-run`, so nothing is transferred.

It is ocync-only, for the same reason the scale scenario is: no competitor has a mode that resolves tags and then stops, so there is nothing to compare against.

**It uses its own corpus, `bench/corpus-prepare.yaml`, and that is load-bearing.** Every tag in the main corpus is a literal, so `TagsConfig::exact_tags` returns them verbatim and `resolve_mapping_inner` skips `list_tags` entirely. A prepare run against the main corpus would exit clean, report a plausible time, and measure none of the phase it is named for. The prepare corpus uses wildcard tags to force enumeration, and `prepare_corpus_tags_are_all_wildcards` fails if anyone pins them back to literals.

The main corpus is 40 entries chosen for blob sharing. The prepare phase costs one round trip per mapping, so the shape that exposes it is many mappings, not a few large ones. That is the second reason for a separate file.

None of the sync scenario's fairness machinery applies here. Nothing is transferred, so there are no blobs to fall out of page cache, no ECR repos to reset, and no CDN edge to pre-warm. Skipping all of it is what keeps this scenario cheap enough to run often.

Two of those exemptions are explicit in the harness rather than automatic, and both were found by running the scenario rather than by reading the code:

- The tmpfs guard is skipped for `prepare`. It exists because staging writes multi-GB blobs, which a dry-run never does, and applying it refuses the one scenario that can run on a laptop.
- CDN pre-warm is skipped when `prepare` is the only scenario. Pre-warm buys cross-tool fairness for a workload that pulls blobs; for a single-tool dry-run reading a different corpus it is hundreds of pointless requests against rate-limited registries. A mixed run (`--scenario all`) still warms, because the scenarios beside it do transfer.

**A run whose instance metadata is unknown does not append to `bench/results/{registry}.json`.** That archive is tracked in git and read as a history of comparable runs; a laptop run landing in it puts uncomparable numbers under the same key. The timestamped summary is still written. This matters here more than elsewhere, because `prepare` is the scenario most likely to be run off the rig.

## Benchmark fairness invariants

The harness enforces these automatically -- do not bypass them.

**Before any tool runs (once per scenario):**
0. **CDN pre-warm** -- HEAD every source manifest AND GET every unique blob (following 302 redirects to CloudFront, consuming full body) with OCI token exchange to populate both manifest and blob CDN edge caches. Wait 6 minutes for CloudFront propagation. Without this, the first tool hits cold CDN while the last tool gets near-100% hits -- a massive unfair advantage from execution order.
1. **Randomize tool order** -- Fisher-Yates shuffle so no tool systematically runs first or last. The randomized order is logged for traceability.

**Before every tool run (including the first):**
2. **Drop OS page cache** -- `sudo sh -c 'sync && echo 3 > /proc/sys/vm/drop_caches'`. Without this, tool 2 benefits from tool 1's source blobs cached in RAM (55GB corpus = massive unfair advantage).
3. **Clean staging tmpdir** -- remove all files in `$TMPDIR`. Prevents staged blobs from a previous tool consuming disk.
4. **Delete + recreate ECR repos** -- not just create-if-not-exists. Repos from aborted runs or previous tools may contain partial data that makes a "cold" sync warm.
5. **10s cooldown** -- allows process cleanup between tool runs. CDN state is handled by pre-warm (invariant 0), not by inter-tool cooldown.

**After every tool run (cold and warm):**
6. **Verify sync correctness** -- list tags in every target ECR repo (per-repo output), confirm all expected tags are present. An unverified benchmark may report fast times for incomplete syncs.

**Never run benchmarks manually via SSH.** The `bench-remote` xtask handles all of the above. Manual `cargo xtask bench` on the instance skips TMPDIR setup and other safeguards. SSH is for debugging only.

**TMPDIR must be on EBS, not tmpfs.** On AL2023 `/tmp` is tmpfs (RAM-backed, ~8GB). The staging path writes 55GB+ of blobs. `bench-remote` sets `TMPDIR=$HOME/ocync/bench/.tmp` on EBS automatically. Running `cargo xtask bench` without this will hit ENOSPC (the harness detects and rejects tmpfs).

## Running on the instance directly

Not recommended. Use `cargo xtask bench-remote` instead. If you must run manually (debugging only):

```bash
ssh ec2-user@<public-ip>
cd ~/ocync
export TMPDIR=$HOME/ocync/bench/.tmp
rm -rf "$TMPDIR" && mkdir -p "$TMPDIR"
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
export BENCH_TARGET_REGISTRY=${ACCOUNT}.dkr.ecr.us-east-2.amazonaws.com
cargo xtask bench --tools ocync,dregsy,regsync sync
```

`BENCH_TARGET_REGISTRY` is set automatically by `bench-remote`. When running manually, derive it from `aws sts get-caller-identity`.

## Instance lifecycle

Managed by Terraform in `bench/terraform/aws/`. `user_data_replace_on_change = true` -- bootstrap changes recreate the instance.

`bench/terraform/gcp/` and `bench/terraform/azure/` hold a host that runs ocync as that cloud's own identity (the Compute Engine metadata server, an Azure managed identity), for measuring sign-in by hand over SSH (`terraform output connect`). `bench-remote` cannot drive them yet: they write no `bench.json`, install nothing at boot, and their disks are too small to stage the corpus. The Azure stack has never been applied.

A benchmark run is a point in time, not a fixed rig. Nothing is pinned: competitor tools install at `@latest` and the image is the current AL2023 default release. What makes two runs comparable is the record of what each used, so the run record carries the tool versions, the relay versions and the image id.

**Versions are fixed at instance creation, not per run.** The `go install` calls live in user-data, and `bench-remote` only rebuilds ocync. An instance built in March still runs March's dregsy in September. To benchmark against current competitor releases, replace the instance. Read the versions out of the run record rather than assuming they match what is current today.

Three resolution details are easy to get wrong. skopeo's module path is `go.podman.io/skopeo` since v1.23.0, and the old `github.com/containers/skopeo` path still serves tags whose `go.mod` declares the new path, so installing from it fails on a path mismatch rather than a missing version. `@latest` never crosses a major version boundary under semantic import versioning, so when one of these tags v2 the install silently sticks on the highest v1 until the path gains a `/v2` suffix. And dregsy tags releases without a `v` prefix, so the proxy reads no valid semver and `@latest` resolves to a pseudo-version off the default branch rather than to a release.

Versions are read from the binaries rather than assumed. `go install` sets none of the ldflags a release build uses, so the ECR credential helper reports `development` and dregsy has no version flag at all. Both are recovered with `go version -m`, which reads the module version stamped into the binary.

That lookup can still come up empty, for a binary built from a working tree (Go stamps `(devel)`, which names no release) or where Go is absent. What the record then holds depends on which path asked:

- A benchmarked tool records `unknown (<reason>)`.
- A relay falls back to asking the binary, so it records either that answer plus `(module version unavailable: <reason>)`, or, if the binary reported nothing either, `unknown (<reason>, and --version reported nothing)`.

So a relay never reads as a plain version when its lookup failed, which matters most for the credential helper: its `development` placeholder is otherwise indistinguishable from a healthy release build. Grep the archive for `unavailable` as well as `unknown (` to find runs whose provenance is incomplete, and treat either as a lost record rather than a harness bug.

Reasons carry no absolute paths and are length-capped, because they are committed to `bench/results/{registry}.json`.

The bootstrap log is `/var/log/user-data.log` on the instance, not `cloud-init-output.log`: `user-data.sh` redirects its own output there. The benchmark run log is streamed to the operator's terminal live and never persisted, and under `--fetch` not even streamed, so a warning it emits is seen once or not at all. That is why anything affecting a run's provenance is written into the record rather than only logged.

dregsy transfers nothing itself. Its config sets `relay: skopeo`, so skopeo moves every byte dregsy is credited with, and the run record captures skopeo's version alongside dregsy's for that reason.

The image floats but `ignore_ami_changes` is set, because `ami` is ForceNew: without it, AWS advancing the AL2023 parameter means the next apply for any unrelated reason destroys a running instance and its results. New images arrive when the instance is next replaced deliberately.

```bash
cd bench/terraform/aws && terraform init && terraform apply   # create
cd bench/terraform/aws && terraform destroy                    # destroy
```

The IAM role includes: `AmazonEC2ContainerRegistryFullAccess`, `ec2:DescribeInstanceTypes`, and ECR public auth.

## Security notes

- **IAM `AmazonEC2ContainerRegistryFullAccess`**: Intentionally broad because bench scenarios create and delete ECR repos (`ecr::create_repos` / `ecr::delete_repos`). Not scoped to specific repos because repo names are derived from the corpus at runtime.
- **Docker Hub token in `.bench-env`**: Credentials are written to `~/.bench-env` (chmod 600) and sourced from `.bashrc`. Visible to any process running as ec2-user. Acceptable for single-operator ephemeral instances.
- **SSH `StrictHostKeyChecking=accept-new`**: First-connect TOFU since the instance is recreated on every `terraform apply`. Host key is not verified against a known value.

## Terraform structure

Terraform writes `bench.json` to the provider directory with connection details. The xtask reads this file -- no manual copy-paste of IPs.

## Analyzing proxy logs

Raw per-request data lives in `<tool>-<pass>-proxy.jsonl` files in `bench/results/<timestamp>/`, one per tool and pass (`<pass>` is `prime`, `cold`, `warm`, `partial`, `scale-<images>` or `prepare`, e.g. `ocync-scale-10-proxy.jsonl`).

```bash
# Count requests by method
jq -r '.method' <tool>-<pass>-proxy.jsonl | sort | uniq -c | sort -rn

# Source blob pulls only (non-target hosts)
# NOTE: filter by exact target_registry, NOT by substring "ecr" — public.ecr.aws
# is a SOURCE host. The aggregate() metric uses `host != target_registry`.
jq -r --arg target "$BENCH_TARGET_REGISTRY" \
  'select(.method=="GET" and (.url | contains("/blobs/sha256:")) and .host != $target)' \
  <tool>-<pass>-proxy.jsonl | wc -l

# Mount attempts and success rate
jq -r 'select(.url | contains("mount="))' <tool>-<pass>-proxy.jsonl | jq -r '.status' | sort | uniq -c

# Total response bytes
jq '[.response_bytes] | add' <tool>-<pass>-proxy.jsonl
```

## Run record archive

Each benchmark run appends a compact record to `bench/results/{registry}.json` (e.g. `ecr.json`). These files are tracked in git for longitudinal comparison across machines, cloud providers, and code versions.

Each record includes: timestamp, git ref, machine metadata (instance type, arch, vCPUs, memory, network, region, and the cloud provider of the target registry, omitted when the registry is none of ECR, GCR or ACR), corpus size, and all 9 per-tool metrics per scenario.

## Competitor config gotchas

- **regsync `type: image` resolves `:latest` first**: When the source has no tag (e.g. `docker.io/nvidia/cuda`), regsync does a manifest HEAD on `:latest` before checking the `tags.allow` list. Images without a `latest` tag (nvidia/cuda, pytorch/pytorch) produce 404 errors in the log. Non-fatal but adds wasted requests to the benchmark metrics.
- **regsync `repoAuth: true`**: Required for registries that issue per-scope tokens (cgr.dev, gcr.io, nvcr.io). Without it, regsync silently fails auth on the second repo from the same registry.
- **dregsy `auth-refresh: 12h`**: Required for ECR targets. Without it, dregsy's ECR token expires mid-run on large corpora. The 12h matches ECR token lifetime.
- **dregsy JSON parse errors**: dregsy occasionally logs `invalid character 'b' looking for beginning of value` when a registry returns a non-JSON error body (e.g. HTML rate limit page). Non-fatal; dregsy retries and continues.
