//! Verbosity-aware progress reporters for sync output.
//!
//! [`TextProgress`] writes plain status lines to stderr (non-TTY and TTY
//! alike). The run summary always goes to stdout.

use std::cell::RefCell;
use std::io::{self, Write};

use ocync_sync::progress::ProgressReporter;
use ocync_sync::{ImageResult, ImageStatus, SyncReport, SyncStats};

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

/// Write the run summary to `stdout`, or do nothing if `suppress_summary`
/// is true or the report contains no images.
fn write_run_summary(
    stdout: &RefCell<Box<dyn Write>>,
    report: &SyncReport,
    suppress_summary: bool,
) {
    if suppress_summary {
        return;
    }
    if report.images.is_empty() {
        return;
    }
    let s = &report.stats;
    let has_discovery = s.discovery_cache_hits > 0
        || s.discovery_cache_misses > 0
        || s.discovery_head_first_skips > 0
        || s.immutable_tag_skips > 0;
    let discovery = if has_discovery {
        let head_first_suffix = if s.discovery_head_first_skips > 0 {
            format!(", {} head_first", s.discovery_head_first_skips)
        } else {
            String::new()
        };
        let immutable_suffix = if s.immutable_tag_skips > 0 {
            format!(", {} immutable", s.immutable_tag_skips)
        } else {
            String::new()
        };
        format!(
            " | discovery: {} cached, {} pulled{}{}",
            s.discovery_cache_hits, s.discovery_cache_misses, head_first_suffix, immutable_suffix,
        )
    } else {
        String::new()
    };
    let artifacts_warn = if s.artifacts_skipped > 0 {
        format!(" | {} artifacts skipped", s.artifacts_skipped)
    } else {
        String::new()
    };
    if let Err(e) = writeln!(
        stdout.borrow_mut(),
        "sync complete: {} synced, {} skipped, {} failed | blobs: {} transferred, {} skipped, {} mounted | {} in {}{}{}",
        s.images_synced,
        s.images_skipped,
        s.images_failed,
        s.blobs_transferred,
        s.blobs_skipped,
        s.blobs_mounted,
        format_bytes(s.bytes_transferred),
        format_duration(report.duration),
        discovery,
        artifacts_warn,
    ) {
        tracing::warn!(error = %e, "failed to write progress summary to stdout");
    }
}

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
        // No-op for text output.
    }

    fn image_completed(&self, result: &ImageResult) {
        if let Some(line) = format_image_line(result, self.verbosity) {
            if let Err(e) = writeln!(self.stderr.borrow_mut(), "{line}") {
                tracing::warn!(error = %e, "failed to write progress to stderr");
            }
        }
    }

    fn run_completed(&self, report: &SyncReport) {
        write_run_summary(&self.stdout, report, self.suppress_summary);
    }
}

/// Watch-mode wrapper that dedupes the per-cycle summary so an idle pod
/// stops re-emitting the same `sync complete: 0 synced, N skipped, ...`
/// line once per cycle. Per-image events still flow through the inner
/// reporter unchanged.
///
/// The first cycle always emits. Subsequent cycles emit only when the
/// aggregate [`SyncStats`] differ from the prior cycle's -- so a transfer,
/// failure, or count change is visible, but a steady-state all-skip cycle
/// is silent.
pub(crate) struct DedupingWatchProgress<'a> {
    inner: &'a dyn ProgressReporter,
    last_stats: RefCell<Option<SyncStats>>,
}

impl<'a> DedupingWatchProgress<'a> {
    pub(crate) fn new(inner: &'a dyn ProgressReporter) -> Self {
        Self {
            inner,
            last_stats: RefCell::new(None),
        }
    }
}

impl ProgressReporter for DedupingWatchProgress<'_> {
    fn image_started(&self, source: &str, target: &str) {
        self.inner.image_started(source, target);
    }

    fn image_completed(&self, result: &ImageResult) {
        self.inner.image_completed(result);
    }

    fn run_completed(&self, report: &SyncReport) {
        let mut last = self.last_stats.borrow_mut();
        let changed = last.as_ref().is_none_or(|prev| prev != &report.stats);
        if changed {
            self.inner.run_completed(report);
            *last = Some(report.stats.clone());
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

    // - write_run_summary tests --

    #[test]
    fn summary_exact_format() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
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
                ..SyncStats::default()
            },
            duration: Duration::from_secs(47),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert_eq!(
            output,
            "sync complete: 3 synced, 47 skipped, 1 failed | blobs: 12 transferred, 5 skipped, 34 mounted | 432.0 MB in 47s\n"
        );
    }

    #[test]
    fn summary_with_discovery_stats() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 3,
                images_skipped: 47,
                images_failed: 0,
                blobs_transferred: 12,
                blobs_skipped: 5,
                blobs_mounted: 34,
                bytes_transferred: 432_000_000,
                discovery_cache_hits: 40,
                discovery_cache_misses: 10,
                discovery_head_failures: 2,
                discovery_target_stale: 1,
                discovery_head_first_skips: 0,
                immutable_tag_skips: 0,
                artifacts_skipped: 0,
            },
            duration: Duration::from_secs(47),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert_eq!(
            output,
            "sync complete: 3 synced, 47 skipped, 0 failed | blobs: 12 transferred, 5 skipped, 34 mounted | 432.0 MB in 47s | discovery: 40 cached, 10 pulled\n"
        );
    }

    #[test]
    fn summary_omits_discovery_when_zero() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(!output.contains("discovery"), "got: {output}");
    }

    #[test]
    fn summary_with_only_cache_hits_includes_discovery() {
        // Distinguishes the `||` from `&&` in the discovery condition:
        // even when misses == 0, hits > 0 should show the discovery suffix.
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 1,
                discovery_cache_hits: 5,
                discovery_cache_misses: 0,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(1),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("discovery: 5 cached, 0 pulled"),
            "got: {output}"
        );
    }

    #[test]
    fn summary_with_only_cache_misses_includes_discovery() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 1,
                discovery_cache_hits: 0,
                discovery_cache_misses: 3,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(1),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("discovery: 0 cached, 3 pulled"),
            "got: {output}"
        );
    }

    #[test]
    fn summary_with_head_first_skips_includes_suffix() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 1,
                images_skipped: 3,
                discovery_cache_hits: 2,
                discovery_cache_misses: 1,
                discovery_head_first_skips: 3,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(5),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("discovery: 2 cached, 1 pulled, 3 head_first"),
            "got: {output}"
        );
    }

    #[test]
    fn summary_with_only_head_first_skips_includes_discovery() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 1,
                discovery_head_first_skips: 5,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(1),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("discovery: 0 cached, 0 pulled, 5 head_first"),
            "got: {output}"
        );
    }

    #[test]
    fn summary_with_immutable_skips_includes_suffix() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 1,
                images_skipped: 10,
                discovery_cache_hits: 2,
                discovery_cache_misses: 1,
                immutable_tag_skips: 8,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(3),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("discovery: 2 cached, 1 pulled, 8 immutable"),
            "got: {output}"
        );
    }

    #[test]
    fn summary_with_only_immutable_skips_includes_discovery() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 1,
                immutable_tag_skips: 50,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(1),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("discovery: 0 cached, 0 pulled, 50 immutable"),
            "got: {output}"
        );
    }

    #[test]
    fn summary_with_artifacts_skipped() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![make_result(ImageStatus::Synced, 1024)],
            stats: SyncStats {
                images_synced: 5,
                artifacts_skipped: 2,
                ..SyncStats::default()
            },
            duration: Duration::from_secs(10),
        };
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(output.contains("2 artifacts skipped"), "got: {output}");
    }

    #[test]
    fn summary_without_artifacts_skipped_omits_suffix() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        write_run_summary(&stdout, &report, false);
        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(!output.contains("artifacts skipped"), "got: {output}");
    }

    #[test]
    fn summary_suppressed_produces_no_output() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        write_run_summary(&stdout, &report, true);
        assert!(buf.borrow().is_empty());
    }

    #[test]
    fn summary_empty_report_produces_no_output() {
        let buf: Buf = Rc::new(RefCell::new(Vec::new()));
        let stdout: RefCell<Box<dyn Write>> = RefCell::new(Box::new(RcWriter(Rc::clone(&buf))));
        let report = SyncReport {
            run_id: Uuid::now_v7(),
            images: vec![],
            stats: SyncStats::default(),
            duration: Duration::ZERO,
        };
        write_run_summary(&stdout, &report, false);
        assert!(buf.borrow().is_empty());
    }

    // - TextProgress tests (wiring: writes to correct streams) --

    fn test_text_progress(verbosity: u8) -> (TextProgress, Buf, Buf) {
        test_text_progress_with_suppress(verbosity, false)
    }

    fn test_text_progress_with_suppress(
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

    #[test]
    fn text_image_started_is_noop() {
        let (progress, stderr, stdout) = test_text_progress(1);
        progress.image_started("source/repo:v1", "target/repo:v1");
        assert!(
            stderr.borrow().is_empty(),
            "image_started should not write to stderr"
        );
        assert!(
            stdout.borrow().is_empty(),
            "image_started should not write to stdout"
        );
    }

    #[test]
    fn text_image_completed_writes_to_stderr() {
        let (progress, stderr, stdout) = test_text_progress(1);
        let result = make_result(ImageStatus::Synced, 187_000_000);
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(output.starts_with("synced  "), "got: {output}");
        assert!(
            stdout.borrow().is_empty(),
            "per-image output must NOT go to stdout"
        );
    }

    #[test]
    fn text_image_completed_silent_at_v0() {
        let (progress, stderr, stdout) = test_text_progress(0);
        let result = make_result(ImageStatus::Synced, 187_000_000);
        progress.image_completed(&result);
        assert!(stderr.borrow().is_empty());
        assert!(stdout.borrow().is_empty());
    }

    #[test]
    fn text_failed_always_writes_to_stderr() {
        let (progress, stderr, stdout) = test_text_progress(0);
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                error: "timeout".into(),
                retries: 2,
                status_code: None,
            },
            0,
        );
        progress.image_completed(&result);
        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        assert!(output.starts_with("FAILED  "), "got: {output}");
        assert!(
            stdout.borrow().is_empty(),
            "per-image output must NOT go to stdout"
        );
    }

    #[test]
    fn text_stream_separation() {
        let (progress, stderr, stdout) = test_text_progress(0);
        let failed = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPull,
                error: "timeout".into(),
                retries: 2,
                status_code: None,
            },
            0,
        );
        progress.image_completed(&failed);

        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        progress.run_completed(&report);

        let stderr_text = String::from_utf8(stderr.borrow().clone()).unwrap();
        let stdout_text = String::from_utf8(stdout.borrow().clone()).unwrap();

        // Per-image output on stderr, summary on stdout, never crossed.
        assert!(stderr_text.contains("FAILED"), "FAILED should be on stderr");
        assert!(
            !stdout_text.contains("FAILED"),
            "FAILED must NOT be on stdout"
        );
        assert!(
            stdout_text.contains("sync complete:"),
            "summary should be on stdout"
        );
        assert!(
            !stderr_text.contains("sync complete:"),
            "summary must NOT be on stderr"
        );
    }

    #[test]
    fn text_multiple_images_mixed_status() {
        let (progress, stderr, _stdout) = test_text_progress(1);

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
                status_code: None,
            },
            0,
        ));

        let output = String::from_utf8(stderr.borrow().clone()).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("synced  ")).count(),
            2
        );
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("skipped ")).count(),
            1
        );
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("FAILED  ")).count(),
            1
        );
    }

    #[test]
    fn text_suppress_summary_still_prints_failures() {
        let (progress, stderr, stdout) = test_text_progress_with_suppress(0, true);
        let result = make_result(
            ImageStatus::Failed {
                kind: ErrorKind::ManifestPush,
                error: "timeout".into(),
                retries: 2,
                status_code: None,
            },
            0,
        );
        progress.image_completed(&result);

        let report = make_report(vec![make_result(ImageStatus::Synced, 1024)]);
        progress.run_completed(&report);

        assert!(
            !stderr.borrow().is_empty(),
            "failures should still go to stderr"
        );
        assert!(
            stdout.borrow().is_empty(),
            "suppress_summary should suppress stdout"
        );
    }

    // -- DedupingWatchProgress -------------------------------------------

    /// Counting progress reporter that records each `run_completed` call.
    /// Used to assert how many times the wrapper passes through.
    #[derive(Default)]
    struct CountingProgress {
        runs: RefCell<usize>,
    }
    impl ProgressReporter for CountingProgress {
        fn image_started(&self, _: &str, _: &str) {}
        fn image_completed(&self, _: &ImageResult) {}
        fn run_completed(&self, _: &SyncReport) {
            *self.runs.borrow_mut() += 1;
        }
    }

    fn report_with(stats: SyncStats) -> SyncReport {
        SyncReport {
            run_id: Uuid::now_v7(),
            images: Vec::new(),
            stats,
            // Duration intentionally varies cycle-to-cycle in production;
            // pick non-zero here so tests notice if the dedup keys on it.
            duration: Duration::from_secs(1),
        }
    }

    /// First cycle always passes through; identical follow-up cycles are
    /// suppressed; a third cycle that differs passes through again.
    #[test]
    fn deduping_watch_emits_only_when_stats_change() {
        let inner = CountingProgress::default();
        let dedup = DedupingWatchProgress::new(&inner as &dyn ProgressReporter);

        let steady = SyncStats {
            images_skipped: 250,
            ..SyncStats::default()
        };
        let active = SyncStats {
            images_synced: 1,
            images_skipped: 249,
            ..SyncStats::default()
        };

        dedup.run_completed(&report_with(steady.clone())); // cycle 1: emit
        dedup.run_completed(&report_with(steady.clone())); // cycle 2: suppress
        dedup.run_completed(&report_with(steady.clone())); // cycle 3: suppress
        dedup.run_completed(&report_with(active.clone())); // cycle 4: emit (changed)
        dedup.run_completed(&report_with(active)); //         cycle 5: suppress
        dedup.run_completed(&report_with(steady)); //         cycle 6: emit (changed)

        assert_eq!(*inner.runs.borrow(), 3);
    }

    /// Wall-clock duration varies cycle-to-cycle even in steady state; the
    /// dedup must key on stats only, not on the report's `duration`.
    #[test]
    fn deduping_watch_ignores_duration_drift() {
        let inner = CountingProgress::default();
        let dedup = DedupingWatchProgress::new(&inner as &dyn ProgressReporter);

        let stats = SyncStats {
            images_skipped: 5,
            ..SyncStats::default()
        };

        let mut report1 = report_with(stats.clone());
        report1.duration = Duration::from_millis(800);
        let mut report2 = report_with(stats);
        report2.duration = Duration::from_millis(1_400);

        dedup.run_completed(&report1);
        dedup.run_completed(&report2);

        assert_eq!(*inner.runs.borrow(), 1, "duration drift must not re-emit");
    }

    /// Per-image events are never deduped -- they describe distinct
    /// transfers and must always reach the inner reporter.
    #[test]
    fn deduping_watch_passes_image_events_through() {
        struct ImageCounter {
            started: RefCell<usize>,
            completed: RefCell<usize>,
        }
        impl ProgressReporter for ImageCounter {
            fn image_started(&self, _: &str, _: &str) {
                *self.started.borrow_mut() += 1;
            }
            fn image_completed(&self, _: &ImageResult) {
                *self.completed.borrow_mut() += 1;
            }
            fn run_completed(&self, _: &SyncReport) {}
        }
        let inner = ImageCounter {
            started: RefCell::new(0),
            completed: RefCell::new(0),
        };
        let dedup = DedupingWatchProgress::new(&inner as &dyn ProgressReporter);

        dedup.image_started("a", "b");
        dedup.image_started("c", "d");
        dedup.image_completed(&make_result(ImageStatus::Synced, 1));
        dedup.image_completed(&make_result(ImageStatus::Synced, 2));
        dedup.image_completed(&make_result(ImageStatus::Synced, 3));

        assert_eq!(*inner.started.borrow(), 2);
        assert_eq!(*inner.completed.borrow(), 3);
    }
}
