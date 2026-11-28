//! The service: asking, timed waits and late replies. Ported from dsh's
//! `user-questions/tests/user-questions.spec.ts`, where its cases fit.
//!
//! Most cases run the plugin beside a stand-in `Agents` registry whose
//! agents only queue what they are sent, in the session's real inbox, as
//! dsh's `liveAgent` stubs do; the inbox signals are fired by hand, as dsh
//! emits them. The last case runs the real loop.
//!
//! Not ported: the transported-error cases ("restores a transported
//! provider rejection", "restores a transported UserQuestionError", and the
//! namesake and non-`Error` rejections), which are about JavaScript values
//! crossing the typert wire; "registers providers with HMR-safe disposal"'s
//! second dispose, which a consumed registration can't repeat; the
//! `DELEGATED_CALLER` case, since Karaka has no child agents; and "answers
//! nothing when the composition carries no Session projections", since the
//! plugin requires them. dsh's `getTimerCount` checks have no Tokio
//! counterpart.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{
    Agent, Agents, FrameListener, FrameListeners, FrameSubscription, InboxError, InboxHooks,
    OpenError, WhenIdle,
};
use karaka_agent_loop::{AgentLoopPlugin, InboxProjection, LoopConfig};
use karaka_fs::{AbortController, AbortSignal};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId, Runtime};
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, ToolCall, ToolSchema, Usage, stream_of,
};
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_session::{
    CancelCause, Entry, Extra, InboxSplice, InboxTarget, SessionEvent, SessionHeader, SessionId,
    SessionStore, SessionsPlugin, SpliceOutcome, TOOL_OUTCOME_UNKNOWN, UserMessage, UserSource,
};
use karaka_session_projection::{SessionProjections, SessionProjectionsPlugin};
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin};
use karaka_user_questions::{
    ASK_ABORTED, ASK_TIMED_OUT, AnswerFuture, AnswerNext, AskUserQuestionAnswer,
    AskUserQuestionAnswerItem, AskUserQuestionIntent, AskUserQuestionItem, AskUserQuestionOption,
    BAD_ANSWER, BAD_INTENT, BAD_TIMEOUT, CALLER_NOT_LIVE, DUPLICATE_WAIT, EMPTY_QUESTIONS,
    MAX_TIMEOUT_MS, NO_PROVIDER, QuestionAnswerer, QuestionError, QuestionRequest, REPLY_QUEUED,
    TIMED_WAIT_PARAMETER, TimedUserQuestionResult, UserQuestionError, UserQuestionProjectionView,
    UserQuestionState, UserQuestions, UserQuestionsPlugin, UserQuestionsProjection, WaitClaim,
    reply_call_id, reply_source,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::time::advance;

// The stand-in registry and its agents.

/// An agent that only queues what it is sent, in its session's durable
/// inbox, as dsh's `liveAgent` does in its arrays. It never runs a turn.
struct Stub {
    id: SessionId,
    /// The agent's preset, if any: its chain is the preset, then its
    /// session.
    preset: Mutex<Option<PresetId>>,
    sessions: Arc<SessionStore>,
    frames: FrameListeners,
    /// What the inbox holds, to splice at the right place.
    next_turn: Mutex<Vec<UserMessage>>,
    next_step: Mutex<Vec<UserMessage>>,
    /// Every steer, refused or not.
    steers: AtomicUsize,
    injects: AtomicUsize,
    /// Refuse the next steer, as `mockImplementationOnce` throws.
    refuse_steer: AtomicBool,
}

impl Stub {
    fn new(id: &str, sessions: &Arc<SessionStore>) -> Arc<Self> {
        Arc::new(Self {
            id: SessionId::new(id),
            preset: Mutex::new(None),
            sessions: Arc::clone(sessions),
            frames: FrameListeners::new(),
            next_turn: Mutex::new(Vec::new()),
            next_step: Mutex::new(Vec::new()),
            steers: AtomicUsize::new(0),
            injects: AtomicUsize::new(0),
            refuse_steer: AtomicBool::new(false),
        })
    }

    fn list(&self, target: InboxTarget) -> &Mutex<Vec<UserMessage>> {
        match target {
            InboxTarget::NextTurn => &self.next_turn,
            InboxTarget::NextStep => &self.next_step,
        }
    }

    /// Queues `message` at the end of `target`, durably.
    fn queue(&self, target: InboxTarget, message: UserMessage) {
        let mut list = self.list(target).lock().unwrap();
        self.splice(target, list.len(), None, vec![message.clone()], None);
        list.push(message);
    }

    /// Takes the first message of `target` out, as a claim, or as a cancel
    /// with `outcome`.
    fn shift(&self, target: InboxTarget, outcome: Option<SpliceOutcome>) -> UserMessage {
        let mut list = self.list(target).lock().unwrap();
        self.splice(target, 0, Some(1), Vec::new(), outcome);
        list.remove(0)
    }

    fn splice(
        &self,
        target: InboxTarget,
        start: usize,
        removed_count: Option<u64>,
        inserted: Vec<UserMessage>,
        outcome: Option<SpliceOutcome>,
    ) {
        self.sessions
            .append(
                &self.id,
                SessionEvent::InboxSpliced(InboxSplice {
                    target,
                    start: start as u64,
                    removed_count,
                    inserted,
                    outcome,
                    extra: Extra::new(),
                }),
            )
            .unwrap();
    }

    fn queued(&self, target: InboxTarget) -> Vec<UserMessage> {
        self.list(target).lock().unwrap().clone()
    }

    fn steered(&self) -> usize {
        self.steers.load(Ordering::SeqCst)
    }

    fn append(&self, event: SessionEvent) {
        self.sessions.append(&self.id, event).unwrap();
    }
}

impl Agent for Stub {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn scope(&self) -> ScopeChain {
        ScopeChain::new(self.preset.lock().unwrap().clone(), Some(self.id.clone()))
    }

    fn followup(&self, message: UserMessage) -> Result<(), InboxError> {
        self.queue(InboxTarget::NextTurn, message);
        Ok(())
    }

    fn steer(&self, message: UserMessage) -> Result<(), InboxError> {
        self.steers.fetch_add(1, Ordering::SeqCst);
        if self.refuse_steer.swap(false, Ordering::SeqCst) {
            return Err(InboxError::Unavailable("steer failed".into()));
        }
        self.queue(InboxTarget::NextStep, message);
        Ok(())
    }

    fn inject(&self, _: UserMessage) -> Result<(), InboxError> {
        self.injects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn cancel(&self, _: CancelCause) {}

    fn when_idle(&self) -> WhenIdle<'_> {
        Box::pin(async {})
    }

    fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        self.frames.subscribe(listener)
    }
}

/// The stand-in registry: the agents a test entered. (dsh: `AgentRegistry`
/// with `enter`.)
struct Registry {
    sessions: Arc<SessionStore>,
    live: Mutex<HashMap<SessionId, Arc<dyn Agent>>>,
}

impl Registry {
    fn enter(&self, agent: Arc<dyn Agent>) {
        self.live.lock().unwrap().insert(agent.id().clone(), agent);
    }
}

impl Agents for Registry {
    fn open(&self, id: SessionId) -> Result<Arc<dyn Agent>, OpenError> {
        if let Some(agent) = self.get(&id) {
            return Ok(agent);
        }
        let agent: Arc<dyn Agent> = Stub::new(id.as_str(), &self.sessions);
        self.enter(Arc::clone(&agent));
        Ok(agent)
    }

    fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>> {
        self.live.lock().unwrap().get(id).cloned()
    }
}

/// Provides the stand-in registry, the inbox hooks, and the `inbox`
/// projection the loop would register.
struct RegistryPlugin;

impl Plugin for RegistryPlugin {
    fn name(&self) -> &str {
        "stand-in-agents"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![
            PortId::of::<dyn Agents>(),
            PortId::of::<Registry>(),
            PortId::of::<InboxHooks>(),
        ]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<SessionStore>(),
            PortId::of::<SessionProjections>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let _ = ctx.get::<SessionProjections>().register(InboxProjection)?;
        let registry = Arc::new(Registry {
            sessions: ctx.get::<SessionStore>(),
            live: Mutex::new(HashMap::new()),
        });
        ctx.provide(Arc::clone(&registry) as Arc<dyn Agents>);
        ctx.provide(registry);
        ctx.provide(Arc::new(InboxHooks::new()));
        Ok(())
    }
}

struct Bench {
    service: Arc<UserQuestions>,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    projections: Arc<SessionProjections>,
    hooks: Arc<InboxHooks>,
    _runtime: Runtime,
}

async fn bench() -> Bench {
    let runtime = Runtime::start(vec![
        Box::new(UserQuestionsPlugin),
        Box::new(RegistryPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SessionsPlugin),
    ])
    .await
    .expect("the runtime starts");
    Bench {
        service: runtime.get::<UserQuestions>(),
        registry: runtime.get::<Registry>(),
        sessions: runtime.get::<SessionStore>(),
        projections: runtime.get::<SessionProjections>(),
        hooks: runtime.get::<InboxHooks>(),
        _runtime: runtime,
    }
}

impl Bench {
    /// A stub agent with a session of its own, entered as live. (dsh:
    /// `liveAgent` and `ctx.agents.enter(agent, undefined)`.)
    fn live(&self, id: &str) -> Arc<Stub> {
        self.sessions.create(SessionId::new(id)).unwrap();
        let agent = Stub::new(id, &self.sessions);
        self.registry.enter(Arc::clone(&agent) as Arc<dyn Agent>);
        agent
    }

    fn register(&self, answerer: impl QuestionAnswerer + 'static) {
        let _ = self.service.answerer(None, Arc::new(answerer));
    }

    fn view(&self, agent: &Stub) -> UserQuestionProjectionView {
        let session = self.sessions.get(&agent.id).unwrap();
        self.projections
            .state_of::<UserQuestionsProjection>(&session)
            .unwrap()
            .questions
            .clone()
    }
}

// Answerers.

/// Answers every question with `answer`, and keeps what it was asked.
/// (dsh: `provider`.)
struct Provider {
    answer: String,
    seen: Arc<Mutex<Vec<QuestionRequest>>>,
}

fn provider(answer: &str) -> (Provider, Arc<Mutex<Vec<QuestionRequest>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (
        Provider {
            answer: answer.into(),
            seen: Arc::clone(&seen),
        },
        seen,
    )
}

impl QuestionAnswerer for Provider {
    fn answer<'a>(&'a self, request: &'a QuestionRequest, _: AnswerNext<'a>) -> AnswerFuture<'a> {
        self.seen.lock().unwrap().push(request.clone());
        let answers = request
            .questions
            .iter()
            .map(|question| item(&question.id, &[&self.answer]))
            .collect();
        Box::pin(async move { Ok(AskUserQuestionAnswer { answers }) })
    }
}

/// An answerer made of a closure.
struct With<F>(F);

impl<F> QuestionAnswerer for With<F>
where
    F: for<'a> Fn(&'a QuestionRequest, AnswerNext<'a>) -> AnswerFuture<'a> + Send + Sync,
{
    fn answer<'a>(
        &'a self,
        request: &'a QuestionRequest,
        next: AnswerNext<'a>,
    ) -> AnswerFuture<'a> {
        (self.0)(request, next)
    }
}

fn with<F>(answer: F) -> With<F>
where
    F: for<'a> Fn(&'a QuestionRequest, AnswerNext<'a>) -> AnswerFuture<'a> + Send + Sync,
{
    With(answer)
}

/// Counts its calls, and answers nothing.
fn counting(calls: &Arc<AtomicUsize>) -> impl QuestionAnswerer + 'static {
    let calls = Arc::clone(calls);
    with(move |_, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(AskUserQuestionAnswer::default()) })
    })
}

/// Fails with `error`.
fn failing(error: QuestionError) -> impl QuestionAnswerer + 'static {
    with(move |_, _| {
        let error = error.clone();
        Box::pin(async move { Err(error) })
    })
}

#[derive(Debug)]
struct Plain(&'static str);

impl std::fmt::Display for Plain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Plain {}

/// A rejection as a client sends it back: an error in the service's terms.
/// (dsh: `wireRejection`.)
fn rejection(code: &str) -> QuestionError {
    UserQuestionError::new(code, format!("client rejected with {code}")).into()
}

// Questions and answers.

fn item(id: &str, selected: &[&str]) -> AskUserQuestionAnswerItem {
    AskUserQuestionAnswerItem {
        id: id.into(),
        selected: selected.iter().map(|label| (*label).to_owned()).collect(),
        custom: None,
    }
}

fn batch(items: Vec<AskUserQuestionAnswerItem>) -> AskUserQuestionAnswer {
    AskUserQuestionAnswer { answers: items }
}

fn with_options(id: &str, question: &str, labels: &[&str]) -> AskUserQuestionItem {
    AskUserQuestionItem {
        options: Some(
            labels
                .iter()
                .map(|label| AskUserQuestionOption::new(*label))
                .collect(),
        ),
        ..AskUserQuestionItem::new(id, question)
    }
}

fn confirm() -> Vec<AskUserQuestionItem> {
    vec![AskUserQuestionItem::new("confirm", "Proceed?")]
}

fn request(questions: Vec<AskUserQuestionItem>) -> QuestionRequest {
    QuestionRequest::new(questions)
}

fn code(error: &QuestionError) -> &str {
    error.code().unwrap_or("<not a question error>")
}

// UserQuestionService

// "delegates ask requests to the registered provider"
#[tokio::test]
async fn an_ask_reaches_the_registered_answerer() {
    let bench = bench().await;
    let (answerer, seen) = provider("yes");
    bench.register(answerer);
    let questions = vec![with_options("confirm", "Proceed?", &["yes"])];
    let answer = bench.service.ask(request(questions.clone())).await.unwrap();
    assert_eq!(answer, batch(vec![item("confirm", &["yes"])]));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].questions, questions);
    assert!(seen[0].agent.is_none() && seen[0].signal.is_none() && seen[0].wait.is_none());
}

// "rejects ask requests when no provider is registered"
#[tokio::test]
async fn with_no_answerer_an_ask_fails_with_no_provider() {
    let bench = bench().await;
    let error = bench.service.ask(request(confirm())).await.unwrap_err();
    let QuestionError::Question(error) = error else {
        panic!("not a question error: {error}");
    };
    assert_eq!(error.code(), NO_PROVIDER);
    assert_eq!(
        error.message(),
        "no user-questions answerer accepted the request"
    );
}

// "registers providers with HMR-safe disposal"
#[tokio::test]
async fn a_removed_answerer_is_out_of_the_chain() {
    let bench = bench().await;
    let (answerer, _) = provider("approved");
    let registration = bench.service.answerer(None, Arc::new(answerer));
    registration.remove();
    let error = bench.service.ask(request(confirm())).await.unwrap_err();
    assert_eq!(code(&error), NO_PROVIDER);
}

// "delegates through composed answerers"
#[tokio::test]
async fn an_answerer_can_hand_the_request_on() {
    let bench = bench().await;
    let delegated = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&delegated);
    bench.register(with(move |_, next| {
        counted.fetch_add(1, Ordering::SeqCst);
        next.run()
    }));
    let (answerer, _) = provider("second");
    bench.register(answerer);
    let answer = bench
        .service
        .ask(request(vec![with_options(
            "confirm",
            "Proceed?",
            &["second"],
        )]))
        .await
        .unwrap();
    assert_eq!(answer, batch(vec![item("confirm", &["second"])]));
    assert_eq!(delegated.load(Ordering::SeqCst), 1);
}

// The chain of an agent's request is the global answerers and that
// agent's own, in the order they were added; another agent's don't run,
// and a request with no agent runs only the global ones. (dsh: the
// agent-scoped waterfall, `scopeTarget(agent, agent)`.)
#[tokio::test]
async fn an_agents_answerers_hear_only_its_requests() {
    let bench = bench().await;
    let agent = bench.live("scoped");
    let other = bench.live("other");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let note = |name: &'static str| {
        let heard = Arc::clone(&heard);
        Arc::new(with(move |_, next| {
            heard.lock().unwrap().push(name);
            next.run()
        }))
    };
    let _ = bench.service.answerer(None, note("global"));
    let _ = bench
        .service
        .answerer(Some(&ScopeKey::from(agent.id.clone())), note("mine"));
    let _ = bench
        .service
        .answerer(Some(&ScopeKey::from(other.id.clone())), note("other's"));
    let _ = bench.service.answerer(None, note("global again"));
    let ask = |agent: Option<Arc<Stub>>| {
        let service = Arc::clone(&bench.service);
        async move {
            service
                .ask(QuestionRequest {
                    agent: agent.map(|agent| agent as Arc<dyn Agent>),
                    ..request(confirm())
                })
                .await
        }
    };
    assert_eq!(
        code(&ask(Some(Arc::clone(&agent))).await.unwrap_err()),
        NO_PROVIDER
    );
    assert_eq!(*heard.lock().unwrap(), ["global", "mine", "global again"]);
    heard.lock().unwrap().clear();
    ask(None).await.unwrap_err();
    assert_eq!(*heard.lock().unwrap(), ["global", "global again"]);
}

// "fails before reaching the provider when the signal is already aborted"
#[tokio::test]
async fn an_aborted_signal_fails_before_the_answerers() {
    let bench = bench().await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let error = bench
        .service
        .ask(QuestionRequest {
            signal: Some(AbortSignal::aborted()),
            ..request(confirm())
        })
        .await
        .unwrap_err();
    assert_eq!(code(&error), ASK_ABORTED);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// "normalizes an in-flight signal cancellation to ASK_ABORTED"
#[tokio::test]
async fn a_cancel_in_flight_fails_with_ask_aborted_and_its_cause() {
    let bench = bench().await;
    let controller = Arc::new(AbortController::new());
    let aborting = Arc::clone(&controller);
    bench.register(with(move |_, _| {
        let aborting = Arc::clone(&aborting);
        Box::pin(async move {
            aborting.abort();
            Err(QuestionError::other(Plain("This operation was aborted")))
        })
    }));
    let error = bench
        .service
        .ask(QuestionRequest {
            signal: Some(controller.signal()),
            ..request(confirm())
        })
        .await
        .unwrap_err();
    let QuestionError::Question(error) = error else {
        panic!("not a question error: {error}");
    };
    assert_eq!(error.code(), ASK_ABORTED);
    assert_eq!(
        error.message(),
        "ask_user_question was aborted before the user answered"
    );
    assert_eq!(
        error.cause().map(ToString::to_string).as_deref(),
        Some("This operation was aborted")
    );
}

// "preserves a domain rejection when its provider also aborts the signal"
#[tokio::test]
async fn a_question_error_passes_through_an_abort() {
    let bench = bench().await;
    let controller = Arc::new(AbortController::new());
    let aborting = Arc::clone(&controller);
    bench.register(with(move |_, _| {
        aborting.abort();
        Box::pin(async {
            Err(
                UserQuestionError::new("ASK_CANCELLED", "the user cancelled ask_user_question")
                    .into(),
            )
        })
    }));
    let error = bench
        .service
        .ask(QuestionRequest {
            signal: Some(controller.signal()),
            ..request(confirm())
        })
        .await
        .unwrap_err();
    assert_eq!(code(&error), "ASK_CANCELLED");
    assert_eq!(error.to_string(), "the user cancelled ask_user_question");
}

// "preserves an ordinary Error from the provider", and "preserves a
// provider rejection outside the UserQuestionError taxonomy"
#[tokio::test]
async fn any_other_failure_passes_through() {
    let bench = bench().await;
    bench.register(failing(QuestionError::other(Plain("provider failed"))));
    let error = bench.service.ask(request(confirm())).await.unwrap_err();
    assert!(matches!(&error, QuestionError::Other(_)));
    assert_eq!(error.to_string(), "provider failed");
}

// "rejects empty question batches before reaching the provider"
#[tokio::test]
async fn an_empty_batch_is_refused_before_the_answerers() {
    let bench = bench().await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let error = bench.service.ask(request(Vec::new())).await.unwrap_err();
    assert_eq!(code(&error), EMPTY_QUESTIONS);
    assert_eq!(
        error.to_string(),
        "ask_user_question requires at least one question"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// "reaches the provider for a lineage-bearing session resumed as a runtime
// root"
#[tokio::test]
async fn a_live_agent_with_a_lineage_reaches_the_answerers() {
    let bench = bench().await;
    let (answerer, _) = provider("yes");
    bench.register(answerer);
    let id = SessionId::new("resumed-root");
    bench
        .sessions
        .create_with(SessionHeader {
            delegation_depth: 1,
            ..SessionHeader::new(id.clone(), 0)
        })
        .unwrap();
    let agent = Stub::new("resumed-root", &bench.sessions);
    bench.registry.enter(Arc::clone(&agent) as Arc<dyn Agent>);
    let answer = bench
        .service
        .ask(QuestionRequest {
            agent: Some(agent),
            ..request(vec![with_options("confirm", "Proceed?", &["yes"])])
        })
        .await
        .unwrap();
    assert_eq!(answer, batch(vec![item("confirm", &["yes"])]));
}

// "rejects a supplied agent when no live registry can attest it": here, an
// agent the registry never entered.
#[tokio::test]
async fn an_agent_the_registry_doesnt_hold_is_refused() {
    let bench = bench().await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let error = bench
        .service
        .ask(QuestionRequest {
            agent: Some(Stub::new("unattested", &bench.sessions)),
            ..request(confirm())
        })
        .await
        .unwrap_err();
    assert_eq!(code(&error), CALLER_NOT_LIVE);
    assert_eq!(
        error.to_string(),
        "human interaction requires the exact live calling agent when an agent is supplied"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// "rejects a stale agent object that reuses a live id"
#[tokio::test]
async fn a_stale_agent_reusing_a_live_id_is_refused() {
    let bench = bench().await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let _live = bench.live("same-id");
    let error = bench
        .service
        .ask(QuestionRequest {
            agent: Some(Stub::new("same-id", &bench.sessions)),
            ..request(confirm())
        })
        .await
        .unwrap_err();
    assert_eq!(code(&error), CALLER_NOT_LIVE);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

fn plan_review(approve: &str) -> AskUserQuestionIntent {
    AskUserQuestionIntent::PlanReview {
        approve: approve.into(),
        call_id: None,
    }
}

// "rejects an intent whose approve label names none of its own options"
#[tokio::test]
async fn an_intent_must_approve_with_an_offered_option() {
    let bench = bench().await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    // A wrong label among offered options, and no options offered at all.
    for options in [Some(vec![AskUserQuestionOption::new("Approve")]), None] {
        let question = AskUserQuestionItem {
            detail: Some("# Plan".into()),
            options,
            intent: Some(plan_review("Ship it")),
            ..AskUserQuestionItem::new("plan-review", "Approve?")
        };
        let error = bench
            .service
            .ask(request(vec![question]))
            .await
            .unwrap_err();
        assert_eq!(code(&error), BAD_INTENT);
        assert_eq!(
            error.to_string(),
            "question plan-review declares intent plan-review whose approve label \"Ship it\" \
             names none of its options"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// "rejects a plan-review intent on a question carrying no plan to review"
#[tokio::test]
async fn a_plan_review_needs_the_plan() {
    let bench = bench().await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let question = AskUserQuestionItem {
        intent: Some(plan_review("Approve")),
        ..with_options("plan-review", "Approve?", &["Approve", "Keep planning"])
    };
    let error = bench
        .service
        .ask(request(vec![question]))
        .await
        .unwrap_err();
    assert_eq!(code(&error), BAD_INTENT);
    assert_eq!(
        error.to_string(),
        "question plan-review declares intent plan-review without the detail it reviews"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// "passes an intent through once its approve label names an offered
// option"
#[tokio::test]
async fn a_well_formed_intent_reaches_the_answerers() {
    let bench = bench().await;
    let (answerer, seen) = provider("Approve");
    bench.register(answerer);
    let intent = plan_review("Approve");
    let answer = bench
        .service
        .ask(request(vec![
            with_options("plain", "Proceed?", &["Approve"]),
            AskUserQuestionItem {
                detail: Some("# Plan".into()),
                intent: Some(intent.clone()),
                ..with_options("plan-review", "Approve?", &["Approve", "Keep planning"])
            },
        ]))
        .await
        .unwrap();
    assert_eq!(
        answer.answers,
        [
            item("plain", &["Approve"]),
            item("plan-review", &["Approve"])
        ]
    );
    assert_eq!(seen.lock().unwrap()[0].questions[1].intent, Some(intent));
    // An intent reads and writes as dsh's.
    assert_eq!(
        serde_json::to_value(plan_review("Approve")).unwrap(),
        json!({ "kind": "plan-review", "approve": "Approve" })
    );
}

// askTimed

const TIMED_CALL_ID: &str = "ask-timed";

fn timed_questions() -> Vec<AskUserQuestionItem> {
    vec![with_options("scope", "Which scope?", &["Tool only"])]
}

fn tool_only() -> AskUserQuestionAnswer {
    batch(vec![item("scope", &["Tool only"])])
}

fn pending(call_id: &str) -> TimedUserQuestionResult {
    TimedUserQuestionResult::Pending {
        call_id: call_id.into(),
    }
}

type Timed = tokio::task::JoinHandle<Result<TimedUserQuestionResult, QuestionError>>;

/// Starts a timed ask on its own task.
fn ask_timed(
    bench: &Bench,
    agent: &Arc<Stub>,
    signal: Option<AbortSignal>,
    call_id: &str,
    timeout_ms: u64,
) -> Timed {
    let service = Arc::clone(&bench.service);
    let agent = Arc::clone(agent) as Arc<dyn Agent>;
    let call_id = call_id.to_owned();
    tokio::spawn(async move {
        service
            .ask_timed(agent, timed_questions(), signal, &call_id, timeout_ms)
            .await
    })
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

async fn pass(ms: u64) {
    advance(Duration::from_millis(ms)).await;
    settle().await;
}

async fn next<S: Stream<Item = WaitClaim> + Unpin>(stream: &mut S) -> Option<WaitClaim> {
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

// "keeps sibling waits independent and cancels the remaining wait when the
// service unloads": here, when the service is closed.
#[tokio::test(start_paused = true)]
async fn sibling_waits_are_independent_and_a_close_ends_the_rest() {
    let bench = bench().await;
    let agent = bench.live("timed-sibling-waits");
    let first = ask_timed(&bench, &agent, None, TIMED_CALL_ID, 1_000);
    let later = ask_timed(&bench, &agent, None, "ask-later", 2_000);
    settle().await;
    pass(1_000).await;
    assert_eq!(first.await.unwrap().unwrap(), pending(TIMED_CALL_ID));
    let mut claim = bench
        .service
        .attach_wait(agent.as_ref(), "ask-later", AbortSignal::never())
        .unwrap();
    assert_eq!(
        next(&mut claim).await,
        Some(WaitClaim {
            remaining_ms: 1_000
        })
    );
    bench.service.close();
    assert_eq!(code(&later.await.unwrap().unwrap_err()), ASK_ABORTED);
    assert_eq!(next(&mut claim).await, None);
}

// "expires a forwarded request even when no Client ever responds or
// delegates"
#[tokio::test(start_paused = true)]
async fn a_forwarded_request_expires_with_its_window() {
    let bench = bench().await;
    let agent = bench.live("timed-no-client");
    let turn = AbortController::new();
    let forwarded: Arc<Mutex<Option<AbortSignal>>> = Arc::default();
    let kept = Arc::clone(&forwarded);
    bench.register(with(move |request, _| {
        let signal = request
            .signal
            .clone()
            .expect("a timed request has a signal");
        *kept.lock().unwrap() = Some(signal.clone());
        Box::pin(async move {
            signal.wait().await;
            Err(QuestionError::other(Plain("forwarded request aborted")))
        })
    }));
    let result = ask_timed(&bench, &agent, Some(turn.signal()), TIMED_CALL_ID, 5_000);
    settle().await;
    // The chain has the wait's own signal, not the turn's.
    assert!(!forwarded.lock().unwrap().as_ref().unwrap().is_aborted());
    pass(5_000).await;
    assert_eq!(result.await.unwrap().unwrap(), pending(TIMED_CALL_ID));
    assert!(forwarded.lock().unwrap().as_ref().unwrap().is_aborted());
    assert!(!turn.signal().is_aborted());
}

// "keeps a Client-claimed wait open and closes its stream when the answer
// settles"
#[tokio::test(start_paused = true)]
async fn a_claimed_wait_stays_open_until_the_answer() {
    let bench = bench().await;
    let agent = bench.live("timed-claimed");
    let (send, receive) = oneshot::channel::<AskUserQuestionAnswer>();
    let receive = Arc::new(Mutex::new(Some(receive)));
    bench.register(with(move |_, _| {
        let receive = receive.lock().unwrap().take().expect("asked once");
        Box::pin(async move { Ok(receive.await.expect("the test answers")) })
    }));
    let result = ask_timed(&bench, &agent, None, TIMED_CALL_ID, 5_000);
    settle().await;
    let mut stream = bench
        .service
        .attach_wait(agent.as_ref(), TIMED_CALL_ID, AbortSignal::never())
        .unwrap();
    assert_eq!(
        next(&mut stream).await,
        Some(WaitClaim {
            remaining_ms: 5_000
        })
    );
    pass(10_000).await;
    assert!(!result.is_finished());
    let duplicate = bench
        .service
        .ask_timed(
            Arc::clone(&agent) as Arc<dyn Agent>,
            timed_questions(),
            None,
            TIMED_CALL_ID,
            5_000,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&duplicate), DUPLICATE_WAIT);
    assert_eq!(
        duplicate.to_string(),
        "the question call already has a foreground wait"
    );
    send.send(tool_only()).unwrap();
    assert_eq!(
        result.await.unwrap().unwrap(),
        TimedUserQuestionResult::Answered(tool_only())
    );
    assert_eq!(next(&mut stream).await, None);
    let mut after = bench
        .service
        .attach_wait(agent.as_ref(), TIMED_CALL_ID, AbortSignal::never())
        .unwrap();
    assert_eq!(next(&mut after).await, None);
}

// "returns the answer when a Client settles the request inside the window"
#[tokio::test]
async fn an_answer_inside_the_window_is_the_result() {
    let bench = bench().await;
    let agent = bench.live("timed-answered");
    let seen: Arc<Mutex<Vec<QuestionRequest>>> = Arc::default();
    let kept = Arc::clone(&seen);
    bench.register(with(move |request, _| {
        kept.lock().unwrap().push(request.clone());
        Box::pin(async { Ok(tool_only()) })
    }));
    let result = bench
        .service
        .ask_timed(
            Arc::clone(&agent) as Arc<dyn Agent>,
            timed_questions(),
            None,
            TIMED_CALL_ID,
            120_000,
        )
        .await
        .unwrap();
    assert_eq!(result, TimedUserQuestionResult::Answered(tool_only()));
    let wait = seen.lock().unwrap()[0].wait.clone().unwrap();
    assert_eq!(wait.call_id, TIMED_CALL_ID);
    assert!(wait.timed);
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({ "answers": [{ "id": "scope", "selected": ["Tool only"] }] })
    );
}

// "maps the Client deadline rejection to the pending result and rethrows
// any other code"
#[tokio::test]
async fn a_client_timeout_is_pending_and_other_codes_pass_through() {
    let bench = bench().await;
    let agent = bench.live("timed-expired");
    let code_now = Arc::new(Mutex::new(ASK_TIMED_OUT.to_owned()));
    let current = Arc::clone(&code_now);
    bench.register(with(move |_, _| {
        let code = current.lock().unwrap().clone();
        Box::pin(async move { Err(rejection(&code)) })
    }));
    let ask = || {
        bench.service.ask_timed(
            Arc::clone(&agent) as Arc<dyn Agent>,
            timed_questions(),
            None,
            TIMED_CALL_ID,
            120_000,
        )
    };
    let result = ask().await.unwrap();
    assert_eq!(result, pending(TIMED_CALL_ID));
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({ "pending": true, "callId": TIMED_CALL_ID })
    );
    *code_now.lock().unwrap() = "ASK_CANCELLED".into();
    assert_eq!(code(&ask().await.unwrap_err()), "ASK_CANCELLED");
}

// "holds an unclaimed request until the deadline and then returns pending"
#[tokio::test(start_paused = true)]
async fn with_no_answerer_the_window_is_waited_out() {
    let bench = bench().await;
    let agent = bench.live("timed-unclaimed");
    let result = ask_timed(&bench, &agent, None, TIMED_CALL_ID, 5_000);
    settle().await;
    pass(4_999).await;
    assert!(!result.is_finished());
    pass(1).await;
    assert_eq!(result.await.unwrap().unwrap(), pending(TIMED_CALL_ID));
}

// "aborts the hold with the turn signal"
#[tokio::test(start_paused = true)]
async fn the_turns_cancel_ends_the_hold() {
    let bench = bench().await;
    let agent = bench.live("timed-aborted-hold");
    let turn = AbortController::new();
    let result = ask_timed(&bench, &agent, Some(turn.signal()), TIMED_CALL_ID, 5_000);
    settle().await;
    pass(1_000).await;
    turn.abort();
    assert_eq!(code(&result.await.unwrap().unwrap_err()), ASK_ABORTED);
}

// "rethrows a provider failure that is not a question error"
#[tokio::test]
async fn a_timed_ask_passes_other_failures_through() {
    let bench = bench().await;
    let agent = bench.live("timed-provider-crash");
    bench.register(failing(QuestionError::other(Plain("provider crashed"))));
    let error = bench
        .service
        .ask_timed(
            Arc::clone(&agent) as Arc<dyn Agent>,
            timed_questions(),
            None,
            TIMED_CALL_ID,
            120_000,
        )
        .await
        .unwrap_err();
    assert!(matches!(&error, QuestionError::Other(_)));
    assert_eq!(error.to_string(), "provider crashed");
}

// "does not start the unclaimed hold when the turn ended while the request
// went unclaimed"
#[tokio::test]
async fn a_turn_that_ended_unclaimed_fails_with_ask_aborted() {
    let bench = bench().await;
    let agent = bench.live("timed-preaborted-hold");
    let turn = Arc::new(AbortController::new());
    let ending = Arc::clone(&turn);
    bench.register(with(move |_, _| {
        ending.abort();
        Box::pin(async { Err(rejection(NO_PROVIDER)) })
    }));
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        bench.service.ask_timed(
            Arc::clone(&agent) as Arc<dyn Agent>,
            timed_questions(),
            Some(turn.signal()),
            TIMED_CALL_ID,
            120_000,
        ),
    )
    .await
    .expect("the hold doesn't start")
    .unwrap_err();
    assert_eq!(code(&error), ASK_ABORTED);
}

// "rejects non-positive waits before dispatching", and one no timer holds
#[tokio::test]
async fn a_window_no_timer_holds_is_refused() {
    let bench = bench().await;
    let agent = bench.live("timed-invalid");
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    for timeout_ms in [0, MAX_TIMEOUT_MS + 1] {
        let error = bench
            .service
            .ask_timed(
                Arc::clone(&agent) as Arc<dyn Agent>,
                timed_questions(),
                None,
                TIMED_CALL_ID,
                timeout_ms,
            )
            .await
            .unwrap_err();
        assert_eq!(code(&error), BAD_TIMEOUT);
        assert_eq!(
            error.to_string(),
            "timeout must fit a positive platform timer"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// Attaching is for the live agent only, as every entry point is.
#[tokio::test]
async fn only_the_live_agent_attaches() {
    let bench = bench().await;
    let _live = bench.live("attach");
    let stale = Stub::new("attach", &bench.sessions);
    let error = bench
        .service
        .attach_wait(stale.as_ref(), TIMED_CALL_ID, AbortSignal::never())
        .unwrap_err();
    assert_eq!(error.code(), CALLER_NOT_LIVE);
}

// late replies

/// The timed tool's schema as the request header records it.
fn timed_tool_schema() -> Value {
    json!({
        "name": "ask_user_question",
        "description": "Ask brief questions.",
        "parameters": {
            "type": "object",
            "properties": { "questions": { "type": "array" }, TIMED_WAIT_PARAMETER: { "type": "integer" } },
        },
    })
}

/// An event from dsh's JSON.
fn event(event_type: &str, data: Value) -> SessionEvent {
    let mut envelope = json!({ "type": event_type, "seq": 0, "time": 0, "data": data });
    if matches!(event_type, "tool/result" | "user/message") {
        envelope["surfaceOp"] = json!("append");
    }
    serde_json::from_value::<Entry>(envelope)
        .expect("the event fits dsh's envelope")
        .event
}

fn ask_in_log(agent: &Stub, call_id: &str, tools: Vec<Value>) {
    agent.append(event("turn/start", json!({ "turn": 1 })));
    agent.append(event("step/start", json!({ "turn": 1, "step": 1 })));
    agent.append(event(
        "request/header",
        json!({
            "header": { "config": { "provider": "deepseek-official", "model": "deepseek-v4-flash" }, "tools": tools },
            "reason": "initial",
        }),
    ));
    ask_again(agent, call_id);
}

fn ask_again(agent: &Stub, call_id: &str) {
    agent.append(event(
        "tool/call",
        json!({
            "turn": 1, "step": 1, "callId": call_id, "name": "ask_user_question",
            "arguments": json!({ "questions": timed_questions() }).to_string(),
        }),
    ));
}

fn tool_result(call_id: &str, text: &str, error: Option<Value>) -> SessionEvent {
    let mut data = json!({
        "turn": 1, "step": 1,
        "message": {
            "id": format!("tool-{call_id}"),
            "role": "tool",
            "source": { "kind": "tool", "callId": call_id },
            "toolCallId": call_id,
            "content": [{ "type": "text", "text": text }],
            "isError": error.is_some(),
        },
    });
    if let Some(error) = error {
        data["error"] = error;
    }
    event("tool/result", data)
}

fn continue_in_log(agent: &Stub, call_id: &str) {
    agent.append(tool_result(
        call_id,
        &json!({ "pending": true, "callId": call_id }).to_string(),
        None,
    ));
}

/// A live agent whose session asked `TIMED_CALL_ID` and left it continued.
fn continued(bench: &Bench, id: &str) -> Arc<Stub> {
    let agent = bench.live(id);
    ask_in_log(&agent, TIMED_CALL_ID, vec![timed_tool_schema()]);
    continue_in_log(&agent, TIMED_CALL_ID);
    agent
}

fn reply(call_id: &str, id: &str, text: &str) -> UserMessage {
    UserMessage::text(id, reply_source(call_id), text)
}

fn admit(agent: &Stub, message: &UserMessage) {
    agent.append(event(
        "user/message",
        serde_json::to_value(message).unwrap(),
    ));
}

fn end_turn(agent: &Stub, reason: Value) {
    agent.append(event("turn/end", json!({ "turn": 1, "reason": reason })));
}

fn aborted_by_user() -> Value {
    json!({ "kind": "aborted", "reason": { "kind": "user" } })
}

fn reply_queued(result: Result<bool, QuestionError>) -> bool {
    matches!(result, Err(error) if code(&error) == REPLY_QUEUED)
}

// "steers an answer into a continued question and closes it once the reply
// is admitted"
#[tokio::test]
async fn a_late_answer_is_steered_and_closes_once_admitted() {
    let bench = bench().await;
    let agent = continued(&bench, "late-answer");
    assert!(
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
            .unwrap()
    );
    assert_eq!(agent.steered(), 1);
    let steered = agent.queued(InboxTarget::NextStep)[0].clone();
    assert_eq!(
        steered.source,
        UserSource::Other(
            json!({ "kind": "user-question-reply", "callId": TIMED_CALL_ID, "outcome": "answered" })
        )
    );
    assert_eq!(reply_call_id(&steered.source), Some(TIMED_CALL_ID));
    let text = match &steered.content[..] {
        [karaka_session::ContentBlock::Text(text)] => text.text.clone(),
        other => panic!("not one text block: {other:?}"),
    };
    // dsh's exact text, key order included.
    assert_eq!(
        text,
        json!({
            "kind": "answer_to_pending_question",
            "tool": "ask_user_question",
            "callId": TIMED_CALL_ID,
            "questions": timed_questions(),
            "answers": tool_only().answers,
        })
        .to_string()
    );
    assert!(text.starts_with(r#"{"kind":"answer_to_pending_question","tool":"ask_user_question","callId":"ask-timed","questions":[{"id":"scope","question":"Which scope?","options":[{"label":"Tool only"}]}]"#));
    agent.shift(InboxTarget::NextStep, None);
    admit(&agent, &steered);
    assert!(
        !bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
            .unwrap()
    );
    assert_eq!(bench.view(&agent).settled[0].answers, tool_only().answers);
}

// "accepts steers for different continued batches while both await the
// next step"
#[tokio::test]
async fn replies_to_different_calls_queue_side_by_side() {
    let bench = bench().await;
    let agent = continued(&bench, "late-out-of-order");
    let second = "another-timed-batch";
    ask_again(&agent, second);
    continue_in_log(&agent, second);
    assert!(
        bench
            .service
            .answer(agent.as_ref(), second, &tool_only())
            .unwrap()
    );
    assert!(
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
            .unwrap()
    );
    let calls: Vec<Option<String>> = agent
        .queued(InboxTarget::NextStep)
        .iter()
        .map(|message| reply_call_id(&message.source).map(str::to_owned))
        .collect();
    assert_eq!(
        calls,
        [Some(second.to_owned()), Some(TIMED_CALL_ID.to_owned())]
    );
    assert!(agent.queued(InboxTarget::NextTurn).is_empty());
}

// "accepts one queued reply and permits retry when it is discarded"
#[tokio::test]
async fn one_reply_queues_and_a_discard_permits_another() {
    let bench = bench().await;
    let agent = continued(&bench, "late-duplicate");
    let answer = || {
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
    };
    assert!(answer().unwrap());
    let error = answer().unwrap_err();
    assert_eq!(code(&error), REPLY_QUEUED);
    assert_eq!(
        error.to_string(),
        "a reply is already queued for this question"
    );
    assert_eq!(agent.steered(), 1);
    let first = agent.shift(InboxTarget::NextStep, Some(SpliceOutcome::Canceled));
    bench.hooks.discarded(agent.as_ref(), &first);
    assert!(answer().unwrap());
    assert_eq!(agent.steered(), 2);
}

// "refuses a reply already present in the durable inbox"
#[tokio::test]
async fn a_reply_already_in_the_inbox_is_refused() {
    let bench = bench().await;
    let agent = continued(&bench, "late-inbox");
    let queued = reply(TIMED_CALL_ID, "queued-reply", "queued reply");
    let answer = || {
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
    };
    agent.queue(InboxTarget::NextStep, queued.clone());
    assert!(reply_queued(answer()));
    agent.shift(InboxTarget::NextStep, None);
    agent.queue(InboxTarget::NextTurn, queued);
    assert!(reply_queued(answer()));
    assert_eq!(agent.steered(), 0);
}

// "reserves a restored inbox reply while it is claimed for admission"
#[tokio::test]
async fn a_claimed_restored_reply_stays_reserved_until_its_turn_ends() {
    let bench = bench().await;
    let agent = continued(&bench, "late-restored-claim");
    let restored = reply(TIMED_CALL_ID, "restored-reply", "restored reply");
    let answer = || {
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
    };
    agent.queue(InboxTarget::NextStep, restored.clone());
    assert!(reply_queued(answer()));
    agent.shift(InboxTarget::NextStep, None);
    bench.hooks.claimed(agent.as_ref(), &restored, 1);
    assert!(reply_queued(answer()));
    // An unexpected second claim for the same call must not replace the
    // first reservation.
    let other = reply(
        TIMED_CALL_ID,
        "another-restored-reply",
        "another restored reply",
    );
    bench.hooks.claimed(agent.as_ref(), &other, 2);
    end_turn(&agent, aborted_by_user());
    assert!(answer().unwrap());
}

// "keeps the reservation through unrelated inbox and Session events"
#[tokio::test]
async fn unrelated_events_keep_the_reservation() {
    let bench = bench().await;
    let agent = continued(&bench, "late-unrelated");
    let other = UserMessage::text("other", UserSource::user(), "other");
    let other_reply = reply("other-call", "other-reply", "other reply");
    let other_session = bench.live("other-session");
    other_session.append(event("turn/start", json!({ "turn": 1 })));
    end_turn(&other_session, json!({ "kind": "completed" }));
    bench.hooks.claimed(agent.as_ref(), &other_reply, 1);

    assert!(
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
            .unwrap()
    );
    bench.hooks.claimed(agent.as_ref(), &other, 1);
    bench.hooks.claimed(agent.as_ref(), &other_reply, 1);
    bench.hooks.discarded(agent.as_ref(), &other);
    bench.hooks.discarded(agent.as_ref(), &other_reply);
    for message in [&other, &other_reply] {
        admit(&agent, message);
    }
    agent.append(event("step/end", json!({ "turn": 1, "step": 1 })));
    end_turn(&agent, json!({ "kind": "completed" }));
    assert!(reply_queued(bench.service.answer(
        agent.as_ref(),
        TIMED_CALL_ID,
        &tool_only()
    )));
    assert_eq!(agent.steered(), 1);
}

// "permits retry when a claimed reply ends without admission"
#[tokio::test]
async fn a_claimed_reply_its_turn_never_admitted_permits_another() {
    let bench = bench().await;
    let agent = continued(&bench, "late-claimed");
    let answer = || {
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
    };
    assert!(answer().unwrap());
    let first = agent.shift(InboxTarget::NextStep, None);
    bench.hooks.claimed(agent.as_ref(), &first, 1);
    assert!(reply_queued(answer()));
    end_turn(&agent, aborted_by_user());
    assert!(answer().unwrap());
    assert_eq!(agent.steered(), 2);
}

// "permits retry when steering a reply throws before it is queued"
#[tokio::test]
async fn a_refused_steer_permits_another() {
    let bench = bench().await;
    let agent = continued(&bench, "late-steer-error");
    let answer = || {
        bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &tool_only())
    };
    agent.refuse_steer.store(true, Ordering::SeqCst);
    let error = answer().unwrap_err();
    assert!(matches!(&error, QuestionError::Other(_)));
    assert_eq!(error.to_string(), "steer failed");
    assert!(answer().unwrap());
    assert_eq!(agent.steered(), 2);
}

// "refuses an open question and an unknown call"
#[tokio::test]
async fn an_open_or_unknown_call_takes_no_reply() {
    let bench = bench().await;
    let agent = bench.live("late-open");
    ask_in_log(&agent, TIMED_CALL_ID, vec![timed_tool_schema()]);
    let empty = AskUserQuestionAnswer::default();
    assert!(
        !bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &empty)
            .unwrap()
    );
    assert!(
        !bench
            .service
            .answer(agent.as_ref(), "never-asked", &empty)
            .unwrap()
    );
    assert_eq!(agent.steered(), 0);
    assert_eq!(agent.injects.load(Ordering::SeqCst), 0);
    assert_eq!(bench.view(&agent).active[0].state, UserQuestionState::Open);
}

// "refuses a batch that does not name each question of the call exactly
// once"
#[tokio::test]
async fn a_batch_must_answer_each_question_once() {
    let bench = bench().await;
    let agent = continued(&bench, "late-malformed");
    for answers in [
        Vec::new(),
        vec![item("other", &["Tool only"])],
        vec![item("scope", &["Tool only"]), item("scope", &[])],
        vec![item("scope", &["Tool only"]), item("other", &[])],
    ] {
        let error = bench
            .service
            .answer(agent.as_ref(), TIMED_CALL_ID, &batch(answers))
            .unwrap_err();
        assert_eq!(code(&error), BAD_ANSWER);
        assert_eq!(
            error.to_string(),
            "the answer batch for ask-timed must name each of its 1 questions exactly once"
        );
    }
    assert_eq!(agent.steered(), 0);
    // The question stays continued and takes a well-formed batch afterwards.
    assert!(
        bench
            .service
            .answer(
                agent.as_ref(),
                TIMED_CALL_ID,
                &batch(vec![item("scope", &[])])
            )
            .unwrap()
    );
}

// "never lists a call the blocking legacy tool made, even one the process
// never finished"
#[tokio::test]
async fn a_legacy_call_is_never_listed() {
    let bench = bench().await;
    let agent = bench.live("legacy-interrupted");
    let mut legacy = timed_tool_schema();
    legacy["parameters"] =
        json!({ "type": "object", "properties": { "questions": { "type": "array" } } });
    ask_in_log(&agent, TIMED_CALL_ID, vec![legacy]);
    agent.append(tool_result(
        TIMED_CALL_ID,
        "The tool call was interrupted after it was recorded.",
        Some(json!({ "name": "SessionFormatError", "code": TOOL_OUTCOME_UNKNOWN })),
    ));
    assert_eq!(bench.view(&agent), UserQuestionProjectionView::default());
    assert!(
        !bench
            .service
            .answer(
                agent.as_ref(),
                TIMED_CALL_ID,
                &batch(vec![item("scope", &[])])
            )
            .unwrap()
    );
    assert_eq!(agent.steered(), 0);
}

// An agent that isn't live can't answer.
#[tokio::test]
async fn only_the_live_agent_answers() {
    let bench = bench().await;
    let _agent = continued(&bench, "late-stale");
    let stale = Stub::new("late-stale", &bench.sessions);
    let error = bench
        .service
        .answer(stale.as_ref(), TIMED_CALL_ID, &tool_only())
        .unwrap_err();
    assert_eq!(code(&error), CALLER_NOT_LIVE);
}

// The real loop.

/// Plays one scripted reply per call.
struct Scripted {
    replies: Mutex<VecDeque<Vec<Block>>>,
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        "mock"
    }

    fn stream(&self, _: Request) -> EventStream {
        let events: Vec<Result<Event, ModelError>> = match self.replies.lock().unwrap().pop_front()
        {
            Some(blocks) => {
                let reason = if blocks
                    .iter()
                    .any(|block| matches!(block, Block::ToolCall(_)))
                {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                stream_of(&blocks, reason, Usage::default())
                    .into_iter()
                    .map(Ok)
                    .collect()
            }
            None => vec![Err(ModelError::new(ErrorKind::InvalidRequest, "exhausted"))],
        };
        Box::pin(Playback(events.into()))
    }
}

struct Playback(VecDeque<Result<Event, ModelError>>);

impl Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

/// A stand-in for the timed `ask_user_question` tool, which isn't ported
/// yet: it asks with a short window and returns what the ask gave.
struct AskTool(Arc<UserQuestions>);

impl Tool for AskTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ask_user_question".into(),
            description: "Ask brief questions.".into(),
            parameters: timed_tool_schema()["parameters"].to_string(),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            let questions =
                karaka_user_questions::questions_of(&arguments.to_string()).expect("questions");
            // The loop's own handle on the agent: a different `Arc` from the
            // registry's, which the service still takes as live.
            let agent = run.agent.expect("the loop names its agent");
            match self
                .0
                .ask_timed(agent, questions, None, &run.call_id, 50)
                .await
            {
                Ok(result) => ToolOutput::ok(serde_json::to_string(&result).unwrap()),
                Err(error) => ToolOutput::error(error.to_string()),
            }
        })
    }
}

// A model asks with the timed tool; nobody answers in the window, so the
// call is left pending and the question continued; a late answer is
// steered in, the agent takes it in a turn of its own, and the question
// settles with it.
#[tokio::test]
async fn a_late_answer_reaches_the_model_through_the_loop() {
    let arguments = json!({ "questions": [{ "id": "scope", "question": "Which scope?", "options": [{ "label": "Tool only" }] }] });
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new("mock-work", "mock"))),
        Box::new(UserQuestionsPlugin),
    ])
    .await
    .unwrap();
    runtime
        .get::<ModelRegistry>()
        .register(
            "mock-work",
            Arc::new(Scripted {
                replies: Mutex::new(
                    vec![
                        vec![Block::ToolCall(ToolCall {
                            id: "call-ask".into(),
                            name: "ask_user_question".into(),
                            arguments: arguments.to_string(),
                        })],
                        vec![Block::Text("I'll wait for your answer.".into())],
                        vec![Block::Text("Thanks, tool only it is.".into())],
                    ]
                    .into(),
                ),
            }) as Arc<dyn Model>,
        )
        .unwrap();
    let service = runtime.get::<UserQuestions>();
    runtime
        .get::<ToolRegistry>()
        .register(Arc::new(AskTool(Arc::clone(&service))))
        .unwrap();
    let agent = runtime
        .get::<dyn Agents>()
        .open(SessionId::new("it-questions"))
        .unwrap();
    let idle = || async {
        tokio::time::timeout(Duration::from_secs(5), agent.when_idle())
            .await
            .expect("the agent goes idle");
    };
    agent
        .followup(UserMessage::text("user-start", UserSource::user(), "start"))
        .unwrap();
    idle().await;

    let session = runtime.get::<SessionStore>().get(agent.id()).unwrap();
    let projections = runtime.get::<SessionProjections>();
    let view = || {
        projections
            .state_of::<UserQuestionsProjection>(&session)
            .unwrap()
            .questions
            .clone()
    };
    assert_eq!(view().active.len(), 1);
    assert_eq!(view().active[0].call_id, "call-ask");
    assert_eq!(view().active[0].state, UserQuestionState::Continued);
    assert_eq!(
        projections.snapshot(&session, None).values["userQuestions"]["active"][0]["state"],
        "continued"
    );

    assert!(
        service
            .answer(agent.as_ref(), "call-ask", &tool_only())
            .unwrap()
    );
    // The steer woke the agent; until its turn admits the reply, another
    // is refused.
    assert!(reply_queued(service.answer(
        agent.as_ref(),
        "call-ask",
        &tool_only()
    )));
    idle().await;

    assert_eq!(view().active, []);
    assert_eq!(view().settled[0].answers, tool_only().answers);
    let replies: Vec<UserMessage> = session
        .events()
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::UserMessage { message, .. }
                if reply_call_id(&message.source) == Some("call-ask") =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 1);
    assert!(
        !service
            .answer(agent.as_ref(), "call-ask", &tool_only())
            .unwrap()
    );
}

// A preset's answerers hear the requests of every agent in the preset, in
// the order they were added among the global ones, and no other agent's.
// (dsh: a listener of an enclosing scope hears its descendants' events.)
#[tokio::test]
async fn a_presets_answerers_hear_its_agents_requests() {
    let bench = bench().await;
    let coached = bench.live("coached");
    *coached.preset.lock().unwrap() = Some(PresetId::new("coach"));
    let alone = bench.live("alone");
    // A session named like the preset isn't in it.
    let lookalike = bench.live("coach");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let note = |name: &'static str| {
        let heard = Arc::clone(&heard);
        Arc::new(with(move |_, next| {
            heard.lock().unwrap().push(name);
            next.run()
        }))
    };
    let _ = bench.service.answerer(None, note("global"));
    let _ = bench
        .service
        .answerer(Some(&ScopeKey::from(PresetId::new("coach"))), note("coach"));
    let _ = bench
        .service
        .answerer(Some(&ScopeKey::from(coached.id.clone())), note("mine"));
    let ask = |agent: Arc<Stub>| {
        let service = Arc::clone(&bench.service);
        async move {
            service
                .ask(QuestionRequest {
                    agent: Some(agent as Arc<dyn Agent>),
                    ..request(confirm())
                })
                .await
        }
    };
    ask(Arc::clone(&coached)).await.unwrap_err();
    assert_eq!(*heard.lock().unwrap(), ["global", "coach", "mine"]);
    for agent in [alone, lookalike] {
        heard.lock().unwrap().clear();
        ask(agent).await.unwrap_err();
        assert_eq!(*heard.lock().unwrap(), ["global"]);
    }
}
