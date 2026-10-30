//! Verbosity-aware progress reporters for sync output.
//!
//! [`TextProgress`] writes per-image status lines to stderr and a periodic
//! liveness line to tracing while a run is in flight. There is no per-cycle
//! aggregate: the CLI driver logs a per-mapping INFO line after the engine
//! returns, which carries the source/target context an aggregate cannot.

use std::cell::RefCell;
use std::io::{self, Write};

use ocync_sync::progress::{ProgressReporter, RunProgress};
use ocync_sync::{ImageResult, ImageStatus, SyncReport};

use crate::cli::output::{format_bytes, format_duration};

/// A per-image status line, or `None` if silent at this verbosity.
///
/// Failed images always produce a line; synced and skipped only at verbosity >= 1.
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

/// Text progress reporter: per-image status lines go to stderr.
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
    fn image_started(&self, _source: &str, _target: &str) {}

    fn image_completed(&self, result: &ImageResult) {
        if let Some(line) = format_image_line(result, self.verbosity) {
            if let Err(e) = writeln!(self.stderr.borrow_mut(), "{line}") {
                tracing::warn!(error = %e, "failed to write progress to stderr");
            }
        }
    }

    fn run_heartbeat(&self, snapshot: RunProgress) {
        // Tracing, not the stderr writer: a periodic status line belongs in the
        // log stream where `--log-format json` and the level filter apply. At
        // verbosity 0 it is the only sign of a long run versus a hung one.
        tracing::info!(
            in_discovery = snapshot.in_discovery,
            pending = snapshot.pending,
            in_flight = snapshot.in_flight,
            completed = snapshot.completed,
            elapsed_secs = snapshot.elapsed.as_secs(),
            "sync in progress"
        );
    }

    fn run_completed(&self, _report: &SyncReport) {
        // The CLI driver logs per-mapping lines and `--json` writes the
        // structured report separately.
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

    fn failed(kind: ErrorKind, error: &str) -> ImageStatus {
        ImageStatus::Failed {
            kind,
            error: error.into(),
            retries: 0,
            status_code: None,
        }
    }

    #[test]
    fn failed_image_lines_appear_at_every_verbosity() {
        let cases = [
            (
                "with error",
                failed(ErrorKind::ManifestPush, "connection refused"),
                "FAILED  source/repo:v1 -> target/repo:v1  (manifest push: connection refused)",
            ),
            (
                "timeout",
                failed(ErrorKind::BlobTransfer, "timeout"),
                "FAILED  source/repo:v1 -> target/repo:v1  (blob transfer: timeout)",
            ),
            (
                "empty error omits the colon",
                failed(ErrorKind::ManifestPull, ""),
                "FAILED  source/repo:v1 -> target/repo:v1  (manifest pull)",
            ),
            (
                "network error",
                failed(ErrorKind::BlobTransfer, "network error"),
                "FAILED  source/repo:v1 -> target/repo:v1  (blob transfer: network error)",
            ),
        ];
        for (name, status, want) in cases {
            let result = make_result(status, 0);
            for v in [0, 1, 2, 255] {
                assert_eq!(
                    format_image_line(&result, v).as_deref(),
                    Some(want),
                    "{name} at verbosity {v}"
                );
            }
        }
    }

    #[test]
    fn synced_and_skipped_image_lines_need_verbosity() {
        let skipped = |reason| ImageStatus::Skipped { reason };
        let cases = [
            (
                "synced v0 is silent",
                ImageStatus::Synced,
                187_000_000,
                false,
                0,
                None,
            ),
            (
                "synced v1 shows bytes and duration",
                ImageStatus::Synced,
                187_000_000,
                false,
                1,
                Some("synced  source/repo:v1 -> target/repo:v1  (187.0 MB, 14s)"),
            ),
            (
                "synced zero bytes",
                ImageStatus::Synced,
                0,
                false,
                1,
                Some("synced  source/repo:v1 -> target/repo:v1  (0 B, 14s)"),
            ),
            (
                "synced exact format",
                ImageStatus::Synced,
                432_000_000,
                false,
                1,
                Some("synced  source/repo:v1 -> target/repo:v1  (432.0 MB, 14s)"),
            ),
            (
                "synced with artifacts skipped",
                ImageStatus::Synced,
                432_000_000,
                true,
                1,
                Some(
                    "synced  source/repo:v1 -> target/repo:v1  (432.0 MB, 14s, artifacts skipped)",
                ),
            ),
            (
                "skipped v0 is silent",
                skipped(SkipReason::DigestMatch),
                0,
                false,
                0,
                None,
            ),
            (
                "skipped v1 shows the reason",
                skipped(SkipReason::DigestMatch),
                0,
                false,
                1,
                Some("skipped source/repo:v1 -> target/repo:v1  (digest match)"),
            ),
            (
                "skipped immutable tag",
                skipped(SkipReason::ImmutableTag),
                0,
                false,
                1,
                Some("skipped source/repo:v1 -> target/repo:v1  (immutable tag)"),
            ),
        ];
        for (name, status, bytes, artifacts_skipped, verbosity, want) in cases {
            let mut result = make_result(status, bytes);
            result.artifacts_skipped = artifacts_skipped;
            assert_eq!(
                format_image_line(&result, verbosity).as_deref(),
                want,
                "{name}"
            );
        }
    }

    fn text_progress(verbosity: u8) -> (TextProgress, Buf) {
        let stderr_buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let progress =
            TextProgress::with_writer(verbosity, Box::new(RcWriter(Rc::clone(&stderr_buf))));
        (progress, stderr_buf)
    }

    #[test]
    fn text_image_completed_writes_to_stderr_by_verbosity() {
        let cases = [
            ("synced at v1", 1, ImageStatus::Synced, Some("synced  ")),
            ("synced at v0", 0, ImageStatus::Synced, None),
            (
                "failed at v0",
                0,
                failed(ErrorKind::BlobTransfer, "timeout"),
                Some("FAILED  "),
            ),
        ];
        for (name, verbosity, status, want_prefix) in cases {
            let (progress, stderr) = text_progress(verbosity);
            progress.image_completed(&make_result(status, 1024));
            let out = String::from_utf8(stderr.borrow().clone()).unwrap();
            match want_prefix {
                Some(prefix) => assert!(out.starts_with(prefix), "{name}: got: {out}"),
                None => assert!(out.is_empty(), "{name}: got: {out}"),
            }
        }
    }

    /// `run_completed` is a no-op: even with a non-empty report nothing reaches stderr.
    #[test]
    fn text_run_completed_emits_nothing() {
        let (progress, stderr) = text_progress(2);
        progress.run_completed(&make_report(vec![make_result(ImageStatus::Synced, 1)]));
        assert!(stderr.borrow().is_empty());
    }
}
