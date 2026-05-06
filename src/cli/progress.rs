//! Verbosity-aware progress reporters for sync output.
//!
//! [`TextProgress`] writes per-image status lines to stderr. There is no
//! per-cycle aggregate stdout line: the CLI driver emits a per-mapping
//! INFO line via tracing after the engine returns, which carries the
//! source/target context an aggregate cannot.

use std::cell::RefCell;
use std::io::{self, Write};

use ocync_sync::progress::ProgressReporter;
use ocync_sync::{ImageResult, ImageStatus, SyncReport};

use crate::cli::output::{format_bytes, format_duration};

/// Format a per-image status line, or `None` if the status should be silent
/// at the given verbosity level.
///
/// Failed images always produce a line. Synced/skipped images produce a
/// line only at verbosity >= 1.
fn format_image_line(result: &ImageResult, verbosity: u8) -> Option<String> {
    match &result.status {
        ImageStatus::Failed { kind, error, .. } if error.is_empty() => Some(format!(
            "FAILED  {} -> {}  ({kind})",
            result.source, result.target,
        )),
        ImageStatus::Failed { kind, error, .. } => Some(format!(
            "FAILED  {} -> {}  ({kind}: {error})",
            result.source, result.target,
        )),
        ImageStatus::Synced if verbosity >= 1 => {
            let suffix = if result.artifacts_skipped {
                ", artifacts skipped"
            } else {
                ""
            };
            Some(format!(
                "synced  {} -> {}  ({}, {}{})",
                result.source,
                result.target,
                format_bytes(result.bytes_transferred),
                format_duration(result.duration),
                suffix,
            ))
        }
        ImageStatus::Skipped { reason } if verbosity >= 1 => Some(format!(
            "skipped {} -> {}  ({reason})",
            result.source, result.target,
        )),
        _ => None,
    }
}

/// Text progress reporter with configurable verbosity.
///
/// Per-image status lines (`synced` / `FAILED`) go to stderr. There is no
/// stdout per-cycle aggregate; the CLI emits per-mapping INFO lines from
/// the sync driver instead.
pub(crate) struct TextProgress {
    verbosity: u8,
    stderr: RefCell<Box<dyn Write>>,
}

impl TextProgress {
    pub(crate) fn new(verbosity: u8) -> Self {
        Self {
            verbosity,
            stderr: RefCell::new(Box::new(io::stderr())),
        }
    }

    #[cfg(test)]
    fn with_writer(verbosity: u8, stderr: Box<dyn Write>) -> Self {
        Self {
            verbosity,
            stderr: RefCell::new(stderr),
        }
    }
}

impl ProgressReporter for TextProgress {
    fn image_started(&self, _source: &str, _target: &str) {
        // No-op for text output.
    }

    fn image_completed(&self, result: &ImageResult) {
        if let Some(line) = format_image_line(result, self.verbosity) {
            if let Err(e) = writeln!(self.stderr.borrow_mut(), "{line}") {
                tracing::warn!(error = %e, "failed to write progress to stderr");
            }
        }
    }

    fn run_completed(&self, _report: &SyncReport) {
        // Per-cycle aggregate is no longer emitted here; the CLI driver
        // emits a per-mapping line via tracing INFO after the engine
        // returns, which carries the source/target context the aggregate
        // lacked. The `--json` path writes the structured report to stdout
        // separately in `write_output`.
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use ocync_sync::{BlobTransferStats, ErrorKind, SkipReason, SyncStats};
    use uuid::Uuid;

    use super::*;

    /// Adapter that writes into a shared `Rc<RefCell<Vec<u8>>>`.
    struct RcWriter(Rc<RefCell<Vec<u8>>>);

    impl Write for RcWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.borrow_mut().flush()
        }
    }

    type Buf = Rc<RefCell<Vec<u8>>>;

    fn make_result(status: ImageStatus, bytes: u64) -> ImageResult {
        ImageResult {
            image_id: Uuid::now_v7(),
            source: "source/repo:v1".into(),
            target: "target/repo:v1".into(),
            status,
            bytes_transferred: bytes,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::from_secs(14),
            artifacts_skipped: false,
        }
    }

    fn make_report(images: Vec<ImageResult>) -> SyncReport {
        SyncReport {
            run_id: Uuid::now_v7(),
            images,
            stats: SyncStats {
                images_synced: 3,
                images_skipped: 47,
                images_failed: 1,
                blobs_transferred: 12,
                blobs_skipped: 0,
                blobs_mounted: 34,
                bytes_transferred: 432_000_000,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(47),
        }
    }

    // - format_image_line tests (shared formatting logic) --

    #[test]
    fn image_line_failed_always_returns_some() {
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                error: "connection refused".into(),
                retries: 3,
                status_code: None,
            },
            0,
        );
        let line = format_image_line(&result, 0).expect("failed should always produce a line");
        assert!(line.starts_with("FAILED  "), "got: {line}");
        assert!(line.contains("manifest push"), "got: {line}");
        assert!(line.contains("connection refused"), "got: {line}");
    }

    #[test]
    fn image_line_failed_at_any_verbosity() {
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::BlobTransfer,
                error: "timeout".into(),
                retries: 1,
                status_code: None,
            },
            0,
        );
        // Failed lines appear at every verbosity level.
        for v in [0, 1, 2, 255] {
            assert!(
                format_image_line(&result, v).is_some(),
                "failed should produce a line at verbosity {v}"
            );
        }
    }

    #[test]
    fn image_line_failed_with_empty_error_omits_colon() {
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPull,
                error: String::new(),
                retries: 0,
                status_code: None,
            },
            0,
        );
        let line = format_image_line(&result, 0).unwrap();
        assert_eq!(
            line,
            "FAILED  source/repo:v1 -> target/repo:v1  (manifest pull)"
        );
    }

    #[test]
    fn image_line_synced_v0_returns_none() {
        let result = make_result(ImageStatus::Synced, 187_000_000);
        assert!(format_image_line(&result, 0).is_none());
    }

    #[test]
    fn image_line_synced_v1_includes_bytes_and_duration() {
        let result = make_result(ImageStatus::Synced, 187_000_000);
        let line = format_image_line(&result, 1).expect("synced at v1 should produce a line");
        assert!(line.starts_with("synced  "), "got: {line}");
        assert!(line.contains("187.0 MB"), "got: {line}");
        assert!(line.contains("14s"), "got: {line}");
    }

    #[test]
    fn image_line_synced_zero_bytes() {
        let result = make_result(ImageStatus::Synced, 0);
        let line = format_image_line(&result, 1).unwrap();
        assert!(line.contains("0 B"), "got: {line}");
    }

    #[test]
    fn image_line_skipped_v0_returns_none() {
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            },
            0,
        );
        assert!(format_image_line(&result, 0).is_none());
    }

    #[test]
    fn image_line_skipped_v1_includes_reason() {
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            },
            0,
        );
        let line = format_image_line(&result, 1).expect("skipped at v1 should produce a line");
        assert!(line.starts_with("skipped "), "got: {line}");
        assert!(line.contains("digest match"), "got: {line}");
    }

    #[test]
    fn image_line_immutable_tag_reason() {
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::ImmutableTag,
            },
            0,
        );
        let line = format_image_line(&result, 1).unwrap();
        assert!(line.starts_with("skipped "), "got: {line}");
        assert!(line.contains("immutable tag"), "got: {line}");
    }

    #[test]
    fn image_line_exact_format_synced() {
        let result = make_result(ImageStatus::Synced, 432_000_000);
        let line = format_image_line(&result, 1).unwrap();
        assert_eq!(
            line,
            "synced  source/repo:v1 -> target/repo:v1  (432.0 MB, 14s)"
        );
    }

    #[test]
    fn image_line_synced_with_artifacts_skipped() {
        let mut result = make_result(ImageStatus::Synced, 432_000_000);
        result.artifacts_skipped = true;
        let line = format_image_line(&result, 1).unwrap();
        assert_eq!(
            line,
            "synced  source/repo:v1 -> target/repo:v1  (432.0 MB, 14s, artifacts skipped)"
        );
    }

    #[test]
    fn image_line_exact_format_failed() {
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::BlobTransfer,
                error: "network error".into(),
                retries: 2,
                status_code: None,
            },
            0,
        );
        let line = format_image_line(&result, 0).unwrap();
        assert_eq!(
            line,
            "FAILED  source/repo:v1 -> target/repo:v1  (blob transfer: network error)"
        );
    }

    #[test]
    fn image_line_exact_format_skipped() {
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            },
            0,
        );
        let line = format_image_line(&result, 1).unwrap();
        assert_eq!(
            line,
            "skipped source/repo:v1 -> target/repo:v1  (digest match)"
        );
    }
    // - TextProgress IO tests (stderr only) --

    fn text_progress(verbosity: u8) -> (TextProgress, Buf) {
        let stderr_buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let progress =
            TextProgress::with_writer(verbosity, Box::new(RcWriter(Rc::clone(&stderr_buf))));
        (progress, stderr_buf)
    }

    #[test]
    fn text_image_completed_writes_to_stderr_at_v1() {
        let (progress, stderr) = text_progress(1);
        progress.image_completed(&make_result(ImageStatus::Synced, 1024));
        let out = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(out.starts_with("synced  "), "got: {out}");
    }

    #[test]
    fn text_image_completed_silent_at_v0() {
        let (progress, stderr) = text_progress(0);
        progress.image_completed(&make_result(ImageStatus::Synced, 1024));
        assert!(stderr.borrow().is_empty());
    }

    #[test]
    fn text_failed_always_writes_to_stderr() {
        let (progress, stderr) = text_progress(0);
        progress.image_completed(&make_result(
            ImageStatus::Failed {
                kind: ErrorKind::BlobTransfer,
                error: "timeout".into(),
                retries: 1,
                status_code: None,
            },
            0,
        ));
        let out = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(out.starts_with("FAILED  "), "got: {out}");
    }

    /// `run_completed` is intentionally a no-op now -- the per-cycle
    /// aggregate moved to per-mapping INFO lines emitted by the CLI driver.
    /// Lock that contract: even with a non-empty report, nothing reaches
    /// stderr from this method.
    #[test]
    fn text_run_completed_emits_nothing() {
        let (progress, stderr) = text_progress(2);
        progress.run_completed(&make_report(vec![make_result(ImageStatus::Synced, 1)]));
        assert!(stderr.borrow().is_empty());
    }
}
