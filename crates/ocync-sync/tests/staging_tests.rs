//! Integration tests for [`BlobStage`].
//!
//! Covers the round-trip, crash recovery, eviction, streaming write/read, and
//! disabled-mode paths.

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
// Streaming write via StagedWriter
// ---------------------------------------------------------------------------

#[test]
fn begin_write_and_finish_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    let mut writer = stage.begin_write(&digest_a()).unwrap();
    writer.write_chunk(b"hello ").unwrap();
    writer.write_chunk(b"world").unwrap();
    writer.finish().unwrap();

    assert!(stage.exists(&digest_a()));
    let data = stage.read(&digest_a()).unwrap();
    assert_eq!(data, b"hello world");
}

#[test]
fn open_read_returns_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    stage.write(&digest_a(), b"read me").unwrap();

    let mut file = stage.open_read(&digest_a()).unwrap();
    let mut buf = Vec::new();
    io::Read::read_to_end(&mut file, &mut buf).unwrap();
    assert_eq!(buf, b"read me");
}

#[test]
fn begin_write_on_disabled_returns_unsupported() {
    let stage = BlobStage::disabled();
    let err = stage.begin_write(&digest_a()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn open_read_on_disabled_returns_unsupported() {
    let stage = BlobStage::disabled();
    let err = stage.open_read(&digest_a()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
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
fn evict_removes_smallest_files_first() {
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
    // Sorted by ascending size: A(10), B(20), C(30).
    stage.evict(40).unwrap();

    // A (10 bytes) evicted first. Total = 50 -- still over 40.
    assert!(!stage.exists(&digest_a()), "smallest blob must be evicted");
    // B (20 bytes) evicted next. Total = 30 -- now under 40. C is kept.
    assert!(
        !stage.exists(&digest_b()),
        "second-smallest blob must be evicted"
    );
    assert!(stage.exists(&digest_c()), "largest blob must be kept");
}

#[test]
fn evict_noop_when_under_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    stage.write(&digest_a(), b"small").unwrap();

    stage.evict(1_000_000).unwrap();

    // Nothing should be evicted.
    assert!(stage.exists(&digest_a()));
}

#[test]
fn evict_noop_when_blobs_dir_missing() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    // No writes; blobs/ dir doesn't exist.
    stage.evict(0).unwrap();
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
    stage.evict(0).unwrap();
}

// ---------------------------------------------------------------------------
// Source-pull dedup: claim_or_check / notify_staged / notify_failed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_or_check_first_caller_returns_pull() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    let action = stage.claim_or_check(&digest_a());
    assert!(
        matches!(action, ocync_sync::staging::StagePullAction::Pull),
        "first caller must get Pull, got: {action:?}"
    );
}

#[tokio::test]
async fn claim_or_check_second_caller_returns_wait() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    // First caller claims the pull.
    let first = stage.claim_or_check(&digest_a());
    assert!(matches!(first, ocync_sync::staging::StagePullAction::Pull));

    // Second caller for the same digest gets Wait with a Notify.
    let second = stage.claim_or_check(&digest_a());
    assert!(
        matches!(second, ocync_sync::staging::StagePullAction::Wait(_)),
        "second caller must get Wait, got: {second:?}"
    );
}

#[tokio::test]
async fn notify_staged_makes_subsequent_claim_return_exists() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    // Claim the pull.
    let action = stage.claim_or_check(&digest_a());
    assert!(matches!(action, ocync_sync::staging::StagePullAction::Pull));

    // Write the blob to disk and signal completion.
    stage.write(&digest_a(), b"staged data").unwrap();
    stage.notify_staged(&digest_a());

    // A new caller should see the blob on disk.
    let action = stage.claim_or_check(&digest_a());
    assert!(
        matches!(action, ocync_sync::staging::StagePullAction::Exists),
        "after notify_staged + disk write, claim_or_check must return Exists, got: {action:?}"
    );
}

#[tokio::test]
async fn notify_failed_makes_subsequent_claim_return_pull() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());

    // First caller claims the pull.
    let action = stage.claim_or_check(&digest_a());
    assert!(matches!(action, ocync_sync::staging::StagePullAction::Pull));

    // Simulate a failure (no data written to disk).
    stage.notify_failed(&digest_a());

    // Next caller should be able to retry (get Pull, not Wait).
    let action = stage.claim_or_check(&digest_a());
    assert!(
        matches!(action, ocync_sync::staging::StagePullAction::Pull),
        "after notify_failed, claim_or_check must return Pull for retry, got: {action:?}"
    );
}

#[tokio::test]
async fn claim_or_check_disabled_always_returns_pull() {
    let stage = BlobStage::disabled();

    // Every call on a disabled stage returns Pull (no dedup possible).
    let first = stage.claim_or_check(&digest_a());
    assert!(
        matches!(first, ocync_sync::staging::StagePullAction::Pull),
        "disabled stage must always return Pull, got: {first:?}"
    );

    let second = stage.claim_or_check(&digest_a());
    assert!(
        matches!(second, ocync_sync::staging::StagePullAction::Pull),
        "disabled stage must always return Pull on repeated calls, got: {second:?}"
    );
}

// ---------------------------------------------------------------------------
// Async wakeup: claim_or_check -> Wait -> notified().await -> Exists
// ---------------------------------------------------------------------------

/// Verify the full async wakeup flow: a waiter spawned via `spawn_local`
/// blocks on `notified().await`, is woken by `notify_staged`, and a
/// subsequent `claim_or_check` returns `Exists`.
#[tokio::test(flavor = "current_thread")]
async fn blob_notify_async_wakeup_flow() {
    use std::cell::Cell;
    use std::rc::Rc;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let stage = Rc::new(BlobStage::new(dir.path().to_path_buf()));
            let digest = digest_a();

            // Task 1 claims the pull.
            let action = stage.claim_or_check(&digest);
            assert!(
                matches!(action, ocync_sync::staging::StagePullAction::Pull),
                "first caller must get Pull"
            );

            // Task 2: call claim_or_check -> Wait, then await the notify.
            let woke_up = Rc::new(Cell::new(false));
            let woke_clone = Rc::clone(&woke_up);
            let stage_clone = Rc::clone(&stage);
            let digest_clone = digest.clone();

            let handle = tokio::task::spawn_local(async move {
                let action = stage_clone.claim_or_check(&digest_clone);
                let notify = match action {
                    ocync_sync::staging::StagePullAction::Wait(n) => n,
                    other => panic!("expected Wait, got: {other:?}"),
                };
                notify.notified().await;
                woke_clone.set(true);

                // After waking, claim_or_check should return Exists.
                let final_action = stage_clone.claim_or_check(&digest_clone);
                assert!(
                    matches!(final_action, ocync_sync::staging::StagePullAction::Exists),
                    "after notify_staged, claim_or_check must return Exists, got: {final_action:?}"
                );
            });

            // Yield to let the spawned task reach the await point.
            tokio::task::yield_now().await;
            assert!(!woke_up.get(), "waiter must not wake before notify_staged");

            // Write the blob and signal completion.
            stage.write(&digest, b"staged blob data").unwrap();
            stage.notify_staged(&digest);

            // Let the spawned task finish.
            handle.await.unwrap();
            assert!(woke_up.get(), "waiter must have woken after notify_staged");
        })
        .await;
}

// ---------------------------------------------------------------------------
// StagedWriter drop cleanup
// ---------------------------------------------------------------------------

/// Verify that dropping a `StagedWriter` without calling `finish()` removes
/// the temporary file and does NOT stage the blob.
#[test]
fn staged_writer_drop_removes_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let stage = BlobStage::new(dir.path().to_path_buf());
    let digest = digest_a();

    // Collect the temp file path before dropping.
    let tmp_path = {
        let mut writer = stage.begin_write(&digest).unwrap();
        writer
            .write_chunk(b"partial data that should be cleaned up")
            .unwrap();
        // Extract the tmp_path from the debug repr for verification.
        // We know the tmp file exists because begin_write creates it.
        let blobs_dir = dir.path().join("blobs").join("sha256");
        let entries: Vec<_> = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tmp."))
            })
            .collect();
        assert_eq!(entries.len(), 1, "temp file must exist before drop");
        let tmp = entries[0].clone();
        // Drop the writer WITHOUT calling finish().
        drop(writer);
        tmp
    };

    // Temp file must be removed by Drop impl.
    assert!(
        !tmp_path.exists(),
        "temp file must be removed after drop without finish()"
    );

    // The blob must NOT be staged.
    assert!(
        !stage.exists(&digest),
        "blob must not be staged when writer is dropped without finish()"
    );
}
