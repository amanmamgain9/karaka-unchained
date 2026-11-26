//! One timed ask's foreground wait, and the claims answer UIs hold on it.
//! (dsh: `TimedQuestionWait` in `user-questions/src/timed-wait.ts`.)
//!
//! The wait has a deadline, but the host counts it down only while no UI
//! holds a claim: a user reading the question on a screen isn't timed out
//! by the host, which leaves that to the UI's own countdown. When the last
//! claim leaves, the countdown resumes toward the original deadline, and a
//! deadline already past ends the wait at once.
//!
//! The wait has its own signal, which the answerer chain gets in place of
//! the turn's, so the wait can end the chain without cancelling the turn.
//! The turn's signal, the parent, ends the wait too.
//!
//! dsh's timer is a `setTimeout`, and its parent is an abort listener. Here
//! both are Tokio tasks, spawned on the runtime the wait was made on, and
//! aborted when the wait ends. A claim is a stream: it yields one
//! [`WaitClaim`], then ends when its own signal or the wait's fires, and
//! dropping it releases the claim, as dsh's consumer returning does. dsh
//! releases a claim the moment its signal fires; here, when the stream is
//! next read or dropped.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use futures_core::Stream;
use karaka_fs::{AbortController, AbortSignal};
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Why a wait ended. (dsh: the wait signal's `reason`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEnd {
    /// The deadline passed with no claim held. (dsh: the `timeout` error.)
    TimedOut,
    /// The parent signal fired. (dsh: the parent's reason.)
    ParentAborted,
    /// [`TimedQuestionWait::close`] ended it.
    Closed,
}

/// What a claim yields: how long the host's countdown had left when the
/// claim began. (dsh: `{ remainingMs }`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitClaim {
    pub remaining_ms: u64,
}

/// One live foreground wait. Cheap to clone: clones share the wait.
#[derive(Clone)]
pub struct TimedQuestionWait {
    inner: Arc<Inner>,
}

struct Inner {
    deadline: Instant,
    /// The wait's own signal, for the answerer chain.
    controller: AbortController,
    /// `None` until the wait ends.
    end: watch::Sender<Option<WaitEnd>>,
    /// Where the timer and the parent's watcher run.
    runtime: Handle,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    claims: usize,
    timer: Option<JoinHandle<()>>,
    parent: Option<JoinHandle<()>>,
}

impl TimedQuestionWait {
    /// A wait that ends at `deadline` unless a claim holds it, or when
    /// `parent` fires.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime, whose timer the wait uses.
    pub fn new(deadline: Instant, parent: Option<AbortSignal>) -> Self {
        let inner = Arc::new(Inner {
            deadline,
            controller: AbortController::new(),
            end: watch::Sender::new(None),
            runtime: Handle::current(),
            state: Mutex::new(State::default()),
        });
        match parent {
            Some(parent) if parent.is_aborted() => inner.close(WaitEnd::ParentAborted),
            parent => {
                if let Some(parent) = parent {
                    let weak = Arc::downgrade(&inner);
                    let watcher = inner.runtime.spawn(async move {
                        parent.wait().await;
                        if let Some(inner) = weak.upgrade() {
                            inner.close(WaitEnd::ParentAborted);
                        }
                    });
                    inner.lock().parent = Some(watcher);
                }
                inner.schedule();
            }
        }
        Self { inner }
    }

    /// The wait's own signal, which the answerer chain gets instead of the
    /// turn's. (dsh: `signal`.)
    pub fn signal(&self) -> AbortSignal {
        self.inner.controller.signal()
    }

    /// Why the wait ended, or `None` while it lasts.
    pub fn end(&self) -> Option<WaitEnd> {
        *self.inner.end.borrow()
    }

    /// Resolves once the wait has ended. (dsh: `done`.)
    pub async fn done(&self) {
        let mut end = self.inner.end.subscribe();
        // The sender lives in `self`, so the wait can't fail.
        let _ = end.wait_for(Option::is_some).await;
    }

    /// Holds the wait for one answer UI until `signal` fires or the wait
    /// ends. The stream yields one [`WaitClaim`], then ends. A wait that
    /// ended, or a signal that already fired, yields nothing; so does a
    /// wait past its deadline with no claim, which that ends. The claim
    /// begins when the stream is first read. (dsh: `attach`.)
    pub fn attach(&self, signal: AbortSignal) -> WaitAttachment {
        WaitAttachment {
            wait: Some(Arc::clone(&self.inner)),
            signal,
            phase: Phase::Fresh,
        }
    }

    /// Ends the wait, its timers and its claims, as [`WaitEnd::Closed`].
    /// Only the first end counts. (dsh: `close`.)
    pub fn close(&self) {
        self.inner.close(WaitEnd::Closed);
    }

    /// Whether `self` and `other` are the same wait.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl fmt::Debug for TimedQuestionWait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimedQuestionWait")
            .field("deadline", &self.inner.deadline)
            .field("end", &self.end())
            .field("claims", &self.inner.lock().claims)
            .finish()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every change is a counter step or a handle swap, so the state is
        // whole even after a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ended(&self) -> bool {
        self.end.borrow().is_some()
    }

    fn close(&self, end: WaitEnd) {
        let first = self.end.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(end);
            true
        });
        if !first {
            return;
        }
        let (timer, parent) = {
            let mut state = self.lock();
            (state.timer.take(), state.parent.take())
        };
        for task in [timer, parent].into_iter().flatten() {
            task.abort();
        }
        self.controller.abort();
    }

    /// Restarts the countdown, which runs only while the wait lasts and no
    /// claim holds it. (dsh: `schedule`.)
    fn schedule(self: &Arc<Self>) {
        let mut state = self.lock();
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        if self.ended() || state.claims > 0 {
            return;
        }
        let weak = Arc::downgrade(self);
        let deadline = self.deadline;
        state.timer = Some(self.runtime.spawn(async move {
            sleep_until(deadline).await;
            if let Some(inner) = weak.upgrade() {
                inner.close(WaitEnd::TimedOut);
            }
        }));
    }

    fn release(self: &Arc<Self>) {
        {
            let mut state = self.lock();
            state.claims = state.claims.saturating_sub(1);
        }
        self.schedule();
    }
}

/// One answer UI's claim on a wait. See [`TimedQuestionWait::attach`].
pub struct WaitAttachment {
    /// `None` for the empty stream of a wait that doesn't exist.
    wait: Option<Arc<Inner>>,
    signal: AbortSignal,
    phase: Phase,
}

enum Phase {
    Fresh,
    /// Holding the claim until either signal fires.
    Claimed(Pin<Box<dyn Future<Output = ()> + Send>>),
    Done,
}

impl WaitAttachment {
    /// A stream that yields nothing, for a call with no wait.
    pub(crate) fn empty() -> Self {
        Self {
            wait: None,
            signal: AbortSignal::never(),
            phase: Phase::Done,
        }
    }
}

impl Stream for WaitAttachment {
    type Item = WaitClaim;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<WaitClaim>> {
        let this = self.get_mut();
        let Some(wait) = this.wait.clone() else {
            return Poll::Ready(None);
        };
        match &mut this.phase {
            Phase::Done => Poll::Ready(None),
            Phase::Fresh => {
                if this.signal.is_aborted() || wait.ended() {
                    this.phase = Phase::Done;
                    return Poll::Ready(None);
                }
                let now = Instant::now();
                {
                    let mut state = wait.lock();
                    if state.claims == 0 && now >= wait.deadline {
                        drop(state);
                        wait.close(WaitEnd::TimedOut);
                        this.phase = Phase::Done;
                        return Poll::Ready(None);
                    }
                    state.claims += 1;
                }
                wait.schedule();
                let own = this.signal.clone();
                let wait_signal = wait.controller.signal();
                this.phase = Phase::Claimed(Box::pin(async move {
                    tokio::select! {
                        () = own.wait() => {}
                        () = wait_signal.wait() => {}
                    }
                }));
                let remaining = wait.deadline.saturating_duration_since(now);
                Poll::Ready(Some(WaitClaim {
                    remaining_ms: u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                }))
            }
            Phase::Claimed(ended) => match ended.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    this.phase = Phase::Done;
                    wait.release();
                    Poll::Ready(None)
                }
            },
        }
    }
}

impl Drop for WaitAttachment {
    fn drop(&mut self) {
        if let (Phase::Claimed(_), Some(wait)) = (&self.phase, &self.wait) {
            wait.release();
        }
    }
}

impl fmt::Debug for WaitAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = match self.phase {
            Phase::Fresh => "fresh",
            Phase::Claimed(_) => "claimed",
            Phase::Done => "done",
        };
        f.debug_struct("WaitAttachment")
            .field("phase", &phase)
            .finish_non_exhaustive()
    }
}
