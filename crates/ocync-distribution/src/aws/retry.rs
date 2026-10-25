//! The AWS SDK's standard and adaptive retry behavior, ported from
//! `aws-smithy-runtime` 1.16.1 and configured by the same settings:
//! `AWS_MAX_ATTEMPTS`, `AWS_RETRY_MODE` and `AWS_NEW_RETRIES_2026`.
//!
//! A retry spends from a quota shared by every call to one service in one
//! region, as the SDK's retry partition is, so a throttled service draws
//! fewer retries from the whole process rather than from each call.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::endpoint::Region;
use super::{ConfigError, Service};
use crate::retry::Policy;

/// The quota a partition starts with and never exceeds.
const QUOTA_CAPACITY: u32 = 500;

/// What a success returns to the quota when its call holds no retry permit.
const PERMIT_REGENERATION_AMOUNT: u32 = 1;

/// What a non-transient retry spends before retry behavior 2.1.
const LEGACY_RETRY_COST: u32 = 5;
/// What a transient retry spends before retry behavior 2.1.
const LEGACY_RETRY_TIMEOUT_COST: u32 = LEGACY_RETRY_COST * 2;
/// What a throttled retry spends under retry behavior 2.1.
const THROTTLING_RETRY_COST: u32 = 5;
/// What a server-error retry spends under retry behavior 2.1.
const DEFAULT_RETRY_COST: u32 = 14;
/// What a transient retry spends under retry behavior 2.1.
const DEFAULT_RETRY_TIMEOUT_COST: u32 = 14;

/// The attempts a call makes by default, the first included.
pub(super) const DEFAULT_MAX_ATTEMPTS: NonZeroU32 = NonZeroU32::new(3).expect("3 is not 0");

/// The first backoff, before jitter, and before retry behavior 2.1 the
/// first backoff of every kind.
pub(super) const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// The longest backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(20);

/// The first backoff of a non-throttling retry under retry behavior 2.1.
const V2_1_NON_THROTTLING_INITIAL_BACKOFF: Duration = Duration::from_millis(50);

/// Under retry behavior 2.1, a server-sent `x-amz-retry-after` may lengthen
/// the computed backoff by at most this.
const MAX_RETRY_AFTER_EXTENSION: Duration = Duration::from_secs(5);

/// How a failure is retried, as the SDK classifies it. The kind sets the
/// quota cost and, under retry behavior 2.1, the first backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A throttle error code, or an HTTP 429.
    Throttling,
    /// A connection, timeout or I/O failure, a timeout error code, or HTTP
    /// 500, 502, 503 or 504.
    Transient,
    /// A signature rejected for clock skew.
    Server,
}

/// What an answer tells the adaptive limiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// A throttle: the rate drops.
    Throttle,
    /// A success or any other failure: the rate grows back.
    Other,
}

impl Answer {
    fn of(kind: Option<Kind>) -> Self {
        match kind {
            Some(Kind::Throttling) => Self::Throttle,
            Some(Kind::Transient | Kind::Server) | None => Self::Other,
        }
    }
}

/// The SDK's retry behavior version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Version {
    /// The default.
    V2_0,
    /// Selected by `AWS_NEW_RETRIES_2026=true`.
    V2_1,
}

impl Version {
    /// What a retry of `kind` spends from the quota.
    fn cost(self, kind: Kind) -> u32 {
        match (self, kind) {
            (Self::V2_0, Kind::Throttling | Kind::Server) => LEGACY_RETRY_COST,
            (Self::V2_0, Kind::Transient) => LEGACY_RETRY_TIMEOUT_COST,
            (Self::V2_1, Kind::Throttling) => THROTTLING_RETRY_COST,
            (Self::V2_1, Kind::Server) => DEFAULT_RETRY_COST,
            (Self::V2_1, Kind::Transient) => DEFAULT_RETRY_TIMEOUT_COST,
        }
    }
}

/// Whether sends are paced by the adaptive rate limiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Not paced.
    Standard,
    /// Paced by the adaptive rate limiter.
    Adaptive,
}

impl std::str::FromStr for Mode {
    type Err = ConfigError;

    /// `standard` or `adaptive`, any case, surrounding space ignored.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mode = text.trim();
        if mode.eq_ignore_ascii_case("standard") {
            Ok(Self::Standard)
        } else if mode.eq_ignore_ascii_case("adaptive") {
            Ok(Self::Adaptive)
        } else {
            Err(ConfigError::InvalidRetryMode(text.to_owned()))
        }
    }
}

/// How an adaptive client paces its retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pacing {
    /// Before retry behavior 2.1: the limiter's wait is the backoff.
    Backoff,
    /// Under 2.1: each retry takes a send token after its backoff.
    SendToken,
}

/// The settings a [`Strategy`] is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Settings {
    /// The attempts a call makes, the first included.
    pub(super) max_attempts: NonZeroU32,
    /// Whether sends are paced.
    pub(super) mode: Mode,
    /// The retry behavior version.
    pub(super) version: Version,
    /// The first backoff before jitter.
    pub(super) initial_backoff: Duration,
    /// The longest backoff.
    pub(super) max_backoff: Duration,
}

/// The SDK's standard retry defaults.
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            mode: Mode::Standard,
            version: Version::V2_0,
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
        }
    }
}

/// The retry strategy of one client, as the SDK's standard strategy is.
#[derive(Debug)]
pub(super) struct Strategy {
    max_attempts: u32,
    initial_backoff: Duration,
    non_throttling_initial_backoff: Duration,
    max_backoff: Duration,
    version: Version,
    mode: Mode,
    /// The jitter factor in `[0, 1)` each backoff is scaled by.
    jitter: fn() -> f64,
}

impl Strategy {
    /// The strategy `settings` set, jittered as the SDK's is.
    pub(super) fn new(settings: &Settings) -> Self {
        Self {
            jitter: fastrand::f64,
            max_attempts: settings.max_attempts.get(),
            initial_backoff: settings.initial_backoff,
            non_throttling_initial_backoff: match settings.version {
                Version::V2_0 => settings.initial_backoff,
                Version::V2_1 => V2_1_NON_THROTTLING_INITIAL_BACKOFF,
            },
            max_backoff: settings.max_backoff,
            version: settings.version,
            mode: settings.mode,
        }
    }

    /// The attempts a call makes, the first included.
    #[cfg(test)]
    pub(super) fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// How retries are paced, or `None` for a standard client.
    fn pacing(&self) -> Option<Pacing> {
        match (self.mode, self.version) {
            (Mode::Standard, _) => None,
            (Mode::Adaptive, Version::V2_0) => Some(Pacing::Backoff),
            (Mode::Adaptive, Version::V2_1) => Some(Pacing::SendToken),
        }
    }
}

/// The retry state every call to one service in one region shares.
#[derive(Debug)]
pub(super) struct Partition {
    /// What remains of the quota.
    available: AtomicU32,
    /// The most the quota holds.
    capacity: u32,
    /// The adaptive rate limiter, made on first use.
    limiter: Mutex<Option<Limiter>>,
}

impl Default for Partition {
    fn default() -> Self {
        Self::with_capacity(QUOTA_CAPACITY)
    }
}

/// Partitions by service and region.
type Partitions = HashMap<(Service, Region), Arc<Partition>>;

/// Every partition in the process. Synchronized, not `Rc<RefCell<_>>`,
/// because the AWS client is a `Send + Sync` leaf like `RegistryClient`:
/// the ECR auth provider holding it must be. No lock is held across an
/// `.await`.
static PARTITIONS: LazyLock<Mutex<Partitions>> = LazyLock::new(Mutex::default);

/// The partition of `service` in `region`, shared by every client for it.
pub(super) fn partition(service: Service, region: &Region) -> Arc<Partition> {
    let mut partitions = PARTITIONS.lock().expect("no panic while held");
    Arc::clone(partitions.entry((service, region.clone())).or_default())
}

impl Partition {
    /// A partition whose quota holds `capacity`.
    pub(super) fn with_capacity(capacity: u32) -> Self {
        Self {
            available: AtomicU32::new(capacity),
            capacity,
            limiter: Mutex::default(),
        }
    }

    /// Spend `cost` from the quota, or `false` when too little is left.
    fn spend(&self, cost: u32) -> bool {
        self.available
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(cost)
            })
            .is_ok()
    }

    /// Return `amount` to the quota, up to its capacity.
    fn refund(&self, amount: u32) {
        let _ = self
            .available
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                Some(left.saturating_add(amount).min(self.capacity))
            });
    }

    /// Run `f` on the adaptive limiter at `now`, making it on first use.
    fn with_limiter<T>(&self, now: f64, f: impl FnOnce(&mut Limiter) -> T) -> T {
        let mut limiter = self.limiter.lock().expect("no panic while held");
        f(limiter.get_or_insert_with(|| Limiter::new(now)))
    }

    /// Wait until an adaptive client's limiter admits attempt `attempt`, 0
    /// for the first, as the SDK's send token does: after each wait it asks
    /// again, so calls that waited together do not all send at once and the
    /// bucket never goes negative. Before retry behavior 2.1 a retry is paced
    /// through its backoff instead.
    pub(super) async fn send_token(&self, strategy: &Strategy, attempt: u32) {
        let paced = match strategy.pacing() {
            None => false,
            Some(Pacing::SendToken) => true,
            Some(Pacing::Backoff) => attempt == 0,
        };
        if !paced {
            return;
        }
        loop {
            let now = now();
            match self.with_limiter(now, |l| l.acquire(now, Paced::Send)) {
                Ok(()) => return,
                Err(delay) => tokio::time::sleep(delay).await,
            }
        }
    }

    /// Record an attempt's outcome with an adaptive client's limiter.
    fn observe(&self, mode: Mode, answer: Answer) {
        if mode == Mode::Adaptive {
            let now = now();
            self.with_limiter(now, |l| l.update(now, answer));
        }
    }
}

/// Seconds since the UNIX epoch, the adaptive limiter's clock.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// The retry policy of one call.
pub(super) struct CallPolicy<'a> {
    strategy: &'a Strategy,
    partition: &'a Partition,
    /// What this call's latest retry spent, returned to the quota if the
    /// call succeeds. A newer retry replaces it and leaves the older cost
    /// spent. The SDK keeps one permit per client, so concurrent calls
    /// overwrite each other's and the overwritten costs never return; kept
    /// per call, each success returns exactly what its own retry spent.
    permit: Option<u32>,
}

impl<'a> CallPolicy<'a> {
    /// A call on a client with `strategy` that spends from `partition`.
    pub(super) fn new(strategy: &'a Strategy, partition: &'a Partition) -> Self {
        Self {
            strategy,
            partition,
            permit: None,
        }
    }

    /// The wait before retry `retry` (1 for the first) of a `kind` failure,
    /// as the SDK computes it.
    fn backoff(&self, retry: u32, kind: Kind, retry_after: Option<Duration>) -> Duration {
        let s = self.strategy;
        if s.pacing() == Some(Pacing::Backoff) {
            // The SDK charges a throttled retry at its timeout cost.
            let paced = match kind {
                Kind::Throttling => Paced::ThrottledRetry,
                Kind::Transient | Kind::Server => Paced::Retry,
            };
            let now = now();
            if let Err(delay) = self.partition.with_limiter(now, |l| l.acquire(now, paced)) {
                return delay.min(s.max_backoff);
            }
        }
        let initial = match kind {
            Kind::Throttling => s.initial_backoff,
            Kind::Transient | Kind::Server => s.non_throttling_initial_backoff,
        };
        let computed = exponential_backoff((s.jitter)(), initial, retry - 1, s.max_backoff);
        match (retry_after, s.version) {
            (None, _) => computed,
            (Some(after), Version::V2_0) => after.min(s.max_backoff),
            (Some(after), Version::V2_1) => {
                after.clamp(computed, computed + MAX_RETRY_AFTER_EXTENSION)
            }
        }
    }
}

/// A failure the SDK's retry policy classifies.
pub(super) trait Retryable {
    /// How the SDK retries this failure, or `None` when it does not.
    fn retry_kind(&self) -> Option<Kind>;

    /// The wait the service asked for before a retry.
    fn retry_after(&self) -> Option<Duration> {
        None
    }
}

impl<E: Retryable> Policy<E> for CallPolicy<'_> {
    fn retry(&mut self, attempt: u32, error: &E) -> Option<Duration> {
        let kind = error.retry_kind();
        self.partition.observe(self.strategy.mode, Answer::of(kind));
        let kind = kind?;
        let attempts = attempt + 1;
        if attempts >= self.strategy.max_attempts {
            return None;
        }
        let backoff = self.backoff(attempts, kind, error.retry_after());
        let cost = self.strategy.version.cost(kind);
        if !self.partition.spend(cost) {
            tracing::debug!(attempts, ?kind, "AWS retry quota exhausted; not retrying");
            return None;
        }
        self.permit = Some(cost);
        Some(backoff)
    }

    /// The call succeeded: the limiter grows back, and the quota gets back
    /// the call's permit, or 1 when it holds none.
    fn succeeded(&mut self) {
        self.partition.observe(self.strategy.mode, Answer::Other);
        self.partition
            .refund(self.permit.take().unwrap_or(PERMIT_REGENERATION_AMOUNT));
    }
}

/// `jitter` times `initial` doubled `doublings` times, capped at `max`.
fn exponential_backoff(jitter: f64, initial: Duration, doublings: u32, max: Duration) -> Duration {
    2u32.checked_pow(doublings)
        .and_then(|factor| {
            Duration::try_from_secs_f64(f64::from(factor) * initial.as_secs_f64()).ok()
        })
        .map_or(max, |backoff| backoff.min(max))
        .mul_f64(jitter)
}

/// A send the adaptive limiter paces, by what it costs.
#[derive(Debug, Clone, Copy)]
enum Paced {
    /// A send token: the first attempt, or under retry behavior 2.1 any
    /// attempt.
    Send,
    /// A retry before 2.1, paced through its backoff.
    Retry,
    /// A throttled retry before 2.1, paced through its backoff.
    ThrottledRetry,
}

impl Paced {
    fn cost(self) -> f64 {
        match self {
            Self::Send => LIMITER_INITIAL_REQUEST_COST,
            Self::Retry => LIMITER_RETRY_COST,
            Self::ThrottledRetry => LIMITER_RETRY_TIMEOUT_COST,
        }
    }
}

/// The limiter tokens a send token takes.
const LIMITER_INITIAL_REQUEST_COST: f64 = 1.0;
/// The limiter tokens a retry paced through its backoff takes.
const LIMITER_RETRY_COST: f64 = 5.0;
/// The limiter tokens a throttled retry paced through its backoff takes.
const LIMITER_RETRY_TIMEOUT_COST: f64 = LIMITER_RETRY_COST * 2.0;

/// The lowest send rate the limiter allows, per second.
const MIN_FILL_RATE: f64 = 0.5;
/// The smallest capacity the limiter keeps.
const MIN_CAPACITY: f64 = 1.0;
/// The weight of the newest send rate measurement.
const SMOOTH: f64 = 0.8;
/// How far the rate drops on a throttle.
const BETA: f64 = 0.7;
/// How fast the rate grows back after a throttle.
const SCALE_CONSTANT: f64 = 0.4;

/// The SDK's adaptive rate limiter: a token bucket whose fill rate follows
/// CUBIC congestion control, off until the first throttle. Times are
/// seconds since the UNIX epoch.
#[derive(Debug)]
struct Limiter {
    fill_rate: f64,
    max_capacity: f64,
    current_capacity: f64,
    last_timestamp: Option<f64>,
    enabled: bool,
    measured_tx_rate: f64,
    last_tx_rate_bucket: f64,
    request_count: u64,
    last_max_rate: f64,
    time_of_last_throttle: f64,
}

impl Limiter {
    fn new(now: f64) -> Self {
        Self {
            measured_tx_rate: MIN_FILL_RATE,
            last_tx_rate_bucket: now.floor(),
            time_of_last_throttle: now,
            ..Self::idle()
        }
    }

    /// A limiter with every measurement at zero.
    fn idle() -> Self {
        Self {
            fill_rate: 0.0,
            max_capacity: f64::MAX,
            current_capacity: 0.0,
            last_timestamp: None,
            enabled: false,
            measured_tx_rate: 0.0,
            last_tx_rate_bucket: 0.0,
            request_count: 0,
            last_max_rate: 0.0,
            time_of_last_throttle: 0.0,
        }
    }

    /// Take the tokens `paced` costs, or the wait until there are enough.
    fn acquire(&mut self, now: f64, paced: Paced) -> Result<(), Duration> {
        if !self.enabled {
            return Ok(());
        }
        let cost = paced.cost();
        self.refill(now);
        if cost <= self.current_capacity {
            self.current_capacity -= cost;
            return Ok(());
        }
        let wait = Duration::from_secs_f64((cost - self.current_capacity) / self.fill_rate);
        if wait.is_zero() {
            // A shortfall below a nanosecond would never be waited out.
            self.current_capacity = (self.current_capacity - cost).max(0.0);
            Ok(())
        } else {
            Err(wait)
        }
    }

    /// Adjust the fill rate after an answer.
    fn update(&mut self, now: f64, answer: Answer) {
        self.measure_tx_rate(now);
        let rate = if answer == Answer::Throttle {
            let rate_to_use = if self.enabled {
                self.measured_tx_rate.min(self.fill_rate)
            } else {
                self.measured_tx_rate
            };
            self.last_max_rate = rate_to_use;
            self.time_of_last_throttle = now;
            self.enabled = true;
            rate_to_use * BETA
        } else {
            self.cubic_success(now)
        };
        let rate = rate.min(2.0 * self.measured_tx_rate);
        self.refill(now);
        self.fill_rate = rate.max(MIN_FILL_RATE);
        self.max_capacity = rate.max(MIN_CAPACITY);
        self.current_capacity = self.current_capacity.min(self.max_capacity);
    }

    fn refill(&mut self, now: f64) {
        if let Some(last) = self.last_timestamp {
            self.current_capacity =
                (self.current_capacity + (now - last) * self.fill_rate).min(self.max_capacity);
        }
        self.last_timestamp = Some(now);
    }

    /// Fold the sends of each half second into a smoothed rate.
    fn measure_tx_rate(&mut self, now: f64) {
        let bucket = (now * 2.0).floor() / 2.0;
        self.request_count += 1;
        if bucket > self.last_tx_rate_bucket {
            let rate = self.request_count as f64 / (bucket - self.last_tx_rate_bucket);
            self.measured_tx_rate = rate * SMOOTH + self.measured_tx_rate * (1.0 - SMOOTH);
            self.request_count = 0;
            self.last_tx_rate_bucket = bucket;
        }
    }

    fn cubic_success(&self, now: f64) -> f64 {
        let window = (self.last_max_rate * (1.0 - BETA) / SCALE_CONSTANT).cbrt();
        let dt = now - self.time_of_last_throttle - window;
        SCALE_CONSTANT * dt.powi(3) + self.last_max_rate
    }
}

#[cfg(test)]
mod tests {
    use http::StatusCode;

    use super::super::{ClockSkew, CodeKind, ErrorCode, Failure, ServiceError};
    use super::*;

    /// A failure the SDK classifies as `kind`, with the server's
    /// `x-amz-retry-after`.
    fn failure(kind: Kind, retry_after: Option<Duration>) -> Failure {
        let error = |status, code: Option<(&str, CodeKind)>| ServiceError {
            status,
            code: code.map(|(text, kind)| ErrorCode {
                text: text.to_owned(),
                kind,
            }),
            message: None,
            retry_after,
        };
        let failure = match kind {
            Kind::Throttling => Failure::Service(error(
                StatusCode::BAD_REQUEST,
                Some(("ThrottlingException", CodeKind::Throttling)),
            )),
            Kind::Transient => Failure::Service(error(StatusCode::SERVICE_UNAVAILABLE, None)),
            Kind::Server => Failure::ClockSkewed {
                error: error(
                    StatusCode::FORBIDDEN,
                    Some(("InvalidSignatureException", CodeKind::ClockSkew)),
                ),
                skew: ClockSkew(600_000),
            },
        };
        assert_eq!(failure.retry_kind(), Some(kind));
        failure
    }

    fn policy<'a>(strategy: &'a Strategy, partition: &'a Partition) -> CallPolicy<'a> {
        CallPolicy::new(strategy, partition)
    }

    /// The SDK's tests run with the jitter factor fixed at 1.
    fn fixed(settings: &Settings) -> Strategy {
        Strategy {
            jitter: || 1.0,
            ..Strategy::new(settings)
        }
    }

    fn attempts(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("a test's attempt count is not 0")
    }

    fn standard() -> Settings {
        Settings::default()
    }

    fn v2_1() -> Settings {
        Settings {
            version: Version::V2_1,
            ..standard()
        }
    }

    fn adaptive() -> Settings {
        Settings {
            mode: Mode::Adaptive,
            ..standard()
        }
    }

    fn v2_1_adaptive() -> Settings {
        Settings {
            version: Version::V2_1,
            ..adaptive()
        }
    }

    fn secs(s: u64) -> Option<Duration> {
        Some(Duration::from_secs(s))
    }

    fn millis(ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(ms))
    }

    fn left(partition: &Partition) -> u32 {
        partition.available.load(Ordering::Relaxed)
    }

    /// Run one call's failed attempts: each failure must draw the paired
    /// delay (or none) and leave the paired quota.
    fn run<'a>(
        strategy: &'a Strategy,
        partition: &'a Partition,
        failures: &[(Failure, Option<Duration>, u32)],
    ) -> CallPolicy<'a> {
        let mut call = policy(strategy, partition);
        for (attempt, (failure, delay, quota)) in (0..).zip(failures) {
            assert_eq!(call.retry(attempt, failure), *delay, "attempt {attempt}");
            assert_eq!(left(partition), *quota, "attempt {attempt}");
        }
        call
    }

    /// `backoff_timing` and `max_backoff_time`: retry behavior 2.0 doubles
    /// from 1 second up to the max backoff, each retry spending 5, and stops
    /// at the attempt limit with the quota untouched.
    #[test]
    fn backs_off_exponentially_to_the_max() {
        for (max_backoff, delays) in [(20, [1, 2, 4, 8]), (3, [1, 2, 3, 3])] {
            let strategy = fixed(&Settings {
                max_attempts: attempts(5),
                max_backoff: Duration::from_secs(max_backoff),
                ..standard()
            });
            let partition = Partition::default();
            let mut failures: Vec<_> = (0..4u32)
                .map(|i| {
                    (
                        failure(Kind::Server, None),
                        secs(delays[i as usize]),
                        495 - 5 * i,
                    )
                })
                .collect();
            failures.push((failure(Kind::Server, None), None, 480));
            run(&strategy, &partition, &failures);
        }
    }

    /// `no_quota`, `quota_replenishes_on_success`: a retry needs its cost in
    /// the quota, and a success after retries returns only the latest
    /// retry's cost.
    #[test]
    fn spends_and_returns_the_quota() {
        let strategy = fixed(&Settings {
            max_attempts: attempts(5),
            ..standard()
        });
        let partition = Partition::with_capacity(5);
        run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Server, None), secs(1), 0),
                (failure(Kind::Server, None), None, 0),
            ],
        );

        let partition = Partition::with_capacity(100);
        let mut call = run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Transient, None), secs(1), 90),
                (failure(Kind::Transient, secs(1)), secs(1), 80),
            ],
        );
        Policy::<Failure>::succeeded(&mut call);
        assert_eq!(left(&partition), 90);
    }

    /// `quota_replenishes_on_first_try_success`: a first-try success returns
    /// one, up to the capacity and no further.
    #[test]
    fn a_first_try_success_refills_the_quota() {
        let strategy = fixed(&Settings {
            max_attempts: NonZeroU32::MAX,
            ..standard()
        });
        let partition = Partition::with_capacity(20);
        run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Transient, None), secs(1), 10),
                (failure(Kind::Transient, None), secs(2), 0),
            ],
        );
        let other_client = fixed(&standard());
        Policy::<Failure>::succeeded(&mut policy(&other_client, &partition));
        assert_eq!(left(&partition), 1);
        for _ in 0..20 {
            Policy::<Failure>::succeeded(&mut policy(&other_client, &partition));
        }
        assert_eq!(left(&partition), 20);
    }

    /// Each call holds its own retry permit. A call that runs out of
    /// attempts leaves its cost spent, and another call's success does not
    /// return it; two calls retrying at once each return their own cost.
    /// Breaks if the permit is shared per client, as the SDK's is, where the
    /// newer retry overwrites the older call's permit.
    #[test]
    fn holds_one_retry_permit_per_call() {
        let strategy = fixed(&Settings {
            max_attempts: attempts(2),
            ..standard()
        });
        let partition = Partition::default();
        run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Transient, None), secs(1), 490),
                (failure(Kind::Transient, None), None, 490),
            ],
        );
        Policy::<Failure>::succeeded(&mut policy(&strategy, &partition));
        assert_eq!(
            left(&partition),
            491,
            "a call holding no permit regenerates one, not the failed call's"
        );

        let mut older = run(
            &strategy,
            &partition,
            &[(failure(Kind::Transient, None), secs(1), 481)],
        );
        let mut newer = run(
            &strategy,
            &partition,
            &[(failure(Kind::Server, None), secs(1), 476)],
        );
        Policy::<Failure>::succeeded(&mut older);
        assert_eq!(left(&partition), 486, "the older call returns its 10");
        Policy::<Failure>::succeeded(&mut newer);
        assert_eq!(left(&partition), 491, "the newer call returns its 5");
    }

    /// `retry_eventually_succeeds`, `fail_due_to_max_attempts_reached`,
    /// `verify_max_backoff_time` and the token bucket cases: retry behavior
    /// 2.1 starts a non-throttle at 50 ms and spends 14 for it.
    #[test]
    fn retry_behavior_2_1_backs_off_and_spends_less_for_a_throttle() {
        let strategy = fixed(&v2_1());
        let partition = Partition::default();
        let mut call = run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Server, None), millis(50), 486),
                (failure(Kind::Server, None), millis(100), 472),
            ],
        );
        Policy::<Failure>::succeeded(&mut call);
        assert_eq!(left(&partition), 486);

        let partition = Partition::default();
        run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Server, None), millis(50), 486),
                (failure(Kind::Server, None), millis(100), 472),
                (failure(Kind::Server, None), None, 472),
            ],
        );

        let strategy = fixed(&Settings {
            max_attempts: attempts(5),
            max_backoff: Duration::from_millis(200),
            ..v2_1()
        });
        let partition = Partition::default();
        run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Server, None), millis(50), 486),
                (failure(Kind::Server, None), millis(100), 472),
                (failure(Kind::Server, None), millis(200), 458),
                (failure(Kind::Server, None), millis(200), 444),
                (failure(Kind::Server, None), None, 444),
            ],
        );

        let strategy = fixed(&v2_1());
        let partition = Partition::default();
        let mut call = run(
            &strategy,
            &partition,
            &[(failure(Kind::Throttling, None), secs(1), 495)],
        );
        Policy::<Failure>::succeeded(&mut call);
        assert_eq!(left(&partition), 500);

        let partition = Partition::with_capacity(20);
        run(
            &strategy,
            &partition,
            &[
                (failure(Kind::Server, None), millis(50), 6),
                (failure(Kind::Server, None), None, 6),
            ],
        );
    }

    /// The `x-amz-retry-after` cases: 2.0 takes it up to the max backoff,
    /// 2.1 keeps it between the computed backoff and 5 seconds past it.
    #[test]
    fn honors_retry_after() {
        for (settings, after, delay) in [
            (standard(), secs(30), secs(20)),
            (v2_1(), secs(3), secs(3)),
            (v2_1(), millis(10), millis(50)),
            (v2_1(), secs(10), millis(5050)),
        ] {
            let strategy = fixed(&settings);
            let partition = Partition::default();
            let mut call = policy(&strategy, &partition);
            assert_eq!(
                call.retry(0, &failure(Kind::Server, after)),
                delay,
                "{after:?}"
            );
        }
    }

    /// Retry behavior 2.0 starts a non-throttle at the 1 second initial
    /// backoff; 2.1 starts it at 50 ms and keeps 1 second for throttles.
    /// The values are `RetryConfig::standard()` and `RetrySpec` in
    /// `aws-smithy-types` 1.9.0.
    #[test]
    fn reads_the_retry_version() {
        let v2_0 = fixed(&standard());
        assert_eq!(v2_0.version, Version::V2_0);
        assert_eq!(v2_0.non_throttling_initial_backoff, Duration::from_secs(1));
        let v2_1 = fixed(&v2_1());
        assert_eq!(v2_1.version, Version::V2_1);
        assert_eq!(
            v2_1.non_throttling_initial_backoff,
            Duration::from_millis(50)
        );
        assert_eq!(v2_1.initial_backoff, Duration::from_secs(1));
        let adaptive = fixed(&adaptive());
        assert_eq!(adaptive.mode, Mode::Adaptive);
        assert_eq!(fixed(&standard()).mode, Mode::Standard);
    }

    /// The `calculate_exponential_backoff` cases.
    #[test]
    fn computes_the_exponential_backoff() {
        let max = Duration::from_secs(20);
        for (initial, expected) in [
            (1.0, [1.0, 2.0, 4.0]),
            (3.0, [3.0, 6.0, 12.0]),
            (0.03, [0.03, 0.06, 0.12]),
        ] {
            for (doublings, expected) in (0..).zip(expected) {
                assert_eq!(
                    exponential_backoff(1.0, Duration::from_secs_f64(initial), doublings, max),
                    Duration::from_secs_f64(expected)
                );
            }
        }
        assert_eq!(
            exponential_backoff(1.0, Duration::from_secs(10), 100_000, max),
            max
        );
        assert_eq!(
            exponential_backoff(0.5, Duration::from_secs(1), 1, max),
            Duration::from_secs(1)
        );
    }

    /// Every client for a service in a region shares one partition, so a
    /// watch cycle's new clients keep the quota the last ones spent.
    #[test]
    fn shares_a_partition_per_service_and_region() {
        let region = |text| Region::parse(text).unwrap();
        let a = partition(Service::Ecr, &region("eu-west-3"));
        assert!(Arc::ptr_eq(
            &a,
            &partition(Service::Ecr, &region("eu-west-3"))
        ));
        assert!(!Arc::ptr_eq(
            &a,
            &partition(Service::Ecr, &region("eu-west-2"))
        ));
        assert!(!Arc::ptr_eq(
            &a,
            &partition(Service::EcrPublic, &region("eu-west-3"))
        ));
    }

    /// A partition whose turned-on limiter holds `capacity` and refills at
    /// `fill_rate` tokens a second.
    fn limited(capacity: f64, fill_rate: f64) -> Partition {
        let partition = Partition::default();
        partition.with_limiter(now(), |l| {
            l.enabled = true;
            l.current_capacity = capacity;
            l.fill_rate = fill_rate;
            l.last_timestamp = Some(now());
        });
        partition
    }

    /// The tokens `partition`'s limiter holds now.
    fn capacity(partition: &Partition) -> f64 {
        let now = now();
        partition.with_limiter(now, |l| {
            l.refill(now);
            l.current_capacity
        })
    }

    /// An adaptive client takes a send token for its first attempt, and for
    /// each retry only under retry behavior 2.1, as the SDK does. A standard
    /// client never takes one.
    #[tokio::test]
    async fn takes_a_send_token_as_the_sdk_does() {
        let adaptive = adaptive();
        let adaptive_2_1 = v2_1_adaptive();
        for (settings, attempt, takes) in [
            (&adaptive, 0, true),
            (&adaptive, 1, false),
            (&adaptive_2_1, 0, true),
            (&adaptive_2_1, 1, true),
            (&standard(), 0, false),
        ] {
            let partition = limited(5.0, 0.0);
            partition.send_token(&fixed(settings), attempt).await;
            let expected = if takes { 4.0 } else { 5.0 };
            assert_eq!(
                capacity(&partition),
                expected,
                "{settings:?} attempt {attempt}"
            );
        }
    }

    /// Before retry behavior 2.1 an empty limiter's wait is the backoff;
    /// under 2.1 the backoff is the computed one, and the limiter is left to
    /// the send token.
    #[test]
    fn only_a_pre_2_1_backoff_waits_on_the_limiter() {
        let adaptive = fixed(&adaptive());
        let adaptive_2_1 = fixed(&v2_1_adaptive());
        for (strategy, waits) in [(&adaptive, true), (&adaptive_2_1, false)] {
            let partition = limited(0.0, 1.0);
            let backoff = policy(strategy, &partition).backoff(1, Kind::Throttling, None);
            let expected = if waits {
                Duration::from_secs(10)
            } else {
                Duration::from_secs(1)
            };
            assert!(
                backoff.abs_diff(expected) < Duration::from_millis(50),
                "{backoff:?}"
            );
        }
    }

    /// A send that had to wait takes its token after the wait, rather than
    /// sending on the strength of the wait alone.
    #[tokio::test]
    async fn takes_the_token_after_waiting_for_it() {
        let partition = limited(0.0, 10.0);
        partition.send_token(&fixed(&adaptive()), 0).await;
        let left = capacity(&partition);
        assert!((0.0..0.5).contains(&left), "{left}");
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
            "{actual} != {expected}"
        );
    }

    /// `should_match_beta_decrease`: a throttle cuts the rate to 70%.
    #[test]
    fn a_throttle_cuts_the_rate_by_beta() {
        let limiter = Limiter {
            last_max_rate: 10.0,
            time_of_last_throttle: 1.0,
            ..Limiter::idle()
        };
        assert_close(limiter.cubic_success(1.0), 7.0);
    }

    /// `test_client_sending_rates`, from the Go SDK: the measured send rate
    /// and the fill rate over sends every 200 ms with two throttles.
    #[test]
    fn follows_the_sdk_send_rate_vectors() {
        use Answer::{Other as O, Throttle as T};
        let mut limiter = Limiter::idle();
        let vectors = [
            (O, 0.2, 0.0, 0.5),
            (O, 0.4, 0.0, 0.5),
            (O, 0.6, 4.800000000000001, 0.5),
            (O, 0.8, 4.800000000000001, 0.5),
            (O, 1.0, 4.16, 0.5),
            (O, 1.2, 4.16, 0.6912),
            (O, 1.4, 4.16, 1.0975999999999997),
            (O, 1.6, 5.632000000000001, 1.6384000000000005),
            (O, 1.8, 5.632000000000001, 2.3328),
            (T, 2.0, 4.3264, 3.0284799999999996),
            (O, 2.2, 4.3264, 3.48663917347026),
            (O, 2.4, 4.3264, 3.821874416040255),
            (O, 2.6, 5.66528, 4.053385727709987),
            (O, 2.8, 5.66528, 4.200373108479454),
            (O, 3.0, 4.333056, 4.282036558348658),
            (T, 3.2, 4.333056, 2.99742559084406),
            (O, 3.4, 4.333056, 3.4522263943863463),
        ];
        for (answer, now, tx_rate, fill_rate) in vectors {
            limiter.update(now, answer);
            assert_close(limiter.measured_tx_rate, tx_rate);
            assert_close(limiter.fill_rate, fill_rate);
        }
    }

    /// The rate never grows past twice the measured send rate, however long
    /// since the last throttle.
    #[test]
    fn caps_the_rate_at_twice_the_send_rate() {
        let mut limiter = Limiter {
            measured_tx_rate: 1.0,
            last_max_rate: 10.0,
            ..Limiter::idle()
        };
        limiter.update(100.0, Answer::Other);
        assert_close(limiter.measured_tx_rate, 0.208);
        assert_close(limiter.fill_rate, MIN_FILL_RATE);
        assert_close(limiter.max_capacity, MIN_CAPACITY);
    }

    /// `test_capacity_never_goes_negative`: a send short of tokens waits and
    /// leaves the capacity as it was.
    #[test]
    fn a_short_send_waits_without_going_negative() {
        let mut limiter = Limiter::idle();
        limiter.update(0.0, Answer::Throttle);
        let wait = limiter.acquire(0.1, Paced::Send).unwrap_err();
        assert!((limiter.current_capacity - 0.05).abs() < 0.01);
        assert!((wait.as_secs_f64() - 1.9).abs() < 0.01);
    }
}
