# ocync Design Specification

---

## Overview

ocync is a Rust-based OCI registry sync tool that copies container images between registries. It is designed as a library-first project: `ocync-distribution` is a standalone OCI Distribution client (Rust's answer to go-containerregistry), and `ocync-sync` provides sync orchestration on top.

### Design Principles

- **Library-first** — crate boundaries designed for embedding; CLI is a thin consumer
- **Pure OCI Distribution API** — no skopeo, no Docker daemon, direct HTTPS to registries
- **Additive sync only** — never deletes; registries handle lifecycle/retention
- **Nothing implicit** — no default tags, no silent `:latest`, explicit config required
- **ECR-first** with broad registry support via native auth providers
- **FIPS-ready** — first-class FIPS 140-3 support via feature flag

---

## Architecture

### Workspace Structure

```
ocync/
├── Cargo.toml                     # workspace root + binary crate
├── src/                           # CLI
│   ├── main.rs                    # Entry point (clap)
│   └── cli/
│       ├── mod.rs
│       ├── config.rs              # Config parsing, env var expansion, validation
│       ├── output.rs              # Human output formatting, progress bars
│       └── commands/              # sync, copy, tags, auth, validate, expand, watch
│
├── crates/
│   ├── ocync-distribution/        # OCI registry client library
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── reference.rs       # Image reference parsing
│   │       ├── digest.rs          # SHA-256 digest computation + verification
│   │       ├── spec.rs            # OCI/Docker manifest types, descriptors, platforms
│   │       ├── error.rs           # Structured OCI error types
│   │       ├── client.rs          # RegistryClient (one per registry, owns auth + HTTP)
│   │       ├── blob.rs            # Blob ops: stream pull, chunked push, mount, exists
│   │       ├── manifest.rs        # Manifest ops: pull, push, HEAD, content negotiation
│   │       ├── tags.rs            # Tag listing with automatic pagination
│   │       ├── catalog.rs         # Catalog listing with pagination
│   │       └── auth/
│   │           ├── mod.rs         # AuthProvider trait, token cache, WWW-Authenticate
│   │           ├── docker.rs      # Docker config.json + credential helpers
│   │           ├── ecr.rs         # [feature = "ecr"]
│   │           ├── gcr.rs         # [feature = "gcr"]
│   │           ├── acr.rs         # [feature = "acr"]
│   │           ├── ghcr.rs        # [feature = "ghcr"]
│   │           └── anonymous.rs   # Anonymous token exchange
│   │
│   └── ocync-sync/               # Sync orchestration library
│       └── src/
│           ├── lib.rs
│           ├── filter.rs          # Tag filtering pipeline (glob, semver, exclude, sort, latest)
│           ├── engine.rs          # Concurrent execution, rate limiting, retry
│           └── progress.rs        # Progress reporting trait
```

### Feature Flags (ocync-distribution)

> **Note:** The feature flag design below is superseded by the "no Cargo feature flags" rule in CLAUDE.md. The current implementation compiles all registry providers and uses runtime detection (`detect_provider_kind()` and `ProviderKind` enum) instead of compile-time feature gates. TLS backend selection uses build-time configuration rather than feature flags. This section is retained for historical context but does not reflect the current architecture.

```toml
[features]
default = ["rustls-aws-lc"]
fips = ["rustls/fips", "dep:aws-lc-rs"]
rustls-aws-lc = ["reqwest/rustls-tls"]
native-tls = ["reqwest/native-tls"]
ecr = ["dep:aws-config", "dep:aws-sdk-ecr", "dep:aws-sdk-sts"]
ecr-fips = ["ecr", "fips", "dep:aws-smithy-http-client"]
gcr = []
acr = []
ghcr = []
```

### Key Dependencies

| Layer | Crate | Purpose |
|---|---|---|
| HTTP | `reqwest` 0.13 | HTTP client (streaming, TLS) |
| Async | `tokio` 1 | Runtime |
| Crypto | `sha2` (default) / `aws-lc-rs` (fips) | Digest computation |
| TLS | `rustls` 0.23 + `aws-lc-rs` | TLS (FIPS 140-3 Certificate #4816) |
| AWS | `aws-config`, `aws-sdk-ecr`, `aws-sdk-sts` | ECR auth (feature-gated) |
| Serialization | `serde`, `serde_json` | JSON serialization |
| Config | `serde-saphyr` (YAML) + `toml` | Config parsing |
| CLI | `clap` 4.6 | CLI framework |
| Logging | `tracing` | Structured logging |
| Progress | `indicatif` | Progress bars (TTY) |
| Semver | `semver` | Version range parsing |
| Glob | `globset` | Glob pattern matching |
| Canonical JSON | `olpc-cjson` | Deterministic manifest serialization |
| Rate limiting | AIMD controller | Adaptive per-action congestion windows (replaces token bucket) |
| Metrics | `prometheus-client` | Prometheus exposition format |
| HTTP server | `tokio` TCP listener | Health endpoints in watch mode (lightweight, no framework dependency) |
| IDs | `ulid` | Correlation IDs (time-ordered) |

### SHA-256 Abstraction

A `#[cfg]`-gated wrapper struct (~40 LOC) provides a unified hashing interface:

- **Default build**: delegates to `sha2::Sha256`
- **FIPS build** (`--features fips`): delegates to `aws_lc_rs::digest::Context` with `SHA256`

Zero runtime dispatch overhead — the `#[cfg]` resolves at compile time. The wrapper exposes `update(&[u8])` and `finalize() -> [u8; 32]`. All call sites use the wrapper; no direct imports of `sha2` or `aws_lc_rs::digest` outside the wrapper module.

### AuthProvider Trait

```rust
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Human-readable provider name (e.g., "ecr", "gcr", "docker-config")
    fn name(&self) -> &'static str;

    /// Obtain a valid token, refreshing if necessary.
    async fn get_token(&self, scopes: &[Scope]) -> Result<Token, AuthError>;
}
```

Providers are selected by hostname auto-detection or explicit `auth_type`. Each provider is behind a feature flag. When a provider's feature is not compiled:

```rust
#[cfg(not(feature = "ecr"))]
impl AuthProvider for EcrStub {
    fn name(&self) -> &'static str { "ecr (not compiled)" }

    async fn get_token(&self, _: &[Scope]) -> Result<Token, AuthError> {
        Err(AuthError::ProviderNotCompiled {
            provider: "ecr",
            feature: "ecr",
            rebuild_cmd: "cargo build --features ecr",
        })
    }
}
```

### CI: cargo tree Audit

CI enforces cryptographic provider hygiene:

```bash
# No native-tls anywhere in the dependency tree
cargo tree -e features | grep native-tls  # must be empty

# ring only present when intentionally used
cargo tree -e features | grep ring        # must be empty (unless explicitly allowed)
```

**CryptoProvider**: A single `rustls::crypto::aws_lc_rs::default_provider().install_default()` call at process startup. This provider is shared by `reqwest` (HTTP/TLS) and the AWS SDK. No duplicate provider installations.

---

## Sync Model

### Pipelined Sync (Discovery + Execution Overlap)

The sync engine uses a pipelined architecture where discovery and execution overlap via `tokio::select!` over two `FuturesUnordered` pools with a `VecDeque` pending queue between them. This eliminates the latency cost of an upfront planning phase — execution begins as soon as the first image is discovered.

**Discovery pool**: Concurrent futures that pull source manifests (once per tag) and HEAD-check each target manifest. Index manifests are fully resolved (all children pulled) before leaving discovery. Source data is shared across targets for the same tag via `Rc<PulledManifest>`.

**Pending queue**: Discovery completions produce `TransferTask` entries that queue here. The engine promotes pending items to execution when the global semaphore has capacity.

**Execution pool**: Each `(tag, target)` pair becomes an independent future. Within each future, blobs are processed in frequency-descending order (most-shared blobs first for maximum cross-image cache benefit):

1. **Cache check** — persistent `TransferStateCache` (survives across runs) records blob status per target
2. **Cross-repo mount** — if blob exists at same target in a different repo, attempt mount
3. **HEAD check** — verify blob existence at target (progressive cache population, not upfront batch)
4. **Pull + push** — stream from source, push to target (or read from disk staging for multi-target)

After all blobs complete for a `(tag, target)`: push manifests (children first for indexes, then top-level by tag).

**Pipeline `select!` discipline**: Uses `biased;` (prefer execution completions to free permits) and emptiness guards on every branch. Shutdown and drain deadline branches use guard conditions — never `std::future::pending()` inside async blocks.

**Stats report**: actual unique layers transferred vs total layer references across all images. Example: "12 unique layers transferred (referenced 89 times across 50 images)".

### Core Algorithm (Intelligent Transfer)

```
For each filtered tag:

1. MANIFEST CHECK (1 HEAD request — cheapest possible check)
   HEAD /v2/{repo}/manifests/{tag} at target
   → Same digest as source? SKIP ENTIRE IMAGE. Zero transfer.
   → 404 or different digest? Continue.

2. RESOLVE DEPENDENCIES (parse manifest from source)
   If image index: resolve all platform manifests + their layers
   If single manifest: resolve config blob + layer blobs
   → Build a dependency DAG of all blobs and manifests needed

3. BLOB EXISTS CHECK (1 HEAD per blob, parallelized)
   HEAD /v2/{repo}/blobs/{digest} at target
   → 200? Already exists. Skip. (Avoids BatchLayerAlreadyExists errors)
   → 404? Need to transfer. Try mount first.

4. CROSS-REPO BLOB MOUNT (for blobs missing at target)
   POST /v2/{repo}/blobs/uploads/?mount={digest}&from={known_repo}
   → 201 Created? Mounted instantly. Zero data transfer.
   → 202 Accepted? Mount unavailable, upload session started. Stream blob.

5. BLOB UPLOAD (only for blobs not mountable)
   Stream from source → push to target (no local disk)
   POST initiate → PATCH chunks (configurable, 4MB default) → PUT finalize with digest

6. PUSH MANIFESTS (after all blobs exist)
   Push in DAG order: platform manifests first, then image index last.

7. SYNC REFERRERS (if enabled)
   Discover and transfer attached artifacts (signatures, SBOMs, attestations).
```

### Why This Matters (vs dregsy/skopeo)

| Scenario | dregsy (skopeo copy) | ocync |
|---|---|---|
| 50 images, 3 changed | Pushes all 50 (gets errors) | 47 skip at manifest HEAD, 3 transferred |
| Shared base layers | `BatchLayerAlreadyExists` per image | HEAD shows exists, skip |
| Same layer across repos | Re-uploads every time | Cross-repo mount (zero transfer) |
| Large unchanged image | Full metadata re-transfer | Single HEAD request, done |

### Push Ordering (DAG)

Images are pushed in strict dependency order to avoid registry rejection:
1. **Blobs** (layers + config) — deduplicated via persistent cache and progressive population
2. **Platform-specific manifests** — only after ALL their blobs exist at target
3. **Image indexes** (manifest lists) — only after ALL referenced manifests exist
4. **Referrers** (signatures, SBOMs) — only after their subject manifest exists

### Blob Deduplication (Persistent Cache)

The engine maintains a `TransferStateCache` wrapping a `BlobDedupMap` that persists across sync runs. The cache uses a binary format (postcard + CRC32 integrity check) and supports TTL-based expiry. On startup, a warm cache from a previous run can skip HEAD checks entirely for known blobs.

```
blob_state: Map<(TargetName, Digest), BlobInfo>

BlobInfo:
  status: BlobStatus
  known_repos: BTreeSet<String>   — repos at this target known to have the blob

BlobStatus:
  ExistsAtTarget    — HEAD returned 200 (or warm cache hit), skip
  InProgress        — another future is uploading, skip (plan-phase eliminated races)
  Completed         — upload finished by another future, skip
  Failed            — upload failed, eligible for retry on next run
```

Only `ExistsAtTarget` and `Completed` states are persisted — transient states (`InProgress`, `Failed`) are stripped before serialization. Cache population is progressive: HEAD checks happen inline during execution rather than in an upfront batch, and results immediately benefit subsequent images sharing the same blob.

**Lazy invalidation**: when a mount or push fails for a cached blob, the entry is invalidated and the operation falls back to a fresh pull+push. This handles target-side staleness (lifecycle policies, manual deletion).

### Cross-Repo Blob Mount `from` Logic

The blob dedup map is extended to track which repositories at each target registry are known to contain each blob:

```
blob_locations: Map<(TargetRegistry, Digest), BlobInfo>

BlobInfo:
  repos: Set<String>   — repository paths known to have this blob
```

When mounting a blob into a new repository, any repo from the `repos` set can serve as the `from` parameter. The engine picks the alphabetically first repo (deterministic via `BTreeSet` iteration).

On mount response:
- **201 Created**: mounted successfully, add the new repo to the `repos` set
- **202 Accepted**: mount unavailable (cross-account, different region, or registry doesn't support it), proceed with upload using the already-started upload session

**ECR constraint**: cross-repo mount only works within the same account AND same region. Cross-account or cross-region mounts will return 202.

### Fan-Out Streaming

**v1 implementation**: Pull once from source, push N times (one independent stream per target). Each target gets its own upload session.

**Same-account ECR optimization**: When multiple targets are in the same ECR account, push to the first target, then cross-repo mount to the others (zero additional data transfer).

**Memory ceiling (streaming)**: `chunk_size * max_concurrent * num_active_upload_streams`. With defaults (8MB chunks, 50 concurrent, 3 targets): bounded by semaphore capacity. When disk staging is enabled for multi-target, blobs are pulled once to `{cache_dir}/blobs/{algorithm}/{hex_digest}` and read by each target push, adding disk I/O but reducing source API calls.

### Streaming Transfer (With Optional Disk Staging)

**Single-target mode** (default): blobs are piped directly from source to target with no local disk usage:
```
source registry GET → [bytes stream] → target registry PATCH/PUT
                         ↓
                   SHA-256 computed on-the-fly for verification
```

Memory usage bounded by chunk size per active upload stream.

**Multi-target mode** (when `global.cache_dir` is set and multiple targets exist): blobs are pulled once to a content-addressable disk staging area, then read by each target push:
```
source registry GET → [bytes stream] → {cache_dir}/blobs/{algo}/{hex} (atomic write)
                                              ↓
                              target 1 push ← read from disk
                              target 2 push ← read from disk
                              target N push ← read from disk
```

Staging uses atomic write protocol (tmp file → fsync → rename → dir fsync) for crash safety. Orphaned tmp files from crashes are cleaned up on startup. Eviction by total staging size keeps disk usage bounded (`global.staging_size_limit`). Staging is automatically disabled for single-target deployments (`BlobStage::disabled()` — zero overhead).

### Layer Recompression (gzip → zstd)

Many existing container images use gzip-compressed layers. zstd offers significantly better compression ratios and faster decompression. ocync supports transparent recompression during the streaming transfer:

```yaml
defaults:
  recompress: false              # default: disabled (preserve original compression)

mappings:
  - from: library/python
    to: mirror/python
    recompress: zstd             # recompress gzip layers to zstd during transfer
```

| Setting | Behavior |
|---|---|
| `false` (default) | Preserve original layer compression. Digests unchanged. |
| `zstd` | Decompress gzip layers in the stream, recompress as zstd, push with new digest. |
| `gzip` | Recompress zstd layers as gzip (for older runtimes). |

**How it works in the streaming pipeline:**

```
source GET → [gzip bytes] → decompress → zstd compress → target PATCH/PUT
                                ↓              ↓
                          verify source    compute new
                          digest (gzip)    digest (zstd)
```

1. The source layer blob is streamed as-is from the registry (gzip-compressed)
2. The stream is piped through a decompressor (`flate2` for gzip, `zstd` crate for zstd)
3. The decompressed bytes are immediately piped through the target compressor
4. The recompressed stream is pushed to the target registry as a new blob
5. **The layer digest changes** (different compression = different bytes = different SHA-256)
6. The manifest is rewritten with the new layer digests and sizes before pushing
7. **The manifest digest changes** (different layer refs = different manifest content)

**Key implications:**

- **Digest changes are expected and logged at INFO.** Source and target digests will differ for recompressed images. This is inherent — different bytes produce different digests.
- **Signatures become invalid.** Cosign/Notation signatures reference the original manifest digest. If `recompress` is enabled alongside `artifacts.enabled: true`, ocync logs a WARNING: "Recompression invalidates existing signatures. Synced artifacts will reference the original digest."
- **`require_artifacts: true` + `recompress` is a CONFIG_ERROR.** You cannot require valid signatures while recompressing, because recompression invalidates them.
- **`skip_existing` and `immutable_tags` still work.** The target digest is tracked per-tag in the target state cache. Once a recompressed image is pushed, subsequent syncs skip it (the target tag exists with the recompressed digest).
- **Only gzip ↔ zstd is supported.** These are the two compression formats in the OCI image spec. Uncompressed layers are left as-is.
- **Memory overhead:** One decompression buffer + one compression buffer per active upload stream. zstd compression requires ~128KB-1MB of state (depending on compression level). Total overhead per stream: `chunk_size + ~2MB`.

**Compression settings:**

```yaml
recompress: zstd
recompress_level: 3              # zstd compression level (1-22, default: 3)
                                 # 3 = good balance of speed and ratio
                                 # higher = smaller but slower
```

**When to use:**
- Migrating large image registries from gzip to zstd (20-40% size reduction, faster pulls)
- Targets that require zstd (newer runtimes that benefit from faster decompression)
- NOT recommended for mirrors where digest fidelity matters (use `recompress: false`)

**Dependencies:** `flate2` (gzip), `zstd` crate (zstd compression/decompression).

### Registry-Specific Optimizations

The standard OCI Distribution API is single-resource per request — no batch endpoints exist in the spec. However, ECR exposes native AWS APIs that dramatically reduce round-trips during the planning phase. ocync uses these when available, falling back to standard OCI calls for other registries.

#### ECR Batch APIs (feature-gated behind `ecr`)

When the target or source is an ECR registry, ocync uses the AWS SDK instead of individual OCI HEAD/GET calls for discovery:

| ECR API | Replaces | Max per batch | Savings |
|---|---|---|---|
| `BatchCheckLayerAvailability` | `HEAD /v2/{repo}/blobs/{digest}` (1 per layer) | 100 digests | 200 HEAD calls → 2 batch calls |
| `BatchGetImage` | `GET /v2/{repo}/manifests/{ref}` (1 per image) | 100 image IDs | 50 GET calls → 1 batch call |
| `DescribeImages` | `HEAD /v2/{repo}/manifests/{tag}` (1 per tag) | 100 image IDs | Bulk digest+size for all images |
| `ListImages` | `GET /v2/{repo}/tags/list` + N HEADs | 1000 per page | Tag+digest pairs in one call |

**Planning phase with ECR batch APIs:**

```
1. ListImages on destination repo → all tag+digest pairs (1 paginated call)
2. Compare with source tag list → identify tags needing sync
3. BatchGetImage on source → pull all needed manifests at once (1-2 calls)
4. Collect all blob digests from manifests
5. BatchCheckLayerAvailability on destination → find missing blobs (2-3 calls)
6. Build transfer plan from results
```

For 50 images with 200 unique layers, this replaces ~300 individual OCI API calls with ~6 batch calls — **98% fewer round-trips in the planning phase**.

**ECR-specific constraints:**

- `PutImage` has **no batch equivalent** and is rate-limited to **10 TPS** (adjustable via Service Quotas). This is the bottleneck for manifest pushes — 50 images takes a minimum of 5 seconds.
- `BatchGetImage` is **not available on ECR Public** (`public.ecr.aws`). ECR Public source registries fall back to standard OCI manifest GETs.
- All batch APIs are **regional** — cross-region requires separate calls per region.
- Cross-account batch calls require repository policies granting the caller's IAM principal the relevant `ecr:Batch*` actions.
- `InitiateLayerUpload` (100 TPS) and `CompleteLayerUpload` (100 TPS) are the upload-phase bottlenecks. `UploadLayerPart` is 500 TPS.

**Implementation:** The `RegistryClient` detects ECR endpoints by hostname pattern (`*.dkr.ecr.*.amazonaws.com`) and delegates discovery operations to the AWS SDK when the `ecr` feature is enabled. The sync orchestrator calls a `plan()` trait method that ECR implements with batch APIs and other registries implement with standard OCI calls.

#### Registry Capability Matrix

| Registry | Cross-repo blob mount | Batch discovery APIs | Chunked upload | HEAD-free rate limit |
|---|---|---|---|---|
| **ECR (private)** | Yes (same account/region, opt-in) | Yes (`BatchCheck`, `BatchGet`, `ListImages`) | Yes | N/A (no pull limits) |
| **ECR Public** | Yes | Partial (`BatchCheck` only, no `BatchGet`) | Yes | N/A |
| **Docker Hub** | Yes | No | Yes | HEAD requests are free (don't count toward pull limits) |
| **GAR** | Likely unsupported | No | **Monolithic only** | N/A |
| **GHCR** | Yes (implicit global dedup) | No | Monolithic in practice | N/A |
| **ACR** | Yes | No | Yes | N/A |
| **Harbor** | Yes | No | Yes | N/A |
| **Quay.io** | Unconfirmed | No | Yes | N/A |
| **Chainguard** | Unconfirmed | No | Unknown | No rate limits |

**GAR monolithic upload constraint:** GAR does not support chunked (`PATCH`) uploads. The blob upload pipeline must detect GAR endpoints and use monolithic `POST`+`PUT` (buffer entire blob in memory). This increases memory usage for large layers but is unavoidable.

**Docker Hub rate limit awareness:** Docker Hub returns `ratelimit-limit` and `ratelimit-remaining` headers on manifest responses. ocync parses these and exposes them in sync reports. HEAD requests do NOT count toward the pull limit — this validates the HEAD-first `skip_existing` design.

---

## Rate Limiting and Retry

### Architecture: Two Distinct Layers

Rate limiting has two independent components that must not be confused:

1. **Steady-state rate limiting** — adaptive per-(registry, action) AIMD congestion windows. Each action (ManifestHead, ManifestPull, BlobHead, etc.) has an independent window that grows on success and shrinks on 429. See the transfer optimization spec for AIMD controller design.

2. **Adaptive backoff** — reactive per-request sleep triggered by 429/5xx responses. This is completely separate from the AIMD windows. When a 429 is received, the individual request sleeps (honoring `Retry-After` if present) and retries. The AIMD window for that action shrinks (once per congestion epoch), but other actions' windows are unaffected.

### Per-Registry Rate Limiting

Each registry has independent rate limiting:

```yaml
registries:
  dockerhub:
    url: docker.io
    rate_limit: 100/hour              # sets both pull and push
    max_concurrent: 5                 # low concurrency for Docker Hub
  target-ecr:
    url: 222222222222.dkr.ecr.us-west-2.amazonaws.com
    rate_limit: 50/second             # ECR API throttle
    rate_limit_pull: 80/second        # override: higher pull rate
    max_concurrent: 20                # ECR handles higher concurrency
    chunk_size: 16777216              # 16MB chunks for large images
```

**Precedence rules**:
- `rate_limit` sets both pull and push rates
- `rate_limit_pull` overrides the pull rate (takes priority over `rate_limit`)
- `rate_limit_push` overrides the push rate (takes priority over `rate_limit`)
- If none set: no cap (rely on adaptive backoff only)

**Example**:
```yaml
# rate_limit: 50/second, rate_limit_pull: 80/second
# Result: pull = 80/second, push = 50/second

# rate_limit: 50/second (no overrides)
# Result: pull = 50/second, push = 50/second

# rate_limit_push: 30/second (no rate_limit)
# Result: pull = unlimited, push = 30/second
```

If not configured, defaults: `max_concurrent: 50`, `chunk_size: 8388608` (8MB).

### Global Concurrent Transfer Cap

```yaml
global:
  max_concurrent_transfers: 50        # default: 50
```

A process-wide Tokio semaphore is acquired before the per-registry semaphore. This bounds total memory usage regardless of how many registries are configured. Without this, 20 registries at `max_concurrent: 50` each could spawn 1000 concurrent transfers.

### Chunk Size Configuration

Default: 8MB (`8388608` bytes). Configurable per registry.

For ML image workloads with multi-GB layers, increase to 16-32MB to reduce round-trips:

```yaml
registries:
  ml-registry:
    url: 222222222222.dkr.ecr.us-west-2.amazonaws.com
    chunk_size: 33554432              # 32MB
```

**Peak memory per upload** = approximately `chunk_size` (one chunk buffered in memory at a time per active upload stream).

### Adaptive Backoff

| Response | Behavior |
|---|---|
| **429 Too Many Requests** | Honor `Retry-After` header if present. Otherwise exponential backoff: 1s → 2s → 4s → 8s (cap 5 min). Max 3 retries, then skip image and continue. |
| **5xx Server Error** | Exponential backoff: 1s → 2s → 4s. Max 3 retries, then skip image. |
| **Network timeout** | Exponential backoff: 1s → 2s → 4s. Max 3 retries. |
| **Connection refused** | Retry once after 2s (transient DNS possible). Then abort for that registry. |
| **TLS handshake failure** | No retry. Abort (misconfigured TLS). |

### Retry Covers ALL Phases

Unlike skopeo (which only retries blob transfers), ocync retries at every phase:
- Initial registry ping (`GET /v2/`)
- Auth token fetch
- Tag listing
- Manifest HEAD/GET
- Blob HEAD/GET/PUT
- Manifest PUT

If a transient error occurs during tag listing, it retries tag listing — not the entire sync.

### Separate Read vs Write Limits

Docker Hub limits pulls (100/6h anonymous, 200/6h authenticated) but not pushes. ECR limits API calls globally. The rate limiter tracks read and write operations independently when configured:

```yaml
registries:
  dockerhub:
    url: docker.io
    rate_limit_pull: 180/hour         # stay under 200/6h authenticated limit
    rate_limit_push: unlimited
```

### Pre-Flight Rate Budget Check

For Docker Hub: before starting a large sync, HEAD the rate-limit-test endpoint to check remaining budget. If insufficient pulls remain for the planned sync, warn immediately rather than failing mid-way.

---

## Manifest Format Preservation

### Problem

Pushing a Docker v2 Schema 2 manifest to a registry that expects OCI format (or vice versa) changes the digest. Some registries reject manifests with unexpected media types. Existing tools (skopeo, regsync) struggle with this — digests change silently or copies fail.

### Policy

Default: **preserve the original format exactly.** The manifest bytes are transferred verbatim to maintain digest integrity.

```yaml
# Per-registry override (rare — only needed for broken registries)
registries:
  legacy-harbor:
    url: harbor.internal.io
    manifest_format: oci              # convert Docker v2 → OCI on push
```

| Setting | Behavior |
|---|---|
| `preserve` (default) | Push manifest bytes as-is. Digest unchanged. |
| `oci` | Convert Docker v2 Schema 2 → OCI Image Manifest before push. Digest WILL change. Log WARNING. |
| `docker-v2` | Convert OCI → Docker v2 Schema 2 before push. Digest WILL change. Log WARNING. |

### ECR Immutable Tag Handling

When pushing to an ECR repository with `image_tag_mutability: IMMUTABLE`, pushing an existing tag returns `ImageTagAlreadyExistsException`. ocync treats this as **success** (the image is already there with that tag):
- Log at DEBUG level (not an error, not even a warning)
- Mark image as `skipped` with reason `tag_immutable_exists`
- Continue to next image

This makes re-runs fully idempotent without requiring `skip_existing: true`.

---

## Platform Subset Filtering

### Default: All Platforms

When `platforms` is omitted from the config, the full image index (all platforms) is copied. This is the safe, faithful mirror default. `platforms: all` is **not** a valid config value — simply omit the field to get all platforms.

### Subset: Trimmed Index

When `platforms` is specified, ocync builds a **new image index** containing only the requested platforms:

```yaml
mappings:
  - from: chainguard/nginx
    to: mirror/nginx
    platforms: [linux/amd64, linux/arm64]
```

Behavior:
1. Pull the full image index from source
2. Filter entries to only `linux/amd64` and `linux/arm64`
3. Copy only the blobs/manifests for those platforms
4. Push a **new** image index containing only the 2 platform entries

The pushed index will have a **different digest** than the source index (because it has fewer entries). This is expected and logged at INFO.

### Missing Platforms

If a requested platform is not available in the source image index:
- Log WARNING: `platform linux/arm64 not found in source image index for library/nginx:1.27`
- Continue with available platforms (do NOT fail)
- If NO requested platforms match any manifest in the source index, return an error with actionable context: the configured platform filter, the platforms available in the source index, and the source reference. An empty filtered index is never pushed to targets — it would leave targets with an invalid manifest that appears synced but contains no usable platform entries. This surfaces platform configuration mismatches immediately rather than silently degrading.

---

## skip_existing and Immutable Tag Optimization

### Three-Tier Skip Hierarchy

When deciding whether to sync a tag, ocync evaluates three tiers in order. The first match wins:

**Tier 1 — `immutable_tags` pattern match + tag exists in target tag list → SKIP (0 API calls)**

```yaml
defaults:
  tags:
    immutable_tags: "v?[0-9]*.[0-9]*.[0-9]*"
```

When a tag matches the `immutable_tags` glob pattern AND the tag name appears in the target's tag list (already fetched during planning): skip immediately. No manifest HEAD, no digest comparison. Zero additional API calls.

Rationale: semver tags (`v1.2.3`, `3.12.1`) are conventionally immutable. Once published, they never change. Checking their digest on every sync run wastes API calls.

**Tier 2 — `skip_existing: true` + HEAD returns 200 → SKIP (1 API call, digest ignored)**

```yaml
mappings:
  - from: library/redis
    to: mirror/redis
    skip_existing: true
```

When `skip_existing: true` is set: send a manifest HEAD request. If the target returns 200 (tag exists), skip. The digest is NOT compared — any version of the image at that tag is considered sufficient.

Use case: mirrors where you only care about initial population, not keeping tags updated.

**Tier 3 — Default: HEAD + digest compare → SKIP if match, SYNC if different**

The default behavior. Send a manifest HEAD to the target. Compare the digest with the source manifest digest:
- Same digest → SKIP (image unchanged)
- Different digest → SYNC (image updated at source, push the new version)
- 404 → SYNC (tag doesn't exist at target)

### Performance Impact

For a repository with 2,000 semver tags where only 5 are new:
- Without optimization: 2,000 HEAD requests (~30s at ECR rate limits)
- With `immutable_tags`: 5 HEAD requests for the new tags (~0.1s), 1,995 skipped from tag list

---

## OCI Artifacts and Referrers

### What Are OCI Artifacts?

OCI 1.1 introduced the referrers API. Artifacts (signatures, SBOMs, attestations) attach to images via a `subject` field in their manifest. When you sync an image, you may also want its attached artifacts.

Common artifact types:
- **Cosign signatures**: `application/vnd.dev.cosign.artifact.sig.v1+json`
- **SBOM (SPDX)**: `application/spdx+json`
- **SBOM (CycloneDX)**: `application/vnd.cyclonedx+json`
- **In-toto/SLSA attestations**: `application/vnd.in-toto+json`
- **Notation signatures**: `application/vnd.cncf.notary.signature`

### Discovery

After syncing an image manifest, ocync queries the referrers API:
```
GET /v2/{repo}/referrers/{digest}?artifactType={type}
```

This returns an image index listing all artifacts that reference the synced manifest.

### Configuration

```yaml
defaults:
  artifacts:
    enabled: true                      # default: true (sync all artifacts)
    include:                           # only sync these artifact types (if specified)
      - "application/vnd.dev.cosign.artifact.sig.v1+json"
      - "application/spdx+json"
      - "application/vnd.in-toto+json"
    exclude: []                        # exclude specific types

mappings:
  - from: chainguard/nginx
    to: mirror/nginx
    artifacts:
      enabled: true                    # inherit from defaults or override

  - from: library/redis
    to: mirror/redis
    artifacts:
      enabled: false                   # skip artifacts for this mapping
```

### `require_artifacts` Flag

```yaml
defaults:
  artifacts:
    enabled: true
    require_artifacts: false           # default: false
```

When `require_artifacts: true`:
- Config validation ERROR if `artifacts.enabled: false` in the same scope (contradictory)
- Sync ERROR if a source image has no referrers (expected signatures/SBOMs are missing)

When `require_artifacts: false` (default):
- Missing referrers are silently acceptable

### Signature Stripping Warning

Config validation emits a WARNING when `artifacts.enabled: false` is set, because this strips signatures and SBOMs from synced images. `--dry-run` queries referrers for each image and reports what would be skipped:

```
WARNING: artifacts.enabled=false will strip the following from chainguard/nginx:1.27:
  - 2 cosign signatures
  - 1 SPDX SBOM
  - 1 SLSA attestation
```

### Filtering Options

| Config | Behavior |
|---|---|
| `artifacts.enabled: true` (default) | Sync all referrers/artifacts alongside the image |
| `artifacts.enabled: false` | Sync image only, skip all artifacts. WARNING emitted. |
| `artifacts.include: [types...]` | Only sync artifacts matching these media types |
| `artifacts.exclude: [types...]` | Sync all artifacts except these types |

### Transfer Order

Artifacts have a `subject` field referencing the parent image. They must be pushed AFTER the parent:

```
1. Push image blobs
2. Push image manifest
3. Discover referrers at source
4. For each matching artifact:
   a. Push artifact blobs (if any)
   b. Push artifact manifest (with subject pointing to parent)
```

### Registries Without Referrers API

Some older registries don't support the OCI referrers API. They may use the "tag fallback" scheme where referrers are stored as tags named `sha256-{digest}`. ocync supports both:
1. Try referrers API first (`GET /v2/{repo}/referrers/{digest}`)
2. If 404, try tag fallback (`GET /v2/{repo}/manifests/sha256-{digest}`)
3. If neither works, log INFO and continue (artifacts not available)

### Concurrency (Three-Level Hierarchy)

Concurrency is three independent layers:

1. **Global image semaphore** (engine-level): `max_concurrent_transfers` (default 50) caps in-flight `(tag, target)` pairs. Each image sync future acquires a permit before starting execution.

2. **Per-registry aggregate semaphore** (client-level): `max_concurrent` per registry (default 50) caps total HTTP requests to one host. Prevents overwhelming any single registry.

3. **Per-(registry, action) AIMD windows** (client-level): Adaptive concurrency using AIMD (Additive Increase, Multiplicative Decrease) with TCP Reno-style congestion epochs. Each window tracks a floating-point limit that increases fractionally on success (`+1/window`) and halves on 429 (`/2`). A 100ms congestion epoch prevents multiple 429s from the same burst from collapsing the window.

   Registry-specific window granularity via `RegistryAction` enum (9 OCI operations):
   - **ECR**: 9 independent windows (each API action has different TPS limits)
   - **Docker Hub**: 3 windows (HEAD unmetered, manifest-read separate, others shared)
   - **GAR**: 1 shared window (per-project quota)
   - **Others**: 5 coarse windows (HEAD, READ, UPLOAD, MANIFEST_WRITE, TAG_LIST)

Every HTTP request acquires permits from levels 2 and 3. Level 1 is engine-level only. When an AIMD window shrinks, the per-action `Arc<Semaphore>` is replaced (not shrunk by forgetting one permit) — Tokio has no `remove_permits` API.

---

## Configuration

### Format

YAML and TOML supported (auto-detected by file extension). Environment variable expansion via `${VAR}`, `${VAR:-default}`, `${VAR:?error}`.

### Config Parse Pipeline

Configuration is processed in 8 sequential steps:

1. **Read raw** — load file(s) as raw text
2. **Expand env vars** — substitute `${VAR}`, `${VAR:-default}`, `${VAR:?error}` (security rules apply — see Env Var Expansion Security)
3. **Serde parse** — deserialize YAML/TOML into typed config structs
4. **Merge partials** — merge multiple config files (see Multiple Config Files)
5. **Apply defaults** — merge `defaults` block into each mapping (see Defaults Merge)
6. **Expand bulk** — expand `bulk` mappings into individual mappings
7. **Resolve references** — validate that all `source`, `targets`, and `target_groups` entries reference names defined in `registries`. Unknown names are CONFIG_ERROR (exit 3).
8. **Validate semantics** — cross-field validation (e.g., `sort` required when `latest` is set, `require_artifacts` conflicts with `artifacts.enabled: false`, registry URL format, duplicate names)

### Schema

```yaml
# Top-level operational settings
log_format: json                       # json | text (default: auto-detect)
log_level: default                     # default | quiet | verbose | debug | trace

global:
  max_concurrent_transfers: 50         # default: 50, caps in-flight (tag, target) pairs
  cache_dir: /var/cache/ocync          # persistent cache + blob staging directory
  cache_ttl: 12h                       # warm cache TTL ("0" disables expiry)
  staging_size_limit: 2GB              # disk staging eviction threshold (SI prefixes)

registries:
  chainguard:
    url: cgr.dev
    # Auth auto-detected via hostname pattern → Docker credential helpers

  dockerhub:
    url: docker.io
    rate_limit_pull: 180/hour          # stay under authenticated limit
    rate_limit_push: unlimited
    max_concurrent: 5

  source-ecr:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr
    aws_role_arn: arn:aws:iam::111111111111:role/ecr-reader
    rate_limit: 50/second
    max_concurrent: 10

  us-ecr:
    url: 222222222222.dkr.ecr.us-west-2.amazonaws.com
    auth_type: ecr
    aws_role_arn: arn:aws:iam::222222222222:role/ecr-writer
    rate_limit: 50/second
    max_concurrent: 20
    chunk_size: 16777216               # 16MB for large images
    ecr:
      auto_create: true
      defaults:
        image_tag_mutability: IMMUTABLE
        image_scanning_on_push: true
        repository_policy_file: policies/cross-account-pull.json
        lifecycle_policy_file: policies/lifecycle.json
      overrides:
        - match: "security/*"
          image_tag_mutability: MUTABLE
          lifecycle_policy_file: policies/lifecycle-keep-all.json

  eu-ecr:
    url: 222222222222.dkr.ecr.eu-west-1.amazonaws.com
    auth_type: ecr
    aws_role_arn: arn:aws:iam::222222222222:role/ecr-writer-eu
    rate_limit: 50/second
    max_concurrent: 20
    ecr:
      auto_create: true
      defaults:
        image_tag_mutability: IMMUTABLE

target_groups:
  all-regions: [us-ecr, eu-ecr]        # MUST reference names in `registries`
  us-only: [us-ecr]

defaults:
  source: chainguard                    # references a registry name
  targets: all-regions                  # references a target_group name
  tags:
    exclude: ["*rc*", "*alpha*", "*beta*"]
    latest: 20
    sort: semver

mappings:
  # Single mapping
  - from: chainguard/nginx
    to: mirror/nginx

  # Bulk mapping
  - bulk:
      from_prefix: chainguard/
      to_prefix: mirror/
      names: [python, node, redis, postgres, go, ruby]

  # With filter override
  - from: library/python
    source: dockerhub                  # override source registry
    to: mirror/python
    tags:
      glob: "3.12*"
      semver: ">= 3.11"
      semver_prerelease: include
      exclude: "*slim*"
      sort: semver
      latest: 10

  # Immutable target, US-only
  - from: library/redis
    source: dockerhub
    to: mirror/redis
    targets: us-only                   # override: target_group name
    skip_existing: true
    tags:
      semver: ">= 7.0"
      sort: semver
      latest: 5

  # Inline target list (alternative to target_group)
  - from: chainguard/experimental
    to: mirror/experimental
    targets: [us-ecr]                  # inline list of registry names
    tags:
      glob: "*"
      latest: 5
      sort: alpha
```

**Constraint**: All entries in `target_groups` MUST reference names defined in `registries`. All `source` and `targets` values in mappings MUST reference registry names or target_group names. Unknown names are CONFIG_ERROR (exit 3) at parse time (step 7 of the config pipeline).

### `source`, `targets`, and `to` Semantics

- **`source`** — references a registry name from the `registries` block. Inline URLs not supported.
- **`targets`** — either a target_group name (string, e.g., `all-regions`) OR an inline list of registry names (e.g., `[us-ecr, eu-ecr]`). Inline URLs not supported.
- **`to`** — a repository path relative to the target registry. MUST NOT contain a hostname. The effective destination is `{registry.url}/{to}:{tag}`.
- **`from`** — a repository path relative to the source registry. MUST NOT contain a hostname.
- **Different paths per target**: not supported within a single mapping. If you need different `to` paths for different targets, create separate mappings.

**`ocync copy`** is the exception: it uses full OCI references (`docker.io/library/nginx:1.27`) and operates standalone without config.

### Defaults Merge

**One-level-deep object merge**. Scalars and lists REPLACE. Objects merge key-by-key (mapping keys override, unset keys inherited). Lists within objects (e.g., `tags.exclude`) REPLACE, never concatenate. To clear an inherited list: set `exclude: []`.

**Worked example:**

Given defaults:
```yaml
defaults:
  source: chainguard
  targets: all-regions
  tags:
    exclude: ["*rc*", "*alpha*", "*beta*"]
    latest: 20
    sort: semver
  artifacts:
    enabled: true
    include: ["application/spdx+json"]
```

And a mapping:
```yaml
- from: chainguard/nginx
  to: mirror/nginx
  tags:
    latest: 5                          # override scalar
    exclude: []                        # clear inherited list
  artifacts:
    include: ["application/vnd.in-toto+json"]  # replace list
```

The effective (merged) mapping is:
```yaml
- from: chainguard/nginx
  to: mirror/nginx
  source: chainguard                   # inherited from defaults
  targets: all-regions                 # inherited from defaults
  tags:
    exclude: []                        # REPLACED (cleared)
    latest: 5                          # REPLACED (overridden)
    sort: semver                       # inherited from defaults.tags
  artifacts:
    enabled: true                      # inherited from defaults.artifacts
    include: ["application/vnd.in-toto+json"]  # REPLACED (overridden)
```

Key behaviors:
- `tags` is an object → merged key-by-key (one level deep)
- `tags.exclude` is a list → REPLACED entirely (not concatenated)
- `tags.sort` was not specified in the mapping → inherited from defaults
- `artifacts.include` is a list within an object → REPLACED entirely

### Multiple Config Files

```bash
ocync sync --config /etc/ocync/              # loads all *.yaml in directory (alphabetical)
ocync sync --config a.yaml --config b.yaml   # explicit multi-file (CLI arg order)
```

**Merge rules for multi-file configs:**

| Section | Merge behavior |
|---|---|
| `registries` | Additive. All files contribute registries. Names must be unique across files — duplicate name = CONFIG_ERROR (exit 3). |
| `target_groups` | Additive. All files contribute groups. Names must be unique — duplicate = CONFIG_ERROR. |
| `defaults` | Single file only. If multiple files define `defaults`, CONFIG_ERROR. |
| `mappings` | Concatenated in file order. Directory: alphabetical. CLI args: argument order. |
| `log_format`, `log_level`, `global` | Single file only. Multiple definitions = CONFIG_ERROR. |

**Directory mode**: loads all `*.yaml` (and `*.yml`, `*.toml`) files in alphabetical order. Typically: one file defines registries + defaults + target_groups, other files contribute mappings.

### Fan-out (Target Groups)

```yaml
target_groups:
  all-regions: [us-ecr, eu-ecr]
  us-only: [us-ecr]

defaults:
  targets: all-regions                 # all mappings fan out to all regions by default

mappings:
  - from: chainguard/experimental
    to: mirror/experimental
    targets: us-only                   # override: only US
```

---

## Tag Filtering

### Pipeline

```
glob (raw string match) → semver (parsed version) → exclude (raw) → sort + latest (cap)
                                                                          ↓
                                                               immutable_tags check on survivors
```

All stages are AND (narrowing). Each stage reduces the set. The filter pipeline runs first, THEN `immutable_tags` is evaluated on surviving tags (see skip_existing section). This ordering doesn't affect correctness — immutable_tags is a skip optimization, not a filter — but it is conceptually clearer: filter first to find what you want, then decide what to skip.

### Fields

| Field | Type | Behavior |
|---|---|---|
| `glob` | string or list | Glob pattern(s), full-match, OR within list |
| `semver` | string | Semver range constraint (e.g., `>= 3.11, < 4.0`) |
| `semver_prerelease` | enum | `include` / `exclude` (default) / `only` |
| `exclude` | string or list | Glob deny patterns, OR within list |
| `sort` | enum | `semver` / `alpha` — REQUIRED when `latest` is set |
| `latest` | integer | Keep N most recent after sort |
| `min_tags` | integer | Fail if fewer than N tags match (opt-in strictness) |
| `immutable_tags` | string (glob) | Tags matching this pattern skip digest comparison when they exist at target |

**`sort` values**:
- `semver` — sort by parsed semantic version (highest first)
- `alpha` — sort by lexicographic string comparison (highest first). Covers date-stamped tags (e.g., `20260410`, `2026-04-10`) since YYYYMMDD sorts correctly in alpha order.

`date` sort is NOT supported. Date-based sorting would require O(n) manifest + config fetches to retrieve creation timestamps, which is prohibitively expensive for large tag sets. Use `alpha` for date-stamped tags.

### Rules

- **No `tags` block = config error.** Must specify at least `glob: "*"` to sync all tags.
- **`latest` requires `sort`** — no implicit sort order. CONFIG_ERROR (exit 3) at parse step 8.
- **Glob semantics**: `*` matches any characters including `-._`; always full-match (anchored); case-sensitive.
- **Semver parsing**: accepts `X.Y.Z`, `vX.Y.Z` (v stripped), `X.Y` (→ X.Y.0), `X.Y.Z-pre` (prerelease). Non-parseable tags are dropped with WARN.
- **Zero matches**: WARNING by default; ERROR if `min_tags` is set and not met.
- **`v` prefix**: stripped for comparison, preserved in synced tag name.

### CLI Equivalent

```bash
ocync tags docker.io/library/nginx --glob "1.*" --semver ">=1.26" --exclude "*rc*" --sort semver --latest 10
```

Shows the pipeline step-by-step for debugging.

---

## Authentication

### Model

- **One RegistryClient per registry** — avoids auth scope accumulation
- **Auto-detect by hostname** — ECR, GCR, ACR, GHCR, Docker Hub patterns (see exact patterns below)
- **Explicit `auth_type` override** — for proxies, mirrors, or non-standard hostnames
- **Per-registry credential chains** — independent role ARNs, credential files, helpers
- **Proactive token refresh** at 75% lifetime + reactive 401 retry (one attempt)

### Credentials Block

Each registry can specify explicit credentials, which take highest priority:

```yaml
registries:
  private-harbor:
    url: harbor.internal.io
    credentials:
      username: ${HARBOR_USER}
      password: ${HARBOR_PASSWORD}

  gcr-with-token:
    url: us-docker.pkg.dev
    credentials:
      token_file: /var/run/secrets/gcp/token    # file read at auth time
```

**Credential fields** (all optional, mutually exclusive auth methods):
- `token` — bearer token (static or from env var)
- `username` + `password` — basic auth
- `token_file` — path to a file containing a bearer token (re-read on each refresh)

### Credential Resolution Priority

For each registry, credentials are resolved in this order (first match wins):

1. **Explicit `credentials` block** — `token`, `username`/`password`, or `token_file` in the registry config
2. **Docker config.json static auth** — base64-encoded credentials in `~/.docker/config.json` `auths` section
3. **Docker credHelpers** — credential helper binary from `credHelpers` section. Requires shell access. If the binary is missing: log WARNING and fall through to next method (critical for distroless images where helpers can't execute).
4. **Native auto-detect** — ECR/GCR/ACR/GHCR provider based on hostname pattern
5. **Anonymous** — WWW-Authenticate challenge → anonymous token exchange

### Hostname Auto-Detection Patterns

| Provider | Pattern | Examples |
|---|---|---|
| **ECR** | Regex: `^[0-9]{12}\.dkr\.ecr(-fips)?\.[a-z0-9-]+\.amazonaws\.com$` | `111111111111.dkr.ecr.us-east-1.amazonaws.com`, `111111111111.dkr.ecr-fips.us-east-1.amazonaws.com` |
| **ECR Public** | Exact: `public.ecr.aws` | `public.ecr.aws` |
| **GCR** | Exact set: `{gcr.io, us.gcr.io, eu.gcr.io, asia.gcr.io}` | `gcr.io`, `us.gcr.io` |
| **GAR** | Regex: `^[a-z0-9-]+-docker\.pkg\.dev$` | `us-docker.pkg.dev`, `europe-west1-docker.pkg.dev` |
| **ACR** | Regex: `^[a-z0-9]+\.azurecr\.io$` | `myregistry.azurecr.io` |
| **GHCR** | Exact: `ghcr.io` | `ghcr.io` |
| **Docker Hub** | Exact set: `{docker.io, registry-1.docker.io}` | `docker.io` |
| **Chainguard** | Exact: `cgr.dev` | `cgr.dev` |

Patterns are evaluated in the order listed. Explicit `auth_type` in config bypasses auto-detection.

### Native Providers

| Provider | Detection | Mechanism |
|---|---|---|
| **ECR** | See pattern above | STS AssumeRole → GetAuthorizationToken (12h token) |
| **ECR Public** | `public.ecr.aws` | GetAuthorizationToken (public API) |
| **GCR/GAR** | See patterns above | Service account JSON → OAuth2 token (1h) |
| **ACR** | See pattern above | OAuth2 token exchange at `{registry}/oauth2/token` |
| **GHCR** | `ghcr.io` | `GITHUB_TOKEN` env var, PATs, credential helpers |
| **Docker Hub** | `docker.io`, `registry-1.docker.io` | Docker credential helpers, static creds |
| **Chainguard** | `cgr.dev` | `docker-credential-cgr` helper |
| **Anonymous** | (fallback) | WWW-Authenticate challenge → anonymous token |

### FIPS ECR

When `ecr-fips` feature is enabled:
- Endpoints resolve to `ecr-fips.{region}.amazonaws.com`
- TLS uses FIPS-validated `aws-lc-rs` (NIST #4816)
- AWS SDK uses `CryptoMode::AwsLcFips`
- Config: `fips: true` on the registry or `AWS_USE_FIPS_ENDPOINT=true` env var

### Multi-Account ECR

```yaml
registries:
  source:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr
    aws_role_arn: arn:aws:iam::111111111111:role/reader

  target:
    url: 222222222222.dkr.ecr.eu-west-1.amazonaws.com
    auth_type: ecr
    aws_role_arn: arn:aws:iam::222222222222:role/writer
    aws_external_id: ocync-cross-account
```

Separate STS sessions per registry. Base credentials from ambient environment (IRSA, instance profile, env vars).

### Fail-Fast Behavior

| Condition | Action |
|---|---|
| 401 on first request | Abort for that registry, continue others |
| 401 after prior success | One token refresh, then abort |
| 403 | Abort for that repository, continue others |
| Token refresh fails | Abort for that registry |
| 429 (rate limit) | Backoff (honor Retry-After, cap 5 min), max 3 retries |
| Network timeout | Exponential backoff, max 3 retries |
| 5xx | Exponential backoff, max 3 retries, then skip image |

### Diagnostics

`ocync auth check` validates all configured registries:

```
Registry: 222222222222.dkr.ecr.us-west-2.amazonaws.com
  Method: ecr (role: arn:aws:iam::222222222222:role/ecr-writer)
  Status: authenticated
   Token: valid (expires in 11h 42m)
   Perms: pull=yes  push=yes  list-tags=yes
    FIPS: endpoint=ecr-fips.us-west-2.amazonaws.com  self-test=pass
```

---

## ECR Integration

### Auto-Create Repos

When `ecr.auto_create: true` is set, repositories are created lazily on first push attempt that returns `RepositoryNotFoundException`.

```yaml
ecr:
  auto_create: true
  defaults:
    image_tag_mutability: IMMUTABLE
    image_scanning_on_push: true
    repository_policy_file: policies/pull-policy.json
    lifecycle_policy_file: policies/lifecycle.json
  overrides:
    - match: "security/*"
      image_tag_mutability: MUTABLE
```

- `overrides[].match` uses glob against the repository path (first-match-wins)
- Policy files contain raw IAM policy JSON
- ECR Public does not support policies, lifecycle, or immutability

---

## Security

### Credential Redaction Policy

All output sinks (stderr, log file, `results.json`, structured output) apply credential redaction:

- **URL userinfo**: stripped from all URLs. `https://user:pass@registry.io/v2/` → `https://[REDACTED]@registry.io/v2/`
- **Auth tokens, passwords, AWS secret keys**: replaced with `[REDACTED]`
- **At `-vvv` verbosity**: `Authorization: Bearer [REDACTED (len=142)]` (length shown to aid debugging token issues)
- **`ocync expand`**: shows `${VAR}: [REDACTED]` for sensitive env vars by default. Use `--show-secrets` to override.

Redaction is implemented as a `tracing` layer that filters all span/event fields before they reach any subscriber (file, stderr, structured output). No sensitive value ever reaches an output sink unredacted.

### Env Var Expansion Security

Environment variable expansion follows different rules for auth vs non-auth fields:

**Auth fields** (`token`, `password`, `aws_role_arn`, `aws_external_id`, `token_file`):
- Expansion allowed for all env vars
- Expanded values are tagged as sensitive for redaction in all output

**Non-auth fields** (`url`, `from`, `to`, glob patterns, file paths):
- Expansion allowed by default
- Vars matching these glob patterns are BLOCKED: `*SECRET*`, `*PASSWORD*`, `*TOKEN*`, `*PRIVATE_KEY*`, `*CREDENTIAL*`, `*API_KEY*`
- Blocked vars produce CONFIG_ERROR with message: `env var $AWS_SECRET_ACCESS_KEY blocked in non-auth field 'url'; use env_var_policy.allow to override`

**Override**:
```yaml
env_var_policy:
  allow: ["MY_CUSTOM_TOKEN_URL"]       # allow specific blocked vars in non-auth fields
```

### FIPS Runtime Self-Check

At process startup (before any network I/O):

1. Call `try_fips_mode()` (NOT the panicking `enable_fips_mode()`)
2. On failure: print clear message to stderr and exit 2:
   ```
   FIPS self-check failed: <reason>
   ocync was compiled with --features fips but the FIPS module failed initialization.
   This binary cannot be used in a FIPS-compliant configuration.
   ```
3. `ocync version` output includes:
   ```
   ocync 0.1.0 (abc1234 2026-04-10)
   FIPS 140-3: compiled=yes  self-test=pass  module=aws-lc-rs (NIST #4816)
   ```
4. `ocync auth check` shows per-registry FIPS endpoint usage (see Diagnostics section above)

---

## CLI Commands

| Command | Purpose |
|---|---|
| `ocync sync --config <path>` | Run all mappings |
| `ocync sync --dry-run` | Validate against live registries (all reads, no writes). Queries referrers, reports what would be synced/skipped. |
| `ocync copy <src> <dst>` | Ad-hoc single image copy using full OCI references (standalone, no config) |
| `ocync tags <repo>` | List/filter tags — shows pipeline step-by-step |
| `ocync auth check` | Validate credentials for all registries (shows FIPS endpoint status) |
| `ocync validate <config>` | Offline config syntax/schema check |
| `ocync expand <config>` | Show fully resolved config (env vars, bulk, defaults). Sensitive vars show `[REDACTED]` unless `--show-secrets`. |
| `ocync watch --config <path>` | Daemon mode (interval scheduling, config reload, SIGHUP) |
| `ocync version` | Version info, FIPS compile-time status + self-test result |

---

## Output and Logging

### Philosophy: Silence Means Success

Existing tools get logging catastrophically wrong — skopeo dumps walls of "existing blob" messages, regsync floods with repeated status noise, dregsy swallows errors while being verbose about everything else.

**ocync's rule: output means action was taken or something went wrong. Anything expected and successful is silent.**

### Log Format

Two formats: `text` (human-readable) and `json` (structured JSONL for log aggregators).

**Defaults (auto-detected):**

| Environment | Detection | Default |
|---|---|---|
| Interactive terminal | `isatty(stderr)` = true | `text` |
| CI / non-interactive | `isatty(stderr)` = false | `text` |
| Kubernetes | `KUBERNETES_SERVICE_HOST` set | `json` |

**Always overridable:**

```bash
# CLI flag (highest priority)
ocync sync --log-format json
ocync sync --log-format text

# Environment variable
OCYNC_LOG_FORMAT=json ocync sync

# Config file
log_format: json    # json | text
log_level: default  # default | quiet | verbose | debug | trace
```

Priority: CLI flag > env var > config file > auto-detection.

**JSON format (each line is self-contained):**

```json
{"ts":"2026-04-10T14:30:14Z","level":"info","event":"synced","run_id":"01JQXYZ...","image_id":"01JQXYZ...","source":"cgr.dev/chainguard/nginx:1.27","target":"mirror/nginx:1.27","bytes":196083712,"duration_ms":14200}
{"ts":"2026-04-10T14:30:14Z","level":"info","event":"skipped","run_id":"01JQXYZ...","image_id":"01JQXYZ...","source":"cgr.dev/chainguard/nginx:1.26","target":"mirror/nginx:1.26","reason":"digest_match"}
{"ts":"2026-04-10T14:30:47Z","level":"error","event":"failed","run_id":"01JQXYZ...","image_id":"01JQXYZ...","source":"cgr.dev/chainguard/node:22","target":"mirror/node:22","error":"rate_limited","retries":3}
{"ts":"2026-04-10T14:30:47Z","level":"info","event":"complete","run_id":"01JQXYZ...","stats":{"images_synced":3,"images_skipped":47,"images_failed":1,"layers_transferred":12,"layers_unique_refs":89,"layers_mounted":34,"bytes_transferred":453181440,"duration_seconds":47.2}}
```

Every line parseable by CloudWatch Insights, Datadog, Loki, jq. No multi-line messages, no embedded newlines.

**Text format (same content, human-friendly):**

```
synced   chainguard/nginx:1.27 → mirror/nginx:1.27       (187 MB, 14s)
FAILED   chainguard/node:22 → mirror/node:22             (429 rate limited)
```

### Correlation IDs

Every sync operation carries two correlation IDs, both ULIDs (time-ordered, 26 characters, monotonic within a millisecond):

- **`run_id`** — one per sync cycle (CLI invocation) or watch-mode cycle. All log lines and results within a single sync run share the same `run_id`.
- **`image_id`** — one per image being synced. All log lines related to a specific image (blob transfers, manifest pushes, retries) share the same `image_id`.

Both IDs appear in:
- All structured log lines (JSON format)
- `results.json` output
- Text log lines at `-v` and above

Implementation: `tracing::Span` fields. The `run_id` span wraps the entire sync cycle; `image_id` spans are nested within it.

### Verbosity Levels

Independent of format — verbosity controls WHAT is logged, format controls HOW.

| Level | What shows | Use case |
|---|---|---|
| Default (no flags) | Images synced (new/updated), images failed, stats summary | Normal operation |
| `--quiet` / `-q` | Errors only + exit code. Zero output on success. | Cron jobs |
| `--verbose` / `-v` | + skipped images with reasons, filter results, auth method selected, correlation IDs | Debugging config |
| `-vv` | + individual blob operations, HTTP status codes, retry decisions | Deep debugging |
| `-vvv` | + full HTTP headers, request/response bodies, token details (`Authorization: Bearer [REDACTED (len=142)]`) | Protocol debugging |

Verbosity is also overridable via env var: `OCYNC_LOG_LEVEL=debug` (maps to `-v`), `OCYNC_LOG_LEVEL=trace` (maps to `-vvv`).

### What You NEVER See at Default Level

- "Blob already exists" (expected case — silence)
- "Manifest unchanged" (it's a skip — silence)
- "Authenticated successfully" (if it works, don't mention it)
- HTTP request/response details
- Filter pipeline internals
- Per-layer transfer details

### What You ALWAYS See at Default Level

```
synced   chainguard/nginx:1.27 → mirror/nginx:1.27       (187 MB, 14s)
synced   chainguard/python:3.12 → mirror/python:3.12     (245 MB, 19s)
FAILED   chainguard/node:22 → mirror/node:22             (429 rate limited)

── Stats ─────────────────────────────────────────────────
  Images:  3 synced, 47 skipped, 1 failed
  Layers:  12 transferred (89 unique references), 34 mounted
  Data:    432 MB transferred (1.2 GB total image size)
  Time:    47s elapsed
───────────────────────────────────────────────────────────
```

A successful sync of 50 images where 47 are unchanged produces **the summary only** — not 500 lines of noise.

### Stats (Always Shown)

The stats summary is always printed at the end (even at default verbosity) because it answers the questions operators care about:

| Stat | What it means |
|---|---|
| Images synced | Tags that were actually transferred (new or updated) |
| Images skipped | Tags already at correct digest (zero work) |
| Images failed | Tags that could not be synced (with error count by type) |
| Layers transferred | Individual blobs actually uploaded to target |
| Layers unique references | Total layer references across all images (shows dedup savings) |
| Layers mounted | Blobs linked via cross-repo mount (zero transfer) |
| Data transferred | Actual bytes sent over the wire |
| Total image size | Uncompressed size of all synced images (context for transfer efficiency) |
| Time elapsed | Wall clock duration |

### CI Heartbeats (Non-TTY)

When stderr is not a TTY, long-running transfers emit periodic heartbeats:
```
[2026-04-10T14:30:30Z] chainguard/large-model:v2 — 234/512 MB (45%) at 12.3 MB/s, ~22s remaining
[2026-04-10T14:31:00Z] chainguard/large-model:v2 — 389/512 MB (76%) at 11.8 MB/s, ~10s remaining
```

Interval: every 30s for transfers > 60s. Shorter transfers get no heartbeat (just the completion line).

### Structured Output (stdout or file)

```bash
ocync sync --output results.json
ocync sync --output-format json    # to stdout
```

JSON schema includes per-image: `run_id`, `image_id`, `source`, `destination`, `tag`, `digest`, `status` (synced/skipped/failed), `action` (created/updated/unchanged), `reason`, `size_bytes`, `duration_ms`.

Also includes aggregate stats:
```json
{
  "run_id": "01JQXYZ...",
  "stats": {
    "images_synced": 3,
    "images_skipped": 47,
    "images_failed": 1,
    "layers_transferred": 12,
    "layers_unique_refs": 89,
    "layers_mounted": 34,
    "bytes_transferred": 453181440,
    "total_image_size": 1288490188,
    "duration_seconds": 47.2
  }
}
```

### Log File (Detailed, Non-Blocking)

`--log-file sync.log` captures verbose-level detail to a file regardless of terminal verbosity. Structured JSONL format for machine parsing:
```json
{"ts":"2026-04-10T14:30:01Z","level":"debug","run_id":"01JQXYZ...","image_id":"01JQXYZ...","msg":"blob exists at target","digest":"sha256:abc...","registry":"ecr-us"}
```

This enables post-mortem debugging without flooding the terminal.

### Exit Codes

| Code | Name | Condition |
|---|---|---|
| 0 | SUCCESS | All synced or skipped (digest match). Also used for SIGTERM/SIGINT graceful shutdown. |
| 1 | PARTIAL_FAILURE | Some succeeded, some failed |
| 2 | TOTAL_FAILURE | Nothing succeeded. Also used for FIPS self-check failure. |
| 3 | CONFIG_ERROR | Config parse/validation failure (including unknown registry references) |
| 4 | AUTH_ERROR | Pre-sync authentication failed for one or more registries (not mid-sync auth failures — those produce per-image failures in the SyncReport) |

### `sync()` Returns `SyncReport`, Not `Result`

The sync engine never "fails" as a whole. It always completes and returns a `SyncReport` containing per-image results:

```rust
pub struct SyncReport {
    pub run_id: Ulid,
    pub images: Vec<ImageResult>,
    pub stats: SyncStats,
    pub duration: Duration,
}

pub enum ImageStatus {
    Synced,
    Skipped { reason: SkipReason },
    Failed { error: SyncError, retries: u32 },
}
```

The CLI derives the exit code from the `SyncReport`:
- All `Synced` or `Skipped` → exit 0
- Mix of `Synced`/`Skipped` and `Failed` → exit 1
- All `Failed` → exit 2

Exit 4 (`AUTH_ERROR`) is only for pre-sync auth failures (during the credential validation phase before any sync work begins). Mid-sync auth failures (e.g., token expired during a long sync) produce per-image `Failed` results with an auth error reason.

---

## FIPS 140-3 Support

Enabled via cargo feature flag: `--features fips` (or `ecr-fips` for ECR + FIPS combo).

### What it enables

1. **TLS**: `rustls` + `aws-lc-rs` FIPS module (NIST Certificate #4816)
2. **Digests**: `aws_lc_rs::digest::SHA256` for all content digest computation (via SHA-256 wrapper)
3. **AWS SDK**: `CryptoMode::AwsLcFips` for STS/ECR API calls
4. **ECR endpoints**: Auto-resolves to `ecr-fips.{region}.amazonaws.com`
5. **Runtime self-check**: `try_fips_mode()` at startup (see Security section)

### Two Dockerfile Build Paths

**Non-FIPS build** (default):
```dockerfile
FROM rust:1.85 AS builder
# Target: x86_64-unknown-linux-musl (Alpine builder, truly static)
RUN rustup target add x86_64-unknown-linux-musl
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM cgr.dev/chainguard/static:latest
COPY --from=builder /target/x86_64-unknown-linux-musl/release/ocync /usr/local/bin/ocync
ENTRYPOINT ["ocync"]
```

**FIPS build**:
```dockerfile
FROM rust:1.85 AS builder
# Target: x86_64-unknown-linux-gnu + static CRT linking
# FIPS certification is against glibc, not musl — must use gnu target
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --features fips --target x86_64-unknown-linux-gnu

FROM cgr.dev/chainguard/static:latest
COPY --from=builder /target/x86_64-unknown-linux-gnu/release/ocync /usr/local/bin/ocync
ENTRYPOINT ["ocync"]
```

**Key difference**: FIPS uses `x86_64-unknown-linux-gnu` (not musl) because FIPS certification is validated against glibc. The `+crt-static` flag statically links glibc so the final binary has no dynamic dependencies and runs on `chainguard/static`.

**CI verification**: Both build paths run `readelf -d ocync | grep NEEDED` to confirm zero dynamic library dependencies. Any NEEDED entry fails the CI check.

### Build Requirements (FIPS only)

- CMake, Go, C compiler (Clang or GCC)
- Supported targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`

### Non-FIPS builds

No FIPS build complexity. Standard `cargo build` with default features uses `rustls` + `aws-lc-rs` (non-FIPS mode) — fast, no CMake/Go required.

---

## Platform Support

### Multi-Architecture Defaults

- Default: copy ALL platforms in an image index (omit `platforms` field)
- Per-mapping override: `platforms: [linux/amd64, linux/arm64]`

### ocync Binary Distribution

Cross-compiled for: `linux/amd64`, `linux/arm64`, `darwin/amd64`, `darwin/arm64`. Multi-arch Docker image published.

---

## Watch Mode

### Definition

Watch mode runs ocync as a long-lived daemon that syncs on a fixed interval:

```yaml
watch:
  interval: 5m                         # global, required in watch mode
  shutdown_timeout: 25s                # default: 25s (see Graceful Shutdown)
  failure:
    consecutive_failure_threshold: 3   # default: 3
```

**Key constraints**:
- ONE global interval. No per-mapping schedules.
- Each cycle runs the full sync (plan + execute) for all mappings
- State is preserved between cycles (see target state cache below)

### SIGHUP Config Reload

On receiving SIGHUP:
1. Read and parse the new config file(s)
2. Validate the new config (full pipeline, steps 1-8)
3. If valid: swap to new config, clear tag digest cache (forces full source re-verification on next cycle)
4. If invalid: log ERROR with validation details, keep old config, continue with next cycle

### Tag Digest Cache (Watch Mode)

Watch mode maintains the transfer state cache in memory across sync cycles to avoid redundant source manifest pulls:

- **Source-side caching**: after a successful source manifest pull, records `(source_authority, repo, tag) → SourceSnapshot` containing the source HEAD digest, filtered digest (after platform filtering), and platform filter config hash
- **Target verification is always live**: target HEAD checks are performed every cycle regardless of cache state — the cache only skips source-side work, never target verification
- **Startup**: loads existing cache from disk (warm start from prior CronJob or watch session)
- **Persistence**: persists to disk on graceful shutdown (SIGTERM/SIGINT) only, not every cycle — avoids per-cycle disk I/O while preserving crash recovery
- **End-of-cycle pruning**: tags removed from source (no longer in filtered tag list) are evicted from cache
- **Cleared on SIGHUP** (config reload) — forces re-verification of all tags after config changes

See the discovery optimization spec for the full tag digest cache design, including platform filter hash determinism, cache format versioning, and fallback behavior.

### Health Endpoints

Watch mode starts an HTTP health server on a configurable port (`--health-port`, default 8080):

| Endpoint | Purpose | Response |
|---|---|---|
| `/healthz` | Liveness probe | HTTP 200 if process is alive (trivial check) |
| `/readyz` | Readiness probe | HTTP 200 if last sync completed within `2 * watch.interval`. HTTP 503 if overdue. |
| `/metrics` | Prometheus metrics | See Prometheus Metrics section |

The health server uses a lightweight TCP listener with manual HTTP response formatting — no framework dependency. Prometheus metrics exposition (`/metrics`) and readiness probes (`/readyz`) are planned for a future spec; the current health server provides liveness checking only.

Health endpoints are NOT enabled in CronJob/one-shot mode — only in `ocync watch`.

### Consecutive Failure Handling

If `watch.failure.consecutive_failure_threshold` consecutive cycles produce only failures (all images failed, zero synced/skipped):
- Log CRITICAL with failure count
- `/readyz` returns 503
- `mappings_consecutive_failures` gauge updates per mapping
- ocync continues running (does not exit — let K8s handle restart if needed via liveness probe timeout)

---

## Prometheus Metrics (Planned)

Prometheus metrics are planned but deferred until after the core sync engine, discovery optimization, and watch mode ship. The initial release uses `SyncStats` counters in `--json` structured output for observability. The metrics schema below defines the target state for the metrics endpoint.

Metrics use the `prometheus-client` crate. Exposed at `/metrics` in watch mode.

### Counters (monotonically increasing)

| Metric | Labels | Description |
|---|---|---|
| `ocync_sync_runs_total` | `status={success,partial_failure,total_failure}` | Total sync cycles |
| `ocync_images_synced_total` | `source_registry`, `target_registry` | Images successfully synced |
| `ocync_images_skipped_total` | `source_registry`, `target_registry`, `reason={digest_match,skip_existing,immutable}` | Images skipped |
| `ocync_images_failed_total` | `source_registry`, `target_registry`, `error_type={auth,rate_limit,timeout,server_error}` | Images that failed |
| `ocync_blobs_transferred_total` | `target_registry`, `method={upload,mount}` | Blob transfer operations |
| `ocync_bytes_transferred_total` | `target_registry` | Bytes transferred |
| `ocync_auth_refreshes_total` | `registry`, `result={success,failure}` | Auth token refreshes |
| `ocync_retries_total` | `registry`, `operation={manifest,blob,auth}` | Retry attempts |

### Gauges

| Metric | Labels | Description |
|---|---|---|
| `ocync_sync_in_progress` | — | 1 if sync is running, 0 otherwise |
| `ocync_mappings_consecutive_failures` | `mapping` | Consecutive failure count per mapping |
| `ocync_rate_limit_remaining` | `registry` | Remaining rate limit budget (if detectable) |
| `ocync_config_last_reload_timestamp` | — | Unix timestamp of last successful config reload |

### Histograms

| Metric | Buckets | Description |
|---|---|---|
| `ocync_sync_duration_seconds` | 1, 5, 15, 30, 60, 120, 300, 600 | Total sync cycle duration |
| `ocync_image_sync_duration_seconds` | 0.1, 0.5, 1, 5, 10, 30, 60, 120 | Per-image sync duration |
| `ocync_blob_transfer_duration_seconds` | 0.1, 0.5, 1, 5, 10, 30, 60 | Per-blob transfer duration |
| `ocync_blob_size_bytes` | 1KB, 10KB, 100KB, 1MB, 10MB, 100MB, 1GB | Blob sizes transferred |

### Pushgateway (CronJob mode)

For CronJob deployments where no long-lived `/metrics` endpoint exists:

```yaml
metrics:
  pushgateway_url: http://prometheus-pushgateway:9091
  job_name: ocync
```

At the end of each sync run, all metrics are pushed to the Pushgateway. OTLP export is deferred to a future version.

---

## Graceful Shutdown (SIGTERM/SIGINT)

### Behavior

Default drain deadline: 25s (configurable via `SyncEngine::with_drain_deadline()`). This is chosen to fit within Kubernetes' default 30s `terminationGracePeriodSeconds` with 5s of margin.

On receiving SIGTERM or SIGINT:

1. **Stop promoting new work** — pending items in the `VecDeque` are not promoted to execution
2. **Stop discovery** — discovery futures are no longer polled
3. **Drain in-flight execution** — active execution futures continue until completion or drain deadline
4. **Persist cache** — `TransferStateCache` is persisted to disk after the pipeline loop exits, preserving progress for the next run
5. **Abandon if deadline exceeded** — if drain deadline fires while execution futures remain, they are abandoned and logged

The `select!` drain deadline branch guards on `!execution_futures.is_empty()` so the engine exits immediately when all work completes rather than waiting for the full deadline.

---

## Deployment

### Container Image

Multi-stage Dockerfile producing a minimal image (see Two Dockerfile Build Paths above):

- **Base**: `chainguard/static` (zero CVEs, no shell, no package manager)
- **Multi-arch**: `linux/amd64` + `linux/arm64` via Docker buildx
- **FIPS variant**: separate tag (`ocync:latest-fips`) built with `--features fips`
- **Size target**: < 20 MB compressed

### Helm Chart

Helm chart for deploying ocync as a Kubernetes CronJob, one-shot Job, or Deployment (watch mode):

```
charts/ocync/
  Chart.yaml
  values.yaml
  templates/
    cronjob.yaml          # primary: scheduled sync
    job.yaml              # one-shot sync (for CI/manual triggers)
    deployment.yaml       # watch mode with health/metrics endpoints
    configmap.yaml        # ocync config (from values)
    secret.yaml           # optional static credentials
    serviceaccount.yaml   # for IRSA/workload identity
    rbac.yaml             # if needed for secret access
    service.yaml          # for health/metrics endpoints (watch mode)
    servicemonitor.yaml   # Prometheus ServiceMonitor (optional)
```

**values.yaml key sections:**

```yaml
# Schedule (CronJob)
schedule: "*/15 * * * *"              # every 15 minutes
concurrencyPolicy: Forbid             # don't overlap runs
backoffLimit: 0                        # ocync has internal retries; K8s retries are redundant
failedJobsHistoryLimit: 3              # keep last 3 failed jobs for debugging
activeDeadlineSeconds: 900             # 15min hard timeout

# Image
image:
  repository: ghcr.io/clowdhaus/ocync
  tag: latest
  # tag: latest-fips                   # for FIPS environments

# ocync config (inlined or from existing ConfigMap)
config:
  log_format: json
  log_level: default
  registries:
    chainguard:
      url: cgr.dev
    us-ecr:
      url: 222222222222.dkr.ecr.us-west-2.amazonaws.com
      auth_type: ecr
      ecr:
        auto_create: true
  target_groups:
    default: [us-ecr]
  defaults:
    source: chainguard
    targets: default
    tags:
      glob: "*"
      latest: 20
      sort: semver
  mappings:
    - bulk:
        from_prefix: chainguard/
        to_prefix: mirror/
        names: [nginx, python, node]

# AWS auth via IRSA
serviceAccount:
  create: true
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::222222222222:role/ocync

# Resources
resources:
  requests:
    cpu: 100m
    memory: 128Mi
  limits:
    memory: 512Mi

# Logging
logFormat: json                        # auto | json | text
logLevel: default                      # default | quiet | verbose

# Metrics (watch mode only)
metrics:
  enabled: false
  port: 9090
  serviceMonitor:
    enabled: false

# Additional ECR policy files (mounted from ConfigMap)
policyFiles:
  cross-account-pull.json: |
    {"Version": "2012-10-17", "Statement": [...]}
  lifecycle.json: |
    {"rules": [...]}
```

**CronJob design decisions:**
- **`backoffLimit: 0`** — ocync has internal retries; the next scheduled run IS the retry. Kubernetes cannot distinguish between exit codes for intelligent retry decisions, so K8s-level retries just waste compute.
- **`failedJobsHistoryLimit: 3`** — keep last 3 failed jobs for `kubectl logs` debugging
- **`activeDeadlineSeconds: 900`** — hard timeout as a safety net (15 minutes default)
- **`concurrencyPolicy: Forbid`** — prevent overlapping syncs

**Key Helm features:**
- IRSA annotation on ServiceAccount (EKS) / workload identity (GKE) — no static AWS credentials
- ConfigMap for ocync config + policy files
- Optional Secret for registries that need static credentials
- Pod annotations for Prometheus scraping (when using `watch` mode as a Deployment)

### Deployment Modes

| Mode | K8s Resource | When |
|---|---|---|
| Scheduled sync | CronJob | Standard: run every N minutes |
| One-shot | Job | CI-triggered, manual, or init-time seeding |
| Continuous | Deployment + `ocync watch` | Real-time with config reload, health endpoints, metrics |

---

## Non-Goals (Explicit)

- **No deletion/pruning** — registries handle lifecycle (ECR lifecycle policies, Harbor retention)
- **No image building** — ocync syncs existing images, it does not build them
- **No signature verification** — ocync transfers signatures faithfully but does NOT verify them; verification is the consumer's responsibility
- **No Docker daemon integration** — pure OCI Distribution API only
- **No skopeo dependency** — everything is native
- **No local disk buffering** — blobs stream source → target without touching disk
- **No `date` sort** — O(n) manifest+config fetches make date-based sorting prohibitively expensive; use `alpha` for date-stamped tags
- **No OTLP export (v1)** — Prometheus metrics are P0; OTLP is deferred to a future version
- **No per-mapping schedules in watch mode** — one global interval for simplicity and predictability
