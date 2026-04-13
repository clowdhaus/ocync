//! Integration tests for [`BlobStage`].
//!
//! Covers the round-trip, crash recovery, eviction, and disabled-mode paths.

use std::collections::HashMap;
use std::io;

use ocync_sync::staging::BlobStage;

const DIGEST_A: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const DIGEST_B: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_C: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn digest_a() -> ocync_distribution::Digest {
    DIGEST_A.parse().unwrap()
}

fn digest_b() -> ocync_distribution::Digest {
    DIGEST_B.parse().unwrap()
}

fn digest_c() -> ocync_distribution::Digest {
    DIGEST_C.parse().unwrap()
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

#[test]
fn stage_and_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    let data = b"hello staging world";
    stage.write(&digest_a(), data).unwrap();

    assert!(stage.exists(&digest_a()));

    let read_back = stage.read(&digest_a()).unwrap();
    assert_eq!(read_back, data);
}

#[test]
fn exists_returns_false_for_unstaged_blob() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    assert!(!stage.exists(&digest_a()));
}

#[test]
fn write_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    // Nest the staging directory several levels deep.
    let base = dir.path().join("a").join("b").join("c");
    let stage = BlobStage::new(base);

    stage.write(&digest_a(), b"data").unwrap();
    assert!(stage.exists(&digest_a()));
}

// ---------------------------------------------------------------------------
// Crash recovery: cleanup_tmp_files
// ---------------------------------------------------------------------------

#[test]
fn cleanup_tmp_files_removes_orphaned_files() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    // Write a real blob so the directory structure exists.
    stage.write(&digest_a(), b"real data").unwrap();

    // Plant a fake orphaned tmp file alongside the real blob.
    let algo_dir = dir.path().join("blobs").join("sha256");
    let orphan =
        algo_dir.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tmp.99999");
    std::fs::write(&orphan, b"incomplete").unwrap();

    assert!(orphan.exists(), "orphan must exist before cleanup");

    stage.cleanup_tmp_files().unwrap();

    assert!(!orphan.exists(), "orphan must be removed after cleanup");
    // The real blob must survive.
    assert!(stage.exists(&digest_a()));
}

#[test]
fn cleanup_tmp_files_is_noop_when_dir_missing() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    // No blobs written; blobs/ dir does not exist yet.
    stage.cleanup_tmp_files().unwrap();
}

// ---------------------------------------------------------------------------
// Eviction
// ---------------------------------------------------------------------------

#[test]
fn evict_keeps_high_ref_removes_low_ref() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    // Write three blobs with known content so we can measure sizes.
    let small = vec![0u8; 10];
    let medium = vec![1u8; 20];
    let large = vec![2u8; 30];

    stage.write(&digest_a(), &small).unwrap();
    stage.write(&digest_b(), &medium).unwrap();
    stage.write(&digest_c(), &large).unwrap();

    // Total = 60 bytes. Limit to 40 bytes, so 20+ bytes must be evicted.
    // Ref counts: A=0 (evicted first), B=1, C=5 (kept last).
    let mut ref_counts = HashMap::new();
    ref_counts.insert(digest_b(), 1_usize);
    ref_counts.insert(digest_c(), 5_usize);

    stage.evict(40, &ref_counts).unwrap();

    // digest_a has no ref count (treated as 0) — evicted first.
    assert!(!stage.exists(&digest_a()), "low-ref blob must be evicted");
    // After evicting A (10 bytes), total = 50 — still over 40.
    // Next evict B (20 bytes), total = 30 — now under 40. C is kept.
    assert!(!stage.exists(&digest_b()), "mid-ref blob must be evicted");
    assert!(stage.exists(&digest_c()), "high-ref blob must be kept");
}

#[test]
fn evict_noop_when_under_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    stage.write(&digest_a(), b"small").unwrap();

    let ref_counts = HashMap::new();
    stage.evict(1_000_000, &ref_counts).unwrap();

    // Nothing should be evicted.
    assert!(stage.exists(&digest_a()));
}

#[test]
fn evict_noop_when_blobs_dir_missing() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    // No writes; blobs/ dir doesn't exist.
    stage.evict(0, &HashMap::new()).unwrap();
}

// ---------------------------------------------------------------------------
// Disabled mode
// ---------------------------------------------------------------------------

#[test]
fn disabled_is_not_enabled() {
    let stage = BlobStage::disabled();
    assert!(!stage.is_enabled());
}

#[test]
fn enabled_is_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    assert!(stage.is_enabled());
}

#[test]
fn disabled_exists_returns_false() {
    let stage = BlobStage::disabled();
    assert!(!stage.exists(&digest_a()));
}

#[test]
fn disabled_write_returns_unsupported_error() {
    let stage = BlobStage::disabled();
    let err = stage.write(&digest_a(), b"data").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn disabled_read_returns_unsupported_error() {
    let stage = BlobStage::disabled();
    let err = stage.read(&digest_a()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn disabled_cleanup_tmp_files_is_noop() {
    let stage = BlobStage::disabled();
    stage.cleanup_tmp_files().unwrap();
}

#[test]
fn disabled_evict_is_noop() {
    let stage = BlobStage::disabled();
    stage.evict(0, &HashMap::new()).unwrap();
}
