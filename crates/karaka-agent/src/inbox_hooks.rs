//! The inbox signals: who hears a message leave an agent's inbox. (dsh:
//! the `agent/inbox/claimed` and `agent/inbox/discarded` events, declared
//! in `agent/src/runtime-types.ts:299-307` and emitted by
//! `agent-loop/src/inbox.ts:112,237`.)
//!
//! A message leaves the inbox in one of two ways. A step claims it, as its
//! input, or a cancel drops it. dsh's inbox can also drop messages by
//! editing the inbox (`splice`, `replace`, `remove`); Karaka's loop has no
//! such editing yet, so a cancel is the only discard. dsh's third signal,
//! `agent/inbox/inserted`, has no listener in what Karaka ports, so it
//! isn't here.
//!
//! As [`RequestErrorHooks`](crate::RequestErrorHooks), this is a small
//! registry port the loop provides: plugins register listeners during
//! their setup and keep them for the life of the runtime.
//!
//! # When they fire
//!
//! `claimed` fires once per message a step takes, in the order the step
//! takes them (next-step first, then the turn's prompt), with the step's
//! turn, after the claim is logged and after the turn's `turn/start`.
//! `discarded` fires once per message a cancel drops, next-step first,
//! after the drop is logged. dsh fires both inside the inbox's mutation;
//! here they fire just after it, once the agent's lock is released, so a
//! listener may call back into the agent.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use karaka_session::UserMessage;

use crate::Agent;

/// One listener of the inbox signals. Both methods do nothing by default,
/// so a listener implements only what it hears.
///
/// A listener runs on the thread that took or dropped the message, so it
/// should be quick, and must not block.
pub trait InboxListener: Send + Sync {
    /// A step of `agent`'s turn `turn` took `message`. (dsh:
    /// `agent/inbox/claimed`, `{ agent, message, turn }`.)
    fn claimed(&self, agent: &dyn Agent, message: &UserMessage, turn: u64) {
        let _ = (agent, message, turn);
    }

    /// `agent` dropped `message` without running it. (dsh:
    /// `agent/inbox/discarded`, `{ agent, message }`.)
    fn discarded(&self, agent: &dyn Agent, message: &UserMessage) {
        let _ = (agent, message);
    }
}

/// The port the inbox listeners are registered in. The loop provides it
/// and fires it.
///
/// One `RwLock` guards the list, held only to add a listener or to copy
/// the list out before firing, so a listener may register another.
#[derive(Default)]
pub struct InboxHooks {
    listeners: RwLock<Vec<Arc<dyn InboxListener>>>,
}

impl InboxHooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `listener`, for the life of the hooks.
    pub fn register(&self, listener: Arc<dyn InboxListener>) {
        self.listeners
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(listener);
    }

    /// Tells every listener that `agent`'s turn `turn` took `message`.
    pub fn claimed(&self, agent: &dyn Agent, message: &UserMessage, turn: u64) {
        for listener in self.copy() {
            listener.claimed(agent, message, turn);
        }
    }

    /// Tells every listener that `agent` dropped `message`.
    pub fn discarded(&self, agent: &dyn Agent, message: &UserMessage) {
        for listener in self.copy() {
            listener.discarded(agent, message);
        }
    }

    /// How many listeners there are.
    pub fn len(&self) -> usize {
        self.listeners
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn copy(&self) -> Vec<Arc<dyn InboxListener>> {
        self.listeners
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl fmt::Debug for InboxHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboxHooks")
            .field("listeners", &self.len())
            .finish()
    }
}
