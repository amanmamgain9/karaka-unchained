//! The handler: whether to retry, how long to wait, and the wait itself.
//! (dsh: `recover` and `backoff` in `llm-retry/src/index.ts:178-238`.)

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use karaka_agent::{
    HandlerError, HookFuture, Next, RequestError, RequestErrorAction, RequestErrorHandler,
};
use karaka_model::{Backoff, RetryPolicy};

use crate::events::{Scheduled, previous, retry_event, started_event};
use crate::policy_key;

/// A source of random numbers from 0 to 1, for the jitter. (dsh:
/// `RetryInternals.random`.) Tests pass a fixed one.
pub type Random = Arc<dyn Fn() -> f64 + Send + Sync>;

/// The request-error handler that retries. See the crate docs.
pub struct RetryHandler {
    random: Random,
}

impl RetryHandler {
    /// A handler drawing its jitter from `random`.
    pub fn new(random: Random) -> Self {
        Self { random }
    }

    /// A handler with a random source of its own. The jitter only spreads
    /// retries out, so it needs no strong randomness: each draw hashes the
    /// time with `std`'s randomly keyed hasher, which saves a dependency.
    pub fn with_system_random() -> Self {
        Self::new(Arc::new(system_random))
    }

    /// dsh: `recover`.
    async fn recover<'a>(
        &'a self,
        error: &'a RequestError,
        next: Next<'a>,
    ) -> Result<Option<RequestErrorAction>, HandlerError> {
        let policy = &error.retry_policy;
        let failure = &error.failure;
        let mut next = Some(next);
        match policy {
            RetryPolicy::Always { .. } => {
                if error.cancel.is_cancelled() {
                    return Ok(None);
                }
                // A handler after this one may know this failure better, so
                // it is asked first. Its failure doesn't stop the retry:
                // dsh logs it and carries on; Karaka has no logger yet.
                let downstream = delegate(&mut next).await;
                if error.cancel.is_cancelled() {
                    return Ok(None);
                }
                if let Ok(Some(RequestErrorAction::Retry)) = downstream {
                    return Ok(Some(RequestErrorAction::Retry));
                }
            }
            RetryPolicy::Normal { .. } if !policy.covers(&failure.code) => {
                return delegate(&mut next).await;
            }
            RetryPolicy::Normal { .. } => {}
        }

        let policy_key = policy_key(policy);
        let previous = previous(&error.session, &error.provider, &policy_key);
        let previous_retry = previous.as_ref().map_or(0, |previous| previous.retry);
        if let RetryPolicy::Normal { max_retries, .. } = policy
            && previous_retry >= *max_retries
        {
            return delegate(&mut next).await;
        }
        let retry = previous_retry + 1;
        // One id for every retry of a step, minted on the first. dsh uses a
        // random UUID; this names the seq the first `llm/retry` gets, in the
        // style of the loop's message ids, which is unique in the session.
        let retry_id = match previous {
            Some(previous) => previous.retry_id,
            None => format!("retry-{}", error.session.len()),
        };

        let backoff = policy.backoff();
        let delay_ms = match failure.provider_retry_after_ms.filter(|&wait| wait > 0) {
            Some(wait) if wait as f64 > backoff.max_delay_ms => match policy {
                RetryPolicy::Normal { .. } => return delegate(&mut next).await,
                RetryPolicy::Always { .. } => local_delay(backoff, retry, (self.random)()),
            },
            Some(wait) => wait as f64,
            None => local_delay(backoff, retry, (self.random)()),
        };

        // dsh: `backoff`. The plan is logged before the wait, so it survives
        // the process dying during it.
        if error.cancel.is_cancelled() {
            return Ok(None);
        }
        let scheduled = Scheduled {
            retry_id: &retry_id,
            turn: error.turn,
            step: error.step,
            provider: &error.provider,
            policy,
            policy_key: &policy_key,
            retry,
            delay_ms,
            failure,
        };
        error.session.append(retry_event(&scheduled)?)?;
        let wait = Duration::try_from_secs_f64(delay_ms / 1000.0).unwrap_or(Duration::ZERO);
        tokio::select! {
            biased;
            _ = error.cancel.cancelled() => return Ok(None),
            () = tokio::time::sleep(wait) => {}
        }
        error
            .session
            .append(started_event(&retry_id, error.turn, error.step, retry))?;
        Ok(Some(RequestErrorAction::Retry))
    }
}

impl RequestErrorHandler for RetryHandler {
    fn handle<'a>(&'a self, error: &'a RequestError, next: Next<'a>) -> HookFuture<'a> {
        Box::pin(self.recover(error, next))
    }
}

/// Hands the decision to the rest of the chain, once. Always mode has asked
/// already by the time a normal-only path could ask again, so a second ask
/// can't happen; it answers `None` if it did.
async fn delegate(next: &mut Option<Next<'_>>) -> Result<Option<RequestErrorAction>, HandlerError> {
    match next.take() {
        Some(next) => next.run().await,
        None => Ok(None),
    }
}

/// The wait before retry number `retry`, from 1: the initial delay doubled
/// for each retry before it, capped at the longest wait, then spread by the
/// jitter, and capped again. `random` is from 0 to 1. (dsh: `localDelay`,
/// `index.ts:58-63`.)
pub fn local_delay(backoff: &Backoff, retry: u64, random: f64) -> f64 {
    let exponent = retry.saturating_sub(1).min(1024) as i32;
    let exponential = (backoff.initial_delay_ms * 2f64.powi(exponent)).min(backoff.max_delay_ms);
    let jitter = 1.0 - backoff.jitter_ratio + 2.0 * backoff.jitter_ratio * random;
    (exponential * jitter).min(backoff.max_delay_ms)
}

/// A number from 0 to 1, not reaching 1. `RandomState::new` is keyed
/// randomly once per thread and moves on with each call, so two draws in
/// the same nanosecond still differ.
fn system_random() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    // The top 53 bits, the most an `f64` holds exactly.
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use karaka_model::Backoff;

    use super::{local_delay, system_random};

    fn backoff(initial: f64, max: f64, jitter: f64) -> Backoff {
        Backoff {
            initial_delay_ms: initial,
            max_delay_ms: max,
            jitter_ratio: jitter,
        }
    }

    /// dsh: "applies bounded exponential jitter and stops after the
    /// configured budget" (`retry.spec.ts:336`): 450, then 1,100.
    #[test]
    fn the_delay_doubles_and_jitters_as_dshs_does() {
        let policy = backoff(500.0, 10_000.0, 0.1);
        assert_eq!(local_delay(&policy, 1, 0.0), 450.0);
        assert_eq!(local_delay(&policy, 2, 1.0), 1_100.0);
        assert_eq!(local_delay(&policy, 30, 0.5), 10_000.0);
        assert_eq!(local_delay(&policy, u64::MAX, 1.0), 10_000.0);
        // "accepts the zero-delay lower jitter bound" (`retry.spec.ts:371`).
        assert_eq!(local_delay(&backoff(1.0, 1.0, 1.0), 1, 0.0), 0.0);
        // "uses local jittered backoff when always mode receives an over-cap
        // Retry-After" (`retry.spec.ts:423`).
        assert_eq!(local_delay(&backoff(2.0, 4.0, 0.5), 1, 1.0), 3.0);
    }

    #[test]
    fn the_system_random_stays_in_range() {
        for _ in 0..1000 {
            let draw = system_random();
            assert!((0.0..1.0).contains(&draw), "{draw}");
        }
    }
}
