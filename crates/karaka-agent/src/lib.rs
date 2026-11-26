//! The agent seam: the [`Agent`] trait a replaceable main loop implements,
//! and the [`Agents`] port callers get each session's agent through.
//! (dsh: `packages/core/agent`, `dsh-agent`.)
//!
//! An agent drives one session. Callers queue what the user said with
//! [`Agent::followup`], input for the next step with [`Agent::steer`] and
//! [`Agent::inject`], stop the running turn with [`Agent::cancel`], and
//! wait for the agent to settle with [`Agent::when_idle`]. What is queued
//! is durable: each change to it is an `agent/inbox/spliced` event in the
//! session's log (`inbox.rs`). Everything the
//! agent does goes into the session's log, so that is where callers read
//! the answer: an agent returns nothing itself.
//!
//! The default loop lives in `karaka-agent-loop`. Another loop can replace
//! it by providing [`Agents`] instead, and nothing that uses this seam
//! changes.
//!
//! The seam also holds the loop's first extension point:
//! [`RequestErrorHooks`], which a failed model request runs so a plugin
//! such as `karaka-retry` can ask for it to be tried again
//! (`request_error.rs`). Its payload can wait on the turn's
//! [`Cancellation`] (`cancel.rs`). And each agent publishes live
//! [`AssistantStreamFrame`]s of its model calls, for a view that shows the
//! answer as it is written (`stream.rs`).

mod cancel;
mod inbox;
mod request_error;
mod stream;

pub use cancel::{CancelSwitch, Cancellation, cancellation};
pub use inbox::{InboxError, SpliceCause};
pub use request_error::{
    HandlerError, HookFuture, Next, RequestError, RequestErrorAction, RequestErrorHandler,
    RequestErrorHooks,
};
pub use stream::{
    AssistantStreamFrame, FrameListener, FrameListeners, FrameSubscription, SettledAs, StreamChunk,
    StreamOutcome,
};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use karaka_session::{CancelCause, SessionError, SessionId, UserMessage};

/// One session's agent: the loop that turns what the user says into turns
/// of the session. (dsh: `Agent` in `agent/src/types.ts`, with the live
/// methods `runtime-types.ts` adds to it.)
///
/// dsh's agent has more: an inbox to edit what is queued, a status,
/// maintenance tasks, and its own plugin scope. Karaka adds them as the
/// loop needs them.
///
/// Queued input is durable: it is the session's `inbox`, folded from its
/// `agent/inbox/spliced` events, so it survives a restore. A restored agent
/// doesn't start on it by itself, as in dsh: it waits for the next
/// followup or steer, which runs it first.
pub trait Agent: Send + Sync {
    /// The session this agent drives. An agent and its session share their
    /// id, as in dsh.
    fn id(&self) -> &SessionId;

    /// Queues `message` as the input of a turn of its own, and starts the
    /// agent if it is idle. A message sent while a turn runs waits for that
    /// turn to end. Messages run in the order they were queued, so a
    /// followup to an idle agent with messages still queued runs those
    /// first. (dsh: `followup`.)
    ///
    /// # Errors
    ///
    /// A message with the id of one still pending, or one the log refuses.
    /// Nothing is queued then.
    fn followup(&self, message: UserMessage) -> Result<(), InboxError>;

    /// Queues `message` for the nearest step, and starts the agent if it is
    /// idle. A running agent takes it at its next step boundary, so the
    /// turn goes on for it; an idle one starts a turn with it. (dsh:
    /// `steer`.)
    ///
    /// # Errors
    ///
    /// As [`followup`](Self::followup).
    fn steer(&self, message: UserMessage) -> Result<(), InboxError>;

    /// Queues `message`, model-facing context, for the next step without
    /// starting the agent. A running agent takes it at the nearest later
    /// step boundary; an idle one keeps it until a followup or a steer
    /// starts a turn. It may miss a step that has already taken its input.
    /// (dsh: `inject`.)
    ///
    /// # Errors
    ///
    /// As [`followup`](Self::followup).
    fn inject(&self, message: UserMessage) -> Result<(), InboxError>;

    /// Drops every queued message, durably, and stops the running turn,
    /// which ends with `cause` as its reason. With no turn running it only
    /// empties the queue: it doesn't stop the next turn. Only the first
    /// cause for a turn counts. (dsh: `cancel`, without `keepInbox`.)
    fn cancel(&self, cause: CancelCause);

    /// Resolves once the agent is idle: no driver is running its turns.
    /// (dsh: `whenIdle`.)
    ///
    /// Idle doesn't mean nothing is queued. A turn that ends with an error
    /// stops the driver, as in dsh, and any messages queued behind it wait
    /// for the next followup, which runs them first, oldest first. A
    /// followup sent before the call is waited for, along with every turn
    /// its driver runs.
    fn when_idle(&self) -> WhenIdle<'_>;

    /// Calls `listener` with every live frame of this agent's model calls,
    /// in order, until the returned subscription is dropped. (dsh: the
    /// agent-scoped `agent/assistant-stream` event.) See `stream.rs` for
    /// what a listener may do.
    fn subscribe(&self, listener: FrameListener) -> FrameSubscription;
}

/// The future [`Agent::when_idle`] returns. Boxed so the trait works as
/// `dyn Agent`, as the kernel boxes a plugin's setup.
pub type WhenIdle<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// The port callers get each session's agent through.
///
/// Karaka serves many connections, and each session gets an agent of its
/// own, made the first time someone asks for it. dsh splits this in two: the
/// `agents` registry in `dsh-agent` looks agents up, and the loop package
/// plugs in a factory that makes them (`AgentRegistry` and `AgentFactory` in
/// `agent/src/index.ts`). The kernel lets the loop's plugin provide this
/// port itself, so here one trait does both, and the plugin that makes the
/// agents is the one that keeps them.
///
/// dsh also tells creating a session from resuming a stored one, and lets
/// the creator dispose of the agent. Here [`open`](Self::open) covers both,
/// since the session store already holds restored sessions, and agents live
/// as long as the runtime.
pub trait Agents: Send + Sync {
    /// The agent for the session `id`. The first call makes it, starting the
    /// session if the store has none. Every later call gives back the same
    /// agent, so two connections to one session share its turns.
    ///
    /// A session the store already holds, such as one restored from
    /// storage, may end in a turn a crash left open. The agent closes that
    /// turn before it is returned, as dsh's resume repairs the log.
    ///
    /// # Errors
    ///
    /// The closing events don't fit the log, or the log's queued input
    /// doesn't fold, as dsh refuses to read an invalid inbox history.
    fn open(&self, id: SessionId) -> Result<Arc<dyn Agent>, OpenError>;
}

/// Why [`Agents::open`] failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The session refused the events that close a turn a crash left open.
    Session(SessionError),
    /// The session's queued input doesn't fold.
    Inbox(InboxError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(error) => error.fmt(f),
            Self::Inbox(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Inbox(error) => Some(error),
        }
    }
}

impl From<SessionError> for OpenError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<InboxError> for OpenError {
    fn from(error: InboxError) -> Self {
        Self::Inbox(error)
    }
}

impl From<karaka_session::LogError> for OpenError {
    fn from(error: karaka_session::LogError) -> Self {
        Self::Session(SessionError::Log(error))
    }
}
