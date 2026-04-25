//! Integration tests for [`TransferStateCache`] persistence.
//!
//! Verifies that the binary cache format survives a round-trip, that expired
//! and corrupt files are silently discarded, and that transient blob states
//! are not written to disk.

use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::RegistryAuthority;
use ocync_distribution::spec::RepositoryName;
use ocync_sync::cache::{PlatformFilterKey, SnapshotKey, SourceSnapshot, TransferStateCache};

const DIGEST_A: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const DIGEST_B: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn digest_a() -> Digest {
    DIGEST_A.parse().unwrap()
}

fn digest_b() -> Digest {
    DIGEST_B.parse().unwrap()
}

fn digest() -> Digest {
    digest_a()
}

fn digest2() -> Digest {
    digest_b()
}

fn repo(name: &str) -> RepositoryName {
    RepositoryName::new(name).unwrap()
}

fn authority(s: &str) -> RegistryAuthority {
    RegistryAuthority::new(s)
}

fn snap_key(auth: &str, repo_name: &str, tag: &str) -> SnapshotKey {
    SnapshotKey::new(&authority(auth), &repo(repo_name), tag)
}

fn no_platform_filter() -> PlatformFilterKey {
    PlatformFilterKey::from_filters(None)
}

/// A large but finite `max_age` used to mean "never expires during the test".
fn long_ttl() -> Duration {
    Duration::from_secs(86_400 * 365)
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_exists_at_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_verified("reg.io", digest_a(), repo("repo/alpine"));

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(
        loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/alpine")),
        "verified blob should survive round-trip"
    );
}

#[test]
fn round_trip_completed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/alpine"));

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(
        loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/alpine")),
        "completed blob should survive round-trip"
    );
}

#[test]
fn round_trip_mount_source_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/b"));

    cache.persist(&path).unwrap();

    let mut loaded = TransferStateCache::load(&path, long_ttl());
    // committed_repos is runtime-only (not persisted). Mark committed
    // to verify the blob repos data survived the round-trip.
    loaded.mark_repo_committed("reg.io", &repo("repo/a"));
    // committed_mount_sources returns an iterator of repos that are present
    // and committed; repo/a should appear as a mount source for repo/b.
    assert_eq!(
        loaded
            .committed_mount_sources("reg.io", &digest_a(), &repo("repo/b"))
            .next()
            .cloned(),
        Some(repo("repo/a"))
    );
}

#[test]
fn round_trip_multiple_targets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg-a.io", digest_a(), repo("repo/x"));
    cache.set_blob_verified("reg-b.io", digest_b(), repo("repo/y"));

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(
        loaded.blob_present_at("reg-a.io", &digest_a(), &repo("repo/x")),
        "completed blob at reg-a should survive round-trip"
    );
    assert!(
        loaded.blob_present_at("reg-b.io", &digest_b(), &repo("repo/y")),
        "verified blob at reg-b should survive round-trip"
    );
}

// ---------------------------------------------------------------------------
// TTL / expiry
// ---------------------------------------------------------------------------

#[test]
fn expired_cache_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));
    cache.persist(&path).unwrap();

    // The cache stores timestamps at second granularity, so we need
    // the file to be at least 1 second old to exceed a 1-second TTL.
    std::thread::sleep(Duration::from_millis(1100));
    let loaded = TransferStateCache::load(&path, Duration::from_secs(1));
    assert!(loaded.is_empty());
    assert!(
        !loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "expired cache should not return blobs"
    );
}

#[test]
fn zero_ttl_means_never_expire() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));
    cache.persist(&path).unwrap();

    // Duration::ZERO disables TTL -- the cache never expires by age.
    let loaded = TransferStateCache::load(&path, Duration::ZERO);
    assert!(!loaded.is_empty());
    assert!(
        loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "zero TTL should never expire the cache"
    );
}

// ---------------------------------------------------------------------------
// Missing / corrupt files
// ---------------------------------------------------------------------------

#[test]
fn missing_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.bin");

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(loaded.is_empty());
}

#[test]
fn corrupt_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");
    std::fs::write(&path, b"this is not valid cache data at all").unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(loaded.is_empty());
}

#[test]
fn bad_crc_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));
    cache.persist(&path).unwrap();

    // Flip a byte in the middle of the file to break the CRC.
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(loaded.is_empty());
}

#[test]
fn empty_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");
    std::fs::write(&path, b"").unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(loaded.is_empty());
}

// ---------------------------------------------------------------------------
// Transient states not persisted
// ---------------------------------------------------------------------------

#[test]
fn failed_blobs_not_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_failed(
        "reg.io",
        digest_a(),
        repo("repo/a"),
        "connection refused".into(),
    );

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(
        !loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "failed blob should not be persisted"
    );
}

#[test]
fn in_progress_blobs_not_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    // claim_blob_upload sets the repo's state to Uploading (the new in-progress).
    cache.claim_blob_upload("reg.io", &digest_a(), &repo("repo/a"));

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(
        !loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "uploading (in-progress) blob should not be persisted"
    );
}

#[test]
fn only_stable_entries_survive_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    // Stable
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));
    // Transient -- should not appear after reload
    cache.set_blob_failed("reg.io", digest_b(), repo("repo/b"), "oops".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(
        loaded.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "completed blob should survive persist"
    );
    assert!(
        !loaded.blob_present_at("reg.io", &digest_b(), &repo("repo/b")),
        "failed blob should not survive persist"
    );
}

// ---------------------------------------------------------------------------
// Invalidation
// ---------------------------------------------------------------------------

#[test]
fn invalidate_blob_removes_entry() {
    let mut cache = TransferStateCache::new();
    cache.set_blob_verified("reg.io", digest_a(), repo("repo/a"));
    assert!(
        cache.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "blob should be present before invalidation"
    );

    cache.invalidate_blob("reg.io", &digest_a());
    assert!(
        !cache.blob_present_at("reg.io", &digest_a(), &repo("repo/a")),
        "blob should not be present after invalidation"
    );
}

#[test]
fn invalidate_nonexistent_is_noop() {
    let mut cache = TransferStateCache::new();
    // Should not panic
    cache.invalidate_blob("reg.io", &digest_a());
    assert!(cache.is_empty());
}

// ---------------------------------------------------------------------------
// Persist creates parent dirs
// ---------------------------------------------------------------------------

#[test]
fn persist_creates_nested_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("b").join("c").join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));

    // Parent dirs don't exist yet -- persist() must create them.
    cache.persist(&path).unwrap();
    assert!(path.exists());
}

// ---------------------------------------------------------------------------
// SourceSnapshot round-trip
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_with_source_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let key = snap_key("cgr.dev:443", "chainguard/nginx", "latest");
    let pfk = PlatformFilterKey::from_filters(Some(&[
        "linux/amd64".parse().unwrap(),
        "linux/arm64".parse().unwrap(),
    ]));

    let mut cache = TransferStateCache::new();
    cache.set_blob_verified("reg.io", digest(), repo("repo/a"));
    cache.set_source_snapshot(
        key.clone(),
        SourceSnapshot {
            source_digest: digest(),
            filtered_digest: digest2(),
            platform_filter_key: pfk.clone(),
        },
    );

    cache.persist(&path).unwrap();
    let loaded = TransferStateCache::load(&path, Duration::from_secs(3600));

    assert!(loaded.blob_present_at("reg.io", &digest(), &repo("repo/a")));
    let snap = loaded.source_snapshot(&key).unwrap();
    assert_eq!(snap.source_digest, digest());
    assert_eq!(snap.filtered_digest, digest2());
    assert_eq!(snap.platform_filter_key, pfk);
}

// ---------------------------------------------------------------------------
// Blobs and snapshots coexist after round-trip
// ---------------------------------------------------------------------------

#[test]
fn coexistence_blobs_and_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let key1 = snap_key("src1.io:443", "lib/nginx", "v1");
    let key2 = snap_key("src2.io:443", "lib/redis", "latest");

    let mut cache = TransferStateCache::new();
    // Multiple blob entries.
    cache.set_blob_verified("reg1.io", digest(), repo("repo/a"));
    cache.set_blob_completed("reg2.io", digest2(), repo("repo/b"));
    // Multiple snapshot entries.
    cache.set_source_snapshot(
        key1.clone(),
        SourceSnapshot {
            source_digest: digest(),
            filtered_digest: digest2(),
            platform_filter_key: PlatformFilterKey::from_filters(Some(&["linux/amd64"
                .parse()
                .unwrap()])),
        },
    );
    cache.set_source_snapshot(
        key2.clone(),
        SourceSnapshot {
            source_digest: digest2(),
            filtered_digest: digest(),
            platform_filter_key: no_platform_filter(),
        },
    );

    cache.persist(&path).unwrap();
    let loaded = TransferStateCache::load(&path, Duration::from_secs(3600));

    // Verify all blob entries survived.
    assert!(loaded.blob_present_at("reg1.io", &digest(), &repo("repo/a")));
    assert!(loaded.blob_present_at("reg2.io", &digest2(), &repo("repo/b")));
    // Verify all snapshot entries survived.
    let s1 = loaded.source_snapshot(&key1).unwrap();
    assert_eq!(
        s1.platform_filter_key,
        PlatformFilterKey::from_filters(Some(&["linux/amd64".parse().unwrap()]))
    );
    let s2 = loaded.source_snapshot(&key2).unwrap();
    assert_eq!(s2.platform_filter_key, no_platform_filter());
}

// ---------------------------------------------------------------------------
// PlatformFilterKey
// ---------------------------------------------------------------------------

#[test]
fn platform_filter_key_sort_independent() {
    use ocync_distribution::spec::PlatformFilter;

    let a: PlatformFilter = "linux/amd64".parse().unwrap();
    let b: PlatformFilter = "linux/arm64".parse().unwrap();
    assert_eq!(
        PlatformFilterKey::from_filters(Some(&[a.clone(), b.clone()])),
        PlatformFilterKey::from_filters(Some(&[b, a])),
    );
}

#[test]
fn platform_filter_key_none_and_empty_are_equal() {
    assert_eq!(
        PlatformFilterKey::from_filters(None),
        PlatformFilterKey::from_filters(Some(&[]))
    );
}

#[test]
fn platform_filter_key_different_sets_differ() {
    use ocync_distribution::spec::PlatformFilter;

    let a: PlatformFilter = "linux/amd64".parse().unwrap();
    let b: PlatformFilter = "linux/arm64".parse().unwrap();
    assert_ne!(
        PlatformFilterKey::from_filters(Some(std::slice::from_ref(&a))),
        PlatformFilterKey::from_filters(Some(&[a, b])),
    );
}

// ---------------------------------------------------------------------------
// SnapshotKey: structured key prevents field collisions
// ---------------------------------------------------------------------------

#[test]
fn snapshot_key_prevents_field_collision() {
    // Same total characters but different field boundaries.
    let k1 = snap_key("reg.io:443", "lib/nginx", "v1");
    let k2 = snap_key("reg.io:443", "lib", "nginx/v1");
    assert_ne!(k1, k2);
}

// ---------------------------------------------------------------------------
// Negative snapshot lookup
// ---------------------------------------------------------------------------

#[test]
fn snapshot_lookup_returns_none_for_unknown_key() {
    let mut cache = TransferStateCache::new();
    cache.set_source_snapshot(
        snap_key("src.io:443", "repo/a", "latest"),
        SourceSnapshot {
            source_digest: digest(),
            filtered_digest: digest2(),
            platform_filter_key: no_platform_filter(),
        },
    );
    // Different authority.
    assert!(
        cache
            .source_snapshot(&snap_key("other.io:443", "repo/a", "latest"))
            .is_none()
    );
    // Different repo.
    assert!(
        cache
            .source_snapshot(&snap_key("src.io:443", "repo/b", "latest"))
            .is_none()
    );
    // Different tag.
    assert!(
        cache
            .source_snapshot(&snap_key("src.io:443", "repo/a", "v2"))
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// prune_snapshots
// ---------------------------------------------------------------------------

#[test]
fn prune_snapshots_removes_unlisted_keys() {
    let mut cache = TransferStateCache::new();
    let keep = snap_key("src.io:443", "repo/a", "v1");
    let drop = snap_key("src.io:443", "repo/b", "v2");
    cache.set_source_snapshot(
        keep.clone(),
        SourceSnapshot {
            source_digest: digest(),
            filtered_digest: digest(),
            platform_filter_key: no_platform_filter(),
        },
    );
    cache.set_source_snapshot(
        drop.clone(),
        SourceSnapshot {
            source_digest: digest2(),
            filtered_digest: digest2(),
            platform_filter_key: no_platform_filter(),
        },
    );

    let mut live = std::collections::HashSet::new();
    live.insert(keep.clone());
    cache.prune_snapshots(&live);

    assert!(cache.source_snapshot(&keep).is_some());
    assert!(cache.source_snapshot(&drop).is_none());
}

// ---------------------------------------------------------------------------
// Version mismatch discards cache
// ---------------------------------------------------------------------------

#[test]
fn wrong_version_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    // Write a valid cache, then patch the version byte to something wrong.
    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), repo("repo/a"));
    cache.persist(&path).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    // Version is the first byte of the postcard-serialized header, at offset 4
    // (after the 4-byte header_len). Patch it to 99.
    bytes[4] = 99;
    // Recompute CRC.
    let payload_len = bytes.len() - 4;
    let crc = crc32fast::hash(&bytes[..payload_len]);
    bytes[payload_len..].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert!(loaded.is_empty());
}
