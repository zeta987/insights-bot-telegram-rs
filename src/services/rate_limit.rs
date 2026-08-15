use std::{
    num::NonZeroU32,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use governor::{
    Quota, RateLimiter, clock::DefaultClock, middleware::NoOpMiddleware,
    state::keyed::DefaultKeyedStateStore,
};
use nonzero_ext::nonzero;
use tokio::time::Instant;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RateKey(pub i64, pub &'static str);

#[derive(Clone)]
pub struct CommandRateLimiter {
    limiter:
        Arc<RateLimiter<RateKey, DefaultKeyedStateStore<RateKey>, DefaultClock, NoOpMiddleware>>,
}

impl CommandRateLimiter {
    pub fn new(ops_per_window: u32, window: Duration) -> Self {
        let quota = Quota::with_period(window)
            .unwrap()
            .allow_burst(NonZeroU32::new(ops_per_window).unwrap_or(nonzero!(1u32)));
        Self {
            limiter: Arc::new(RateLimiter::keyed(quota)),
        }
    }

    pub fn check(&self, key: RateKey) -> Result<()> {
        self.limiter
            .check_key(&key)
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Go's `go.uber.org/ratelimit` limiter
// ---------------------------------------------------------------------------

/// Go's default slack, from `buildConfig` in `ratelimit.go`.
pub const GO_RATE_LIMITER_DEFAULT_SLACK: u32 = 10;

/// Go's `go.uber.org/ratelimit@v0.3.1` `atomicInt64Limiter`, ported literally.
///
/// This is deliberately *not* [`CommandRateLimiter`]: that one is keyed per
/// chat, non-blocking, and models Telegram's per-command quota, whereas Go's
/// OpenAI limiter is unkeyed, blocking, and shared by every caller of one
/// `OpenAIClient`. Go builds it as `ratelimit.New(1)`, so `per` is one second,
/// `rate` is one, and `slack` is the package default of ten.
///
/// The algorithm, from `limiter_atomic_int64.go`:
///
/// ```text
/// now  = clock.Now()
/// next = state == 0                             -> now
///        maxSlack > 0 && now-state > maxSlack+perRequest -> now - maxSlack
///        otherwise                              -> state + perRequest
/// sleep(next - now) when positive
/// ```
///
/// `state` is "the unix nanoseconds of the next permissions issue", and its
/// zero value doubles as "never taken", which is [`Option::None`] here.
///
/// The clock is [`tokio::time::Instant`], so a test that runs under
/// `#[tokio::test(start_paused = true)]` observes the exact same arithmetic
/// without spending real time.
#[derive(Debug)]
pub struct GoRateLimiter {
    per_request: Duration,
    max_slack: Duration,
    /// Go's `state`; `None` is Go's `0` sentinel.
    state: Mutex<Option<Instant>>,
    takes: AtomicU64,
}

impl GoRateLimiter {
    /// Go's `ratelimit.New(rate)`: one-second window, default slack of ten.
    pub fn per_second(rate: u32) -> Self {
        Self::new(rate, Duration::from_secs(1), GO_RATE_LIMITER_DEFAULT_SLACK)
    }

    /// Go's `ratelimit.New(rate, Per(per), WithSlack(slack))`.
    ///
    /// A `rate` of zero would divide by zero in Go; it is clamped to one here
    /// because a bot process must not abort over a configuration value.
    pub fn new(rate: u32, per: Duration, slack: u32) -> Self {
        let per_request = per / rate.max(1);
        Self {
            per_request,
            max_slack: per_request * slack,
            state: Mutex::new(None),
            takes: AtomicU64::new(0),
        }
    }

    /// Go's `Take`, minus the sleep: the state transition plus the sleep the
    /// caller still owes. Splitting it out is what makes the timing assertable
    /// without a real wait.
    pub fn reserve(&self) -> Duration {
        self.takes.fetch_add(1, Ordering::SeqCst);

        let now = Instant::now();
        let mut state = self.state.lock().expect("rate limiter state");

        // Go compares `now - state` against durations, and a `state` in the
        // future makes that difference negative, which fails both `>` tests and
        // lands in the default branch. `checked_duration_since` returns `None`
        // for exactly that case.
        let elapsed = state.and_then(|issued| now.checked_duration_since(issued));

        let next = match (*state, elapsed) {
            // `timeOfNextPermissionIssue == 0`. Go's `maxSlack == 0` half of
            // this branch cannot fire here: `slack` is never zero in this port.
            (None, _) => now,
            // `now-state > maxSlack+perRequest`: cap the accumulated credit.
            (Some(_), Some(elapsed))
                if !self.max_slack.is_zero() && elapsed > self.max_slack + self.per_request =>
            {
                now.checked_sub(self.max_slack).unwrap_or(now)
            }
            // `state + perRequest`.
            (Some(issued), _) => issued + self.per_request,
        };

        *state = Some(next);
        drop(state);

        next.checked_duration_since(now).unwrap_or(Duration::ZERO)
    }

    /// Go's `Take`.
    pub async fn take(&self) {
        let sleep_for = self.reserve();
        if !sleep_for.is_zero() {
            tokio::time::sleep(sleep_for).await;
        }
    }

    /// Go's `limiter.Take()` in `NewClient`, which runs before the client is
    /// ever handed out.
    ///
    /// A limiter that has never been taken from always issues immediately, so
    /// this can stay synchronous exactly as Go's constructor is. It does move
    /// the first permission issue one `per_request` into the future, which is
    /// why Go's very first completion after start-up waits.
    pub fn prime(&self) {
        let sleep_for = self.reserve();
        debug_assert!(
            sleep_for.is_zero(),
            "a fresh limiter issues the first permission immediately"
        );
    }

    /// How many times `Take` has been called, including [`Self::prime`].
    pub fn takes(&self) -> u64 {
        self.takes.load(Ordering::SeqCst)
    }

    pub fn per_request(&self) -> Duration {
        self.per_request
    }

    pub fn max_slack(&self) -> Duration {
        self.max_slack
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_first_take_issues_immediately() {
        let limiter = GoRateLimiter::per_second(1);
        assert_eq!(limiter.reserve(), Duration::ZERO);
        assert_eq!(limiter.takes(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_take_owes_one_whole_period() {
        let limiter = GoRateLimiter::per_second(1);
        limiter.prime();
        assert_eq!(limiter.reserve(), Duration::from_secs(1));
        assert_eq!(limiter.takes(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_limiter_caps_its_credit_at_the_slack() {
        let limiter = GoRateLimiter::per_second(1);
        limiter.prime();

        // Well past `maxSlack + perRequest`, so Go rewinds the issue time to
        // `now - maxSlack` instead of letting unlimited credit build up.
        tokio::time::sleep(Duration::from_secs(120)).await;

        assert_eq!(limiter.reserve(), Duration::ZERO);
        // Ten seconds of credit remain, so ten more takes issue immediately.
        for _ in 0..10 {
            assert_eq!(limiter.reserve(), Duration::ZERO);
        }
        assert_eq!(limiter.reserve(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn take_sleeps_for_exactly_the_reserved_duration() {
        let limiter = GoRateLimiter::per_second(1);
        limiter.prime();

        let started = Instant::now();
        limiter.take().await;
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }

    #[test]
    fn the_period_is_the_window_divided_by_the_rate() {
        let limiter = GoRateLimiter::per_second(1000);
        assert_eq!(limiter.per_request(), Duration::from_millis(1));
        assert_eq!(limiter.max_slack(), Duration::from_millis(10));
    }
}
