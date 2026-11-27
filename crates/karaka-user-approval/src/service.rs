//! The service: requests, their audit pair, the answerer chain, and each
//! session's policy. (dsh: `ApprovalService` in `user-approval/src/index.ts`.)
//!
//! # A request
//!
//! [`ApprovalService::request`] needs an open turn in the agent's session:
//! the audit pair must sit inside the turn, the log's commit boundary,
//! since a bare event between turns would read as a crash tail. Outside
//! one it fails before logging anything. Otherwise it logs
//! `approval/asked`, decides, and logs `approval/decided`:
//!
//! 1. A signal that already fired: `cancelled`.
//! 2. The `never` policy: `rejected`, before any answerer, so no answerer
//!    can get ahead of it, whenever it was registered.
//! 3. The answerer chain, raced against the signal. A fired signal wins,
//!    as `cancelled`, and the answerer's late answer is dropped with its
//!    future. An answerer that fails or panics gives `unavailable`, as
//!    does the end of the chain: a missing answerer fails closed.
//!
//! # The chain
//!
//! dsh's `approval/request` waterfall, dispatched to the global listeners
//! and those scoped to the request's agent. Here it is the service's own
//! list, as `karaka-user-questions` keeps its answerers: an answerer is
//! global, or belongs to one agent by its session's id, and they run in
//! the order they were added.
//!
//! # The policy
//!
//! A session's policy is its last `approval/policy`, or else the
//! configured default. dsh reads it back from the log on each use; here a
//! host-only projection, [`ApprovalStateProjection`], folds it, with
//! whether a turn is open, so neither copies the log.

use std::future::{Future, ready};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll};

use karaka_agent::Agent;
use karaka_fs::AbortSignal;
use karaka_session::{
    Entry, LogError, Passthrough, Session, SessionEvent, SessionHeader, SessionId, SessionStore,
    UserMessage, UserSource,
};
use karaka_session_projection::{Projection, SessionProjections};
use karaka_tools::{ToolApproval, ToolApprovalFuture, ToolApprovalOutcome, ToolApprovalRequest};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApprovalError;
use crate::types::{
    ASKED_EVENT, ApprovalAsked, ApprovalDecided, ApprovalOutcome, ApprovalPolicy,
    ApprovalPolicyChange, ApprovalRequest, ApprovalRequestId, DECIDED_EVENT, POLICY_EVENT,
};

/// The runtime-context line for the `never` policy. (dsh: `NEVER_SENTENCE`.)
pub const NEVER_SENTENCE: &str = "Approval prompts are disabled in this session: actions that \
     require approval are rejected automatically — do not request sandbox escalation (do not set \
     `sandbox_permissions`).";

/// The runtime-context line for the `ask` policy. (dsh: `ASK_SENTENCE`.)
pub const ASK_SENTENCE: &str = "Approval policy: ask. Operations that require approval may ask \
     through the configured answerers; without an available answerer, the request fails closed.";

/// Why an answerer failed. It fails the question closed, as `unavailable`.
pub type AnswererError = Box<dyn std::error::Error + Send + Sync>;

/// What an answerer, and the rest of a chain, resolve to.
pub type ApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ApprovalOutcome, AnswererError>> + Send + 'a>>;

/// One answerer in the chain, such as a UI that asks the user. (dsh: an
/// `approval/request` listener.)
pub trait ApprovalAnswerer: Send + Sync {
    /// Decides `request`, or hands it on with `next.run()`.
    fn answer<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        next: ApprovalNext<'a>,
    ) -> ApprovalFuture<'a>;
}

/// The rest of a chain, after the answerer it was given to.
pub struct ApprovalNext<'a> {
    rest: &'a [Arc<dyn ApprovalAnswerer>],
    request: &'a ApprovalRequest,
}

impl<'a> ApprovalNext<'a> {
    /// Runs the rest of the chain, which answers `unavailable` once it is
    /// empty.
    pub fn run(self) -> ApprovalFuture<'a> {
        match self.rest.split_first() {
            Some((first, rest)) => first.answer(
                self.request,
                ApprovalNext {
                    rest,
                    request: self.request,
                },
            ),
            None => Box::pin(ready(Ok(ApprovalOutcome::Unavailable))),
        }
    }
}

/// An answerer in the chain, which [`remove`](Self::remove) takes out.
/// Dropping it leaves the answerer in.
#[must_use = "dropping a registration keeps the answerer; call `remove` to take it out"]
pub struct AnswererRegistration {
    answerers: Weak<Mutex<Answerers>>,
    id: u64,
}

impl AnswererRegistration {
    /// Takes the answerer out. (dsh: the listener's disposer.)
    pub fn remove(self) {
        if let Some(answerers) = self.answerers.upgrade() {
            lock(&answerers).list.retain(|(id, _, _)| *id != self.id);
        }
    }
}

impl std::fmt::Debug for AnswererRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnswererRegistration")
            .field("id", &self.id)
            .finish()
    }
}

#[derive(Default)]
struct Answerers {
    next: u64,
    list: Vec<(u64, Option<SessionId>, Arc<dyn ApprovalAnswerer>)>,
}

/// What the service reads of a session: whether a turn is open, and its
/// policy override.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalState {
    /// A `turn/start` not yet closed by a `turn/end`. (dsh: `hasOpenTurn`.)
    pub open_turn: bool,
    /// The last `approval/policy`. (dsh: `overrideOf`.)
    pub policy: Option<ApprovalPolicy>,
}

/// A host-only projection of each session's [`ApprovalState`]. dsh reads
/// the log back for both; folding them keeps a read from copying it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApprovalStateProjection;

impl Projection for ApprovalStateProjection {
    const KEY: &'static str = "approval";
    type State = ApprovalState;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, _: u64) -> ApprovalState {
        ApprovalState::default()
    }

    fn apply(&self, state: &ApprovalState, entry: &Entry) -> Option<ApprovalState> {
        let next = match &entry.event {
            SessionEvent::TurnStart(_) => ApprovalState {
                open_turn: true,
                ..*state
            },
            SessionEvent::TurnEnd(_) => ApprovalState {
                open_turn: false,
                ..*state
            },
            SessionEvent::Passthrough(Passthrough {
                event_type, data, ..
            }) if event_type == POLICY_EVENT => {
                // One that doesn't read is no override, as dsh's fold only
                // sees accepted events.
                let change: ApprovalPolicyChange = serde_json::from_value(data.clone()).ok()?;
                ApprovalState {
                    policy: Some(change.policy),
                    ..*state
                }
            }
            _ => return None,
        };
        (next != *state).then_some(next)
    }
}

/// Appends a policy override to `session`: the policy in effect until the
/// next switch. A session being set up uses it directly, as there is no
/// policy yet to tell the model about. (dsh: `setApprovalPolicy`, whose
/// check of the policy string is [`ApprovalPolicy`]'s `FromStr` here.)
///
/// # Errors
///
/// The log refused the event.
pub fn set_approval_policy(session: &Session, policy: ApprovalPolicy) -> Result<(), LogError> {
    let change = ApprovalPolicyChange {
        policy,
        source: None,
    };
    session.append(passthrough(POLICY_EVENT, &change))?;
    Ok(())
}

/// A plugin event, kept as a passthrough as the session crate keeps every
/// type it doesn't model.
fn passthrough(event_type: &str, data: &impl Serialize) -> SessionEvent {
    SessionEvent::Passthrough(Passthrough {
        event_type: event_type.into(),
        data: serde_json::to_value(data).expect("approval events serialize to JSON"),
        surface_op: None,
        source_event_seqs: None,
    })
}

/// The approval service. (dsh: `ctx.approval`.)
pub struct ApprovalService {
    sessions: Arc<SessionStore>,
    projections: Arc<SessionProjections>,
    /// The policy of a session with no override. (dsh: `config.policy`.)
    default_policy: ApprovalPolicy,
    answerers: Arc<Mutex<Answerers>>,
}

impl ApprovalService {
    /// A service over the sessions of `sessions`, read through
    /// `projections`, which must have [`ApprovalStateProjection`]
    /// registered, with `default_policy` for a session with no override.
    pub fn new(
        sessions: Arc<SessionStore>,
        projections: Arc<SessionProjections>,
        default_policy: ApprovalPolicy,
    ) -> Self {
        Self {
            sessions,
            projections,
            default_policy,
            answerers: Arc::default(),
        }
    }

    /// Adds `answerer` at the end of the chain: for every request when
    /// `scope` is `None`, or for the requests of the agent whose session is
    /// `scope`.
    pub fn answerer(
        &self,
        scope: Option<&SessionId>,
        answerer: Arc<dyn ApprovalAnswerer>,
    ) -> AnswererRegistration {
        let mut answerers = lock(&self.answerers);
        answerers.next += 1;
        let id = answerers.next;
        answerers.list.push((id, scope.cloned(), answerer));
        AnswererRegistration {
            answerers: Arc::downgrade(&self.answerers),
            id,
        }
    }

    /// Decides `request`, logging the question and its outcome to the
    /// agent's session. See the module docs for the order. The id pairing
    /// the two events is named after the session's length, as Karaka names
    /// its messages; dsh's is random.
    ///
    /// # Errors
    ///
    /// No open turn, before anything is logged; no session for the agent;
    /// or the log refusing either event. The decision itself never fails.
    pub async fn request(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        let session = self.session(request.agent.id())?;
        if !self.state(&session).open_turn {
            return Err(ApprovalError::OutsideTurn);
        }
        let id = ApprovalRequestId(format!("approval-{}", session.len()));
        session.append(passthrough(
            ASKED_EVENT,
            &ApprovalAsked {
                id: id.clone(),
                tool_name: request.tool_name.clone(),
                call_id: request.call_id.clone(),
                reason: request.reason.clone(),
            },
        ))?;
        let outcome = self.decide(request, &session).await;
        session.append(passthrough(DECIDED_EVENT, &ApprovalDecided { id, outcome }))?;
        Ok(outcome)
    }

    /// The policy every request for `session` is decided under now: its
    /// override, or the default. (dsh: `effectivePolicy`.)
    pub fn effective_policy(&self, session: &Session) -> ApprovalPolicy {
        self.override_of(session).unwrap_or(self.default_policy)
    }

    /// The session's last `approval/policy`, with no default. (dsh:
    /// `overrideOf`.)
    pub fn override_of(&self, session: &Session) -> Option<ApprovalPolicy> {
        self.state(session).policy
    }

    /// Switches a live agent's policy, and queues the change's notice for
    /// its next step. Nothing happens when the policy is already in effect.
    /// (dsh: `setPolicy`.)
    ///
    /// # Errors
    ///
    /// No session for the agent, the log refusing the event, or the agent
    /// refusing the notice, which is then not queued though the switch is
    /// logged, as in dsh.
    pub fn set_policy(
        &self,
        agent: &dyn Agent,
        policy: ApprovalPolicy,
    ) -> Result<(), ApprovalError> {
        let session = self.session(agent.id())?;
        let previous = self.effective_policy(&session);
        if previous == policy {
            return Ok(());
        }
        set_approval_policy(&session, policy)?;
        // dsh's id is random; this one is named after the seq it follows.
        let notice = UserMessage::text(
            format!("user-approval-{}", session.len()),
            UserSource::Other(json!({ "kind": "user-approval" })),
            format!(
                "The approval policy changed from \"{previous}\" to \"{policy}\" (changed by the \
                 user)."
            ),
        );
        agent.inject(notice)?;
        Ok(())
    }

    /// The runtime-context line for the session `id`, or nothing without
    /// one. (dsh: the `approval:policy` context's text.)
    pub fn context_text(&self, id: Option<&SessionId>) -> String {
        let Some(session) = id.and_then(|id| self.sessions.get(id)) else {
            return String::new();
        };
        match self.effective_policy(&session) {
            ApprovalPolicy::Ask => ASK_SENTENCE,
            ApprovalPolicy::Never => NEVER_SENTENCE,
        }
        .to_owned()
    }

    fn session(&self, id: &SessionId) -> Result<Session, ApprovalError> {
        self.sessions
            .get(id)
            .ok_or_else(|| ApprovalError::NoSession(id.clone()))
    }

    fn state(&self, session: &Session) -> ApprovalState {
        self.projections
            .state_of::<ApprovalStateProjection>(session)
            .map(|state| *state)
            .unwrap_or_default()
    }

    /// The decision, which never fails. (dsh: `decide`.)
    async fn decide(&self, request: &ApprovalRequest, session: &Session) -> ApprovalOutcome {
        if request.signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return ApprovalOutcome::Cancelled;
        }
        // Decided here, before the chain, so no answerer, however early,
        // can grant what `never` rejects.
        if self.effective_policy(session) == ApprovalPolicy::Never {
            return ApprovalOutcome::Rejected;
        }
        let chain = self.chain(request.agent.id());
        let answer = async {
            // An answerer that panics, even before its future, fails the
            // question closed, not the caller's call open.
            let started = catch_unwind(AssertUnwindSafe(|| {
                ApprovalNext {
                    rest: &chain,
                    request,
                }
                .run()
            }));
            match started {
                Ok(future) => match CatchUnwind(future).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(_)) | Err(()) => ApprovalOutcome::Unavailable,
                },
                Err(_) => ApprovalOutcome::Unavailable,
            }
        };
        match &request.signal {
            None => answer.await,
            Some(signal) => tokio::select! {
                biased;
                () = signal.wait() => ApprovalOutcome::Cancelled,
                outcome = answer => outcome,
            },
        }
    }

    /// The global answerers and those of the agent of session `scope`.
    fn chain(&self, scope: &SessionId) -> Vec<Arc<dyn ApprovalAnswerer>> {
        lock(&self.answerers)
            .list
            .iter()
            .filter(|(_, owner, _)| owner.as_ref().is_none_or(|owner| owner == scope))
            .map(|(_, _, answerer)| Arc::clone(answerer))
            .collect()
    }
}

impl std::fmt::Debug for ApprovalService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalService")
            .field("default_policy", &self.default_policy)
            .field("answerers", &lock(&self.answerers).list.len())
            .finish_non_exhaustive()
    }
}

/// The tool registry's approval seam: an `ask` decision before a tool call
/// is this service's request, with no signal, as a tool call has none.
/// (dsh: `serviceAsk`'s `approval.request`.) A request that fails, such as
/// one outside an open turn, fails the call with its message.
impl ToolApproval for ApprovalService {
    fn request<'a>(&'a self, request: ToolApprovalRequest) -> ToolApprovalFuture<'a> {
        Box::pin(async move {
            let request = ApprovalRequest {
                agent: request.agent,
                tool_name: request.tool_name,
                call_id: Some(request.call_id),
                reason: request.reason,
                display_reason: request.display_reason,
                signal: None,
            };
            let outcome = ApprovalService::request(self, &request)
                .await
                .map_err(|error| error.to_string())?;
            Ok(match outcome {
                ApprovalOutcome::AllowedOnce => ToolApprovalOutcome::AllowedOnce,
                ApprovalOutcome::Rejected => ToolApprovalOutcome::Rejected,
                ApprovalOutcome::Cancelled => ToolApprovalOutcome::Cancelled,
                ApprovalOutcome::Unavailable => ToolApprovalOutcome::Unavailable,
            })
        })
    }
}

/// Polls a future, turning a panic into `Err(())`.
struct CatchUnwind<F>(F);

impl<F: Future + Unpin> Future for CatchUnwind<F> {
    type Output = Result<F::Output, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| Pin::new(&mut self.0).poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every change is one push or removal, so the list is whole even after
    // a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
