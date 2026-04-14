//! Integration tests for [`TransferStateCache`] persistence.
//!
//! Verifies that the binary cache format survives a round-trip, that expired
//! and corrupt files are silently discarded, and that transient blob states
//! are not written to disk.

use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::RepositoryName;
use ocync_sync::cache::{SourceSnapshot, TransferStateCache, platform_filter_key, snapshot_key};
use ocync_sync::plan::BlobStatus;

const DIGEST_A: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const DIGEST_B: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn digest_a() -> Digest {
    DIGEST_A.parse().unwrap()
}

fn digest_b() -> Digest {
    DIGEST_B.parse().unwrap()
}

/// Alias for `digest_a()` — used in new v2 tests for readability.
fn digest() -> Digest {
    digest_a()
}

/// Alias for `digest_b()` — used in new v2 tests for readability.
fn digest2() -> Digest {
    digest_b()
}

fn repo(name: &str) -> RepositoryName {
    RepositoryName::new(name)
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
    cache.set_blob_exists("reg.io", digest_a(), "repo/alpine".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(
        loaded.blob_status("reg.io", &digest_a()),
        Some(&BlobStatus::ExistsAtTarget)
    );
}

#[test]
fn round_trip_completed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), "repo/alpine".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(
        loaded.blob_status("reg.io", &digest_a()),
        Some(&BlobStatus::Completed)
    );
}

#[test]
fn round_trip_mount_source_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), "repo/a".into());
    cache.set_blob_completed("reg.io", digest_a(), "repo/b".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(
        loaded.blob_mount_source("reg.io", &digest_a(), &RepositoryName::from("repo/b")),
        Some(&RepositoryName::from("repo/a"))
    );
}

#[test]
fn round_trip_multiple_targets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg-a.io", digest_a(), "repo/x".into());
    cache.set_blob_exists("reg-b.io", digest_b(), "repo/y".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(
        loaded.blob_status("reg-a.io", &digest_a()),
        Some(&BlobStatus::Completed)
    );
    assert_eq!(
        loaded.blob_status("reg-b.io", &digest_b()),
        Some(&BlobStatus::ExistsAtTarget)
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
    cache.set_blob_completed("reg.io", digest_a(), "repo/a".into());
    cache.persist(&path).unwrap();

    // The cache stores timestamps at second granularity, so we need
    // the file to be at least 1 second old to exceed a 1-second TTL.
    std::thread::sleep(Duration::from_millis(1100));
    let loaded = TransferStateCache::load(&path, Duration::from_secs(1));
    assert!(loaded.is_empty());
    assert_eq!(loaded.blob_status("reg.io", &digest_a()), None);
}

#[test]
fn zero_ttl_means_never_expire() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed("reg.io", digest_a(), "repo/a".into());
    cache.persist(&path).unwrap();

    // Duration::ZERO disables TTL — the cache never expires by age.
    let loaded = TransferStateCache::load(&path, Duration::ZERO);
    assert!(!loaded.is_empty());
    assert!(loaded.blob_status("reg.io", &digest_a()).is_some());
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
    cache.set_blob_completed("reg.io", digest_a(), "repo/a".into());
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
    cache.set_blob_failed("reg.io", digest_a(), "connection refused".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(loaded.blob_status("reg.io", &digest_a()), None);
}

#[test]
fn in_progress_blobs_not_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_in_progress("reg.io", digest_a());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(loaded.blob_status("reg.io", &digest_a()), None);
}

#[test]
fn only_stable_entries_survive_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    // Stable
    cache.set_blob_completed("reg.io", digest_a(), "repo/a".into());
    // Transient — should not appear after reload
    cache.set_blob_failed("reg.io", digest_b(), "oops".into());

    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());
    assert_eq!(
        loaded.blob_status("reg.io", &digest_a()),
        Some(&BlobStatus::Completed)
    );
    assert_eq!(loaded.blob_status("reg.io", &digest_b()), None);
}

// ---------------------------------------------------------------------------
// Invalidation
// ---------------------------------------------------------------------------

#[test]
fn invalidate_blob_removes_entry() {
    let mut cache = TransferStateCache::new();
    cache.set_blob_exists("reg.io", digest_a(), "repo/a".into());
    assert!(cache.blob_status("reg.io", &digest_a()).is_some());

    cache.invalidate_blob("reg.io", &digest_a());
    assert_eq!(cache.blob_status("reg.io", &digest_a()), None);
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
    cache.set_blob_completed("reg.io", digest_a(), "repo/a".into());

    // Parent dirs don't exist yet — persist() must create them.
    cache.persist(&path).unwrap();
    assert!(path.exists());
}

// ---------------------------------------------------------------------------
// v2 format: SourceSnapshot round-trip
// ---------------------------------------------------------------------------

#[test]
fn v2_roundtrip_with_source_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_exists("reg.io", digest(), repo("repo/a"));
    cache.set_source_snapshot(
        "cgr.dev:443",
        &repo("chainguard/nginx"),
        "latest",
        SourceSnapshot {
            source_digest: digest(),
            filtered_digest: digest2(),
            platform_filter_key: "linux/amd64,linux/arm64".to_string(),
        },
    );

    cache.persist(&path).unwrap();
    let loaded = TransferStateCache::load(&path, Duration::from_secs(3600));

    assert!(loaded.blob_known_at_repo("reg.io", &digest(), &repo("repo/a")));
    let snap = loaded
        .source_snapshot("cgr.dev:443", &repo("chainguard/nginx"), "latest")
        .unwrap();
    assert_eq!(snap.source_digest, digest());
    assert_eq!(snap.filtered_digest, digest2());
    assert_eq!(snap.platform_filter_key, "linux/amd64,linux/arm64");
}

// ---------------------------------------------------------------------------
// v2 format: blobs and snapshots coexist after round-trip
// ---------------------------------------------------------------------------

#[test]
fn v2_coexistence_blobs_and_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    // Multiple blob entries.
    cache.set_blob_exists("reg1.io", digest(), repo("repo/a"));
    cache.set_blob_completed("reg2.io", digest2(), repo("repo/b"));
    // Multiple snapshot entries.
    cache.set_source_snapshot(
        "src1.io:443",
        &repo("lib/nginx"),
        "v1",
        SourceSnapshot {
            source_digest: digest(),
            filtered_digest: digest2(),
            platform_filter_key: "linux/amd64".to_string(),
        },
    );
    cache.set_source_snapshot(
        "src2.io:443",
        &repo("lib/redis"),
        "latest",
        SourceSnapshot {
            source_digest: digest2(),
            filtered_digest: digest(),
            platform_filter_key: String::new(),
        },
    );

    cache.persist(&path).unwrap();
    let loaded = TransferStateCache::load(&path, Duration::from_secs(3600));

    // Verify all blob entries survived.
    assert!(loaded.blob_known_at_repo("reg1.io", &digest(), &repo("repo/a")));
    assert!(loaded.blob_known_at_repo("reg2.io", &digest2(), &repo("repo/b")));
    // Verify all snapshot entries survived.
    let s1 = loaded
        .source_snapshot("src1.io:443", &repo("lib/nginx"), "v1")
        .unwrap();
    assert_eq!(s1.platform_filter_key, "linux/amd64");
    let s2 = loaded
        .source_snapshot("src2.io:443", &repo("lib/redis"), "latest")
        .unwrap();
    assert_eq!(s2.platform_filter_key, "");
}

// ---------------------------------------------------------------------------
// platform_filter_key
// ---------------------------------------------------------------------------

#[test]
fn platform_filter_key_sort_independent() {
    use ocync_distribution::spec::PlatformFilter;

    let a: PlatformFilter = "linux/amd64".parse().unwrap();
    let b: PlatformFilter = "linux/arm64".parse().unwrap();
    assert_eq!(
        platform_filter_key(Some(&[a.clone(), b.clone()])),
        platform_filter_key(Some(&[b, a])),
    );
}

#[test]
fn platform_filter_key_none_and_empty_are_equal() {
    assert_eq!(platform_filter_key(None), "");
    assert_eq!(platform_filter_key(Some(&[])), "");
}

#[test]
fn platform_filter_key_different_sets_differ() {
    use ocync_distribution::spec::PlatformFilter;

    let a: PlatformFilter = "linux/amd64".parse().unwrap();
    let b: PlatformFilter = "linux/arm64".parse().unwrap();
    assert_ne!(
        platform_filter_key(Some(&[a.clone()])),
        platform_filter_key(Some(&[a, b])),
    );
}

// ---------------------------------------------------------------------------
// snapshot_key: NUL separator prevents prefix collisions
// ---------------------------------------------------------------------------

#[test]
fn snapshot_key_nul_separator_prevents_collision() {
    // These have the same total characters but different NUL positions.
    let k1 = snapshot_key("reg.io:443", &repo("lib/nginx"), "v1");
    let k2 = snapshot_key("reg.io:443", &repo("lib"), "nginx/v1");
    assert_ne!(k1, k2);
}
