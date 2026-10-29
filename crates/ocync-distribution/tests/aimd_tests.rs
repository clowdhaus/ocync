//! Integration tests for the AIMD concurrency controller.
//!
//! Arithmetic tests that require access to the raw window float live in
//! `aimd.rs` as unit tests (they use the `#[cfg(test)]`-gated `window_value()`
//! method), as does the registry key mapping. This file covers controller
//! behaviour observable through `window_limit()`.

use std::time::Duration;

use ocync_distribution::aimd::{AimdController, RegistryAction};

// ---------------------------------------------------------------------------
// AimdController -- observable through window_limit()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn controller_window_limit_before_any_acquire() {
    let ctrl = AimdController::new("ghcr.io", 16);
    let limit = ctrl.window_limit(RegistryAction::ManifestRead);
    assert!(limit > 0);
    assert!(limit <= 16);
}

#[tokio::test]
async fn controller_acquire_success_increases_limit() {
    let ctrl = AimdController::new("ghcr.io", 64);
    let before = ctrl.window_limit(RegistryAction::BlobUploadChunk);

    let permit = ctrl.acquire(RegistryAction::BlobUploadChunk).await;
    permit.success();

    let after = ctrl.window_limit(RegistryAction::BlobUploadChunk);
    // After a success the window float has grown, so limit must be >= before.
    assert!(after >= before);
}

#[tokio::test]
async fn controller_acquire_throttle_decreases_limit() {
    let ctrl = AimdController::new("123456789012.dkr.ecr.us-east-1.amazonaws.com", 64);

    // Warm up the window so it has room to decrease.
    for _ in 0..20 {
        let p = ctrl.acquire(RegistryAction::BlobUploadChunk).await;
        p.success();
    }
    let before = ctrl.window_limit(RegistryAction::BlobUploadChunk);
    assert!(
        before > 1,
        "window should have grown past 1 after 20 successes"
    );

    // Sleep past the default epoch (100 ms) so the decrease is not suppressed.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let permit = ctrl.acquire(RegistryAction::BlobUploadChunk).await;
    permit.throttled();

    let after = ctrl.window_limit(RegistryAction::BlobUploadChunk);
    assert!(
        after < before,
        "throttle should shrink the window (before={before}, after={after})"
    );
}

#[tokio::test]
async fn controller_drop_without_report_treated_as_success() {
    let ctrl = AimdController::new("ghcr.io", 64);

    for _ in 0..10 {
        let p = ctrl.acquire(RegistryAction::ManifestRead).await;
        p.success();
    }
    let before = ctrl.window_limit(RegistryAction::ManifestRead);

    // Drop without reporting -- should not decrease the window.
    {
        let _permit = ctrl.acquire(RegistryAction::ManifestRead).await;
    }

    let after = ctrl.window_limit(RegistryAction::ManifestRead);
    assert!(
        after >= before,
        "unreported drop should not decrease window"
    );
}

#[tokio::test]
async fn controller_multiple_throttles_across_epochs() {
    let ctrl = AimdController::new("123456789012.dkr.ecr.us-east-1.amazonaws.com", 64);
    let op = RegistryAction::BlobUploadChunk;

    // Warm up to grow the window.
    for _ in 0..50 {
        let p = ctrl.acquire(op).await;
        p.success();
    }
    let initial = ctrl.window_limit(op);

    // Two throttles in separate epochs should each halve the window.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let p1 = ctrl.acquire(op).await;
    p1.throttled();
    let after_first = ctrl.window_limit(op);
    assert!(after_first < initial, "first throttle should shrink window");

    tokio::time::sleep(Duration::from_millis(150)).await;
    let p2 = ctrl.acquire(op).await;
    p2.throttled();
    let after_second = ctrl.window_limit(op);
    assert!(
        after_second < after_first,
        "second throttle in new epoch should shrink further (first={after_first}, second={after_second})"
    );
}
