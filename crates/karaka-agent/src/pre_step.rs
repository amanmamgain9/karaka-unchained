//! The pre-step hook: what may reject a step, or change the messages that
//! enter it, before the step starts. (dsh: the `agent/pre-step` waterfall,
//! declared in `agent/src/runtime-types.ts:320` and run by the loop's
//! `preStep` in `agent-loop/src/agent.ts:268-290`.)
//!
//! After a step has claimed its input from the inbox and assembled its
//! prompt, the loop runs the chain. Each listener gets the proposed step
//! and a [`PreStepNext`], and decides, usually by asking the rest of the
//! chain first and then changing what it answered. The end of the chain
//! enters the step with the claimed messages. A listener is global, or
//! scoped to one agent by its session's id, as the other registry ports
//! scope theirs, and they run in the order they were added.
//!
//! dsh's end of the chain also adds the runtime-context snapshot to the
//! step's messages. Karaka's loop logs that snapshot with each attempt of
//! the step's request, after the step's messages, so a listener here
//! doesn't see it.
//!
//! A [`PreStepDecision::Reject`] ends the turn as `blocked`. A listener
//! that fails ends it as an error, unless the turn was cancelled meanwhile.
//! The loop stops waiting for the chain the moment the turn is cancelled.

use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};

use karaka_session::{SessionId, UserMessage};

use crate::{Agent, Cancellation, HandlerError};

/// Whether, and with which messages, a step starts. (dsh:
/// `PreStepDecision`, without `startsRequestSeries`, which waits on
/// compaction.)
#[derive(Clone, Debug, PartialEq)]
pub enum PreStepDecision {
    /// Don't start it: the turn ends as `blocked`.
    Reject,
    /// Start it with these messages, in order.
    Enter { messages: Vec<UserMessage> },
}

/// A proposed step. (dsh: the `agent/pre-step` payload.)
#[derive(Clone)]
pub struct PreStep {
    /// The agent proposing it.
    pub agent: Arc<dyn Agent>,
    /// What the step took from the inbox.
    pub messages: Vec<UserMessage>,
    pub turn: u64,
    pub step: u64,
    /// The turn's cancel switch.
    pub cancel: Cancellation,
}

impl fmt::Debug for PreStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreStep")
            .field("agent", self.agent.id())
            .field("messages", &self.messages)
            .field("turn", &self.turn)
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

/// What a listener, and the rest of a chain, resolve to.
pub type PreStepFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PreStepDecision, HandlerError>> + Send + 'a>>;

/// One listener of the hook. (dsh: an `agent/pre-step` listener.)
pub trait PreStepListener: Send + Sync {
    /// Decides about `step`, usually by running `next` and changing what it
    /// gives.
    fn decide<'a>(&'a self, step: &'a PreStep, next: PreStepNext<'a>) -> PreStepFuture<'a>;
}

/// The rest of a chain, after the listener it was given to.
pub struct PreStepNext<'a> {
    rest: &'a [Arc<dyn PreStepListener>],
    step: &'a PreStep,
}

impl<'a> PreStepNext<'a> {
    /// Runs the rest of the chain, which enters the step with its claimed
    /// messages once it is empty.
    pub fn run(self) -> PreStepFuture<'a> {
        match self.rest.split_first() {
            Some((first, rest)) => first.decide(
                self.step,
                PreStepNext {
                    rest,
                    step: self.step,
                },
            ),
            None => Box::pin(ready(Ok(PreStepDecision::Enter {
                messages: self.step.messages.clone(),
            }))),
        }
    }
}

/// A listener, with the session it is scoped to, if any.
type Scoped = (Option<SessionId>, Arc<dyn PreStepListener>);

/// The port the pre-step listeners are registered in. The loop provides it
/// and runs it.
#[derive(Default)]
pub struct PreStepHooks {
    listeners: RwLock<Vec<Scoped>>,
}

impl PreStepHooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `listener` at the end of the chain, for every agent when
    /// `scope` is `None`, or for the agent whose session is `scope`, for
    /// the life of the hooks.
    pub fn register(&self, scope: Option<&SessionId>, listener: Arc<dyn PreStepListener>) {
        self.listeners
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((scope.cloned(), listener));
    }

    /// Whether any listener would run for the agent of session `scope`.
    pub fn is_empty_for(&self, scope: &SessionId) -> bool {
        self.chain(scope).is_empty()
    }

    /// Runs the chain for `step`, first listener first.
    pub fn run<'a>(&self, step: &'a PreStep) -> PreStepFuture<'a> {
        let chain = self.chain(step.agent.id());
        Box::pin(async move { PreStepNext { rest: &chain, step }.run().await })
    }

    fn chain(&self, scope: &SessionId) -> Vec<Arc<dyn PreStepListener>> {
        self.listeners
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(owner, _)| owner.as_ref().is_none_or(|owner| owner == scope))
            .map(|(_, listener)| Arc::clone(listener))
            .collect()
    }
}

impl fmt::Debug for PreStepHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self
            .listeners
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        f.debug_struct("PreStepHooks")
            .field("listeners", &count)
            .finish()
    }
}
