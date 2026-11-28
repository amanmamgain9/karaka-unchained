//! The service: asking, the answerer chain, timed waits, and late replies.
//! (dsh: `UserQuestionService` in `user-questions/src/index.ts`.)
//!
//! # The answerer chain
//!
//! dsh asks through the Cordis waterfall `user-questions/request`: each
//! listener answers, or hands the request to the next, and the end of the
//! chain fails with `NO_PROVIDER`. Here the chain is the service's own
//! list. An answerer is global, or filed under a scope (`karaka-scope`),
//! as dsh scopes a listener. A request from an agent runs the global
//! answerers and those of the scopes on its chain, in the order they were
//! registered; one with no agent runs only the global ones.
//!
//! # The calling agent
//!
//! A request from an agent is refused unless that agent is the live one
//! the [`Agents`] registry holds for its id (`CALLER_NOT_LIVE`). dsh
//! compares the objects. A Rust agent can be reached through more than one
//! `Arc`: the loop's tool calls get the driver's own handle, not the one
//! [`Agents::open`] gave. So the comparison is the id and
//! [`Agent::instance`], which every handle on one agent shares, and which
//! another agent reusing the id doesn't. dsh also refuses an agent owned
//! by another live agent (`DELEGATED_CALLER`); Karaka has no child agents
//! yet, so every live agent is a root, and the check isn't made.
//!
//! # Late replies
//!
//! A timed question whose window closed stays answerable, as `continued`
//! in the `userQuestions` projection. [`UserQuestions::answer`] steers the
//! answer into the agent as a user message whose source names the call;
//! that message, once admitted, is what settles the question. Until then
//! the reply is queued, and a second is refused (`REPLY_QUEUED`). The
//! service tracks the queued reply from the inbox signals: a step claiming
//! it notes its turn, a cancel dropping it, or its `user/message` being
//! logged, releases it, and so does the end of the turn that claimed it
//! without admitting it.

use std::collections::HashMap;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use karaka_agent::{Agent, Agents, InboxHooks, InboxListener};
use karaka_fs::AbortSignal;
use karaka_scope::{ScopeChain, ScopeKey};
use karaka_session::{
    Appended, Entry, FlushFuture, Session, SessionEvent, SessionId, SessionListener, SessionStore,
    UserMessage,
};
use karaka_session_projection::SessionProjections;
use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;

use crate::error::{
    BAD_ANSWER, BAD_INTENT, BAD_TIMEOUT, Cause, DUPLICATE_WAIT, EMPTY_QUESTIONS, NO_PROVIDER,
    QuestionError, REPLY_QUEUED, UserQuestionError,
};
use crate::projection::{ASK_USER_QUESTION_TOOL, UserQuestionsProjection};
use crate::timed_wait::{TimedQuestionWait, WaitAttachment, WaitEnd};
use crate::types::{
    AskUserQuestionAnswer, AskUserQuestionAnswerItem, AskUserQuestionItem, PendingUserQuestion,
    QuestionRequest, QuestionWait, TimedUserQuestionResult, UserQuestionState, reply_call_id,
    reply_call_id_of, reply_source,
};

/// The longest window a timed ask may have, the most a platform timer
/// holds. (dsh: `2_147_483_647`, the limit of `setTimeout`.)
pub const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// What an answerer, and the rest of a chain, resolve to.
pub type AnswerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AskUserQuestionAnswer, QuestionError>> + Send + 'a>>;

/// One answerer in the chain: a UI that puts the question to the user.
/// (dsh: a `user-questions/request` listener.)
pub trait QuestionAnswerer: Send + Sync {
    /// Answers `request`, or hands it on with `next.run()`. A request with
    /// a signal should end when it fires. An answerer of a timed request
    /// ends its wait by failing with `ASK_TIMED_OUT`.
    fn answer<'a>(&'a self, request: &'a QuestionRequest, next: AnswerNext<'a>)
    -> AnswerFuture<'a>;
}

/// The rest of a chain, after the answerer it was given to.
pub struct AnswerNext<'a> {
    rest: &'a [Arc<dyn QuestionAnswerer>],
    request: &'a QuestionRequest,
}

impl<'a> AnswerNext<'a> {
    /// Runs the rest of the chain, which fails with `NO_PROVIDER` once it
    /// is empty.
    pub fn run(self) -> AnswerFuture<'a> {
        match self.rest.split_first() {
            Some((first, rest)) => first.answer(
                self.request,
                AnswerNext {
                    rest,
                    request: self.request,
                },
            ),
            None => Box::pin(ready(Err(UserQuestionError::no_provider().into()))),
        }
    }
}

/// An answerer in the chain, which [`remove`](Self::remove) takes out.
/// Dropping it leaves the answerer in, as a dsh listener stays until its
/// disposer runs.
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
    list: Vec<(u64, Option<ScopeKey>, Arc<dyn QuestionAnswerer>)>,
}

/// A queued late reply. (dsh: `QueuedReply`.)
struct QueuedReply {
    message_id: String,
    /// The turn whose step claimed it, once one has.
    claimed_turn: Option<u64>,
}

/// The queued late replies, by session and call. (dsh: `queuedReplies`,
/// a `WeakMap` by session; here by the session's id.) It hears the inbox
/// signals and the session's events.
#[derive(Default)]
struct Replies(Mutex<HashMap<SessionId, HashMap<String, QueuedReply>>>);

impl Replies {
    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, HashMap<String, QueuedReply>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has(&self, session: &SessionId, call_id: &str) -> bool {
        self.lock()
            .get(session)
            .is_some_and(|calls| calls.contains_key(call_id))
    }

    /// Queues `message_id` for the call, unless a reply is already queued.
    fn reserve(&self, session: &SessionId, call_id: &str, message_id: &str) -> bool {
        let mut sessions = self.lock();
        let calls = sessions.entry(session.clone()).or_default();
        if calls.contains_key(call_id) {
            return false;
        }
        calls.insert(
            call_id.to_owned(),
            QueuedReply {
                message_id: message_id.to_owned(),
                claimed_turn: None,
            },
        );
        true
    }

    /// Forgets the call's reply, if it is `message_id`. (dsh:
    /// `releaseReply`.)
    fn release(&self, session: &SessionId, call_id: &str, message_id: &str) {
        let mut sessions = self.lock();
        let Some(calls) = sessions.get_mut(session) else {
            return;
        };
        if calls
            .get(call_id)
            .is_some_and(|reply| reply.message_id == message_id)
        {
            calls.remove(call_id);
            if calls.is_empty() {
                sessions.remove(session);
            }
        }
    }
}

impl InboxListener for Replies {
    fn claimed(&self, agent: &dyn Agent, message: &UserMessage, turn: u64) {
        let Some(call_id) = reply_call_id(&message.source) else {
            return;
        };
        let mut sessions = self.lock();
        let calls = sessions.entry(agent.id().clone()).or_default();
        match calls.get_mut(call_id) {
            // A reply restored with the inbox, which this service never
            // queued, is reserved once a step claims it.
            None => {
                calls.insert(
                    call_id.to_owned(),
                    QueuedReply {
                        message_id: message.id.clone(),
                        claimed_turn: Some(turn),
                    },
                );
            }
            Some(reply) if reply.message_id == message.id => reply.claimed_turn = Some(turn),
            // Another reply for the call doesn't replace the first.
            Some(_) => {}
        }
    }

    fn discarded(&self, agent: &dyn Agent, message: &UserMessage) {
        if let Some(call_id) = reply_call_id(&message.source) {
            self.release(agent.id(), call_id, &message.id);
        }
    }
}

impl SessionListener for Replies {
    fn event(&self, session: &Appended<'_>, entry: &Entry) {
        match &entry.event {
            SessionEvent::UserMessage { message, .. } => {
                if let Some(call_id) = reply_call_id(&message.source) {
                    self.release(session.id(), call_id, &message.id);
                }
            }
            SessionEvent::TurnEnd(end) => {
                let mut sessions = self.lock();
                let Some(calls) = sessions.get_mut(session.id()) else {
                    return;
                };
                // A reply claimed by this turn and not admitted in it won't
                // be: the turn ended without it.
                calls.retain(|_, reply| reply.claimed_turn != Some(end.turn));
                if calls.is_empty() {
                    sessions.remove(session.id());
                }
            }
            _ => {}
        }
    }

    fn flush(&self, _: &SessionId) -> Option<FlushFuture> {
        None
    }
}

/// The user-questions service. (dsh: `ctx.userQuestions`.)
pub struct UserQuestions {
    agents: Arc<dyn Agents>,
    sessions: Arc<SessionStore>,
    projections: Arc<SessionProjections>,
    answerers: Arc<Mutex<Answerers>>,
    /// The live timed waits, by the calling agent's instance and the call.
    /// (dsh: `waits`, by agent object.)
    waits: Mutex<HashMap<(usize, String), TimedQuestionWait>>,
    replies: Arc<Replies>,
}

impl UserQuestions {
    /// A service whose callers `agents` vouches for, whose questions are
    /// read from the `userQuestions` projection of `projections`, which
    /// must have [`UserQuestionsProjection`] registered, and whose late
    /// replies are looked for in its `inbox` projection. It listens to
    /// `hooks` and to `sessions` to track the replies it queues.
    pub fn new(
        agents: Arc<dyn Agents>,
        sessions: Arc<SessionStore>,
        projections: Arc<SessionProjections>,
        hooks: &InboxHooks,
    ) -> Self {
        let replies = Arc::new(Replies::default());
        hooks.register(Arc::clone(&replies) as Arc<dyn InboxListener>);
        sessions.listen(Arc::clone(&replies) as Arc<dyn SessionListener>);
        Self {
            agents,
            sessions,
            projections,
            answerers: Arc::default(),
            waits: Mutex::new(HashMap::new()),
            replies,
        }
    }

    /// Adds `answerer` at the end of the chain: for every request when
    /// `scope` is `None`, or for the requests of the agents whose chain
    /// holds `scope`.
    pub fn answerer(
        &self,
        scope: Option<&ScopeKey>,
        answerer: Arc<dyn QuestionAnswerer>,
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

    /// Asks the answerer chain and waits for the user's answer. (dsh:
    /// `ask`.)
    ///
    /// # Errors
    ///
    /// `ASK_ABORTED` when the request's signal has fired, or fires and the
    /// chain then fails with an error outside the service's terms;
    /// `EMPTY_QUESTIONS`; `CALLER_NOT_LIVE` for an agent that isn't the
    /// registry's live one; `BAD_INTENT` for an intent whose approve label
    /// names none of its question's options, or whose question has no
    /// detail; `NO_PROVIDER` when no answerer took it. Any other failure of
    /// the chain is passed on as it was.
    pub async fn ask(
        &self,
        request: QuestionRequest,
    ) -> Result<AskUserQuestionAnswer, QuestionError> {
        let aborted = |request: &QuestionRequest| {
            request.signal.as_ref().is_some_and(AbortSignal::is_aborted)
        };
        if aborted(&request) {
            return Err(UserQuestionError::aborted(None).into());
        }
        if request.questions.is_empty() {
            return Err(UserQuestionError::new(
                EMPTY_QUESTIONS,
                "ask_user_question requires at least one question",
            )
            .into());
        }
        if let Some(agent) = &request.agent {
            self.assert_live_root(agent.as_ref())?;
        }
        // An intent asserts what the types can't: that its approve label is
        // one of the question's own options, and that a plan-review carries
        // the plan it reviews. Caught here, where the mistake is, rather
        // than in each UI.
        for question in &request.questions {
            let Some(intent) = &question.intent else {
                continue;
            };
            let offered = question
                .options
                .iter()
                .flatten()
                .any(|option| option.label == intent.approve());
            if !offered {
                return Err(UserQuestionError::new(
                    BAD_INTENT,
                    format!(
                        "question {} declares intent {} whose approve label {} names none of \
                         its options",
                        question.id,
                        intent.kind(),
                        json_string(intent.approve())
                    ),
                )
                .into());
            }
            if question.detail.is_none() {
                return Err(UserQuestionError::new(
                    BAD_INTENT,
                    format!(
                        "question {} declares intent {} without the detail it reviews",
                        question.id,
                        intent.kind()
                    ),
                )
                .into());
            }
        }
        let viewer = request
            .agent
            .as_ref()
            .map_or_else(ScopeChain::global, |agent| agent.scope());
        let chain = self.chain(&viewer);
        let result = AnswerNext {
            rest: &chain,
            request: &request,
        }
        .run()
        .await;
        match result {
            Ok(answer) => Ok(answer),
            Err(QuestionError::Other(error)) if aborted(&request) => {
                Err(UserQuestionError::aborted(Some(error)).into())
            }
            Err(error) => Err(error),
        }
    }

    /// Asks with a foreground window of `timeout_ms`, in the tool call
    /// `call_id` of `agent`. The answerers get a request whose wait names
    /// the call as timed, and whose signal is the wait's own: it fires when
    /// `signal`, the turn's, does, or when the window closes with no UI
    /// holding it (see [`attach_wait`](Self::attach_wait)). (dsh:
    /// `askTimed`.)
    ///
    /// The answer, if it comes in time; otherwise the call left pending.
    /// An answerer ends the window itself by failing with `ASK_TIMED_OUT`.
    /// With no answerer, the window is waited out.
    ///
    /// # Errors
    ///
    /// `BAD_TIMEOUT` for a window of 0 or over [`MAX_TIMEOUT_MS`];
    /// `CALLER_NOT_LIVE`; `DUPLICATE_WAIT` when the call already waits;
    /// `ASK_ABORTED` when `signal` fires, or the service is
    /// [closed](Self::close); and whatever [`ask`](Self::ask) fails with.
    pub async fn ask_timed(
        &self,
        agent: Arc<dyn Agent>,
        questions: Vec<AskUserQuestionItem>,
        signal: Option<AbortSignal>,
        call_id: &str,
        timeout_ms: u64,
    ) -> Result<TimedUserQuestionResult, QuestionError> {
        if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
            return Err(UserQuestionError::new(
                BAD_TIMEOUT,
                "timeout must fit a positive platform timer",
            )
            .into());
        }
        self.assert_live_root(agent.as_ref())?;
        let key = (agent.instance(), call_id.to_owned());
        let wait = {
            let mut waits = lock(&self.waits);
            if waits.contains_key(&key) {
                return Err(UserQuestionError::new(
                    DUPLICATE_WAIT,
                    "the question call already has a foreground wait",
                )
                .into());
            }
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let wait = TimedQuestionWait::new(deadline, signal);
            waits.insert(key.clone(), wait.clone());
            wait
        };
        // Closes the wait and forgets it however this ends, dropped
        // included. (dsh: the `finally`.)
        let _forget = Forget {
            waits: &self.waits,
            key,
            wait: wait.clone(),
        };
        let asked = self
            .ask(QuestionRequest {
                questions,
                agent: Some(agent),
                signal: Some(wait.signal()),
                wait: Some(QuestionWait {
                    call_id: call_id.to_owned(),
                    timed: true,
                }),
            })
            .await;
        let error = match asked {
            Ok(answer) => return Ok(TimedUserQuestionResult::Answered(answer)),
            Err(error) => error,
        };
        // Once the wait has ended, why it ended is the outcome. With no
        // answerer, the window is waited out first.
        let error = match wait.end() {
            Some(end) => reason(end),
            None if error.code() == Some(NO_PROVIDER) => {
                wait.done().await;
                reason(wait.end().unwrap_or(WaitEnd::Closed))
            }
            None => Some(error),
        };
        match error {
            Some(error) if error.code() == Some(crate::error::ASK_TIMED_OUT) => {
                Ok(TimedUserQuestionResult::Pending {
                    call_id: call_id.to_owned(),
                })
            }
            error if wait.end().is_some() => {
                Err(UserQuestionError::aborted(error.map(|error| Arc::new(error) as Cause)).into())
            }
            Some(error) => Err(error),
            None => Err(UserQuestionError::aborted(None).into()),
        }
    }

    /// Lets one answer UI hold the timed wait of `agent`'s call `call_id`:
    /// while it does, the host doesn't count the window down. The stream
    /// yields how long the window had left, then ends when `signal` fires,
    /// such as when the client disconnects, or when the wait ends;
    /// dropping it releases the hold too. It yields nothing when the call
    /// has no wait. (dsh: `attachWait`, a remote stream.)
    ///
    /// # Errors
    ///
    /// `CALLER_NOT_LIVE`.
    pub fn attach_wait(
        &self,
        agent: &dyn Agent,
        call_id: &str,
        signal: AbortSignal,
    ) -> Result<WaitAttachment, UserQuestionError> {
        self.assert_live_root(agent)?;
        let wait = lock(&self.waits)
            .get(&(agent.instance(), call_id.to_owned()))
            .cloned();
        Ok(match wait {
            Some(wait) => wait.attach(signal),
            None => WaitAttachment::empty(),
        })
    }

    /// Answers the continued question of `agent`'s call `call_id` with
    /// `answer`, a batch with one item per question of the call. The reply
    /// is steered into the agent as a user message whose source names the
    /// call, and stays queued until the agent admits it. Gives whether the
    /// call was continued and took the reply. (dsh: `answer`.)
    ///
    /// # Errors
    ///
    /// `CALLER_NOT_LIVE`; `REPLY_QUEUED` while a reply for the call waits
    /// to be admitted; `BAD_ANSWER` for a batch that doesn't name each of
    /// the call's questions exactly once; and the agent's refusal of the
    /// steer, after which the reply can be sent again.
    pub fn answer(
        &self,
        agent: &dyn Agent,
        call_id: &str,
        answer: &AskUserQuestionAnswer,
    ) -> Result<bool, QuestionError> {
        self.assert_live_root(agent)?;
        let Some(session) = self.sessions.get(agent.id()) else {
            return Ok(false);
        };
        let Some(question) = self
            .continued(&session)
            .into_iter()
            .find(|question| question.call_id == call_id)
        else {
            return Ok(false);
        };
        let queued =
            || UserQuestionError::new(REPLY_QUEUED, "a reply is already queued for this question");
        if self.replies.has(agent.id(), call_id) || self.inbox_holds_reply(&session, call_id) {
            return Err(queued().into());
        }
        // The model-facing contract promises one item per question, which
        // only this owner of the asked questions can check before the
        // batch reaches the model.
        let mut answered = std::collections::HashSet::new();
        let each_once = answer
            .answers
            .iter()
            .all(|item| answered.insert(item.id.as_str()))
            && question.questions.len() == answer.answers.len()
            && question
                .questions
                .iter()
                .all(|item| answered.contains(item.id.as_str()));
        if !each_once {
            return Err(UserQuestionError::new(
                BAD_ANSWER,
                format!(
                    "the answer batch for {call_id} must name each of its {} questions exactly once",
                    question.questions.len()
                ),
            )
            .into());
        }
        let text = serde_json::to_string(&ReplyText {
            kind: "answer_to_pending_question",
            tool: ASK_USER_QUESTION_TOOL,
            call_id,
            questions: &question.questions,
            answers: &answer.answers,
        })
        .expect("the questions and answers serialize to JSON");
        // dsh's id is random; here it is named after the call and the
        // session's length, as Karaka names its other messages.
        let message = UserMessage::text(
            format!("user-question-reply-{call_id}-{}", session.len()),
            reply_source(call_id),
            text,
        );
        let message_id = message.id.clone();
        if !self.replies.reserve(agent.id(), call_id, &message_id) {
            return Err(queued().into());
        }
        // No lock is held here: the steer logs, and the log's listeners
        // include this service's.
        if let Err(error) = agent.steer(message) {
            self.replies.release(agent.id(), call_id, &message_id);
            return Err(QuestionError::other(error));
        }
        Ok(true)
    }

    /// Ends every live timed wait, whose asks fail with `ASK_ABORTED`.
    /// (dsh: the service's disposal effect.)
    pub fn close(&self) {
        let waits: Vec<TimedQuestionWait> =
            lock(&self.waits).drain().map(|(_, wait)| wait).collect();
        for wait in waits {
            wait.close();
        }
    }

    /// Refuses an agent that isn't the registry's live one for its id.
    /// (dsh: `assertLiveRoot`, without its `DELEGATED_CALLER` check.)
    fn assert_live_root(&self, agent: &dyn Agent) -> Result<(), UserQuestionError> {
        match self.agents.get(agent.id()) {
            Some(live) if live.instance() == agent.instance() => Ok(()),
            _ => Err(UserQuestionError::caller_not_live()),
        }
    }

    /// The answerers a request from an agent seeing `viewer` runs.
    fn chain(&self, viewer: &ScopeChain) -> Vec<Arc<dyn QuestionAnswerer>> {
        lock(&self.answerers)
            .list
            .iter()
            .filter(|(_, owner, _)| viewer.admits(owner.as_ref()))
            .map(|(_, _, answerer)| Arc::clone(answerer))
            .collect()
    }

    /// The session's continued questions. (dsh: `continued`.)
    fn continued(&self, session: &Session) -> Vec<PendingUserQuestion> {
        self.projections
            .state_of::<UserQuestionsProjection>(session)
            .map(|state| {
                state
                    .questions
                    .active
                    .iter()
                    .filter(|question| question.state == UserQuestionState::Continued)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the session's durable inbox holds a reply to `call_id`, as
    /// a restored one may. Read from the `inbox` projection's view.
    fn inbox_holds_reply(&self, session: &Session, call_id: &str) -> bool {
        let snapshot = self.projections.snapshot(session, Some(&["inbox"]));
        let Some(inbox) = snapshot.values.get("inbox") else {
            return false;
        };
        ["next-turn", "next-step"].iter().any(|list| {
            inbox
                .get(list)
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message
                            .get("source")
                            .and_then(reply_call_id_of)
                            .is_some_and(|replied| replied == call_id)
                    })
                })
        })
    }
}

impl std::fmt::Debug for UserQuestions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserQuestions")
            .field("answerers", &lock(&self.answerers).list.len())
            .field("waits", &lock(&self.waits).len())
            .finish_non_exhaustive()
    }
}

/// The reply's text, in dsh's key order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplyText<'a> {
    kind: &'static str,
    tool: &'static str,
    call_id: &'a str,
    questions: &'a [AskUserQuestionItem],
    answers: &'a [AskUserQuestionAnswerItem],
}

/// Why an ended wait failed its ask: the timeout, which becomes the
/// pending result, or the close's `ASK_ABORTED`. A parent's cancel has no
/// reason of its own here. (dsh: `wait.signal.reason`.)
fn reason(end: WaitEnd) -> Option<QuestionError> {
    match end {
        WaitEnd::TimedOut => Some(UserQuestionError::timed_out().into()),
        WaitEnd::ParentAborted => None,
        WaitEnd::Closed => Some(UserQuestionError::aborted(None).into()),
    }
}

/// Closes a timed ask's wait and forgets it, once the ask ends.
struct Forget<'a> {
    waits: &'a Mutex<HashMap<(usize, String), TimedQuestionWait>>,
    key: (usize, String),
    wait: TimedQuestionWait,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.wait.close();
        let mut waits = lock(self.waits);
        // A close of the whole service may have forgotten it already, and
        // a new ask in the same call taken its place.
        if waits
            .get(&self.key)
            .is_some_and(|wait| wait.same(&self.wait))
        {
            waits.remove(&self.key);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every change is one insert, removal or push, so the map is whole
    // even after a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A string as JSON, as dsh's `JSON.stringify` quotes the approve label.
fn json_string(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}
