//! Log capture for tests that check what a call logs.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::sync::Once;

use tokio::runtime::{Handle, RuntimeFlavor};

thread_local! {
    /// The log lines of this thread's capture, `None` outside one.
    static CAPTURED: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Writes a log line into the capture of the thread that wrote it, and
/// drops it when that thread is not capturing.
struct CaptureWriter;

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // A thread logging as it exits finds its capture already gone.
        let _ = CAPTURED.try_with(|captured| {
            if let Some(captured) = captured.borrow_mut().as_mut() {
                captured.extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Await `future` with every log line it writes at any level captured.
///
/// One global subscriber serves every test in the process, because a
/// subscriber set per test races with the others over tracing's global
/// level and callsite caches and loses lines when tests run in parallel. It
/// has no filter, since tracing caches a filter's answer per log site.
/// Lines go to the capture of the thread that wrote them, so the future must
/// run on this thread, as a `current_thread` runtime does. Captures do not
/// nest, and one dropped before it finishes fails the next on its thread.
pub(crate) async fn capture_logs<T>(future: impl Future<Output = T>) -> (T, String) {
    assert_eq!(
        Handle::current().runtime_flavor(),
        RuntimeFlavor::CurrentThread,
        "lines logged on a runtime's other threads would be missed"
    );
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(|| CaptureWriter)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in this test binary");
    });
    let open = CAPTURED.replace(Some(Vec::new()));
    assert!(open.is_none(), "a capture is already open on this thread");
    let output = future.await;
    let logs = CAPTURED.take().unwrap();
    (output, String::from_utf8(logs).unwrap())
}

mod tests {
    use super::*;

    /// The capture holds what the awaited future logs at any level, and nothing
    /// another thread logs meanwhile or this thread logs after it. Every line
    /// comes from one log site, first reached on a thread that is not capturing,
    /// as when parallel tests share one. Breaks if that site stays unheard in the
    /// capture, a level is filtered out, or lines reach a capture they were not
    /// written in.
    #[tokio::test(flavor = "current_thread")]
    async fn captures_only_the_awaited_futures_lines() {
        fn log(line: &str) {
            tracing::trace!("{line}");
        }
        // Installs the subscriber, as an earlier test would.
        capture_logs(async {}).await;
        let ((), logs) = capture_logs(async {
            std::thread::spawn(|| log("on another thread"))
                .join()
                .unwrap();
            log("inside the capture");
        })
        .await;
        log("after the capture");
        assert!(logs.contains("inside the capture"), "{logs}");
        for line in ["on another thread", "after the capture"] {
            assert!(!logs.contains(line), "{line}: {logs}");
        }
    }
}
