//! One session's agent, and the task that drives its turns.
//! (dsh: `ReactLoopAgent` in `agent-loop/src/agent.ts`: `send`,
//! `followup`, `steer`, `inject`, `cancel`, `whenIdle`, `wakeDriver` and
//! `kick`.)
//!
//! # The inbox
//!
//! What waits is the session's durable inbox (`inbox.rs`): next-turn, the
//! prompts that get a turn each, and next-step, the input the next step
//! takes. [`followup`](Agent::followup) queues for a turn and wakes the
//! agent; [`steer`](Agent::steer) queues for the next step and wakes it;
//! [`inject`](Agent::inject) queues for the next step and doesn't.
//!
//! # The driver
//!
//! An idle agent runs nothing. A waking send starts a driver, a task that
//! runs turns while anything is pending, and then ends. Each step takes
//! all of next-step, and the first step of a turn also the oldest of
//! next-turn; a turn goes on for as long as a step leaves next-step
//! non-empty. A turn that fails stops the driver, as dsh's `kick` does,
//! and leaves what is pending for the next wake. Input sent while a driver
//! runs only joins the inbox, so an agent runs one turn at a time.
//!
//! A cancel stops the running turn and drops what is pending. Input that
//! wakes the agent after a cancel, while the cancelled turn winds down,
//! goes to next-turn and runs next; input that doesn't wake it waits. This
//! is dsh's latched wake.
//!
//! # Cancelling
//!
//! Each turn has a switch, `karaka-agent`'s [`CancelSwitch`], that holds
//! the cancel's cause once someone cancels. The turn races every model stream and every tool
//! call against it, and drops whichever is still running when the switch
//! flips. Dropping a stream closes its connection; dropping a tool's future
//! stops the tool. dsh aborts an `AbortSignal` instead and waits for the
//! work to notice it.
//!
//! # Locking
//!
//! Every change to the inbox, and the switch, sit behind one `std::sync`
//! mutex, held only to read or change them, never across an `.await`, so
//! sending and cancelling never wait on a model call. Each splice is worked
//! out and logged under it, so splices never race. Starting the driver and
//! the driver finishing both happen under it, so a send can't slip in
//! between the driver deciding it is done and saying so.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use karaka_agent::{
    Agent, CancelSwitch, Cancellation, FrameListener, FrameListeners, FrameSubscription,
    InboxError, WhenIdle, cancellation,
};
use karaka_scope::ScopeChain;
use karaka_session::{CancelCause, InboxTarget, Session, SessionEvent, SessionId, UserMessage};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::inbox::Inbox;
use crate::live::FrameCounters;
use crate::plugin::Shared;
use crate::runtime_context::RuntimeContextProjection;
use crate::turn::Ran;

/// The switch that cancels one turn. Only the first cause counts.
pub(crate) type Switch = CancelSwitch;

/// What a turn watches for a cancel.
pub(crate) type Cancel = Cancellation;

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
    /// Who hears the live frames of this agent's model calls.
    pub(crate) frames: FrameListeners,
    pub(crate) frame_counters: FrameCounters,
    /// `true` while no driver runs. [`Agent::when_idle`] waits on it.
    /// Messages may still be queued, after a failed turn.
    idle: watch::Sender<bool>,
    /// The runtime-context snapshot in effect, worked out from the log the
    /// agent was made with. Only the driver uses it.
    pub(crate) runtime_context: Mutex<RuntimeContextProjection>,
}

struct State {
    /// The current turn's switch while a driver runs, `None` while the
    /// agent is idle. (dsh: the `running` phase's `AbortController`.)
    switch: Option<Switch>,
    /// A waking send arrived after the running turn was cancelled: the
    /// driver runs another turn once that one ends. (dsh:
    /// `wakeRequested`.)
    wake_requested: bool,
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
        let runtime_context =
            RuntimeContextProjection::restore(&session.events(), &session.surface_nodes());
        let inner = Inner {
            runtime_context: Mutex::new(runtime_context),
            header_logged: AtomicBool::new(false),
            frames: FrameListeners::new(),
            frame_counters: FrameCounters::default(),
            request_generation: AtomicU64::new(generation),
            session,
            shared,
            runtime: Handle::current(),
            state: Mutex::new(State {
                switch: None,
                wake_requested: false,
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

    fn scope(&self) -> ScopeChain {
        self.inner.scope()
    }

    fn followup(&self, message: UserMessage) -> Result<(), InboxError> {
        self.send(message, InboxTarget::NextTurn, true)
    }

    fn steer(&self, message: UserMessage) -> Result<(), InboxError> {
        self.send(message, InboxTarget::NextStep, true)
    }

    fn inject(&self, message: UserMessage) -> Result<(), InboxError> {
        self.send(message, InboxTarget::NextStep, false)
    }

    fn cancel(&self, cause: CancelCause) {
        let mut state = self.inner.lock();
        // dsh clears first and throws if the log refuses; here the turn is
        // stopped either way. A refused clear may have dropped next-step
        // already, unheard.
        let dropped = self.inner.inbox().clear().unwrap_or_default();
        state.wake_requested = false;
        if let Some(switch) = &state.switch {
            switch.cancel(cause);
        }
        drop(state);
        // dsh: `agent/inbox/discarded`, for each message the clear dropped.
        for message in &dropped {
            self.inner.shared.inbox_hooks.discarded(self, message);
        }
    }

    /// Every handle on this agent wraps one core, and the driver's own
    /// handle for its tool calls is another, so the core's address is the
    /// agent's identity.
    fn instance(&self) -> usize {
        Arc::as_ptr(&self.inner).cast::<()>() as usize
    }

    fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        self.inner.frames.subscribe(listener)
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

impl LoopAgent {
    /// Queues `message` in `target`, and starts the driver when `wake` and
    /// the agent is idle. (dsh: `send`.)
    fn send(
        &self,
        message: UserMessage,
        target: InboxTarget,
        wake: bool,
    ) -> Result<(), InboxError> {
        let mut state = self.inner.lock();
        // A cancelled turn can't take waking input at a step: it goes to
        // the next turn, and runs once the cancelled one ends.
        let after_cancel = wake
            && state
                .switch
                .as_ref()
                .is_some_and(CancelSwitch::is_cancelled);
        let target = if after_cancel {
            InboxTarget::NextTurn
        } else {
            target
        };
        self.inner.inbox().append(target, message)?;
        if !wake {
            return Ok(());
        }
        if state.switch.is_some() {
            // A live driver takes it itself.
            if after_cancel {
                state.wake_requested = true;
            }
            return Ok(());
        }
        let (switch, cancel) = cancellation();
        state.switch = Some(switch);
        self.inner.idle.send_replace(false);
        drop(state);
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(inner.drive(cancel));
        Ok(())
    }
}

impl Inner {
    /// The scopes this agent sees registrations through: the preset its
    /// composer names, read afresh each time so a selection takes effect at
    /// once, then its session's.
    pub(crate) fn scope(&self) -> ScopeChain {
        ScopeChain::new(
            self.shared.composition.preset(&self.session),
            Some(self.session.id().clone()),
        )
    }

    /// Runs turns while anything is pending, until a turn fails. (dsh:
    /// `kick`, and the end of `turn`.)
    async fn drive(self: Arc<Self>, mut cancel: Cancel) {
        // If the driver panics, or the runtime shuts down under it, the
        // guard still marks the agent idle, so nobody waits for it forever.
        let mut guard = IdleOnDrop {
            inner: &self,
            armed: true,
        };
        // The agent the driver's tool calls are made by. It lives as long as
        // the driver, which holds `self` anyway; nothing in `self` holds it,
        // so it makes no cycle.
        let caller: Arc<dyn Agent> = Arc::new(LoopAgent {
            inner: Arc::clone(&self),
        });
        loop {
            let turn = self.lock().last_turn + 1;
            let ran = self.turn(turn, &cancel, &caller).await;
            let mut state = self.lock();
            if !matches!(ran, Ran::NotStarted) {
                state.last_turn = turn;
            }
            // After a turn that failed, or one the log refused to start, the
            // driver stops, as dsh's does, and leaves what is pending for
            // the next wake. After a cancelled one it goes on only for input
            // that woke it since (dsh's latched wake).
            let pending = self.inbox().has_pending().unwrap_or(false);
            let more = match ran {
                Ran::Ended => pending,
                Ran::Aborted => state.wake_requested && pending,
                Ran::Failed | Ran::NotStarted => false,
            };
            if !more {
                guard.armed = false;
                self.finish(&mut state);
                return;
            }
            state.wake_requested = false;
            // A new switch for the next turn: a cancel of the turn that
            // just ended doesn't reach it. (dsh: `phase.abort = new
            // AbortController()`.)
            let (switch, next) = cancellation();
            state.switch = Some(switch);
            cancel = next;
        }
    }

    /// Takes one step's input, for turn `turn` of `caller`, the driver's
    /// handle on this agent, and tells the inbox listeners. (dsh: the
    /// pre-step's `claim`, and `agent/inbox/claimed`.)
    pub(crate) fn claim(
        &self,
        target: InboxTarget,
        turn: u64,
        caller: &dyn Agent,
    ) -> Result<Vec<UserMessage>, InboxError> {
        let claimed = {
            let _state = self.lock();
            self.inbox().claim(target)?
        };
        for message in &claimed {
            self.shared.inbox_hooks.claimed(caller, message, turn);
        }
        Ok(claimed)
    }

    /// Whether next-step holds input, so the turn goes on.
    pub(crate) fn has_next_step(&self) -> bool {
        let _state = self.lock();
        self.inbox().has_next_step().unwrap_or(false)
    }

    pub(crate) fn inbox(&self) -> Inbox<'_> {
        Inbox {
            projections: &self.shared.projections,
            session: &self.session,
        }
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
