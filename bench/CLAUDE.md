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
  --tools ocync --limit 3 --rounds 3

# ocync at a commit against the current build, same rounds (sync and partial only;
# the commit must be on a branch of origin, and a verdict needs 5 or more rounds)
cargo xtask bench-remote --provider aws --scenario sync --baseline-ref <commit>

# All scenarios (cold+warm + partial + scale)
cargo xtask bench-remote --provider aws --scenario all

# Fetch results from the last run without re-running
# (the newest results on the instance; a live run's partial ones once its benchmark has started)
cargo xtask bench-remote --provider aws --fetch

# Sync into Google Artifact Registry instead of ECR (see below)
cargo xtask bench-remote --provider aws --target artifact-registry --scenario sync --tools ocync

# Replace a run that is still going
cargo xtask bench-remote --provider aws --force --scenario sync
```

**`--target artifact-registry` syncs from the AWS instance into Google Artifact Registry.** Artifact Registry keeps a blob after the image that held it is deleted, so a used repository is no longer cold; every round therefore syncs into a repository of its own, made fresh. The bench service account can write to a repository but not create one, and the instance has no Google access to the repository API, so `bench-remote` makes and deletes the repositories on this machine with your `gcloud` sign-in:

- It needs `bench/terraform/gcp` applied (its `repository`, `service_account` and `credentials_file` outputs name the project, location, service account and federation file) and `gcloud` signed in with rights to create repositories and set their IAM policy. The outputs are read, and a lock on this machine is taken, before the push, so a missing apply is refused before anything leaves it.
- Before the push it refuses anything but `--provider aws`, `--scenario sync` and ocync alone (no `--baseline-ref`, which adds a second build; `--regression` is refused on the instance, since the one stored baseline is ECR's): regsync and dregsy configs hardcode ECR credentials, partial and scale are not built for it, and two builds would share one repository.
- After the push it deletes every repository whose id starts with `ocync-bench-run-` (an earlier run that was killed), creates `ocync-bench-run-<run timestamp>-r<round>` for each round, grants the service account `roles/artifactregistry.writer` on each, and copies the federation file to the instance (mode 0600). It then deletes them all when the run ends, on success and on failure (one already gone counts as deleted), except when the end is unknown: if `ssh` drops (exit 255 or a stream error) the detached benchmark may still be pushing, so the repositories are kept and the next `--target artifact-registry` run deletes them once the instance is free. A process killed outright leaves them for the next run's sweep. The sweep would delete a live run's repositories, so two guards stand before it: an exclusive lock (`ocync-bench-remote.lock` in the temp directory) held for the whole local life of an Artifact Registry or ACR run refuses a second one on this machine by name, and a probe of the instance's run lock refuses (without `--force`) a run whose local process died but whose benchmark still goes on the instance. The probe treats only `flock` exit 75 as held and any other failure as an error. `--force` kills that run, so its repositories go with it.
- The instance runs `cargo xtask bench --target artifact-registry` (`bench-remote` passes `--target` through for Artifact Registry and ACR only, so an ECR run still works on an older `--git-ref`; the default is `ecr`; a `--git-ref` for an Artifact Registry run needs this option), reads `BENCH_TARGET_REGISTRY`, `BENCH_GAR_PROJECT`, `BENCH_GAR_REPOSITORIES` and `GOOGLE_APPLICATION_CREDENTIALS` (all required, an empty one counts as unset; an ECR run reads none of the last three), creates and deletes nothing, writes the ocync registry as `auth_type: gar`, syncs round N into `<project>/<repository N>/bench/...`, and checks each pass with manifest HEADs (the tag's digest, then each locked index child) through ocync's own Google sign-in, outside the proxy window. One HEAD of a tag that cannot exist, in repository 1, runs first so a broken federation file or a missing grant fails before the build.
- Its records go to `bench/results/gcr.json` and carry the machine's cloud (`aws`), never the project, repositories or host. It writes no block to `docs/src/content/performance.md`, which is the ECR comparison.

**A run in progress is refused unless `--force`.** A run lock on the instance (`bench/.bench-run.lock`) is held from the start of checkout until the benchmark and every tool it spawned have exited, so a second `bench-remote` cannot delete `target/` under a build. The remote script, the build and the benchmark runner share one process group, recorded in `bench/.bench-run.pgid` and never removed (the lock, not the file, says whether a run is live). `--force` kills that group, then takes the lock. Git runs with the lock's fd closed so a daemonized git cannot hold it from outside the group. Only `flock` exit 75 means held; any other `flock` failure aborts without the refusal. `--fetch` conflicts with every option that shapes a run, since it starts nothing. A run fetches results only after the script prints `benchmark started`; a failure before that (lock held after the wait, `flock` failed, build failed) returns an error and leaves local results alone. The instance writes results only into the run's own directory, which `bench-remote` names locally and fetches exactly (`bench --archive defer`); the run record and performance-page blocks are written locally after the fetch, idempotently (the run's one start time, `--run-timestamp`, names the directory and stamps the record), and the script still restores the two tracked files before checkout to discard edits from runs on older code, and a missing directory means the run wrote no results. `--fetch` copies the newest directory. A script that exits nonzero, or exits 0 with no results, makes `bench-remote` exit nonzero after any fetch.

**What it does:**
1. `git push` the current branch
2. SSH into the instance
3. Pull code, build, run benchmarks with live streamed output
4. SCP the run's results directory back, then append its record to the local archive and update the local performance page

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

## The corpus lock

`bench/corpus.lock.json` pins every (source, tag) the benchmark syncs to its manifest digest and the blob digests it holds, so runs on different dates sync the same content. It is tracked in git, sits beside the corpus it locks, and is written only by `cargo xtask bench-lock`. A prepare-only run reads no lock.

- **`cargo xtask bench-lock`** resolves every ref in the corpus and the partial overrides and writes the lock. A ref that cannot be resolved fails the command and is named. Over unchanged sources it leaves the file, and its `locked_at`, untouched. It needs the network and Docker Hub credentials (anonymous works for a small corpus), and writes nothing to a cloud.
- **Every tool syncs the locked digest under the corpus tag**, so a moved source tag changes nothing. Before the build, `bench` checks once that each locked digest is still served (one HEAD per distinct digest). A ref missing from the lock stops the run before any request; a digest answered 404, 403 or 401 stops it naming every tag that shares it; a source that cannot be reached after retries stops it naming `--skip-registries`. Re-lock with `cargo xtask bench-lock`; a new lock starts a new comparison series.
- **Any scenario failure keeps the finished scenarios.** They are still written to the summary and appended to the archive, and the run exits non-zero with every message. When a scenario fails and a locked digest is no longer served, the error says so.
- **`corpus_locked_at`** in each run record is the lock's `locked_at`. Records under different values are different series. `--regression` refuses a baseline recorded under another lock, under none, or over another corpus size; delete `bench/results/baseline.json` to record a new one.
- A floating tag stays in the corpus only where the source publishes no more specific tag for the same content, or, as with alpine, where the corpus comment says the shared index is what it measures.

## Benchmark fairness invariants

The harness enforces these automatically -- do not bypass them.

**Before the build, except prepare:** every locked digest the run syncs is still served by its source (see [The corpus lock](#the-corpus-lock)).

**Before any tool runs (once per run):**

0. **CDN pre-warm** -- GET every blob the lock lists for the run's refs, with a registry token (an unauthenticated 401 is not cached) and reading the full body, 8 at a time per registry, within 8 minutes, then wait 6 minutes if any blob missed the CDN. Registries measured to have no blob cache are on the `NO_BLOB_CACHE_HOSTS` list (quay.io and cgr.dev, which redirect blobs to object storage that sends no `X-Cache` or `CF-Cache-Status`). Each gets one probe, its first blob read for headers only. With no cache header, its other blobs are not requested and it is named in the run record's `no_cache`. If the probe carries a cache header, the registry is warmed in full and a warning says to remove it from the list. A registry is never judged cacheless from a sample of answers: public.ecr.aws, which has a CDN, answers a few small blobs directly with no header. Only a response whose header reports a miss (`X-Cache` saying miss, or a `CF-Cache-Status` that is not a hit state) triggers the wait; a header-less answer from any other registry is downloaded, counted on its own and does not. Manifests are not warmed, because no registry measured caches them. A registry not fully warmed is warned about and recorded in the run record's `prewarm` field (`unwarmed`); a registry with no blob cache is in the summary and `no_cache` only. Without this, the first tool meets a cold CDN and the last a warm one.

**Before each round (once per scenario for scale and prepare):**
1. **Randomize tool order** -- Fisher-Yates shuffle so no tool systematically runs first or last. The order and its seed are recorded.

**Before every tool run (including the first):**
2. **Drop OS page cache** -- `sudo sh -c 'sync && echo 3 > /proc/sys/vm/drop_caches'`. Without this, tool 2 benefits from tool 1's source blobs cached in RAM (55GB corpus = massive unfair advantage).
3. **Clean staging tmpdir** -- remove all files in `$TMPDIR`. Prevents staged blobs from a previous tool consuming disk.
4. **Delete + recreate ECR repos** -- not just create-if-not-exists. Repos from aborted runs or previous tools may contain partial data that makes a "cold" sync warm. A repo that cannot be deleted fails the pass, since its content would satisfy the next tool's digest check.
5. **10s cooldown** -- allows process cleanup between tool runs. CDN state is handled by pre-warm (invariant 0), not by inter-tool cooldown.

**After every tool run (cold and warm):**
6. **Verify synced content by digest** (after the sync, partial and scale passes) -- list every image in each target ECR repo, tagged or not (per-repo output). Each expected tag must name the digest in `bench/corpus.lock.json`, and every child manifest the lock lists for an index must be in the repo. A tool that converted a format, dropped a platform or transferred nothing fails the run instead of being credited with a fast time. This is the only post-sync check; there is no tag-presence fallback.

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

`bench/terraform/gcp/` and `bench/terraform/azure/` hold a host that runs ocync as that cloud's own identity (the Compute Engine metadata server, an Azure managed identity), for measuring sign-in by hand over SSH (`terraform output connect`). `bench-remote` cannot drive them yet: they write no `bench.json`, install nothing at boot, and their disks are too small to stage the corpus. The Azure stack also holds the Entra app the AWS instance signs in as: it trusts the AWS stack's token issuer for the instance role's ARN and holds `AcrPush` (push, no delete) on the registry. `bench-remote --target acr` deletes the run's repositories with the operator's `az` sign-in. Only the Entra objects are applied: the subscription was frozen for inactivity when the registry and VM were first applied.

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

Raw per-request data lives in `<tool>-<pass>-proxy.jsonl` files in `bench/results/<timestamp>/`, one per tool and pass (`<pass>` is `prime`, `cold`, `warm`, `partial`, `scale-<images>` or `prepare`, e.g. `ocync-scale-10-proxy.jsonl`). Sync and partial passes are under `round-<n>/`.

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

Each record includes: timestamp, git ref, machine metadata (instance type, arch, vCPUs, memory, network, region, and the cloud the machine runs in, `aws` when EC2's metadata answered, whichever cloud the target registry is in), corpus size, the pre-warm outcome, and the per-tool metrics per scenario, one record per round for sync and partial.

## Competitor config gotchas

- **regsync `type: image` resolves `:latest` first**: When the source has no tag (e.g. `docker.io/nvidia/cuda`), regsync does a manifest HEAD on `:latest` before checking the `tags.allow` list. Images without a `latest` tag (nvidia/cuda, pytorch/pytorch) produce 404 errors in the log. Non-fatal but adds wasted requests to the benchmark metrics.
- **regsync `repoAuth: true`**: Required for registries that issue per-scope tokens (cgr.dev, gcr.io, nvcr.io). Without it, regsync silently fails auth on the second repo from the same registry.
- **dregsy `auth-refresh: 12h`**: Required for ECR targets. Without it, dregsy's ECR token expires mid-run on large corpora. The 12h matches ECR token lifetime.
- **dregsy JSON parse errors**: dregsy occasionally logs `invalid character 'b' looking for beginning of value` when a registry returns a non-JSON error body (e.g. HTML rate limit page). Non-fatal; dregsy retries and continues.
