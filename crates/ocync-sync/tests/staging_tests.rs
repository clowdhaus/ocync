//! Integration tests for [`BlobStage`]: round-trip, crash recovery, eviction,
//! streaming writes and disabled mode.

mod helpers;

use std::io;

use ocync_distribution::Digest;
use ocync_sync::staging::BlobStage;

use helpers::{digest_a, digest_b};

const DIGEST_C: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn digest_c() -> Digest {
    DIGEST_C.parse().unwrap()
}

fn stage_blob(stage: &BlobStage, digest: &Digest, data: &[u8]) {
    let mut writer = stage.begin_write(digest).unwrap();
    writer.write_chunk(data).unwrap();
    writer.finish().unwrap();
}

fn read_staged(stage: &BlobStage, digest: &Digest) -> Vec<u8> {
    let mut file = stage.open_read(digest).unwrap();
    let mut buf = Vec::new();
    io::Read::read_to_end(&mut file, &mut buf).unwrap();
    buf
}

/// An enabled stage over a fresh temp directory (kept alive by the guard).
fn enabled_stage() -> (tempfile::TempDir, BlobStage) {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    (dir, stage)
}

/// Chunks written through `begin_write` read back as their concatenation.
#[test]
fn stage_and_read_roundtrip() {
    let cases: [(&str, &[&[u8]]); 2] = [
        ("one chunk", &[b"hello staging world"]),
        ("two chunks", &[b"hello ", b"world"]),
    ];
    for (name, chunks) in cases {
        let (_dir, stage) = enabled_stage();

        let mut writer = stage.begin_write(&digest_a()).unwrap();
        for chunk in chunks {
            writer.write_chunk(chunk).unwrap();
        }
        writer.finish().unwrap();

        assert!(stage.exists(&digest_a()), "{name}");
        assert_eq!(read_staged(&stage, &digest_a()), chunks.concat(), "{name}");
    }
}

#[test]
fn write_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().join("a").join("b").join("c"));

    stage_blob(&stage, &digest_a(), b"data");
    assert!(stage.exists(&digest_a()));
}

/// A blob that was never staged does not exist, in either mode.
#[test]
fn exists_is_false_for_an_unstaged_blob() {
    let (_dir, enabled) = enabled_stage();
    for (name, stage) in [("enabled", enabled), ("disabled", BlobStage::disabled())] {
        assert!(!stage.exists(&digest_a()), "{name}");
    }
}

#[test]
fn is_enabled_reflects_mode() {
    let (_dir, enabled) = enabled_stage();
    assert!(enabled.is_enabled());
    assert!(!BlobStage::disabled().is_enabled());
}

/// A disabled stage refuses reads and writes with `Unsupported`.
#[test]
fn disabled_stage_rejects_io() {
    let stage = BlobStage::disabled();
    let cases: [(&str, io::Result<()>); 2] = [
        ("begin_write", stage.begin_write(&digest_a()).map(drop)),
        ("open_read", stage.open_read(&digest_a()).map(drop)),
    ];
    for (name, result) in cases {
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::Unsupported,
            "{name}"
        );
    }
}

/// Cleanup and eviction succeed and do nothing when there is nothing to act
/// on: a disabled stage, or an enabled one whose `blobs/` directory does not
/// exist yet.
#[test]
fn cleanup_and_evict_are_noops_without_blobs() {
    let (_dir, empty) = enabled_stage();
    for (name, stage) in [("disabled", BlobStage::disabled()), ("empty dir", empty)] {
        stage
            .cleanup_tmp_files()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        stage.evict(0).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// Cleanup removes an orphaned `.tmp.` file and keeps the real blob.
#[test]
fn cleanup_tmp_files_removes_orphaned_files() {
    let (dir, stage) = enabled_stage();
    stage_blob(&stage, &digest_a(), b"real data");

    let orphan = dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(format!("{}.tmp.99999", "a".repeat(64)));
    std::fs::write(&orphan, b"incomplete").unwrap();
    assert!(orphan.exists(), "orphan must exist before cleanup");

    stage.cleanup_tmp_files().unwrap();

    assert!(!orphan.exists(), "orphan must be removed after cleanup");
    assert!(stage.exists(&digest_a()), "the real blob must survive");
}

/// Eviction removes the smallest files first until the total fits: 10, 20 and
/// 30 byte blobs under a 40 byte limit lose the first two (60, then 50, then 30).
#[test]
fn evict_removes_smallest_files_first() {
    let (_dir, stage) = enabled_stage();
    stage_blob(&stage, &digest_a(), &[0u8; 10]);
    stage_blob(&stage, &digest_b(), &[1u8; 20]);
    stage_blob(&stage, &digest_c(), &[2u8; 30]);

    stage.evict(40).unwrap();

    assert!(!stage.exists(&digest_a()), "smallest blob must be evicted");
    assert!(
        !stage.exists(&digest_b()),
        "second-smallest blob must be evicted"
    );
    assert!(stage.exists(&digest_c()), "largest blob must be kept");
}

#[test]
fn evict_noop_when_under_threshold() {
    let (_dir, stage) = enabled_stage();
    stage_blob(&stage, &digest_a(), b"small");

    stage.evict(1_000_000).unwrap();

    assert!(stage.exists(&digest_a()));
}

/// Dropping a `StagedWriter` without `finish()` removes the temp file and does
/// not stage the blob.
#[test]
fn staged_writer_drop_removes_temp_file() {
    let (dir, stage) = enabled_stage();
    let digest = digest_a();

    let mut writer = stage.begin_write(&digest).unwrap();
    writer
        .write_chunk(b"partial data that should be cleaned up")
        .unwrap();

    let blobs_dir = dir.path().join("blobs").join("sha256");
    let tmp_files = || -> Vec<_> {
        std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tmp."))
            })
            .collect()
    };
    assert_eq!(tmp_files().len(), 1, "temp file must exist before drop");

    drop(writer);

    assert!(
        tmp_files().is_empty(),
        "temp file must be removed after drop without finish()"
    );
    assert!(
        !stage.exists(&digest),
        "blob must not be staged when writer is dropped without finish()"
    );
}
