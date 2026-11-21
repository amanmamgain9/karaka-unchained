//! One session's agent, and the task that drives its turns.
//! (dsh: `ReactLoopAgent` in `agent-loop/src/agent.ts`: `followup`,
//! `cancel`, `whenIdle`, `wakeDriver` and `kick`.)
//!
//! # The driver
//!
//! An idle agent runs nothing. The first [`followup`](Agent::followup)
//! starts a driver, a task that runs one turn per queued message until the
//! queue is empty, or until a turn fails, and then ends. A failed turn
//! leaves the rest of the queue for the next followup's driver, as dsh's
//! `kick` does. A followup sent while the driver runs only
//! joins the queue: the driver picks it up when the current turn ends. So
//! an agent runs one turn at a time, and turns run in the order their
//! messages came. This is dsh's `wakeDriver` and `kick`.
//!
//! # Cancelling
//!
//! Each turn has a switch, a watch channel that holds the cancel's cause
//! once someone cancels. The turn races every model stream and every tool
//! call against it, and drops whichever is still running when the switch
//! flips. Dropping a stream closes its connection; dropping a tool's future
//! stops the tool. dsh aborts an `AbortSignal` instead and waits for the
//! work to notice it.
//!
//! # Locking
//!
//! The queue and the switch sit behind one `std::sync` mutex, held only to
//! read or change them, never across an `.await`, so `followup` and
//! `cancel` never wait on a model call. Starting the driver and the driver
//! finishing both happen under it, so a followup can't slip in between the
//! driver deciding it is done and saying so.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use karaka_agent::{Agent, WhenIdle};
use karaka_session::{CancelCause, Session, SessionEvent, SessionId, UserMessage};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::plugin::Shared;
use crate::turn::Ran;

/// The switch that cancels one turn: `None` until someone cancels it, then
/// the cause. Only the first cause counts.
pub(crate) type Switch = watch::Sender<Option<CancelCause>>;

/// What a turn watches for a cancel.
pub(crate) type Cancel = watch::Receiver<Option<CancelCause>>;

/// One session's agent. Cheap to share; every clone of the inner `Arc`
/// reaches the same queue and session.
pub(crate) struct LoopAgent {
    inner: Arc<Inner>,
}

/// What an agent and its driver share.
pub(crate) struct Inner {
    pub(crate) session: Session,
    pub(crate) shared: Arc<Shared>,
    /// Where the driver runs. Kept from when the agent was made, so a
    /// followup can start the driver from a thread outside the runtime,
    /// such as one reading stdin.
    runtime: Handle,
    state: Mutex<State>,
    /// Whether this agent has logged a `request/header` yet. Its first
    /// request always logs one. (dsh: `requestHeaderLogged`.) Only the
    /// driver reads and writes it, one turn at a time.
    pub(crate) header_logged: AtomicBool,
    /// The surface's replacement count when the last request was built. A
    /// request built after a replacement starts a new series.
    /// (dsh: `requestSurfaceGeneration`.)
    pub(crate) request_generation: AtomicU64,
    /// `true` while no driver runs. [`Agent::when_idle`] waits on it.
    /// Messages may still be queued, after a failed turn.
    idle: watch::Sender<bool>,
}

struct State {
    /// The messages waiting for a turn each, oldest first. (dsh: the
    /// inbox's `next-turn` list.)
    queue: VecDeque<UserMessage>,
    /// The current turn's switch while a driver runs, `None` while the
    /// agent is idle. (dsh: the `running` phase's `AbortController`.)
    switch: Option<Switch>,
    /// The number of the last turn that started, 0 before the first. Turns
    /// are numbered from 1 in each session, carrying on after the turns of
    /// a restored log. (dsh: the `turnBoundary` projection's `lastTurn`.)
    last_turn: u64,
}

impl LoopAgent {
    /// The agent for `session`, which has no turn open.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime: the agent keeps that runtime to
    /// run its driver on.
    pub(crate) fn new(session: Session, shared: Arc<Shared>) -> Self {
        let last_turn = session
            .events()
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                SessionEvent::TurnStart(start) => Some(start.turn),
                _ => None,
            })
            .unwrap_or(0);
        let generation = session.surface_generation();
        let inner = Inner {
            header_logged: AtomicBool::new(false),
            request_generation: AtomicU64::new(generation),
            session,
            shared,
            runtime: Handle::current(),
            state: Mutex::new(State {
                queue: VecDeque::new(),
                switch: None,
                last_turn,
            }),
            idle: watch::Sender::new(true),
        };
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl Agent for LoopAgent {
    fn id(&self) -> &SessionId {
        self.inner.session.id()
    }

    fn followup(&self, message: UserMessage) {
        let mut state = self.inner.lock();
        state.queue.push_back(message);
        if state.switch.is_some() {
            // The running driver takes it when the current turn ends.
            return;
        }
        let (switch, cancel) = watch::channel(None);
        state.switch = Some(switch);
        self.inner.idle.send_replace(false);
        drop(state);
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(inner.drive(cancel));
    }

    fn cancel(&self, cause: CancelCause) {
        let mut state = self.inner.lock();
        state.queue.clear();
        if let Some(switch) = &state.switch {
            switch.send_if_modified(|current| {
                if current.is_some() {
                    return false;
                }
                *current = Some(cause);
                true
            });
        }
    }

    fn when_idle(&self) -> WhenIdle<'_> {
        // Subscribed now, not when first polled, so a followup sent just
        // before this call is waited for.
        let mut idle = self.inner.idle.subscribe();
        Box::pin(async move {
            // The sender lives as long as the agent, which this borrows, so
            // the wait can't fail.
            let _ = idle.wait_for(|idle| *idle).await;
        })
    }
}

impl Inner {
    /// Runs turns until the queue is empty or a turn fails. (dsh: `kick`.)
    async fn drive(self: Arc<Self>, mut cancel: Cancel) {
        // If the driver panics, or the runtime shuts down under it, the
        // guard still marks the agent idle, so nobody waits for it forever.
        let mut guard = IdleOnDrop {
            inner: &self,
            armed: true,
        };
        loop {
            let turn = self.lock().last_turn + 1;
            let ran = self.turn(turn, &mut cancel).await;
            let mut state = self.lock();
            if !matches!(ran, Ran::NotStarted) {
                state.last_turn = turn;
            }
            // After a turn that failed, or one the log refused to start, the
            // driver stops, as dsh's does, and leaves the queue as it is.
            // The next followup starts a driver, which takes the oldest
            // queued message first. An aborted turn needs no such rule: the
            // cancel emptied the queue.
            if !matches!(ran, Ran::Ended) || state.queue.is_empty() {
                guard.armed = false;
                self.finish(&mut state);
                return;
            }
            // A new switch for the next turn: a cancel of the turn that
            // just ended doesn't reach it. (dsh: `phase.abort = new
            // AbortController()`.)
            let (switch, next) = watch::channel(None);
            state.switch = Some(switch);
            cancel = next;
        }
    }

    /// Takes the oldest queued message, for the first step of a turn.
    /// (dsh: the inbox's `claim`.)
    pub(crate) fn claim(&self) -> Option<UserMessage> {
        self.lock().queue.pop_front()
    }

    /// Marks the agent idle. The caller holds the lock, so no followup can
    /// arrive between the decision to stop and this.
    fn finish(&self, state: &mut State) {
        state.switch = None;
        self.idle.send_replace(true);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A lock is "poisoned" when a thread panicked while holding it.
        // Every change to the state is a single assignment or push, so it is
        // still whole, and it's safe to carry on with it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Marks the agent idle if the driver ends without doing it itself.
///
/// It is disarmed before a normal finish. Otherwise a followup sent just
/// after that finish could start a new driver, and this guard, dropped a
/// moment later, would mark that one's agent idle.
struct IdleOnDrop<'a> {
    inner: &'a Inner,
    armed: bool,
}

impl Drop for IdleOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.inner.finish(&mut self.inner.lock());
        }
    }
}
