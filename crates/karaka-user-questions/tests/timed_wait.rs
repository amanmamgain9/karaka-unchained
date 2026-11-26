//! A timed wait's deadline and its claims. Ported from dsh's
//! `user-questions/tests/timed-wait.spec.ts`, on Tokio's paused clock where
//! dsh uses fake timers. dsh's `getTimerCount` checks have no Tokio
//! counterpart and aren't ported; that a closed wait leaves no timer is
//! covered by its end, which only one close sets.

use std::time::Duration;

use futures_core::Stream;
use karaka_fs::{AbortController, AbortSignal};
use karaka_user_questions::{TimedQuestionWait, WaitClaim, WaitEnd};
use tokio::time::{Instant, advance};

fn start(parent: Option<AbortSignal>) -> TimedQuestionWait {
    TimedQuestionWait::new(Instant::now() + Duration::from_secs(5), parent)
}

/// Lets the wait's tasks run after the clock moved.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// Moves the paused clock by `ms` and lets what was due run.
async fn pass(ms: u64) {
    advance(Duration::from_millis(ms)).await;
    settle().await;
}

async fn next<S: Stream<Item = WaitClaim> + Unpin>(stream: &mut S) -> Option<WaitClaim> {
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

fn claim(remaining_ms: u64) -> Option<WaitClaim> {
    Some(WaitClaim { remaining_ms })
}

// "expires with no answer UI and releases its timer"
#[tokio::test(start_paused = true)]
async fn an_unclaimed_wait_expires_at_its_deadline() {
    let wait = start(None);
    pass(4_999).await;
    assert!(!wait.signal().is_aborted());
    pass(1).await;
    wait.done().await;
    assert_eq!(wait.end(), Some(WaitEnd::TimedOut));
    assert!(wait.signal().is_aborted());
}

// "keeps a claimed wait open beyond the Host deadline and expires after the
// last claim leaves"
#[tokio::test(start_paused = true)]
async fn a_claimed_wait_outlives_its_deadline_until_the_last_claim_leaves() {
    let wait = start(None);
    let first = AbortController::new();
    let second = AbortController::new();
    let mut a = wait.attach(first.signal());
    let mut b = wait.attach(second.signal());
    assert_eq!(next(&mut a).await, claim(5_000));
    assert_eq!(next(&mut b).await, claim(5_000));
    pass(10_000).await;
    first.abort();
    assert_eq!(next(&mut a).await, None);
    assert!(!wait.signal().is_aborted());
    second.abort();
    assert_eq!(next(&mut b).await, None);
    pass(0).await;
    assert_eq!(wait.end(), Some(WaitEnd::TimedOut));
}

// "resumes the original remaining duration after a claim disconnects"
#[tokio::test(start_paused = true)]
async fn a_disconnected_claim_resumes_the_original_countdown() {
    let wait = start(None);
    let client = AbortController::new();
    let mut stream = wait.attach(client.signal());
    pass(1_000).await;
    // The claim starts when the stream is first read, as dsh's generator
    // body runs on its first `next`.
    assert_eq!(next(&mut stream).await, claim(4_000));
    pass(2_000).await;
    client.abort();
    assert_eq!(next(&mut stream).await, None);
    pass(1_999).await;
    assert!(!wait.signal().is_aborted());
    pass(1).await;
    assert!(wait.signal().is_aborted());
}

// "releases a claim when its consumer returns": here, drops the stream.
#[tokio::test(start_paused = true)]
async fn a_dropped_stream_releases_its_claim() {
    let wait = start(None);
    let mut stream = wait.attach(AbortSignal::never());
    next(&mut stream).await;
    drop(stream);
    pass(5_000).await;
    assert!(wait.signal().is_aborted());
}

// "ends claim streams with the parent cancellation without expiring the
// parent itself"
#[tokio::test(start_paused = true)]
async fn the_parents_cancel_ends_the_wait_and_its_claims() {
    let parent = AbortController::new();
    let wait = start(Some(parent.signal()));
    let mut stream = wait.attach(AbortSignal::never());
    next(&mut stream).await;
    parent.abort();
    assert_eq!(next(&mut stream).await, None);
    assert_eq!(wait.end(), Some(WaitEnd::ParentAborted));

    let other_parent = AbortController::new();
    let other = start(Some(other_parent.signal()));
    pass(5_000).await;
    assert_eq!(other.end(), Some(WaitEnd::TimedOut));
    assert!(!other_parent.signal().is_aborted());
}

// "does not claim a cancelled or expired wait"
#[tokio::test(start_paused = true)]
async fn a_cancelled_or_expired_wait_takes_no_claim() {
    let wait = start(Some(AbortSignal::aborted()));
    assert_eq!(wait.end(), Some(WaitEnd::ParentAborted));
    assert_eq!(next(&mut wait.attach(AbortSignal::never())).await, None);

    let live = start(None);
    assert_eq!(next(&mut live.attach(AbortSignal::aborted())).await, None);
    assert!(!live.signal().is_aborted());
    // dsh moves the system clock without running timers; here the clock
    // moves and the attach comes first, before the timer task can run.
    advance(Duration::from_secs(6)).await;
    assert_eq!(next(&mut live.attach(AbortSignal::never())).await, None);
    assert_eq!(live.end(), Some(WaitEnd::TimedOut));
}

// A close ends the wait once: a later close, or the deadline, doesn't
// change why it ended. (dsh: `close`'s early return.)
#[tokio::test(start_paused = true)]
async fn a_wait_ends_once() {
    let wait = start(None);
    wait.close();
    assert_eq!(wait.end(), Some(WaitEnd::Closed));
    pass(5_000).await;
    assert_eq!(wait.end(), Some(WaitEnd::Closed));
    wait.done().await;
}
