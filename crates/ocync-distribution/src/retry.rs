//! Bounded retry for transient failures in the prepare phase.
//!
//! The client surfaces most failures to its caller, and the sync engine
//! retries the ones it drives. The prepare phase has no such layer above it:
//! it runs before the engine exists, so a transient failure there fails a
//! whole mapping or image. The operations that run there retry themselves,
//! through [`retrying`], and share one schedule and one predicate so they
//! cannot drift apart.

use std::cell::Cell;
use std::future::Future;
use std::time::Duration;

use crate::error::Error;

/// How many times a transient failure is re-sent before it surfaces.
///
/// Bounded so a registry that never recovers still returns rather than
/// holding a mapping open forever.
pub const MAX_TRANSIENT_RETRIES: u32 = 4;

/// Total retries allowed across one multi-page tag listing.
///
/// [`MAX_TRANSIENT_RETRIES`] still caps any single page, which keeps that
/// page's backoff ladder short. This is the ceiling on the walk as a whole:
/// a long listing may legitimately meet a throttle on several different
/// pages, but four retries on each of up to `MAX_TAG_PAGES` pages is a bound
/// only on paper.
pub const MAX_LISTING_RETRIES: u32 = 32;

/// Delay before the first retry.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Upper bound on a single backoff, before jitter.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Whether a transport-level failure is worth re-sending.
///
/// # Predicate set
///
/// `is_request()` covers errors raised during request dispatch, which on the
/// async hyper path includes connect failures and timeouts: reqwest wraps both
/// as `Kind::Request`. `is_body()` and `is_decode()` cover the response-body
/// and decoder phases, which are not classified as `Kind::Request`. A status
/// error means the registry answered, so whether to re-send it is the status
/// arm of [`is_transient`] rather than this.
///
/// This is the single definition of the transient-transport surface for the
/// workspace. `ocync_sync::retry::should_retry_transport` delegates here
/// rather than repeating the predicate, because two copies drift.
pub fn is_transient_transport(error: &reqwest::Error) -> bool {
    error.is_request() || error.is_body() || error.is_decode()
}

/// Whether an error is worth re-sending: a throttle, or a transport failure.
///
/// A 429 is backpressure rather than a refusal. AIMD has already halved the
/// window by the time one surfaces, so the retry is paced by the throttle it
/// caused.
pub(crate) fn is_transient(error: &Error) -> bool {
    match error {
        Error::Http(e) => is_transient_transport(e),
        other => other.status_code() == Some(http::StatusCode::TOO_MANY_REQUESTS),
    }
}

/// Exponential backoff for attempt `attempt` (0-indexed), jittered.
///
/// Jitter is not decoration here. The prepare phase runs up to 16 mappings at
/// once against one registry, so a burst of 429s throttles them together; an
/// unjittered schedule would send all 16 retries back in lockstep and produce
/// the next burst. The factor is drawn from `[0.75, 1.25)`, matching
/// `ocync_sync::retry`.
fn backoff(attempt: u32) -> Duration {
    let base = INITIAL_BACKOFF
        .saturating_mul(1u32 << attempt.min(16))
        .min(MAX_BACKOFF);
    base.mul_f64(0.75 + fastrand::f64() * 0.5)
}

/// Run `op`, re-running it while it fails transiently.
///
/// `what` names the operation in the retry log. `op` must be idempotent: the
/// callers are reads and token exchanges, which are.
///
/// `budget` is drawn down by every retry and may be shared across calls, which
/// is the bound that matters for an operation made of many of them. A tag
/// listing may walk thousands of pages, and four retries on each of them is a
/// bound only on paper: it permits hours of backing off against a registry that
/// is plainly refusing to serve the listing.
pub(crate) async fn retrying<T, F, Fut>(what: &str, budget: &Cell<u32>, op: F) -> Result<T, Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) if is_transient(&e) && attempt < MAX_TRANSIENT_RETRIES && budget.get() > 0 => {
                budget.set(budget.get() - 1);
                let delay = backoff(attempt);
                tracing::debug!(
                    what,
                    attempt,
                    delay_ms = delay.as_millis(),
                    error = %e,
                    "transient failure; backing off and retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Send `request`, re-sending it while the answer looks transient.
///
/// Returns the **final response**, including a throttled one, rather than
/// turning a 429 into an error. That distinction is load-bearing for the auth
/// paths: their callers collapse any non-success into `Error::AuthFailed`,
/// which carries no `status_code()`, and the engine's `with_retry` therefore
/// cannot classify it as retryable. Surfacing a 429 as a typed error instead
/// would hand the engine a retryable auth failure and multiply this retry by
/// the engine's own, aiming 4x the requests at a registry that is already
/// asking for less.
///
/// `Err` is only for a transport failure that outlived its attempts.
pub(crate) async fn send_retrying(
    request: reqwest::RequestBuilder,
    what: &str,
) -> Result<reqwest::Response, Error> {
    // Bodyless requests always clone. The guard keeps a future request with a
    // streaming body correct rather than silently replaying something that
    // cannot be replayed.
    let Some(probe) = request.try_clone() else {
        return Ok(request.send().await?);
    };
    drop(probe);

    let mut attempt: u32 = 0;
    loop {
        let this_attempt = request
            .try_clone()
            .expect("request was confirmed cloneable above");
        let outcome = this_attempt.send().await;

        let transient = match &outcome {
            Ok(resp) => resp.status() == http::StatusCode::TOO_MANY_REQUESTS,
            Err(e) => is_transient_transport(e),
        };
        if !transient || attempt >= MAX_TRANSIENT_RETRIES {
            return Ok(outcome?);
        }

        let delay = backoff(attempt);
        tracing::debug!(
            what,
            attempt,
            delay_ms = delay.as_millis(),
            "transient failure; backing off and retrying"
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bounds rather than equality: the schedule is jittered on purpose.
    #[test]
    fn backoff_grows_and_is_capped() {
        for attempt in 0..4u32 {
            let base = INITIAL_BACKOFF * (1 << attempt);
            let observed = backoff(attempt);
            assert!(
                observed >= base.mul_f64(0.75) && observed < base.mul_f64(1.25),
                "attempt {attempt}: {observed:?} outside the jitter band around {base:?}"
            );
        }
        // Far past any real attempt count: must saturate, not overflow.
        assert!(backoff(u32::MAX) <= MAX_BACKOFF.mul_f64(1.25));
    }

    /// Sixteen mappings throttled together must not retry in lockstep.
    #[test]
    fn backoff_is_decorrelated() {
        let draws: Vec<Duration> = (0..16).map(|_| backoff(1)).collect();
        let distinct = draws
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert!(
            distinct > 1,
            "an unjittered schedule sends every concurrent retry back together: {draws:?}"
        );
    }

    /// The predicate has to match the failure that actually shows up.
    ///
    /// A run against a live registry lost a mapping to "error sending request
    /// for url (.../token/...)". Asserting against a hand-built error would
    /// prove nothing, so this provokes a real one from a closed port.
    #[tokio::test]
    async fn a_refused_connection_is_transient() {
        let err = crate::test_http_client()
            .get("http://127.0.0.1:1/v2/")
            .send()
            .await
            .expect_err("port 1 refuses connections");
        assert!(
            is_transient_transport(&err),
            "a refused connection must be retryable: {err:?}"
        );
        assert!(
            is_transient(&Error::Http(err)),
            "and must stay retryable once wrapped"
        );
    }

    /// A status error means the registry answered, so it is not a transport
    /// failure. Whether it is re-sent at all is the status arm's business.
    #[tokio::test]
    async fn a_status_error_is_not_a_transport_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = crate::test_http_client()
            .get(server.uri())
            .send()
            .await
            .expect("the server answered")
            .error_for_status()
            .expect_err("500 is an error status");
        assert!(
            !is_transient_transport(&err),
            "a status error is not a transport failure: {err:?}"
        );
    }

    /// Only a 429 is re-sent; other registry statuses are the caller's answer.
    #[test]
    fn only_a_throttle_is_transient_among_statuses() {
        let throttled = Error::RegistryError {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            message: "Rate exceeded".into(),
        };
        assert!(is_transient(&throttled));

        for status in [
            http::StatusCode::NOT_FOUND,
            http::StatusCode::FORBIDDEN,
            http::StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let err = Error::RegistryError {
                status,
                message: "no".into(),
            };
            assert!(!is_transient(&err), "{status} must not be re-sent");
        }
    }

    #[tokio::test]
    async fn retrying_gives_up_and_surfaces_the_last_error() {
        let calls = Cell::new(0u32);
        let err = retrying::<(), _, _>(
            "always throttled",
            &Cell::new(MAX_TRANSIENT_RETRIES),
            || {
                calls.set(calls.get() + 1);
                async {
                    Err(Error::RegistryError {
                        status: http::StatusCode::TOO_MANY_REQUESTS,
                        message: "Rate exceeded".into(),
                    })
                }
            },
        )
        .await
        .expect_err("a permanently throttled op must surface");
        assert_eq!(
            calls.get(),
            MAX_TRANSIENT_RETRIES + 1,
            "one initial attempt plus every retry"
        );
        assert_eq!(err.status_code(), Some(http::StatusCode::TOO_MANY_REQUESTS));
    }

    #[tokio::test]
    async fn retrying_does_not_re_run_a_permanent_failure() {
        let calls = Cell::new(0u32);
        let _ = retrying::<(), _, _>("not found", &Cell::new(MAX_TRANSIENT_RETRIES), || {
            calls.set(calls.get() + 1);
            async {
                Err(Error::RegistryError {
                    status: http::StatusCode::NOT_FOUND,
                    message: "absent".into(),
                })
            }
        })
        .await;
        assert_eq!(calls.get(), 1, "a 404 must be surfaced on the first answer");
    }
}
