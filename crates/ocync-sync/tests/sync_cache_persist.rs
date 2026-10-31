//! Integration tests for [`TransferStateCache`] persistence.
//!
//! Verifies that the binary cache format survives a round-trip, that expired
//! and corrupt files are silently discarded, and that transient blob states
//! are not written to disk.

#[path = "helpers/digests.rs"]
mod digests;

use std::path::Path;
use std::time::Duration;

use ocync_distribution::Digest;
use ocync_distribution::spec::{PlatformFilter, RegistryAuthority};
use ocync_sync::cache::{PlatformFilterKey, SnapshotKey, SourceSnapshot, TransferStateCache};

use digests::{authority, digest_a, digest_b, repo};

fn snap_key_at(authority: &RegistryAuthority, repo_name: &str, tag: &str) -> SnapshotKey {
    SnapshotKey::new(authority, &repo(repo_name), tag)
}

fn no_platform_filter() -> PlatformFilterKey {
    PlatformFilterKey::from_filters(None)
}

fn platform_filter(platforms: &[&str]) -> PlatformFilterKey {
    let filters: Vec<PlatformFilter> = platforms.iter().map(|p| p.parse().unwrap()).collect();
    PlatformFilterKey::from_filters(Some(&filters))
}

fn snapshot(
    source: Digest,
    filtered: Digest,
    platform_filter_key: PlatformFilterKey,
) -> SourceSnapshot {
    SourceSnapshot {
        source_digest: source,
        filtered_digest: filtered,
        platform_filter_key,
    }
}

/// A large but finite `max_age` used to mean "never expires during the test".
fn long_ttl() -> Duration {
    Duration::from_secs(86_400 * 365)
}

/// A cache with one completed blob, persisted to `path`.
fn persist_one_completed_blob(path: &Path) {
    let mut cache = TransferStateCache::new();
    cache.set_blob_completed(&authority("reg.io"), digest_a(), repo("repo/a"));
    cache.persist(path).unwrap();
}

/// Which stable state a blob is recorded in.
#[derive(Clone, Copy, Debug)]
enum Stable {
    Verified,
    Completed,
}

/// Both stable states survive a round trip, per registry, and are not offered
/// for a registry they were not recorded at.
#[test]
fn round_trip_stable_blobs_survive() {
    // (registry, state, digest, repo)
    type Entry = (&'static str, Stable, fn() -> Digest, &'static str);
    let cases: [(&str, Vec<Entry>); 3] = [
        (
            "verified",
            vec![("reg.io", Stable::Verified, digest_a, "repo/alpine")],
        ),
        (
            "completed",
            vec![("reg.io", Stable::Completed, digest_a, "repo/alpine")],
        ),
        (
            "two registries",
            vec![
                ("reg-a.io", Stable::Completed, digest_a, "repo/x"),
                ("reg-b.io", Stable::Verified, digest_b, "repo/y"),
            ],
        ),
    ];
    for (name, entries) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.bin");

        let mut cache = TransferStateCache::new();
        for (registry, state, digest, repo_name) in &entries {
            match state {
                Stable::Verified => {
                    cache.set_blob_verified(&authority(registry), digest(), repo(repo_name))
                }
                Stable::Completed => {
                    cache.set_blob_completed(&authority(registry), digest(), repo(repo_name))
                }
            }
        }
        cache.persist(&path).unwrap();

        let loaded = TransferStateCache::load(&path, long_ttl());
        for (registry, state, digest, repo_name) in &entries {
            assert!(
                loaded.blob_present_at(&authority(registry), &digest(), &repo(repo_name)),
                "{name}: {state:?} blob at {registry} should survive"
            );
            assert!(
                !loaded.blob_present_at(&authority("other.io"), &digest(), &repo(repo_name)),
                "{name}: blob must not appear at a registry it was not recorded at"
            );
        }
    }
}

#[test]
fn round_trip_mount_source_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    let mut cache = TransferStateCache::new();
    cache.set_blob_completed(&authority("reg.io"), digest_a(), repo("repo/a"));
    cache.set_blob_completed(&authority("reg.io"), digest_a(), repo("repo/b"));

    cache.persist(&path).unwrap();

    let mut loaded = TransferStateCache::load(&path, long_ttl());
    // Committed repos are runtime-only: a loaded `Verified` entry is offered
    // as a mount source only once its repo is committed.
    loaded.mark_repo_committed(&authority("reg.io"), &repo("repo/a"));
    assert_eq!(
        loaded
            .mount_sources(&authority("reg.io"), &digest_a(), &repo("repo/b"))
            .next()
            .cloned(),
        Some(repo("repo/a"))
    );
}

/// The cache stores timestamps at second granularity, so the file must be
/// over a second old to exceed a 1-second TTL; a zero TTL disables expiry.
#[test]
fn ttl_expires_old_caches_and_zero_never_does() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");
    persist_one_completed_blob(&path);
    std::thread::sleep(Duration::from_millis(1100));

    for (name, ttl, expect_kept) in [
        ("older than the ttl", Duration::from_secs(1), false),
        ("zero ttl never expires", Duration::ZERO, true),
    ] {
        let loaded = TransferStateCache::load(&path, ttl);
        assert_eq!(loaded.is_empty(), !expect_kept, "{name}");
        assert_eq!(
            loaded.blob_present_at(&authority("reg.io"), &digest_a(), &repo("repo/a")),
            expect_kept,
            "{name}"
        );
    }
}

/// A file that cannot be read back as a cache is discarded, never an error.
#[test]
fn unreadable_files_load_as_empty() {
    // Turns a valid cache file's bytes into the file under test; `None` leaves no file.
    type Damage = fn(Vec<u8>) -> Option<Vec<u8>>;
    let cases: [(&str, Damage); 5] = [
        ("missing file", |_| None),
        ("not a cache", |_| {
            Some(b"this is not valid cache data at all".to_vec())
        }),
        ("empty file", |_| Some(Vec::new())),
        ("flipped byte breaks the CRC", |mut bytes| {
            let mid = bytes.len() / 2;
            bytes[mid] ^= 0xFF;
            Some(bytes)
        }),
        ("wrong version with a valid CRC", |mut bytes| {
            // The version is the first byte of the postcard header, at offset 4
            // (after the 4-byte header length). The trailing 4 bytes are the CRC.
            bytes[4] = 99;
            let payload_len = bytes.len() - 4;
            let crc = crc32fast::hash(&bytes[..payload_len]);
            bytes[payload_len..].copy_from_slice(&crc.to_le_bytes());
            Some(bytes)
        }),
    ];
    for (name, damage) in cases {
        let dir = tempfile::tempdir().unwrap();
        let valid = dir.path().join("valid.bin");
        persist_one_completed_blob(&valid);
        let path = dir.path().join("cache.bin");
        if let Some(bytes) = damage(std::fs::read(&valid).unwrap()) {
            std::fs::write(&path, bytes).unwrap();
        }

        let loaded = TransferStateCache::load(&path, long_ttl());
        assert!(loaded.is_empty(), "{name}");
    }
}

/// Only `Verified` and `Completed` blobs reach the disk; a stable blob in the
/// same cache still does, so the filter is not just dropping everything.
#[test]
fn transient_blobs_are_not_persisted() {
    type Mark = fn(&mut TransferStateCache);
    let cases: [(&str, Mark); 2] = [
        ("failed", |cache| {
            cache.set_blob_failed(
                &authority("reg.io"),
                digest_a(),
                repo("repo/a"),
                "connection refused".into(),
            );
        }),
        ("uploading (in progress)", |cache| {
            cache.claim_blob_upload(&authority("reg.io"), &digest_a(), &repo("repo/a"));
        }),
    ];
    for (name, mark_transient) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.bin");

        // A transient state reads as absent even in a live cache, so only an
        // empty load proves it was not written.
        let mut cache = TransferStateCache::new();
        mark_transient(&mut cache);
        cache.persist(&path).unwrap();
        assert!(
            TransferStateCache::load(&path, long_ttl()).is_empty(),
            "{name} blob should not be persisted"
        );

        cache.set_blob_completed(&authority("reg.io"), digest_b(), repo("repo/b"));
        cache.persist(&path).unwrap();
        let loaded = TransferStateCache::load(&path, long_ttl());
        assert!(
            loaded.blob_present_at(&authority("reg.io"), &digest_b(), &repo("repo/b")),
            "{name}: the completed blob should survive persist"
        );
    }
}

#[test]
fn persist_creates_nested_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("b").join("c").join("cache.bin");

    persist_one_completed_blob(&path);
    assert!(path.exists());
}

/// Blobs and source snapshots share one file: every snapshot field, including
/// the platform filter key, comes back, alongside the blobs.
#[test]
fn blobs_and_source_snapshots_round_trip_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.bin");

    // (key, snapshot) with a two-platform, a one-platform and no platform filter.
    let snapshots = [
        (
            snap_key_at(
                &RegistryAuthority::new("cgr.dev:443"),
                "chainguard/nginx",
                "latest",
            ),
            snapshot(
                digest_a(),
                digest_b(),
                platform_filter(&["linux/amd64", "linux/arm64"]),
            ),
        ),
        (
            snap_key_at(&RegistryAuthority::new("src1.io:443"), "lib/nginx", "v1"),
            snapshot(digest_a(), digest_b(), platform_filter(&["linux/amd64"])),
        ),
        (
            snap_key_at(
                &RegistryAuthority::new("src2.io:443"),
                "lib/redis",
                "latest",
            ),
            snapshot(digest_b(), digest_a(), no_platform_filter()),
        ),
    ];

    let mut cache = TransferStateCache::new();
    cache.set_blob_verified(&authority("reg1.io"), digest_a(), repo("repo/a"));
    cache.set_blob_completed(&authority("reg2.io"), digest_b(), repo("repo/b"));
    for (key, snap) in &snapshots {
        cache.set_source_snapshot(key.clone(), snap.clone());
    }
    cache.persist(&path).unwrap();

    let loaded = TransferStateCache::load(&path, long_ttl());

    assert!(loaded.blob_present_at(&authority("reg1.io"), &digest_a(), &repo("repo/a")));
    assert!(loaded.blob_present_at(&authority("reg2.io"), &digest_b(), &repo("repo/b")));
    for (key, want) in &snapshots {
        let got = loaded
            .source_snapshot(key)
            .unwrap_or_else(|| panic!("snapshot survives: {key:?}"));
        assert_eq!(got.source_digest, want.source_digest, "{key:?}");
        assert_eq!(got.filtered_digest, want.filtered_digest, "{key:?}");
        assert_eq!(got.platform_filter_key, want.platform_filter_key, "{key:?}");
    }
}

/// Whether two platform filter sets must map to the same key.
#[derive(Clone, Copy, Debug)]
enum Relation {
    Same,
    Different,
}

/// Platform list as a filter takes it; `None` is no filter.
type Platforms<'a> = Option<&'a [&'a str]>;

#[test]
fn platform_filter_key_identity() {
    let cases: [(&str, Platforms<'_>, Platforms<'_>, Relation); 3] = [
        (
            "order does not matter",
            Some(&["linux/amd64", "linux/arm64"]),
            Some(&["linux/arm64", "linux/amd64"]),
            Relation::Same,
        ),
        ("none equals empty", None, Some(&[]), Relation::Same),
        (
            "a subset differs from the full set",
            Some(&["linux/amd64"]),
            Some(&["linux/amd64", "linux/arm64"]),
            Relation::Different,
        ),
    ];
    for (name, left, right, relation) in cases {
        let key = |platforms: Platforms<'_>| match platforms {
            Some(p) => platform_filter(p),
            None => no_platform_filter(),
        };
        match relation {
            Relation::Same => assert_eq!(key(left), key(right), "{name}"),
            Relation::Different => assert_ne!(key(left), key(right), "{name}"),
        }
    }
}

#[test]
fn snapshot_key_prevents_field_collision() {
    // Same total characters but different field boundaries.
    let k1 = snap_key_at(&RegistryAuthority::new("reg.io:443"), "lib/nginx", "v1");
    let k2 = snap_key_at(&RegistryAuthority::new("reg.io:443"), "lib", "nginx/v1");
    assert_ne!(k1, k2);
}

#[test]
fn snapshot_lookup_returns_none_for_unknown_key() {
    let mut cache = TransferStateCache::new();
    cache.set_source_snapshot(
        snap_key_at(&RegistryAuthority::new("src.io:443"), "repo/a", "latest"),
        snapshot(digest_a(), digest_b(), no_platform_filter()),
    );
    for (name, key) in [
        (
            "authority",
            snap_key_at(&RegistryAuthority::new("other.io:443"), "repo/a", "latest"),
        ),
        (
            "repo",
            snap_key_at(&RegistryAuthority::new("src.io:443"), "repo/b", "latest"),
        ),
        (
            "tag",
            snap_key_at(&RegistryAuthority::new("src.io:443"), "repo/a", "v2"),
        ),
    ] {
        assert!(
            cache.source_snapshot(&key).is_none(),
            "a different {name} must not match"
        );
    }
}
