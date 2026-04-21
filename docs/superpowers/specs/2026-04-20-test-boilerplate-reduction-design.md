# Test Boilerplate Reduction

## Problem

Integration tests are ~61% structural boilerplate. Each new feature adds 500-1000 lines of mostly-identical setup. 103 manual `ImageManifest` constructions, 17 manual `ImageIndex` constructions, and verbose assertion patterns inflate every PR that touches the sync engine.

## Goals

1. New tests are 25-40 lines of intent-expressing code (setup + assert)
2. Each test file is <1500 lines, focused on one subsystem
3. Shared helpers live in one place, all test files import from it
4. `cargo test --test sync_artifacts` runs just the artifact tests (fast iteration)
5. Zero change to what is tested or test coverage

## Non-goals

- Full test harness that owns the engine lifecycle (over-abstraction)
- Custom test macros (proc macros add compile time and debugging complexity)
- Changing the public API of ocync-sync to make it more testable
- Table-driven test collapse (see Appendix A)

## Design

### Phase 1: Structural Split (DONE)

Moved the 12,232-line `engine_integration.rs` monolith into 12 focused `sync_*.rs` files + a shared `tests/helpers/` module. Zero line reduction, zero behavioral change. Merged in PR #46.

### Phase 2: Builder Adoption and Boilerplate Reduction

#### Principle: builders own fixture data, free functions own HTTP mocks

`ManifestParts` keeps only `mount_source` and `mount_target` (the 80% default case). Non-default mock setups compose the parts' fields with free functions in `mocks.rs`. These responsibilities do not merge -- no `mount_source_with_rate_limit` on `ManifestParts`.

#### `builders.rs` extensions

**`ManifestBuilder`** (extend existing):

No changes to the builder API itself. It already covers the common case (config + N layers). The existing `subject: None` and `artifact_type: None` defaults are correct for image manifests -- artifacts use `ArtifactBuilder`.

**`ArtifactBuilder`** (new):

```rust
pub struct ArtifactBuilder {
    config_data: Vec<u8>,
    layer_data: Vec<u8>,
    artifact_type: String,
}

impl ArtifactBuilder {
    /// Create an artifact manifest with the given config and layer blobs.
    pub fn new(config_data: &[u8], layer_data: &[u8]) -> Self {
        Self {
            config_data: config_data.to_vec(),
            layer_data: layer_data.to_vec(),
            artifact_type: "application/vnd.dev.cosign.artifact.sig.v1+json".to_string(),
        }
    }

    /// Override the default artifact type.
    pub fn artifact_type(mut self, t: &str) -> Self {
        self.artifact_type = t.to_string();
        self
    }

    pub fn build(self) -> ArtifactParts { /* ... */ }
}
```

`ArtifactParts` output:

```rust
#[derive(Clone)]
pub struct ArtifactParts {
    pub manifest: ImageManifest,
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub config_data: Vec<u8>,
    pub config_desc: Descriptor,
    pub layer_data: Vec<u8>,
    pub layer_desc: Descriptor,
    pub artifact_type: String,
}
```

Key design decisions:
- `subject` is NOT set on the artifact manifest. Current tests have `subject: None` on artifact manifests. The `subject` field is only used in one test (`artifact_sync_tag_fallback_transfers_referrer`) which constructs it manually -- that test stays explicit.
- `artifact_type` IS set (it's what distinguishes artifacts from images).
- Single layer only (all artifact tests use exactly one config + one layer).
- `ArtifactParts` has `mount_source` and `mount_target` methods mirroring `ManifestParts`, but `mount_source` mounts by digest (not tag) since artifacts are referenced by digest.

```rust
impl ArtifactParts {
    /// Mount source mocks: manifest GET by digest + blob GETs.
    pub async fn mount_source(&self, server: &MockServer, repo: &str);

    /// Mount target mocks: blob HEAD 404s + blob push + manifest push by digest.
    pub async fn mount_target(&self, server: &MockServer, repo: &str);

    /// Build a referrers index descriptor for this artifact.
    pub fn referrers_descriptor(&self) -> Descriptor;
}
```

**`IndexBuilder`** (new, for multi-arch):

```rust
pub struct IndexBuilder {
    children: Vec<(ManifestParts, Option<Platform>)>,
}

impl IndexBuilder {
    pub fn new() -> Self { Self { children: Vec::new() } }

    /// Add a child manifest with an optional platform.
    pub fn manifest(mut self, parts: &ManifestParts, platform: Option<Platform>) -> Self {
        self.children.push((parts.clone(), platform));
        self
    }

    pub fn build(self) -> IndexParts { /* ... */ }
}
```

`IndexParts` output:

```rust
#[derive(Clone)]
pub struct IndexParts {
    pub index: ImageIndex,
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub children: Vec<ManifestParts>,
}

impl IndexParts {
    /// Mount source mocks: index GET by tag + child manifest GETs by digest + all blob GETs.
    pub async fn mount_source(&self, server: &MockServer, repo: &str, tag: &str);

    /// Mount target mocks: index HEAD 404 + all child blob HEAD 404s + push endpoints.
    pub async fn mount_target(&self, server: &MockServer, repo: &str, tag: &str);
}
```

Key: `IndexBuilder` takes `Option<Platform>` directly (not a convenience constructor). The caller builds `Platform { os, architecture, .. }` themselves. This avoids the orphan rule problem -- `Platform` is from `ocync_distribution::spec`, so we cannot add inherent methods to it from the test crate.

**`ReferrersIndexBuilder`** (new, for artifact tests):

```rust
pub struct ReferrersIndexBuilder {
    descriptors: Vec<Descriptor>,
}

impl ReferrersIndexBuilder {
    pub fn new() -> Self { Self { descriptors: Vec::new() } }

    /// Add an artifact's referrers descriptor to the index.
    pub fn descriptor(mut self, desc: Descriptor) -> Self {
        self.descriptors.push(desc);
        self
    }

    /// Convenience: add from ArtifactParts directly.
    pub fn artifact(self, parts: &ArtifactParts) -> Self {
        self.descriptor(parts.referrers_descriptor())
    }

    /// Build the referrers index (serialized bytes).
    pub fn build(self) -> Vec<u8> { /* ... */ }

    /// Build an empty referrers index (for require-artifacts-fails-on-empty tests).
    pub fn build_empty() -> Vec<u8> { /* ... */ }
}
```

Takes `Descriptor` as the primitive input (via `.descriptor()`), with `.artifact(&parts)` as convenience. This keeps it composable with manually-constructed descriptors.

#### `mocks.rs` extensions

Add one new free function:

```rust
/// Mount referrers API GET response for a parent digest.
pub async fn mount_referrers(
    server: &MockServer,
    repo: &str,
    parent_digest: &Digest,
    body: &[u8],
);
```

This mounts a 200 response with `content-type: application/vnd.oci.image.index.v1+json`. For 404 (API unsupported) tests, the test mounts its own `Mock::given(...)` inline -- it's a 4-line deviation, not a common case worth abstracting.

#### `assert_status!` macro

```rust
/// Assert an image report's status matches a pattern, with diagnostic output on failure.
macro_rules! assert_status {
    ($report:expr, $idx:expr, $pattern:pat) => {
        assert!(
            matches!($report.images[$idx].status, $pattern),
            "image[{}]: expected {}, got {:?}",
            $idx,
            stringify!($pattern),
            $report.images[$idx].status,
        );
    };
}
```

Lives in `helpers/mod.rs`. 10 lines. Eliminates the repetitive 4-line assertion-with-debug-message pattern across 100+ call sites.

#### What stays explicit (never abstract away)

- `MockServer::start().await` -- communicates test's server topology
- `SyncEngine::new(...)` -- visible in every test
- `.run(vec![mapping], cache, staging, &NullProgress, shutdown)` -- visible
- `ShutdownSignal::new()` / `None` -- semantically meaningful choice
- `assert_eq!(report.images.len(), N)` -- visible

These are the test's contract with the engine. Hiding them behind a harness makes tests harder to debug and review.

### Example: before and after

**Before** (`artifact_sync_transfers_referrer`, 152 lines):

```rust
let config_data = b"parent-config";
let layer_data = b"parent-layer";
let config_desc = blob_descriptor(config_data, MediaType::OciConfig);
let layer_desc = blob_descriptor(layer_data, MediaType::OciLayerGzip);
let parent_manifest = ImageManifest {
    schema_version: 2, media_type: None,
    config: config_desc.clone(), layers: vec![layer_desc.clone()],
    subject: None, artifact_type: None, annotations: None,
};
let (parent_bytes, parent_digest) = serialize_manifest(&parent_manifest);
// ... 20 more lines of sig manifest construction ...
// ... 15 lines of referrers index construction ...
// ... 30 lines of source mock mounting ...
// ... 20 lines of target mock mounting ...
// ... engine run + assertions ...
```

**After** (~35 lines):

```rust
#[tokio::test]
async fn artifact_sync_transfers_referrer() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;

    let parent = ManifestBuilder::new(b"parent-config").layer(b"parent-layer").build();
    let sig = ArtifactBuilder::new(b"sig-config", b"sig-payload").build();
    let referrers = ReferrersIndexBuilder::new().artifact(&sig).build();

    parent.mount_source(&source, "repo", "v1.0.0").await;
    parent.mount_target(&target, "repo", "v1.0.0").await;
    mount_referrers(&source, "repo", &parent.digest, &referrers).await;
    sig.mount_source(&source, "repo").await;
    sig.mount_target(&target, "repo").await;
    mount_manifest_push(&target, "repo", &sig.digest.to_string()).await;

    let source_client = mock_client(&source);
    let target_client = mock_client(&target);
    let mapping = ResolvedMapping {
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        ..resolved_mapping(
            source_client, "repo", "repo",
            vec![target_entry("target", target_client)],
            vec![TagPair::same("v1.0.0")],
        )
    };

    let engine = SyncEngine::new(fast_retry(), 50);
    let report = engine
        .run(vec![mapping], empty_cache(), BlobStage::disabled(), &NullProgress, None)
        .await;

    assert_eq!(report.images.len(), 1);
    assert_status!(report, 0, ImageStatus::Synced);
}
```

### Migration strategy

Single PR. Migrate all 103 `ImageManifest` constructions and 17 `ImageIndex` constructions to use builders. Order:

1. Add `ArtifactBuilder`, `ArtifactParts`, `IndexBuilder`, `IndexParts`, `ReferrersIndexBuilder` to `helpers/builders.rs`
2. Add `mount_referrers` to `helpers/mocks.rs`
3. Add `assert_status!` macro to `helpers/mod.rs`
4. Migrate `sync_basic.rs` first (validates the pattern, highest count at 17 manual constructions)
5. Migrate remaining files in order of descending manual-construction count: `sync_cache.rs` (20), `sync_concurrent.rs` (18), `sync_artifacts.rs` (15), `sync_discovery.rs` (14), `sync_multi_target.rs` (4), `sync_staging.rs` (4), `sync_shutdown.rs` (3), `sync_platforms.rs` (3), `sync_immutable.rs` (2)
6. Remove `simple_image_manifest`, `make_descriptor`, `serialize_manifest` from `fixtures.rs` once no callers remain

Verification gate per file: `cargo fmt --check && cargo clippy --tests -- -D warnings && cargo test --package ocync-sync`

### Expected outcome

| Metric | Before (Phase 1) | After (Phase 2) |
|--------|-------------------|-----------------|
| Total integration test lines | ~13,000 | ~5,500 |
| Lines per new test | 100-150 | 25-40 |
| Manual ImageManifest constructions | 103 | 0 |
| Manual ImageIndex constructions | 17 | 0 |
| Largest file | 3,817 (sync_discovery) | ~1,500 |
| ManifestBuilder adoption | ~5% | 100% |

## Appendix A: Why not table-driven tests

Reviewed and rejected for engine integration tests. Table-driven (`for case in cases`) is appropriate for pure functions but not for tests where mock topology varies. Specific problems:

1. **Early exit** -- First failing case panics, skipping remaining cases. Loses visibility.
2. **MockServer isolation** -- Each iteration needs fresh servers (wiremock mocks accumulate).
3. **State isolation** -- Cache, staging, shutdown all need fresh instances per iteration.
4. **Test filtering lost** -- Cannot `cargo test` a specific case name within the loop.
5. **Meta-builder smell** -- A 7-field `Case` struct where most tests vary 2-3 fields is complexity disguised as data.

For test clusters that genuinely vary by 1-2 parameters, use a helper function with those parameters -- not a struct:

```rust
async fn run_head_first_test(
    source_response: u16,
    target_head_digest: Option<&Digest>,
) -> SyncReport { ... }
```
