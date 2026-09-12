//! Retry with exponential backoff.
//!
//! The observable behaviour is `MediumParser.query`'s (`core.py:170-204`): at
//! most `retry` attempts, sleeping `2 ** attempt` seconds between them. What is
//! not ported is its control flow, which §7 item 3 requires replacing.
//!
//! # The bug this does not reproduce
//!
//! The legacy loop is a `while ... else` with a `break` on success, and `reason`
//! is initialised **once, outside the loop**:
//!
//! ```text
//! reason = None
//! while not post_data and attempt < retry:
//!     ...
//!     if not post_data: reason = "No post data returned"
//!     ...
//!     if reason is None: break
//! ```
//!
//! So when attempt 0 returns nothing (`reason` set) and attempt 1 returns a
//! perfectly good post, `reason` is *still* set from the previous iteration —
//! `if reason is None` is false, the `break` is skipped, and the loop falls out
//! through its condition into the `else`, which raises
//! `MediumPostQueryError`. **A successful second attempt is reported as a
//! failure.**
//!
//! [`with_retry`] cannot have that bug, because there is no state that outlives
//! an attempt: the outcome of one is a `Result` value that is matched and
//! discarded. The decision of whether to try again lives in
//! [`FetchError::is_retryable`], not in a variable the loop carries forward.
//!
//! # Sleeping is injected
//!
//! [`Sleeper`] exists so the backoff sequence can be asserted as *data* rather
//! than by waiting. A test that called `tokio::time::sleep` for real would need
//! `tokio`'s `test-util` feature and a paused clock, which would put a
//! dev-dependency in a workspace where no crate has one; recording the
//! requested durations tests the policy more directly anyway.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::FetchError;

/// How many attempts to make, and how long to wait between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, not retries. `2` means one try plus one retry, which is
    /// what `core.py:163`'s `retry: int = 2` produces.
    pub attempts: u32,

    /// The delay before the second attempt. Each subsequent one doubles it.
    pub base: Duration,
}

impl RetryPolicy {
    /// `core.py:163-164`: `retry=2`, sleeping `2 ** attempt` seconds — so 1s
    /// then 2s, and the last sleep is never taken because no attempt follows it.
    pub const DEFAULT: Self = Self {
        attempts: 2,
        base: Duration::from_secs(1),
    };

    /// A policy of `attempts` total attempts. Zero is treated as one: an
    /// operation that is never attempted cannot report a meaningful error, and
    /// silently returning "no result" would be worse than trying once.
    pub const fn new(attempts: u32, base: Duration) -> Self {
        Self {
            attempts: if attempts == 0 { 1 } else { attempts },
            base,
        }
    }

    /// The delay to take *after* `attempt` (zero-based) failed.
    ///
    /// `saturating_pow` rather than `pow`: `attempt` is caller-supplied, and
    /// `2u32.pow(32)` panics in debug. A saturated value yields an absurd but
    /// finite sleep instead of a panic on a configuration mistake.
    pub fn delay(&self, attempt: u32) -> Duration {
        self.base.saturating_mul(2u32.saturating_pow(attempt))
    }
}

/// Waits between attempts.
///
/// Implemented for real by [`TokioSleeper`], and by tests with something that
/// returns immediately and records what it was asked for.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

/// The production [`Sleeper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Runs `attempt_fn` until it succeeds, fails unrecoverably, or runs out of
/// attempts.
///
/// `attempt_fn` receives the zero-based attempt number, so a caller can vary
/// the request (a different proxy, say) between tries.
///
/// The last error is returned, not the first: it is the one that describes why
/// the operation finally gave up.
pub async fn with_retry<T, F, Fut>(
    policy: RetryPolicy,
    sleeper: &dyn Sleeper,
    mut attempt_fn: F,
) -> Result<T, FetchError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, FetchError>>,
{
    let mut last: Option<FetchError> = None;

    for attempt in 0..policy.attempts {
        match attempt_fn(attempt).await {
            Ok(value) => return Ok(value),
            Err(err) => {
                // Terminal failures short-circuit rather than burning the
                // remaining attempts. `core.py` reaches the same outcome by
                // accident of truthiness; here it is stated.
                if !err.is_retryable() {
                    return Err(err);
                }

                let is_last = attempt + 1 == policy.attempts;
                if !is_last {
                    sleeper.sleep(policy.delay(attempt)).await;
                }
                last = Some(err);
            }
        }
    }

    // `RetryPolicy::new` guarantees at least one attempt, so `last` is set.
    Err(last.expect("a policy always permits at least one attempt"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the delays it was asked for, and returns at once.
    #[derive(Default)]
    struct RecordingSleeper {
        slept: Mutex<Vec<Duration>>,
    }

    impl RecordingSleeper {
        fn delays(&self) -> Vec<Duration> {
            self.slept.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Sleeper for RecordingSleeper {
        async fn sleep(&self, duration: Duration) {
            self.slept.lock().unwrap().push(duration);
        }
    }

    fn transport_failure() -> FetchError {
        FetchError::Transport(crate::error::TransportError::Timeout)
    }

    /// The backoff is `2 ** attempt` seconds, and the delay after the final
    /// attempt is never taken because nothing follows it.
    #[test]
    fn backoff_doubles_from_the_base() {
        let policy = RetryPolicy::DEFAULT;
        assert_eq!(policy.delay(0), Duration::from_secs(1));
        assert_eq!(policy.delay(1), Duration::from_secs(2));
        assert_eq!(policy.delay(2), Duration::from_secs(4));
    }

    /// A large attempt number must not panic. In debug builds `2u32.pow(32)`
    /// does; this is reached by a misconfigured policy, not by a request.
    ///
    /// The exponent saturates first, at `u32::MAX`, so the result is that many
    /// seconds rather than [`Duration::MAX`] — absurd, but an hour count and not
    /// a panic.
    #[test]
    fn a_huge_attempt_number_saturates_instead_of_panicking() {
        let policy = RetryPolicy::new(64, Duration::from_secs(1));
        assert_eq!(policy.delay(64), Duration::from_secs(u64::from(u32::MAX)));
    }

    /// And the multiply itself can saturate, when the base is large enough.
    #[test]
    fn a_huge_base_saturates_at_the_duration_limit() {
        let policy = RetryPolicy::new(64, Duration::MAX);
        assert_eq!(policy.delay(1), Duration::MAX);
    }

    #[test]
    fn zero_attempts_is_treated_as_one() {
        assert_eq!(RetryPolicy::new(0, Duration::from_secs(1)).attempts, 1);
    }

    /// Success on the first try never sleeps and never retries.
    #[tokio::test]
    async fn success_on_the_first_attempt_does_not_sleep() {
        let sleeper = RecordingSleeper::default();
        let result = with_retry(RetryPolicy::DEFAULT, &sleeper, |_| async {
            Ok::<_, FetchError>("post")
        })
        .await;

        assert_eq!(result, Ok("post"));
        assert!(sleeper.delays().is_empty());
    }

    /// **The bug from the module doc.** A first attempt that fails and a second
    /// that succeeds must return `Ok`. The legacy loop raises here.
    #[tokio::test]
    async fn success_on_the_second_attempt_is_a_success() {
        let sleeper = RecordingSleeper::default();
        let result = with_retry(RetryPolicy::DEFAULT, &sleeper, |attempt| async move {
            if attempt == 0 {
                Err(transport_failure())
            } else {
                Ok("post")
            }
        })
        .await;

        assert_eq!(result, Ok("post"));
        assert_eq!(sleeper.delays(), vec![Duration::from_secs(1)]);
    }

    /// Exhausting the attempts returns the last error, after one sleep for a
    /// two-attempt policy.
    #[tokio::test]
    async fn exhausting_the_attempts_returns_the_last_error() {
        let sleeper = RecordingSleeper::default();
        let result: Result<&str, _> = with_retry(RetryPolicy::DEFAULT, &sleeper, |_| async {
            Err(FetchError::Transport(crate::error::TransportError::Proxy(
                "refused".into(),
            )))
        })
        .await;

        assert_eq!(
            result,
            Err(FetchError::Transport(crate::error::TransportError::Proxy(
                "refused".into()
            )))
        );
        // Two attempts, one sleep between them.
        assert_eq!(sleeper.delays(), vec![Duration::from_secs(1)]);
    }

    /// A terminal failure must not consume the remaining attempts — retrying a
    /// GraphQL error just spends a request on an answer already given.
    #[tokio::test]
    async fn a_terminal_failure_does_not_retry() {
        let sleeper = RecordingSleeper::default();
        let attempts = Mutex::new(0);

        let result: Result<&str, _> = with_retry(RetryPolicy::DEFAULT, &sleeper, |_| {
            *attempts.lock().unwrap() += 1;
            async { Err(FetchError::GraphQl("rejected".into())) }
        })
        .await;

        assert!(matches!(result, Err(FetchError::GraphQl(_))));
        assert_eq!(*attempts.lock().unwrap(), 1, "should not have retried");
        assert!(sleeper.delays().is_empty());
    }

    /// The attempt number is handed to the operation, so it can vary the
    /// request — the pool uses this to move to another exit.
    #[tokio::test]
    async fn the_attempt_number_is_passed_through() {
        let sleeper = RecordingSleeper::default();
        let seen = Mutex::new(Vec::new());

        let _: Result<&str, _> = with_retry(
            RetryPolicy::new(3, Duration::from_millis(1)),
            &sleeper,
            |attempt| {
                seen.lock().unwrap().push(attempt);
                async { Err(transport_failure()) }
            },
        )
        .await;

        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2]);
        assert_eq!(
            sleeper.delays(),
            vec![Duration::from_millis(1), Duration::from_millis(2)]
        );
    }
}
