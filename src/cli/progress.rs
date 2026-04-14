//! Verbosity-aware text progress reporter.

use std::cell::RefCell;
use std::io::{self, Write};

use ocync_sync::progress::ProgressReporter;
use ocync_sync::{ImageResult, ImageStatus, SyncReport};

use crate::cli::output::{format_bytes, format_duration};

/// Text progress reporter with configurable verbosity.
///
/// Per-image status lines go to stderr (alongside tracing logs).
/// The run summary goes to stdout (pipeable, parseable).
///
/// Uses [`RefCell`] for interior mutability because the
/// [`ProgressReporter`] trait takes `&self` and the engine runs on a
/// single-threaded tokio runtime.
pub(crate) struct TextProgress {
    verbosity: u8,
    /// When true, suppress the stdout summary line. Used when JSON output
    /// owns stdout (`--json`) or when the summary is redundant (e.g., `copy`
    /// with a single image where the per-image line says everything).
    suppress_summary: bool,
    stderr: RefCell<Box<dyn Write>>,
    stdout: RefCell<Box<dyn Write>>,
}

impl std::fmt::Debug for TextProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextProgress")
            .field("verbosity", &self.verbosity)
            .field("suppress_summary", &self.suppress_summary)
            .finish_non_exhaustive()
    }
}

impl TextProgress {
    /// Create a new text progress reporter writing to real stderr/stdout.
    ///
    /// When `suppress_summary` is true, the run summary is suppressed on
    /// stdout. Per-image status lines still go to stderr. Used when JSON
    /// output owns stdout or when the summary is redundant (single-image copy).
    pub(crate) fn new(verbosity: u8, suppress_summary: bool) -> Self {
        Self {
            verbosity,
            suppress_summary,
            stderr: RefCell::new(Box::new(io::stderr())),
            stdout: RefCell::new(Box::new(io::stdout())),
        }
    }

    /// Create a text progress reporter with custom writers (for testing).
    #[cfg(test)]
    fn with_writers(
        verbosity: u8,
        suppress_summary: bool,
        stderr: Box<dyn Write>,
        stdout: Box<dyn Write>,
    ) -> Self {
        Self {
            verbosity,
            suppress_summary,
            stderr: RefCell::new(stderr),
            stdout: RefCell::new(stdout),
        }
    }
}

impl ProgressReporter for TextProgress {
    fn image_started(&self, _source: &str, _target: &str) {
        // No-op for text output — only progress bar implementations need this.
    }

    fn image_completed(&self, result: &ImageResult) {
        match &result.status {
            ImageStatus::Failed { kind, error, .. } => {
                if let Err(e) = writeln!(
                    self.stderr.borrow_mut(),
                    "FAILED  {} -> {}  ({kind}: {error})",
                    result.source,
                    result.target,
                ) {
                    tracing::warn!(error = %e, "failed to write progress to stderr");
                }
            }
            ImageStatus::Synced if self.verbosity >= 1 => {
                if let Err(e) = writeln!(
                    self.stderr.borrow_mut(),
                    "synced  {} -> {}  ({}, {})",
                    result.source,
                    result.target,
                    format_bytes(result.bytes_transferred),
                    format_duration(result.duration),
                ) {
                    tracing::warn!(error = %e, "failed to write progress to stderr");
                }
            }
            ImageStatus::Skipped { reason } if self.verbosity >= 1 => {
                if let Err(e) = writeln!(
                    self.stderr.borrow_mut(),
                    "skipped {} -> {}  ({reason})",
                    result.source,
                    result.target,
                ) {
                    tracing::warn!(error = %e, "failed to write progress to stderr");
                }
            }
            _ => {}
        }
    }

    fn run_completed(&self, report: &SyncReport) {
        if self.suppress_summary {
            return;
        }
        if report.images.is_empty() {
            return;
        }
        let s = &report.stats;
        if let Err(e) = writeln!(
            self.stdout.borrow_mut(),
            "sync complete: {} synced, {} skipped, {} failed | blobs: {} transferred, {} skipped, {} mounted | {} in {}",
            s.images_synced,
            s.images_skipped,
            s.images_failed,
            s.blobs_transferred,
            s.blobs_skipped,
            s.blobs_mounted,
            format_bytes(s.bytes_transferred),
            format_duration(report.duration),
        ) {
            tracing::warn!(error = %e, "failed to write progress summary to stdout");
        }
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

    fn test_progress(verbosity: u8) -> (TextProgress, Buf, Buf) {
        test_progress_with_suppress(verbosity, false)
    }

    fn test_progress_with_suppress(
        verbosity: u8,
        suppress_summary: bool,
    ) -> (TextProgress, Buf, Buf) {
        let stderr_buf = Rc::new(RefCell::new(Vec::new()));
        let stdout_buf = Rc::new(RefCell::new(Vec::new()));

        let progress = TextProgress::with_writers(
            verbosity,
            suppress_summary,
            Box::new(RcWriter(Rc::clone(&stderr_buf))),
            Box::new(RcWriter(Rc::clone(&stdout_buf))),
        );

        (progress, stderr_buf, stdout_buf)
    }

    fn make_result(status: ImageStatus, bytes: u64) -> ImageResult {
        ImageResult {
            image_id: Uuid::now_v7(),
            source: "source/repo:v1".into(),
            target: "target/repo:v1".into(),
            status,
            bytes_transferred: bytes,
            blob_stats: BlobTransferStats::default(),
            duration: Duration::from_secs(14),
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
            },
            duration: Duration::from_secs(47),
        }
    }

    // -- image_completed tests --

    #[test]
    fn verbosity_0_prints_failed() {
        let (progress, stderr, _stdout) = test_progress(0);
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                error: "connection refused".into(),
                retries: 3,
            },
            0,
        );
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(
            output.contains("FAILED"),
            "should print FAILED, got: {output}"
        );
        assert!(
            output.contains("manifest push"),
            "should contain error kind"
        );
        assert!(
            output.contains("connection refused"),
            "should contain error message"
        );
    }

    #[test]
    fn verbosity_0_silent_on_synced() {
        let (progress, stderr, _stdout) = test_progress(0);
        let result = make_result(ImageStatus::Synced, 187_000_000);
        progress.image_completed(&result);
        let output = stderr.borrow();
        assert!(
            output.is_empty(),
            "verbosity 0 should not print synced images"
        );
    }

    #[test]
    fn verbosity_0_silent_on_skipped() {
        let (progress, stderr, _stdout) = test_progress(0);
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            },
            0,
        );
        progress.image_completed(&result);
        let output = stderr.borrow();
        assert!(
            output.is_empty(),
            "verbosity 0 should not print skipped images"
        );
    }

    #[test]
    fn verbosity_1_prints_synced() {
        let (progress, stderr, _stdout) = test_progress(1);
        let result = make_result(ImageStatus::Synced, 187_000_000);
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(
            output.contains("synced"),
            "should print synced, got: {output}"
        );
        assert!(
            output.contains("187.0 MB"),
            "should contain formatted bytes"
        );
    }

    #[test]
    fn verbosity_1_prints_skipped() {
        let (progress, stderr, _stdout) = test_progress(1);
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            },
            0,
        );
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(
            output.contains("skipped"),
            "should print skipped, got: {output}"
        );
        assert!(
            output.contains("digest match"),
            "should contain skip reason"
        );
    }

    #[test]
    fn verbosity_1_prints_skip_existing() {
        let (progress, stderr, _stdout) = test_progress(1);
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::SkipExisting,
            },
            0,
        );
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(output.contains("skipped"), "should print skipped");
        assert!(
            output.contains("skip existing"),
            "should contain skip existing reason"
        );
    }

    // -- run_completed tests --

    #[test]
    fn run_completed_prints_summary() {
        let (progress, _stderr, stdout) = test_progress(0);
        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        progress.run_completed(&report);
        let output = String::from_utf8(stdout.borrow().clone()).unwrap();
        assert!(
            output.contains("sync complete:"),
            "should print summary, got: {output}"
        );
        assert!(output.contains("3 synced"), "should contain synced count");
        assert!(
            output.contains("47 skipped"),
            "should contain skipped count"
        );
        assert!(output.contains("1 failed"), "should contain failed count");
        assert!(
            output.contains("12 transferred"),
            "should contain blobs transferred count"
        );
        assert!(
            output.contains("0 skipped, 34 mounted"),
            "should contain blobs_skipped and blobs_mounted in order"
        );
        assert!(
            output.contains("432.0 MB"),
            "should contain formatted bytes"
        );
    }

    #[test]
    fn run_completed_empty_report_no_output() {
        let (progress, _stderr, stdout) = test_progress(0);
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        };
        progress.run_completed(&report);
        let output = stdout.borrow();
        assert!(output.is_empty(), "empty report should produce no summary");
    }

    #[test]
    fn stream_separation_failed_stderr_summary_stdout() {
        let (progress, stderr, stdout) = test_progress(0);
        let failed = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPull,
                error: "timeout".into(),
                retries: 2,
            },
            0,
        );
        progress.image_completed(&failed);

        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        progress.run_completed(&report);

        let stderr_text = String::from_utf8(stderr.borrow().clone()).unwrap();
        let stdout_text = String::from_utf8(stdout.borrow().clone()).unwrap();

        assert!(stderr_text.contains("FAILED"), "FAILED should be on stderr");
        assert!(
            !stdout_text.contains("FAILED"),
            "FAILED should NOT be on stdout"
        );
        assert!(
            stdout_text.contains("sync complete:"),
            "summary should be on stdout"
        );
        assert!(
            !stderr_text.contains("sync complete:"),
            "summary should NOT be on stderr"
        );
    }

    #[test]
    fn multiple_images_mixed_status() {
        let (progress, stderr, _stdout) = test_progress(1);

        progress.image_completed(&make_result(ImageStatus::Synced, 100_000_000));
        progress.image_completed(&make_result(ImageStatus::Synced, 200_000_000));
        progress.image_completed(&make_result(
            ImageStatus::Skipped {
                reason: SkipReason::DigestMatch,
            },
            0,
        ));
        progress.image_completed(&make_result(
            ImageStatus::Failed {
                kind: ErrorKind::BlobTransfer,
                error: "connection lost".into(),
                retries: 1,
            },
            0,
        ));

        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        let synced_lines = lines.iter().filter(|l| l.starts_with("synced  ")).count();
        let skipped_lines = lines.iter().filter(|l| l.starts_with("skipped ")).count();
        let failed_lines = lines.iter().filter(|l| l.starts_with("FAILED  ")).count();
        assert_eq!(synced_lines, 2, "should have 2 synced lines");
        assert_eq!(skipped_lines, 1, "should have 1 skipped line");
        assert_eq!(failed_lines, 1, "should have 1 FAILED line");
    }

    #[test]
    fn verbosity_1_prints_immutable_tag_skip() {
        let (progress, stderr, _stdout) = test_progress(1);
        let result = make_result(
            ImageStatus::Skipped {
                reason: SkipReason::ImmutableTag,
            },
            0,
        );
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(output.starts_with("skipped "), "should print skipped");
        assert!(
            output.contains("immutable tag"),
            "should contain immutable tag reason"
        );
    }

    #[test]
    fn run_completed_exact_format() {
        let (progress, _stderr, stdout) = test_progress(0);
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 3,
                images_skipped: 47,
                images_failed: 1,
                blobs_transferred: 12,
                blobs_skipped: 5,
                blobs_mounted: 34,
                bytes_transferred: 432_000_000,
            },
            duration: Duration::from_secs(47),
        };
        progress.run_completed(&report);
        let output = String::from_utf8(stdout.borrow().clone()).unwrap();
        assert_eq!(
            output,
            "sync complete: 3 synced, 47 skipped, 1 failed | blobs: 12 transferred, 5 skipped, 34 mounted | 432.0 MB in 47s\n"
        );
    }

    // -- suppress_summary tests --

    #[test]
    fn run_completed_suppressed_when_flag_set() {
        let (progress, _stderr, stdout) = test_progress_with_suppress(0, true);
        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        progress.run_completed(&report);
        assert!(
            stdout.borrow().is_empty(),
            "suppress_summary should suppress summary on stdout"
        );
    }

    #[test]
    fn suppress_summary_still_prints_failures_to_stderr() {
        let (progress, stderr, stdout) = test_progress_with_suppress(0, true);
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                error: "timeout".into(),
                retries: 2,
            },
            0,
        );
        progress.image_completed(&result);
        let stderr_text = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(
            stderr_text.contains("FAILED"),
            "suppress_summary should still print failures to stderr"
        );
        assert!(
            stdout.borrow().is_empty(),
            "suppress_summary should not write to stdout on image_completed"
        );
    }
}
