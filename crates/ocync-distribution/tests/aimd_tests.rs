//! Integration tests for the AIMD concurrency controller.
//!
//! Arithmetic tests that require access to the raw window float live in
//! `aimd.rs` as unit tests (they use the `#[cfg(test)]`-gated `window_value()`
//! method). This file covers the public API: registry key mapping and controller
//! behaviour observable through `window_limit()`.

use std::time::Duration;

use ocync_distribution::aimd::{
    AimdController, DockerHubGroup, RegistryAction, WindowKey, window_key_for_registry,
};

// ---------------------------------------------------------------------------
// Window key registry mapping
// ---------------------------------------------------------------------------

const ALL_ACTIONS: [RegistryAction; 9] = [
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

/// ECR, FIPS endpoint included, gives each of its 9 actions its own
/// window. Breaks if two actions share one, so one's throttling slows the
/// other.
#[test]
fn ecr_every_action_gets_distinct_key() {
    for host in [
        "123456789012.dkr.ecr.us-east-1.amazonaws.com",
        "123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com",
    ] {
        let keys: std::collections::HashSet<WindowKey> = ALL_ACTIONS
            .iter()
            .map(|&a| window_key_for_registry(host, a))
            .collect();
        assert_eq!(keys.len(), 9, "{host}");
    }
}

/// Docker Hub meters HEADs, manifest reads and everything else apart.
/// Breaks if HEADs or manifest reads fall into the shared window, where a
/// throttled upload slows them.
#[test]
fn docker_hub_groups_actions_by_quota() {
    use RegistryAction::*;
    let cases = [
        (ManifestHead, DockerHubGroup::Heads),
        (BlobHead, DockerHubGroup::Heads),
        (ManifestRead, DockerHubGroup::ManifestRead),
        (BlobRead, DockerHubGroup::Other),
        (ManifestWrite, DockerHubGroup::Other),
        (BlobUploadInit, DockerHubGroup::Other),
        (BlobUploadChunk, DockerHubGroup::Other),
        (BlobUploadComplete, DockerHubGroup::Other),
        (TagList, DockerHubGroup::Other),
    ];
    for (action, group) in cases {
        assert_eq!(
            window_key_for_registry("registry-1.docker.io", action),
            WindowKey::DockerHub(group),
            "{action:?}"
        );
    }
}

/// GAR meters every action against one project quota. Breaks if any
/// action gets its own window.
#[test]
fn gar_all_actions_share_single_key() {
    for action in ALL_ACTIONS {
        assert_eq!(
            window_key_for_registry("us-central1-docker.pkg.dev", action),
            WindowKey::GarShared,
            "{action:?}"
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
