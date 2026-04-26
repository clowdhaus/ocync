//! Integration tests for the AIMD concurrency controller.
//!
//! Arithmetic tests that require access to the raw window float live in
//! `aimd.rs` as unit tests (they use the `#[cfg(test)]`-gated `window_value()`
//! method). This file covers the public API: registry key mapping and controller
//! behaviour observable through `window_limit()`.

use std::time::Duration;

use ocync_distribution::aimd::{
    AimdController, RegistryAction, WindowKey, window_key_for_registry,
};

// ---------------------------------------------------------------------------
// Window key registry mapping
// ---------------------------------------------------------------------------

#[test]
fn ecr_every_action_gets_distinct_key() {
    let host = "123456789012.dkr.ecr.us-east-1.amazonaws.com";
    let actions = [
        RegistryAction::ManifestHead,
        RegistryAction::ManifestRead,
        RegistryAction::ManifestWrite,
        RegistryAction::BlobHead,
        RegistryAction::BlobRead,
        RegistryAction::BlobUploadInit,
        RegistryAction::BlobUploadChunk,
        RegistryAction::BlobUploadComplete,
        RegistryAction::TagList,
    ];
    let keys: Vec<WindowKey> = actions
        .iter()
        .map(|&a| window_key_for_registry(host, a))
        .collect();
    let unique: std::collections::HashSet<_> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        9,
        "ECR should map all 9 actions to distinct window keys"
    );
}

#[test]
fn ecr_fips_every_action_gets_distinct_key() {
    let host = "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com";
    let actions = [
        RegistryAction::ManifestHead,
        RegistryAction::ManifestRead,
        RegistryAction::ManifestWrite,
        RegistryAction::BlobHead,
        RegistryAction::BlobRead,
        RegistryAction::BlobUploadInit,
        RegistryAction::BlobUploadChunk,
        RegistryAction::BlobUploadComplete,
        RegistryAction::TagList,
    ];
    let keys: Vec<WindowKey> = actions
        .iter()
        .map(|&a| window_key_for_registry(host, a))
        .collect();
    let unique: std::collections::HashSet<_> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        9,
        "ECR FIPS should map all 9 actions to distinct window keys"
    );
}

#[test]
fn docker_hub_head_operations_share_key() {
    let host = "registry-1.docker.io";
    let manifest_head = window_key_for_registry(host, RegistryAction::ManifestHead);
    let blob_head = window_key_for_registry(host, RegistryAction::BlobHead);
    assert_eq!(
        manifest_head, blob_head,
        "Docker Hub HEAD operations should share a window"
    );
}

#[test]
fn docker_hub_manifest_read_separate_from_heads() {
    let host = "registry-1.docker.io";
    let head_key = window_key_for_registry(host, RegistryAction::ManifestHead);
    let read_key = window_key_for_registry(host, RegistryAction::ManifestRead);
    assert_ne!(
        head_key, read_key,
        "Docker Hub manifest reads should have their own quota window"
    );
}

#[test]
fn docker_hub_non_head_non_read_share_key() {
    let host = "registry-1.docker.io";
    let init = window_key_for_registry(host, RegistryAction::BlobUploadInit);
    let chunk = window_key_for_registry(host, RegistryAction::BlobUploadChunk);
    let complete = window_key_for_registry(host, RegistryAction::BlobUploadComplete);
    let write = window_key_for_registry(host, RegistryAction::ManifestWrite);
    assert_eq!(init, chunk);
    assert_eq!(chunk, complete);
    assert_eq!(complete, write);
}

#[test]
fn gar_all_actions_share_single_key() {
    let host = "us-central1-docker.pkg.dev";
    let actions = [
        RegistryAction::ManifestHead,
        RegistryAction::ManifestRead,
        RegistryAction::ManifestWrite,
        RegistryAction::BlobHead,
        RegistryAction::BlobRead,
        RegistryAction::BlobUploadInit,
        RegistryAction::BlobUploadChunk,
        RegistryAction::BlobUploadComplete,
        RegistryAction::TagList,
    ];
    let first_key = window_key_for_registry(host, actions[0]);
    for &a in &actions[1..] {
        assert_eq!(
            window_key_for_registry(host, a),
            first_key,
            "GAR should map all actions to the same window key"
        );
    }
}

#[test]
fn unknown_registry_coarse_grouping() {
    // Use a host with no provider-specific routing so the `_` arm
    // (coarse 5-group grouping) is exercised. ghcr.io routes to a
    // single GhcrShared window; cgr.dev and ecr/gar/azurecr also have
    // dedicated arms. quay.io currently has no detect_provider_kind
    // mapping and falls through to the coarse default.
    let host = "quay.io";

    let manifest_head = window_key_for_registry(host, RegistryAction::ManifestHead);
    let blob_head = window_key_for_registry(host, RegistryAction::BlobHead);
    assert_eq!(
        manifest_head, blob_head,
        "HEADs should share a window for unknown registry"
    );

    let manifest_read = window_key_for_registry(host, RegistryAction::ManifestRead);
    let blob_read = window_key_for_registry(host, RegistryAction::BlobRead);
    assert_eq!(
        manifest_read, blob_read,
        "reads should share a window for unknown registry"
    );

    let upload_init = window_key_for_registry(host, RegistryAction::BlobUploadInit);
    let upload_chunk = window_key_for_registry(host, RegistryAction::BlobUploadChunk);
    let upload_complete = window_key_for_registry(host, RegistryAction::BlobUploadComplete);
    assert_eq!(upload_init, upload_chunk);
    assert_eq!(upload_chunk, upload_complete);

    let manifest_write = window_key_for_registry(host, RegistryAction::ManifestWrite);
    let tag_list = window_key_for_registry(host, RegistryAction::TagList);

    // Verify the five groups are all distinct.
    let groups = [
        manifest_head,
        manifest_read,
        upload_init,
        manifest_write,
        tag_list,
    ];
    let unique: std::collections::HashSet<_> = groups.iter().collect();
    assert_eq!(
        unique.len(),
        5,
        "unknown registry should have 5 distinct window groups"
    );
}

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

// ---------------------------------------------------------------------------
// Concurrent permits -- verifies semaphore replacement under throttle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn controller_throttle_with_outstanding_permits() {
    let ctrl = AimdController::new("123456789012.dkr.ecr.us-east-1.amazonaws.com", 64);
    let op = RegistryAction::BlobUploadChunk;

    // Warm up window so there's room to shrink.
    for _ in 0..30 {
        let p = ctrl.acquire(op).await;
        p.success();
    }
    let before = ctrl.window_limit(op);
    assert!(
        before > 2,
        "window should have grown past 2 after 30 successes"
    );

    // Acquire multiple permits concurrently (simulating in-flight requests).
    let p1 = ctrl.acquire(op).await;
    let p2 = ctrl.acquire(op).await;
    let p3 = ctrl.acquire(op).await;

    // Sleep past the epoch so throttle takes effect.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Throttle one -- this replaces the semaphore. The other permits
    // hold references to the old semaphore and should complete normally.
    p1.throttled();

    let after_throttle = ctrl.window_limit(op);
    // Window should approximately halve -- verify it shrank by more than 1.
    assert!(
        after_throttle <= before.div_ceil(2),
        "throttle should approximately halve the window (before={before}, after={after_throttle})"
    );

    // Remaining permits from old semaphore complete without issue.
    p2.success();
    p3.success();

    // New acquires go through the replacement semaphore at the reduced limit.
    let p4 = ctrl.acquire(op).await;
    p4.success();
    let final_limit = ctrl.window_limit(op);
    assert!(
        final_limit <= before,
        "limit after recovery should stay at or below pre-throttle level"
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
