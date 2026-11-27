//! The gate before a tool runs: the `tools/pre-execute` chain, and the
//! approval seam its `ask` decision goes through. (dsh: the
//! `tools/pre-execute` waterfall and `PreToolDecision` in
//! `tools/src/index.ts`, `prepareExecution` and `serviceAsk`.)
//!
//! # The chain
//!
//! Each listener gets the pending call and a [`PreExecuteNext`], and
//! decides, or hands the call on. The end of the chain allows. A listener
//! is global, or scoped to one agent by its session's id, as the other
//! registry ports scope theirs, and they run in the order they were added.
//! A call made outside any agent runs only the global ones. A listener
//! that fails fails the call, `Error: <message>`, and the tool doesn't run.
//!
//! # Ask
//!
//! An `ask` decision runs the call only if the approval seam grants it,
//! once. With no seam, or no agent to ask for, it is denied; otherwise each
//! outcome but the grant denies with its own reason, so the model can tell
//! a person's "no" from a missing approval channel. The registry knows the
//! seam only as the [`ToolApproval`] trait, which `karaka-user-approval`
//! implements, so this crate doesn't depend on that one.
//!
//! # Cancelling
//!
//! dsh checks the call's signal before the chain, after it, and before
//! the tool. A Karaka tool call has no signal: a cancel drops the call's
//! future at whatever it is awaiting, the chain or the approval included,
//! and nothing after that runs.

use std::collections::BTreeMap;
use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_agent::Agent;
use karaka_session::SessionId;
use serde_json::Value;

/// What a listener decides about a call. (dsh: `PreToolDecision`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreToolDecision {
    /// Run it.
    Allow,
    /// Don't, and tell the model `reason`.
    Deny { reason: String },
    /// Don't, as a cancellation rather than a policy's denial: the result is
    /// the one a cancel before the tool runs gives.
    Cancel,
    /// Run it only if the approval seam grants it. `reason` is what the
    /// approval logs; `display_reason` only what a UI shows.
    Ask {
        reason: Option<String>,
        display_reason: Option<DisplayReason>,
    },
}

/// A reason, localized for display only. (dsh: `displayReason`, `{ en,
/// [locale]: string }`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayReason {
    pub en: String,
    /// Other locales, by their tag.
    pub others: BTreeMap<String, String>,
}

impl DisplayReason {
    /// A reason in English only.
    pub fn en(text: impl Into<String>) -> Self {
        Self {
            en: text.into(),
            others: BTreeMap::new(),
        }
    }
}

/// The pending call a listener decides about. (dsh: `ToolExecution`, its
/// identity and arguments.)
#[derive(Clone)]
pub struct ToolExecution {
    pub call_id: String,
    pub name: String,
    /// The arguments as parsed: empty text is `{}`, and text that isn't JSON
    /// is the string itself, as dsh's loop hands it on. They are checked
    /// against the tool's parameters only after the gate.
    pub arguments: Value,
    /// The agent whose turn made the call; `None` outside any agent.
    pub agent: Option<Arc<dyn Agent>>,
}

impl fmt::Debug for ToolExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolExecution")
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field("arguments", &self.arguments)
            .field("agent", &self.agent.as_ref().map(|agent| agent.id()))
            .finish()
    }
}

/// Why a listener failed.
pub type PreExecuteError = Box<dyn std::error::Error + Send + Sync>;

/// What a listener, and the rest of a chain, resolve to.
pub type PreExecuteFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PreToolDecision, PreExecuteError>> + Send + 'a>>;

/// One listener of the gate. (dsh: a `tools/pre-execute` listener.)
pub trait PreExecuteListener: Send + Sync {
    /// Decides about `call`, or hands it on with `next.run()`.
    fn decide<'a>(
        &'a self,
        call: &'a ToolExecution,
        next: PreExecuteNext<'a>,
    ) -> PreExecuteFuture<'a>;
}

/// The rest of a chain, after the listener it was given to.
pub struct PreExecuteNext<'a> {
    rest: &'a [Arc<dyn PreExecuteListener>],
    call: &'a ToolExecution,
}

impl<'a> PreExecuteNext<'a> {
    /// Runs the rest of the chain, which allows once it is empty.
    pub fn run(self) -> PreExecuteFuture<'a> {
        match self.rest.split_first() {
            Some((first, rest)) => first.decide(
                self.call,
                PreExecuteNext {
                    rest,
                    call: self.call,
                },
            ),
            None => Box::pin(ready(Ok(PreToolDecision::Allow))),
        }
    }
}

/// A listener in the gate, which [`remove`](Self::remove) takes out.
/// Dropping it leaves the listener in.
#[must_use = "dropping a registration keeps the listener; call `remove` to take it out"]
pub struct PreExecuteRegistration {
    listeners: Weak<Mutex<Listeners>>,
    id: u64,
}

impl PreExecuteRegistration {
    /// Takes the listener out. (dsh: the listener's disposer.)
    pub fn remove(self) {
        if let Some(listeners) = self.listeners.upgrade() {
            lock(&listeners).list.retain(|(id, _, _)| *id != self.id);
        }
    }
}

impl fmt::Debug for PreExecuteRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreExecuteRegistration")
            .field("id", &self.id)
            .finish()
    }
}

#[derive(Default)]
pub(crate) struct Listeners {
    next: u64,
    list: Vec<(u64, Option<SessionId>, Arc<dyn PreExecuteListener>)>,
}

/// One approval question about a call. (dsh: the `ApprovalRequest`
/// `serviceAsk` builds.)
#[derive(Clone)]
pub struct ToolApprovalRequest {
    pub agent: Arc<dyn Agent>,
    pub tool_name: String,
    pub call_id: String,
    pub reason: Option<String>,
    pub display_reason: Option<DisplayReason>,
}

impl fmt::Debug for ToolApprovalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolApprovalRequest")
            .field("agent", self.agent.id())
            .field("tool_name", &self.tool_name)
            .field("call_id", &self.call_id)
            .field("reason", &self.reason)
            .field("display_reason", &self.display_reason)
            .finish()
    }
}

/// How an approval question ended. (dsh: `ApprovalOutcome`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

/// What an approval resolves to. An error, such as a question asked
/// outside an open turn, fails the call with its message.
pub type ToolApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolApprovalOutcome, String>> + Send + 'a>>;

/// The approval seam an `ask` decision goes through. (dsh: `ctx.approval`,
/// which `serviceAsk` takes when it is there.)
pub trait ToolApproval: Send + Sync {
    fn request<'a>(&'a self, request: ToolApprovalRequest) -> ToolApprovalFuture<'a>;
}

/// The gate's listeners and approval seam, shared with each call's future.
#[derive(Default)]
pub(crate) struct Gate {
    pub(crate) listeners: Arc<Mutex<Listeners>>,
    pub(crate) approval: Mutex<Option<Arc<dyn ToolApproval>>>,
}

impl Gate {
    pub(crate) fn listen(
        &self,
        scope: Option<&SessionId>,
        listener: Arc<dyn PreExecuteListener>,
    ) -> PreExecuteRegistration {
        let mut listeners = lock(&self.listeners);
        listeners.next += 1;
        let id = listeners.next;
        listeners.list.push((id, scope.cloned(), listener));
        PreExecuteRegistration {
            listeners: Arc::downgrade(&self.listeners),
            id,
        }
    }

    /// The listeners a call from the agent of session `scope` runs.
    pub(crate) fn chain(&self, scope: Option<&SessionId>) -> Vec<Arc<dyn PreExecuteListener>> {
        lock(&self.listeners)
            .list
            .iter()
            .filter(|(_, owner, _)| owner.is_none() || owner.as_ref() == scope)
            .map(|(_, _, listener)| Arc::clone(listener))
            .collect()
    }

    pub(crate) fn approval(&self) -> Option<Arc<dyn ToolApproval>> {
        lock(&self.approval).clone()
    }
}

/// What the gate decided, once `ask` is resolved.
pub(crate) enum Passage {
    Run,
    /// The text of an error result.
    Stop(String),
}

/// dsh's `toolAbortedBeforeDispatchResult` text.
pub(crate) const ABORTED_BEFORE_DISPATCH: &str = "Error: tool call aborted before dispatch";

/// Runs the chain for `call`, then resolves `ask` through `approval`.
/// (dsh: the gate half of `prepareExecution`, and `serviceAsk`.)
pub(crate) async fn pass(
    chain: Vec<Arc<dyn PreExecuteListener>>,
    approval: Option<Arc<dyn ToolApproval>>,
    call: &ToolExecution,
) -> Passage {
    let decision = match (PreExecuteNext { rest: &chain, call }).run().await {
        Ok(decision) => decision,
        Err(error) => return Passage::Stop(format!("Error: {error}")),
    };
    let decision = match decision {
        PreToolDecision::Ask {
            reason,
            display_reason,
        } => match ask(approval, call, reason, display_reason).await {
            Ok(decision) => decision,
            Err(message) => return Passage::Stop(format!("Error: {message}")),
        },
        decision => decision,
    };
    match decision {
        PreToolDecision::Allow => Passage::Run,
        PreToolDecision::Deny { reason } => Passage::Stop(format!("Error: {reason}")),
        PreToolDecision::Cancel => Passage::Stop(ABORTED_BEFORE_DISPATCH.into()),
        PreToolDecision::Ask { .. } => unreachable!("an ask is resolved above"),
    }
}

/// Resolves an `ask` to allow or deny. (dsh: `serviceAsk`.)
async fn ask(
    approval: Option<Arc<dyn ToolApproval>>,
    call: &ToolExecution,
    reason: Option<String>,
    display_reason: Option<DisplayReason>,
) -> Result<PreToolDecision, String> {
    let name = &call.name;
    let deny = |reason: String| Ok(PreToolDecision::Deny { reason });
    let Some(approval) = approval else {
        return deny(
            reason.unwrap_or_else(|| {
                format!("tool \"{name}\" requires approval (not yet supported)")
            }),
        );
    };
    let Some(agent) = call.agent.clone() else {
        return deny(format!(
            "tool \"{name}\" requires approval, but the call has no agent to route it through"
        ));
    };
    let outcome = approval
        .request(ToolApprovalRequest {
            agent,
            tool_name: name.clone(),
            call_id: call.call_id.clone(),
            reason,
            display_reason,
        })
        .await?;
    match outcome {
        ToolApprovalOutcome::AllowedOnce => Ok(PreToolDecision::Allow),
        ToolApprovalOutcome::Rejected => deny(format!("the user rejected tool \"{name}\"")),
        ToolApprovalOutcome::Cancelled => {
            deny(format!("approval for tool \"{name}\" was cancelled"))
        }
        ToolApprovalOutcome::Unavailable => deny(format!(
            "tool \"{name}\" requires approval, but no approval channel is available"
        )),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every change is one push, removal or swap, so the state is whole even
    // after a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
