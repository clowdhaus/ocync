//! Bounded retry for transient failures in the prepare phase.
//!
//! The client surfaces most failures to its caller, and the sync engine
//! retries the ones it drives. The prepare phase has no such layer above it:
//! it runs before the engine exists, so a transient failure there fails a
//! whole mapping or image. The operations that run there retry themselves,
//! through `retrying`, and share one schedule so they cannot drift apart.
//! Each passes the predicate for its own errors, built on
//! `is_transient_transport`. The AWS API calls run the same loop under the AWS
//! SDK's retry policy instead (see `crate::aws`).

use std::cell::Cell;
use std::future::Future;
use std::time::{Duration, SystemTime};

use crate::error::Error;

/// How many times a transient failure is re-sent before it surfaces.
///
/// Bounded so a registry that never recovers still returns rather than
/// holding a mapping open forever.
pub const MAX_TRANSIENT_RETRIES: u32 = 4;

/// Total retries allowed across one multi-page tag listing.
///
/// [`MAX_TRANSIENT_RETRIES`] still caps each page. A per-page cap alone over
/// up to `MAX_TAG_PAGES` pages would permit hours of backing off at a
/// registry that has stopped serving the listing.
pub const MAX_LISTING_RETRIES: u32 = 32;

/// The most `Retry-After` time one operation waits out, summed over its
/// retries (a tag listing's pages share one sum).
///
/// More is a quota that will not recover within a sync (Docker Hub's pull
/// limit answers with hours), so the 429 surfaces at once instead of holding
/// the run open. Kept under the engine's 180 s wait for a leader's blob, so a
/// throttled leader still lands before its followers give up on mounting.
const MAX_HONORED_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Delay before the first retry.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Upper bound on a single backoff, before jitter.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Whether a transport-level failure is worth re-sending.
///
/// `is_request()` covers dispatch, including connect failures and timeouts,
/// which reqwest wraps as `Kind::Request`. `is_body()` and `is_decode()`
/// cover the response body: reqwest reports a body cut short as a decode
/// failure. A status error means the registry answered, so it is judged by
/// status instead.
///
/// The single definition for the workspace;
/// `ocync_sync::retry::should_retry_transport` delegates here.
pub fn is_transient_transport(error: &reqwest::Error) -> bool {
    error.is_request() || error.is_body() || error.is_decode()
}

/// Whether a server that answered `status` may answer differently if asked
/// again: 408, 429 or any 5xx. The single definition for the workspace, as
/// crane and regsync retry the same set.
pub fn is_transient_status(status: http::StatusCode) -> bool {
    status.is_server_error()
        || status == http::StatusCode::REQUEST_TIMEOUT
        || status == http::StatusCode::TOO_MANY_REQUESTS
}

/// Whether an error from a [`retrying`] call is worth re-sending: a
/// throttle, or a transport failure. A 5xx here is the caller's answer;
/// only [`send_retrying`]'s auth requests re-send one.
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

/// The wait a `Retry-After` value names, in either of its forms (RFC 9110
/// section 10.2.3): delta seconds, or an HTTP date in the IMF-fixdate form
/// senders must use, measured from the registry's own clock `now`, so a past
/// date is zero. A number too large to
/// hold counts as one second over [`MAX_HONORED_RETRY_AFTER`], so the cap
/// refuses it. Anything else is `None`, and the caller backs off as it
/// would without the header.
fn retry_after_value(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let beyond_cap = MAX_HONORED_RETRY_AFTER + Duration::from_secs(1);
        return Some(value.parse().map_or(beyond_cap, Duration::from_secs));
    }
    let date = crate::aws::calendar::parse_imf_fixdate(value)?;
    Some(date.duration_since(now).unwrap_or(Duration::ZERO))
}

/// How long a 429 `response` asked to be left alone, from its `Retry-After`
/// header. Only a throttle's header is read. A date is measured against the
/// response's `Date`, when it has one, so local clock skew does not change
/// the wait.
pub(crate) fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    if response.status() != http::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let header = |name| response.headers().get(name)?.to_str().ok();
    let now = header(http::header::DATE)
        .and_then(crate::aws::calendar::parse_imf_fixdate)
        .unwrap_or_else(SystemTime::now);
    retry_after_value(header(http::header::RETRY_AFTER)?, now)
}

/// The wait before re-sending after `error`, given the jittered `backoff` a
/// retry loop would use: `backoff`, plus the registry's `Retry-After` when
/// it named one. `honored` is the `Retry-After` time this operation has
/// already waited out, and grows by this one. `None`, with a WARN naming
/// `error`, when the sum would pass 120 s and the error
/// should surface. The single definition every registry retry loop shares.
pub fn retry_delay(error: &Error, backoff: Duration, honored: &mut Duration) -> Option<Duration> {
    let asked = match error {
        Error::RegistryError { retry_after, .. } => *retry_after,
        _ => None,
    };
    let delay = delay_unless_over_cap(asked, backoff, honored);
    if delay.is_none() {
        tracing::warn!(
            error = %error,
            retry_after_s = asked.unwrap_or_default().as_secs(),
            already_waited_s = honored.as_secs(),
            max_s = MAX_HONORED_RETRY_AFTER.as_secs(),
            "registry asked for a longer wait than a sync waits out; not retrying"
        );
    }
    delay
}

/// `backoff` when the registry `asked` for no wait, and `asked` plus
/// `backoff` when it did, adding `asked` to `honored`. The registry counts
/// `Retry-After` down to the end of its quota window, so the jittered
/// `backoff` on top keeps every request it throttled from re-sending at the
/// same instant. `None` when `honored` plus `asked` is over
/// [`MAX_HONORED_RETRY_AFTER`]: the wait is refused, not shortened.
fn delay_unless_over_cap(
    asked: Option<Duration>,
    backoff: Duration,
    honored: &mut Duration,
) -> Option<Duration> {
    let Some(asked) = asked else {
        return Some(backoff);
    };
    let total = honored.saturating_add(asked);
    if total > MAX_HONORED_RETRY_AFTER {
        return None;
    }
    *honored = total;
    Some(asked + backoff)
}

/// Exponential backoff for attempt `attempt` (0-indexed), jittered.
///
/// Jitter is not decoration here. The prepare phase runs up to 16 mappings at
/// once against one registry, so a burst of 429s throttles them together; an
/// unjittered schedule would send all 16 retries back in lockstep and produce
/// the next burst. The factor comes from [`jittered`].
fn backoff(attempt: u32) -> Duration {
    let base = INITIAL_BACKOFF
        .saturating_mul(1u32 << attempt.min(16))
        .min(MAX_BACKOFF);
    jittered(base)
}

/// Scale `base` by a random factor in `[0.75, 1.25)` to decorrelate concurrent
/// retries. `fastrand`'s thread-local PRNG makes each call one `f64` draw with
/// no syscall. The single definition for the workspace; the sync engine's
/// backoff uses it too.
pub fn jittered(base: Duration) -> Duration {
    base.mul_f64(0.75 + fastrand::f64() * 0.5)
}

/// Decides, after each attempt of a call, whether and when to send it again.
pub(crate) trait Policy<E> {
    /// The wait before sending again after `error` ended attempt `attempt`
    /// (0-indexed), or `None` to surface `error`.
    fn retry(&mut self, attempt: u32, error: &E) -> Option<Duration>;

    /// The call succeeded.
    fn succeeded(&mut self) {}
}

/// The retries one operation may still spend and the `Retry-After` time it
/// has waited out, shared across its calls (see [`MAX_LISTING_RETRIES`]).
pub(crate) struct RetryBudget {
    retries: Cell<u32>,
    honored: Cell<Duration>,
}

impl RetryBudget {
    /// A budget of `retries`, with no `Retry-After` waited out yet.
    pub(crate) fn new(retries: u32) -> Self {
        Self {
            retries: Cell::new(retries),
            honored: Cell::new(Duration::ZERO),
        }
    }
}

/// The crate's policy: an [`is_transient`] failure is sent again on the jittered
/// schedule, at most [`MAX_TRANSIENT_RETRIES`] times per call, and each retry
/// and each `Retry-After` wait is drawn from the budget.
pub(crate) struct Budgeted<'a>(pub(crate) &'a RetryBudget);

impl Policy<Error> for Budgeted<'_> {
    fn retry(&mut self, attempt: u32, error: &Error) -> Option<Duration> {
        let budget = self.0;
        if budget.retries.get() == 0 {
            return None;
        }
        let mut honored = budget.honored.get();
        let delay = retry_delay(
            error,
            Bounded(is_transient).retry(attempt, error)?,
            &mut honored,
        )?;
        budget.honored.set(honored);
        budget.retries.set(budget.retries.get() - 1);
        Some(delay)
    }
}

/// A failure the predicate calls transient is sent again on the jittered
/// schedule, at most [`MAX_TRANSIENT_RETRIES`] times per call.
pub(crate) struct Bounded<F>(pub(crate) F);

impl<E, F: Fn(&E) -> bool> Policy<E> for Bounded<F> {
    fn retry(&mut self, attempt: u32, error: &E) -> Option<Duration> {
        ((self.0)(error) && attempt < MAX_TRANSIENT_RETRIES).then(|| backoff(attempt))
    }
}

/// Run `op`, re-running it while `policy` says to.
///
/// `what` names the operation in the retry log. `op` is given the attempt
/// number, 0 for the first, and must be idempotent: the callers are reads
/// and token exchanges, which are.
pub(crate) async fn retrying<T, E, P, F, Fut>(what: &str, policy: &mut P, op: F) -> Result<T, E>
where
    E: std::error::Error + 'static,
    P: Policy<E>,
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op(attempt).await {
            Ok(value) => {
                policy.succeeded();
                return Ok(value);
            }
            Err(e) => {
                let Some(delay) = policy.retry(attempt, &e) else {
                    return Err(e);
                };
                tracing::debug!(
                    what,
                    attempt,
                    delay_ms = delay.as_millis(),
                    error = %crate::error::error_chain(&e),
                    "transient failure; backing off and retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// Send `request`, re-sending it while the answer looks transient: a
/// transport failure or an [`is_transient_status`] answer.
///
/// Returns the final response, a throttled or failing one included, rather
/// than an error. The auth callers collapse any non-success into
/// `Error::RegistryPing`, `Error::TokenEndpoint` or `Error::CloudCredentials`,
/// none of which the engine's `with_retry` retries. A typed 429 would
/// multiply this retry by the engine's own, aiming 4x the requests at a
/// registry already asking for less.
///
/// `Err` is only for a transport failure that outlived its attempts.
pub(crate) async fn send_retrying(
    request: reqwest::RequestBuilder,
    what: &str,
) -> Result<reqwest::Response, Error> {
    // Bodyless requests always clone; a streaming body cannot be replayed.
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

        let cause = match &outcome {
            Ok(resp) if is_transient_status(resp.status()) => resp.status().to_string(),
            Err(e) if is_transient_transport(e) => crate::error::error_chain(e),
            _ => return Ok(outcome?),
        };
        if attempt >= MAX_TRANSIENT_RETRIES {
            return Ok(outcome?);
        }
        // Callers hold a request permit, and for a push an upload slot, while
        // they sign in, so sitting out a registry's wait here would starve
        // every other request to it. The throttle surfaces instead.
        if let Some(asked) = outcome.as_ref().ok().and_then(retry_after) {
            tracing::warn!(
                what,
                retry_after_s = asked.as_secs(),
                "registry asked a sign-in request to wait; not retrying while holding a request slot"
            );
            return Ok(outcome?);
        }
        let delay = backoff(attempt);
        tracing::debug!(
            what,
            attempt,
            delay_ms = delay.as_millis(),
            error = %cause,
            "transient failure; backing off and retrying"
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

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
            is_transient(&Error::from(err)),
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

    /// Through `retrying`, only a 429 is re-sent; other registry statuses
    /// are the caller's answer.
    #[test]
    fn retrying_resends_only_a_throttle_among_statuses() {
        let throttled = Error::RegistryError {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            message: "Rate exceeded".into(),
            retry_after: None,
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
                retry_after: None,
            };
            assert!(!is_transient(&err), "{status} must not be re-sent");
        }
    }

    #[tokio::test]
    async fn retrying_gives_up_and_surfaces_the_last_error() {
        let attempts = std::cell::RefCell::new(Vec::new());
        let result: Result<(), Error> = retrying(
            "always throttled",
            &mut Budgeted(&RetryBudget::new(MAX_TRANSIENT_RETRIES)),
            |attempt| {
                attempts.borrow_mut().push(attempt);
                async {
                    Err(Error::RegistryError {
                        status: http::StatusCode::TOO_MANY_REQUESTS,
                        message: "Rate exceeded".into(),
                        retry_after: None,
                    })
                }
            },
        )
        .await;
        let err = result.expect_err("a permanently throttled op must surface");
        assert_eq!(
            *attempts.borrow(),
            (0..=MAX_TRANSIENT_RETRIES).collect::<Vec<_>>(),
            "one initial attempt plus every retry, numbered from 0"
        );
        assert_eq!(err.status_code(), Some(http::StatusCode::TOO_MANY_REQUESTS));
    }

    /// A retry's log names the failure's causes, not only its top line.
    /// Breaks if why a retry was needed is lost from the log.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_retry_logs_the_cause_of_the_failure() {
        let (_, logs) = crate::test_logs::capture_logs(retrying(
            "a lookup",
            &mut Bounded(|_: &crate::google::Error| true),
            |_| async {
                Err::<(), _>(crate::google::Error::Url {
                    field: "token_url",
                    source: url::ParseError::EmptyHost,
                })
            },
        ))
        .await;
        assert!(logs.contains("empty host"), "{logs}");
    }

    /// Both forms are read, a date against the registry's clock. Breaks if
    /// a sign or garbage is taken as a wait, if a number or a date is
    /// ignored, or if a number too large to hold slips under the cap.
    #[test]
    fn retry_after_reads_seconds_and_dates() {
        let now = crate::aws::calendar::parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 GMT")
            .expect("a valid date");
        let cases = [
            ("7", Some(Duration::from_secs(7))),
            (" 60 ", Some(Duration::from_secs(60))),
            ("0", Some(Duration::ZERO)),
            ("99999999999999999999", Some(Duration::from_secs(121))),
            (
                "Wed, 21 Oct 2015 07:28:30 GMT",
                Some(Duration::from_secs(30)),
            ),
            ("Wed, 21 Oct 2015 07:27:00 GMT", Some(Duration::ZERO)),
            ("+5", None),
            ("-5", None),
            ("soon", None),
            ("", None),
        ];
        for (value, expected) in cases {
            assert_eq!(retry_after_value(value, now), expected, "{value:?}");
        }
    }

    /// A date is measured from the answer's `Date`, not the local clock,
    /// and only a 429's header counts. Breaks if the local clock is used
    /// (a 2015 date reads as zero) or a 503's header is taken.
    #[test]
    fn retry_after_measures_a_date_from_the_answer() {
        let answer = |status: u16| {
            reqwest::Response::from(
                http::Response::builder()
                    .status(status)
                    .header(http::header::DATE, "Wed, 21 Oct 2015 07:28:00 GMT")
                    .header(http::header::RETRY_AFTER, "Wed, 21 Oct 2015 07:28:30 GMT")
                    .body(Vec::<u8>::new())
                    .expect("a valid response"),
            )
        };
        assert_eq!(retry_after(&answer(429)), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(&answer(503)), None);
    }

    /// Breaks if the wait drops the backoff on top of a `Retry-After` (every
    /// throttled request would wake together), ignores the header, moves the
    /// cap's edge, or caps each wait instead of an operation's sum.
    #[test]
    fn delay_adds_the_backoff_up_to_the_cap() {
        let backoff = Duration::from_millis(250);
        let secs = Duration::from_secs;
        // (already waited out, asked, delay, waited out after)
        let cases = [
            (0, None, Some(Duration::from_millis(250)), 0),
            (0, Some(0), Some(Duration::from_millis(250)), 0),
            (0, Some(120), Some(Duration::from_millis(120_250)), 120),
            (0, Some(121), None, 0),
            (60, Some(60), Some(Duration::from_millis(60_250)), 120),
            (60, Some(61), None, 60),
        ];
        for (before, asked, expected, after) in cases {
            let mut honored = secs(before);
            let delay = delay_unless_over_cap(asked.map(secs), backoff, &mut honored);
            assert_eq!(delay, expected, "{before} s then {asked:?}");
            assert_eq!(honored, secs(after), "{before} s then {asked:?}");
        }
    }

    /// The calls of one operation share its `Retry-After` allowance: a
    /// second page asked to wait 70 s after a first page waited 70 s
    /// surfaces. Breaks if the sum is kept per call, which lets a listing
    /// wait 120 s on every page.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_shared_budget_caps_the_total_retry_after() {
        let budget = RetryBudget::new(MAX_LISTING_RETRIES);
        let page = || {
            let calls = Cell::new(0u32);
            let budget = &budget;
            async move {
                let result: Result<(), Error> = retrying("page", &mut Budgeted(budget), |_| {
                    calls.set(calls.get() + 1);
                    let first = calls.get() == 1;
                    async move {
                        if first {
                            Err(Error::RegistryError {
                                status: http::StatusCode::TOO_MANY_REQUESTS,
                                message: "slow down".into(),
                                retry_after: Some(Duration::from_secs(70)),
                            })
                        } else {
                            Ok(())
                        }
                    }
                })
                .await;
                (result.is_ok(), calls.get())
            }
        };
        assert_eq!(page().await, (true, 2), "the first wait is within the cap");
        assert_eq!(page().await, (false, 1), "the second would pass the cap");
    }

    #[tokio::test]
    async fn retrying_does_not_re_run_a_permanent_failure() {
        let calls = Cell::new(0u32);
        let _: Result<(), Error> = retrying(
            "not found",
            &mut Budgeted(&RetryBudget::new(MAX_TRANSIENT_RETRIES)),
            |_| {
                calls.set(calls.get() + 1);
                async {
                    Err(Error::RegistryError {
                        status: http::StatusCode::NOT_FOUND,
                        message: "absent".into(),
                        retry_after: None,
                    })
                }
            },
        )
        .await;
        assert_eq!(calls.get(), 1, "a 404 must be surfaced on the first answer");
    }

    #[tokio::test]
    async fn send_retrying_surfaces_a_throttle_that_names_a_wait() {
        // Catches: a sign-in request (ping, token endpoint, ACR OAuth)
        // sleeping out a registry's wait while its caller holds a request
        // permit, or re-sending into the throttle.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(429).insert_header("Retry-After", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let response = send_retrying(crate::test_http_client().get(server.uri()), "test")
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
