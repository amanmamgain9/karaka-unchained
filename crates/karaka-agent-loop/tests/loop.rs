//! The default loop, started in a runtime with a scripted model and fake
//! tools. Ported from dsh's `agent-loop/tests/loop.spec.ts`,
//! `tool-calls.spec.ts`, `cancel.spec.ts` and `coverage-edges.spec.ts`,
//! where their cases fit step one. Each test asserts the exact events a
//! turn logs.

use std::collections::VecDeque;
use std::future::pending;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{
    Agent, Agents, AssistantStreamFrame, FrameSubscription, HookFuture, InboxError, Next,
    RequestError, RequestErrorAction, RequestErrorHandler, RequestErrorHooks, StreamChunk,
    StreamOutcome,
};
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_kernel::Runtime;
use karaka_model::{
    Block, BlockAssembler, Effort, ErrorKind, Event, EventStream, FinishReason, Message, Model,
    ModelError, ModelRegistry, ModelsPlugin, Origin, Replay, Request, RetryPolicy, Role, ToolCall,
    ToolResult, ToolSchema, Usage, stream_of,
};
use karaka_session::{
    CallConfig, CancelCause, ContentBlock, Entry, Extra, Failure, IN_HISTORY, InboxSplice,
    InboxTarget, RequestContext, RequestHeader, RequestHeaderData, RequestHeaderReason, Seq,
    SessionEvent, SessionId, SessionStore, SessionsPlugin, SpliceOutcome, SurfaceIntent, SurfaceOp,
    TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN, Time, TimedChunk, TokenUsage, TurnEndReason,
    UserMessage, UserSource, check_assistant_streams, chunk_event, chunk_json, expand_stream,
};
use karaka_session_projection::{SessionProjections, SessionProjectionsPlugin};
use karaka_system_prompt::{
    HARNESS_IDENTITY, PromptContext, Scope, Section, SystemPrompt, SystemPromptConfig,
    SystemPromptPlugin, TOOL_ORDER_REST, Text,
};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::timeout;

/// The route the loop asks. Deliberately not the provider's own name, which
/// is what an answer's source must record.
const ROUTE: &str = "mock-work";
const PROVIDER: &str = "mock";
const MODEL: &str = "mock-model";

// The scripted model.

/// One model call's script.
enum Reply {
    /// These events, one per poll, then the end of the stream.
    Events(Vec<Result<Event, ModelError>>),
    /// These events, then a wait that never ends, until the stream is
    /// dropped. `waiting` is notified when the wait begins.
    Hang(Vec<Event>, Arc<Notify>),
    /// These events, then a panic in the stream.
    Panic(Vec<Event>),
    /// Runs the closure as the call starts, then plays the reply.
    Before(Box<dyn FnOnce() + Send>, Box<Reply>),
}

/// Plays one reply per call, in order, and keeps every request it got.
/// (dsh: `MockAdapter` in `tests/mock-adapter.ts`.)
struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        PROVIDER
    }

    fn stream(&self, request: Request) -> EventStream {
        self.requests.lock().unwrap().push(request);
        let mut reply = self.replies.lock().unwrap().pop_front();
        while let Some(Reply::Before(run, then)) = reply {
            run();
            reply = Some(*then);
        }
        let playback = match reply {
            Some(Reply::Events(events)) => Playback {
                events: events.into(),
                hang: None,
                panic: false,
            },
            Some(Reply::Hang(events, waiting)) => Playback {
                events: events.into_iter().map(Ok).collect(),
                hang: Some(waiting),
                panic: false,
            },
            Some(Reply::Panic(events)) => Playback {
                events: events.into_iter().map(Ok).collect(),
                hang: None,
                panic: true,
            },
            Some(Reply::Before(..)) => unreachable!("played above"),
            // dsh's mock throws "script exhausted".
            None => Playback {
                events: VecDeque::from([Err(ModelError::new(
                    ErrorKind::InvalidRequest,
                    "the script is exhausted",
                ))]),
                hang: None,
                panic: false,
            },
        };
        Box::pin(playback)
    }
}

struct Playback {
    events: VecDeque<Result<Event, ModelError>>,
    hang: Option<Arc<Notify>>,
    /// Panics once the events are played.
    panic: bool,
}

impl Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(event) = this.events.pop_front() {
            return Poll::Ready(Some(event));
        }
        if this.panic {
            panic!("the stream broke");
        }
        match &this.hang {
            // Never woken: only dropping the stream ends it.
            Some(waiting) => {
                waiting.notify_one();
                Poll::Pending
            }
            None => Poll::Ready(None),
        }
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 10,
        output_tokens: 5,
        ..Usage::default()
    }
}

/// The chunks of an answer of `blocks`, as dsh's mock adapter scripts one.
fn answer(blocks: &[Block], reason: FinishReason) -> Reply {
    Reply::Events(
        stream_of(blocks, reason, usage())
            .into_iter()
            .map(Ok)
            .collect(),
    )
}

/// dsh's `textResponse`: the block's start, the text, its end, then usage
/// and the finish.
fn text(answer_text: &str) -> Reply {
    answer(&[Block::Text(answer_text.into())], FinishReason::Stop)
}

fn delta(text: &str) -> Event {
    Event::TextDelta {
        index: 0,
        text: text.into(),
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

/// dsh's `toolCallResponse`, for any number of calls.
fn calls(calls: &[ToolCall]) -> Reply {
    let blocks: Vec<Block> = calls.iter().cloned().map(Block::ToolCall).collect();
    answer(&blocks, FinishReason::ToolCalls)
}

// The fake tools.

fn schema(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("The {name} tool."),
        parameters: r#"{"type":"object","properties":{"text":{"type":"string"}}}"#.into(),
    }
}

/// dsh's `echo` fixture: answers `echo: <text>`.
struct Echo;

impl Tool for Echo {
    fn schema(&self) -> ToolSchema {
        schema("echo")
    }

    fn execute<'a>(&'a self, arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            let text = arguments["text"].as_str().unwrap_or_default();
            ToolOutput::ok(format!("echo: {text}"))
        })
    }
}

/// Always fails.
struct Broken;

impl Tool for Broken {
    fn schema(&self) -> ToolSchema {
        schema("broken")
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::error("the weather service is down") })
    }
}

/// Panics instead of answering.
struct Panicky;

impl Tool for Panicky {
    fn schema(&self) -> ToolSchema {
        schema("panicky")
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { panic!("the tool broke") })
    }
}

/// Never finishes. Notifies `started` when it runs, and sets `dropped`
/// when its call is dropped.
struct Stuck {
    started: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Tool for Stuck {
    fn schema(&self) -> ToolSchema {
        schema("stuck")
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        let guard = SetOnDrop(Arc::clone(&self.dropped));
        Box::pin(async move {
            let _guard = guard;
            self.started.notify_one();
            pending::<ToolOutput>().await
        })
    }
}

// The harness.

/// A started runtime with the loop, the scripted model under [`ROUTE`] and
/// the fake tools, as dsh's tests build theirs (`harness` in
/// `loop.spec.ts`).
struct Harness {
    model: Arc<Scripted>,
    sessions: Arc<SessionStore>,
    tools: Arc<ToolRegistry>,
    hooks: Arc<RequestErrorHooks>,
    agents: Arc<dyn Agents>,
    prompt: Arc<SystemPrompt>,
    projections: Arc<SessionProjections>,
    stuck: Arc<Notify>,
    stuck_dropped: Arc<AtomicBool>,
    _runtime: Runtime,
}

impl Harness {
    async fn new(replies: Vec<Reply>) -> Self {
        Self::with(replies, config(None), Vec::new()).await
    }

    /// A harness with `config`, whose store already holds `restored` as the
    /// session [`id`].
    async fn with(replies: Vec<Reply>, config: impl Into<Setup>, restored: Vec<Entry>) -> Self {
        let Setup {
            loop_config,
            prompt,
        } = config.into();
        let runtime = Runtime::start(vec![
            Box::new(AgentLoopPlugin::new(loop_config)),
            Box::new(SystemPromptPlugin::new(prompt)),
            Box::new(SessionsPlugin),
            Box::new(SessionProjectionsPlugin),
            Box::new(ToolsPlugin),
            Box::new(ModelsPlugin),
        ])
        .await
        .expect("the runtime starts");
        let model = Arc::new(Scripted::new(replies));
        runtime
            .get::<ModelRegistry>()
            .register(ROUTE, Arc::clone(&model) as Arc<dyn Model>)
            .unwrap();
        let stuck = Arc::new(Notify::new());
        let stuck_dropped = Arc::new(AtomicBool::new(false));
        let tools = runtime.get::<ToolRegistry>();
        tools.register(Arc::new(Echo)).unwrap();
        tools.register(Arc::new(Broken)).unwrap();
        tools
            .register(Arc::new(Stuck {
                started: Arc::clone(&stuck),
                dropped: Arc::clone(&stuck_dropped),
            }))
            .unwrap();
        let sessions = runtime.get::<SessionStore>();
        if !restored.is_empty() {
            sessions.restore(id(), restored).expect("the log restores");
        }
        Self {
            model,
            sessions,
            tools,
            hooks: runtime.get::<RequestErrorHooks>(),
            agents: runtime.get::<dyn Agents>(),
            prompt: runtime.get::<SystemPrompt>(),
            projections: runtime.get::<SessionProjections>(),
            stuck,
            stuck_dropped,
            _runtime: runtime,
        }
    }

    fn agent(&self) -> Arc<dyn Agent> {
        self.agents.open(id()).expect("the agent opens")
    }

    /// Every event, to restore elsewhere.
    fn log(&self) -> Vec<Entry> {
        self.sessions.events(&id()).expect("the session exists")
    }

    /// Every event but the inbox's splices, which a followup and each
    /// step's claim log around a turn; the inbox's own tests read them,
    /// with [`log_types`] and [`splices`].
    fn events(&self) -> Vec<Entry> {
        self.sessions
            .events(&id())
            .expect("the session exists")
            .into_iter()
            .filter(|entry| !matches!(entry.event, SessionEvent::InboxSpliced(_)))
            .collect()
    }

    /// The type of every event logged, in order.
    fn types(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|entry| entry.event.event_type().to_owned())
            .collect()
    }

    /// Every `request/header`, in order.
    fn headers(&self) -> Vec<RequestHeaderData> {
        self.events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                SessionEvent::RequestHeader(data) => Some(data),
                _ => None,
            })
            .collect()
    }

    /// Every `request/context`, in order.
    fn contexts(&self) -> Vec<RequestContext> {
        self.events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                SessionEvent::RequestContext(context) => Some(context),
                _ => None,
            })
            .collect()
    }

    /// The reason of every `turn/end`, in order.
    fn endings(&self) -> Vec<TurnEndReason> {
        self.events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                SessionEvent::TurnEnd(end) => Some(end.reason),
                _ => None,
            })
            .collect()
    }

    /// Sends `text` as a followup and waits for the agent to be idle.
    async fn say(&self, text: &str) {
        let agent = self.agent();
        agent.followup(user(text)).unwrap();
        idle(agent.as_ref()).await;
    }
}

/// Every log a test leaves is held to dsh's saved-stream rule: each answer
/// is what its embedded stream assembles to. A test that panicked already
/// failed, so its logs aren't checked again.
impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for session in self.sessions.list() {
            let events = self.sessions.events(&session).expect("the session exists");
            if let Err(error) = check_assistant_streams(&events) {
                panic!("session \"{session}\" disagrees with its streams: {error}");
            }
        }
    }
}

fn id() -> SessionId {
    SessionId::new("a1")
}

/// How a harness's loop and system prompt are set up.
struct Setup {
    loop_config: LoopConfig,
    prompt: SystemPromptConfig,
}

/// Without the identity line, so a test's prompt is exactly its persona.
/// `the_step_prompt_and_tools_come_from_the_registry` covers the default.
fn quiet(persona: Option<&str>) -> SystemPromptConfig {
    SystemPromptConfig {
        include_harness_identity: false,
        persona_prefix: persona.unwrap_or_default().to_owned(),
        ..SystemPromptConfig::default()
    }
}

impl From<LoopConfig> for Setup {
    fn from(loop_config: LoopConfig) -> Self {
        Self {
            loop_config,
            prompt: quiet(None),
        }
    }
}

/// A loop at [`ROUTE`] whose prompt is `system`, the persona prefix.
fn config(system: Option<&str>) -> Setup {
    Setup {
        loop_config: LoopConfig::new(ROUTE, MODEL),
        prompt: quiet(system),
    }
}

/// The context the loop logs for `model` at the scripted provider.
fn context(model: &str) -> RequestContext {
    RequestContext {
        provider: PROVIDER.into(),
        model: model.into(),
        context_window: None,
        system_prompt_update: Some(IN_HISTORY.into()),
        extra: Extra::new(),
    }
}

fn user(text: &str) -> UserMessage {
    UserMessage::text(format!("user-{text}"), UserSource::user(), text)
}

/// Waits for `agent` to be idle, failing the test rather than hanging it.
async fn idle(agent: &dyn Agent) {
    timeout(Duration::from_secs(5), agent.when_idle())
        .await
        .expect("the agent goes idle");
}

/// The events of a session's first step, from `step/start` to `step/end`:
/// the prompt, the message, the first request's header and context, and the
/// answer.
const ANSWER_STEP: [&str; 7] = [
    "step/start",
    "system/message",
    "user/message",
    "request/header",
    "request/context",
    "assistant/message",
    "step/end",
];

fn tool_result(entry: &Entry) -> (ToolResult, Vec<u64>, Option<String>) {
    match &entry.event {
        SessionEvent::ToolResult { data, surface } => (
            data.message.to_model(),
            surface.sources.iter().map(|seq| seq.0).collect(),
            data.error.as_ref().map(|error| error.code.clone()),
        ),
        other => panic!("expected a tool result, got {other:?}"),
    }
}

// The cases.

/// dsh: "runs a simple turn: queued message → model → idle, with ordered
/// events" (`loop.spec.ts:452`).
#[tokio::test]
async fn a_plain_answer_logs_one_step() {
    let harness = Harness::with(
        vec![text("hello there")],
        config(Some("Be brief.")),
        Vec::new(),
    )
    .await;
    harness.say("hi").await;

    let mut expected = vec!["turn/start"];
    expected.extend(ANSWER_STEP);
    expected.push("turn/end");
    assert_eq!(harness.types(), expected);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);

    // The answer names the provider, not the route, and keeps its usage.
    let events = harness.events();
    let SessionEvent::AssistantMessage { data, .. } = &events[6].event else {
        panic!("expected the answer, got {:?}", events[6].event);
    };
    assert_eq!((data.turn, data.step), (1, 1));
    assert_eq!(data.message.source.provider, PROVIDER);
    assert_eq!(data.message.source.model, MODEL);
    assert_eq!(data.usage, Some(TokenUsage::from(usage())));
    assert!(!data.interrupted);

    // The first request logs an `initial` header, naming the provider, the
    // model and every tool, by name as the prompt assembly orders them,
    // then the route's context.
    let mut all_tools: Vec<ToolSchema> = harness.tools.schemas();
    all_tools.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        harness.headers(),
        [RequestHeaderData {
            header: RequestHeader::new(CallConfig::new(PROVIDER, MODEL), &all_tools).unwrap(),
            reason: RequestHeaderReason::Initial,
            starts_series: false,
            extra: Extra::new(),
        }]
    );
    assert_eq!(
        serde_json::to_value(&events[4]).unwrap()["data"]["header"]["tools"][1],
        serde_json::json!({
            "name": "echo",
            "description": "The echo tool.",
            "parameters": { "type": "object", "properties": { "text": { "type": "string" } } },
        })
    );
    assert_eq!(harness.contexts(), [context(MODEL)]);

    // The request came from the log: the prompt, the user's message, and
    // every tool.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, MODEL);
    assert_eq!(requests[0].system.as_deref(), Some("Be brief."));
    assert_eq!(requests[0].messages, [Message::user("hi")]);
    let tools: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(tools, ["broken", "echo", "stuck"]);

    let history = harness.sessions.history(&id()).unwrap();
    assert_eq!(history.system.as_deref(), Some("Be brief."));
    assert_eq!(
        history.messages,
        [
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![Block::Text("hello there".into())]
            }
        ]
    );
}

/// dsh: "round-trips tool calls: model requests tool → executes → result in
/// next request" (`loop.spec.ts:488`).
#[tokio::test]
async fn a_tool_round_trip_logs_the_call_and_its_result() {
    let ping = call("c1", "echo", r#"{"text":"ping"}"#);
    let harness = Harness::new(vec![calls(std::slice::from_ref(&ping)), text("done")]).await;
    harness.say("use the tool").await;

    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);

    // The result cites the `tool/call` that started it.
    let events = harness.events();
    let (result, sources, error) = tool_result(&events[8]);
    assert_eq!(
        result,
        ToolResult {
            call_id: "c1".into(),
            content: "echo: ping".into(),
            is_error: false
        }
    );
    assert_eq!(sources, [events[7].seq.0]);
    assert_eq!(error, None);

    // The second request carries the call and its result.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages,
        [
            Message::user("use the tool"),
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolCall(ping)]
            },
            Message {
                role: Role::User,
                content: vec![Block::ToolResult(result)]
            },
        ]
    );
}

/// dsh: "commits tool/result in model order" (`tool-calls.spec.ts:230`),
/// here with the calls run one after another.
#[tokio::test]
async fn two_calls_in_one_step_run_in_the_models_order() {
    let harness = Harness::new(vec![
        calls(&[
            call("c1", "echo", r#"{"text":"one"}"#),
            call("c2", "echo", r#"{"text":"two"}"#),
        ]),
        text("done"),
    ])
    .await;
    harness.say("use both").await;

    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/message",
            "tool/call",
            "tool/result",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    let events = harness.events();
    let first = tool_result(&events[8]);
    let second = tool_result(&events[10]);
    assert_eq!(
        (first.0.call_id.as_str(), first.0.content.as_str()),
        ("c1", "echo: one")
    );
    assert_eq!(first.1, [events[7].seq.0]);
    assert_eq!(
        (second.0.call_id.as_str(), second.0.content.as_str()),
        ("c2", "echo: two")
    );
    assert_eq!(second.1, [events[9].seq.0]);

    // Both results go back in one user message, in the model's order.
    let requests = harness.model.requests();
    let results = &requests[1].messages[2];
    assert_eq!(results.role, Role::User);
    assert_eq!(results.content.len(), 2);
}

/// dsh: an unknown name is answered with `ToolNotFoundError`'s text, and the
/// turn carries on.
#[tokio::test]
async fn an_unknown_tool_gets_an_error_result_and_the_turn_carries_on() {
    let harness = Harness::new(vec![calls(&[call("c1", "weather", "{}")]), text("sorry")]).await;
    harness.say("weather?").await;

    let events = harness.events();
    assert_eq!(events[7].event.event_type(), "tool/call");
    let (result, sources, _) = tool_result(&events[8]);
    assert_eq!(result.content, "Error: unknown tool \"weather\"");
    assert!(result.is_error);
    assert_eq!(sources, [events[7].seq.0]);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
    assert_eq!(harness.model.requests().len(), 2);
}

/// dsh: "passes through non-JSON arguments string without crashing"
/// (`coverage-edges.spec.ts:54`). The raw text is logged as the model wrote
/// it, and the model reads why it failed.
#[tokio::test]
async fn arguments_that_arent_json_get_an_error_result() {
    let harness = Harness::new(vec![calls(&[call("c1", "echo", "not json")]), text("oops")]).await;
    harness.say("use the tool").await;

    let events = harness.events();
    let SessionEvent::ToolCall(started) = &events[7].event else {
        panic!("expected the call, got {:?}", events[7].event);
    };
    assert_eq!(started.arguments, "not json");
    let (result, _, _) = tool_result(&events[8]);
    assert_eq!(
        result.content,
        "Error: invalid arguments: \"arguments\" must be an object"
    );
    assert!(result.is_error);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

#[tokio::test]
async fn a_tool_that_fails_gets_an_error_result_and_the_turn_carries_on() {
    let harness = Harness::new(vec![
        calls(&[call("c1", "broken", "{}")]),
        text("it's down"),
    ])
    .await;
    harness.say("weather?").await;

    let events = harness.events();
    let (result, _, error) = tool_result(&events[8]);
    assert_eq!(
        result,
        ToolResult {
            call_id: "c1".into(),
            content: "the weather service is down".into(),
            is_error: true
        }
    );
    // A tool's own failure carries no error code in step one.
    assert_eq!(error, None);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
    let second = &harness.model.requests()[1];
    assert_eq!(second.messages[2].content, [Block::ToolResult(result)]);
}

/// dsh's loop assembles each step's prompt and tools from the registry,
/// with `provider` and `model` variables of its own
/// (`agent-loop/src/index.ts:389-390`, `agent.ts:272` and `407`). The
/// identity opens it; tools go by name.
#[tokio::test]
async fn the_step_prompt_and_tools_come_from_the_registry() {
    let harness = Harness::with(
        vec![text("hi")],
        Setup {
            loop_config: LoopConfig::new(ROUTE, MODEL),
            prompt: SystemPromptConfig {
                persona_prefix: "You run {{model}} at {{provider}}.".into(),
                ..SystemPromptConfig::default()
            },
        },
        Vec::new(),
    )
    .await;
    let _ = harness
        .prompt
        .section(None, Section::new("guidance", 100.0, "Use tools."))
        .unwrap();
    harness.say("hi").await;
    let request = &harness.model.requests()[0];
    assert_eq!(
        request.system.as_deref(),
        Some(format!("{HARNESS_IDENTITY}\n\nYou run {MODEL} at {ROUTE}.\n\nUse tools.").as_str())
    );
    let tools: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(tools, ["broken", "echo", "stuck"]);
    // The header records the same order.
    let header = &harness.headers()[0];
    let logged: Vec<&str> = header
        .header
        .tools
        .iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(logged, ["broken", "echo", "stuck"]);
}

/// An assembly that fails ends the turn before its step, as dsh's pre-step
/// assembles before `step/start` (`agent.ts:272`).
#[tokio::test]
async fn a_failed_assembly_ends_the_turn_before_its_step() {
    let mut prompt = quiet(None);
    prompt.tool_order = Some(vec!["ghost".into(), TOOL_ORDER_REST.into()]);
    let harness = Harness::with(
        vec![text("never")],
        Setup {
            loop_config: LoopConfig::new(ROUTE, MODEL),
            prompt,
        },
        Vec::new(),
    )
    .await;
    harness.say("hi").await;
    assert_eq!(harness.types(), ["turn/start", "turn/end"]);
    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("the turn failed: {endings:?}");
    };
    assert_eq!(failure.code, "UNKNOWN");
    assert_eq!(
        failure.message,
        "toolOrder lists unregistered tool \"ghost\"; known tools: broken, echo, stuck"
    );
    assert!(harness.model.requests().is_empty());
}

/// A prompt that doesn't render fails its step, as dsh renders it at the
/// step's start (`agent.ts:407`).
#[tokio::test]
async fn a_prompt_that_doesnt_render_fails_its_step() {
    let harness = Harness::with(
        vec![text("never")],
        Setup {
            loop_config: LoopConfig::new(ROUTE, MODEL),
            prompt: quiet(Some("Hello {{missing}}.")),
        },
        Vec::new(),
    )
    .await;
    harness.say("hi").await;
    assert_eq!(
        harness.types(),
        ["turn/start", "step/start", "step/end", "turn/end"]
    );
    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("the turn failed: {endings:?}");
    };
    assert!(
        failure.message.starts_with(
            "unknown prompt variable \"{{missing}}\" in section \"deployment:persona-prefix\""
        ),
        "{}",
        failure.message
    );
    assert!(harness.model.requests().is_empty());
}

/// A section registered in one session's scope reaches that agent alone.
#[tokio::test]
async fn a_section_scoped_to_one_session_reaches_its_agent_only() {
    let harness = Harness::new(vec![text("one"), text("two")]).await;
    let _ = harness
        .prompt
        .section(
            Some(&Scope::new(id().as_str())),
            Section::new("child:extra", 50.0, "Only a1."),
        )
        .unwrap();
    harness.say("hi").await;
    let other = harness.agents.open(SessionId::new("b1")).unwrap();
    other.followup(user("hello")).unwrap();
    idle(other.as_ref()).await;
    let requests = harness.model.requests();
    assert_eq!(requests[0].system.as_deref(), Some("Only a1."));
    assert_eq!(requests[1].system, None);
}

/// The saved-stream check every harness runs when dropped would catch an
/// answer that its stream doesn't assemble to: here, one edited after it
/// was logged.
#[tokio::test]
async fn the_stream_check_catches_an_answer_its_stream_doesnt_give() {
    let harness = Harness::new(vec![text("hello")]).await;
    harness.say("hi").await;
    let mut events = harness.events();
    assert_eq!(check_assistant_streams(&events), Ok(()));
    let at = events
        .iter()
        .position(|entry| matches!(entry.event, SessionEvent::AssistantMessage { .. }))
        .unwrap();
    let SessionEvent::AssistantMessage { data, .. } = &mut events[at].event else {
        unreachable!()
    };
    assert!(!data.stream.is_empty());
    data.message.content = vec![karaka_session::ContentBlock::text("edited")];
    assert_eq!(
        check_assistant_streams(&events),
        Err(format!(
            "seed assistant/message at index {at} content disagrees with its embedded stream"
        ))
    );
}

/// A tool that panics gets an error result, as a dsh tool that throws gets
/// `Error: <message>` (`core/tools/src/index.ts:1917-1924`), and the turn
/// carries on. The agent's next followup runs too.
#[tokio::test]
async fn a_tool_that_panics_gets_an_error_result_and_the_agent_carries_on() {
    let harness = Harness::new(vec![
        calls(&[call("c1", "panicky", "{}")]),
        text("it broke"),
        text("next"),
    ])
    .await;
    harness.tools.register(Arc::new(Panicky)).unwrap();
    harness.say("try it").await;

    let events = harness.events();
    let (result, _, _) = tool_result(&events[8]);
    assert_eq!(
        result,
        ToolResult {
            call_id: "c1".into(),
            content: "Error: the tool broke".into(),
            is_error: true
        }
    );
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);

    harness.say("again").await;
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Completed, TurnEndReason::Completed]
    );
}

/// A model stream that panics ends its turn with an `UNKNOWN` error, as a
/// dsh stream that throws does (`agent.ts:375-382`), after logging the
/// attempt. The agent's next followup runs.
#[tokio::test]
async fn a_model_stream_that_panics_fails_its_turn_and_the_agent_carries_on() {
    let harness = Harness::new(vec![Reply::Panic(vec![delta("hel")]), text("next")]).await;
    harness.say("try it").await;

    let mut expected = vec!["turn/start"];
    expected.extend(&ANSWER_STEP[..5]);
    expected.extend(["assistant/attempt", "step/end", "turn/end"]);
    assert_eq!(harness.types(), expected);
    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("the turn failed: {endings:?}");
    };
    assert_eq!(failure.code, "UNKNOWN");
    assert!(
        failure.message.contains("the stream broke"),
        "{}",
        failure.message
    );

    harness.say("again").await;
    assert_eq!(harness.endings().len(), 2);
    assert_eq!(harness.endings()[1], TurnEndReason::Completed);
}

/// dsh: "surfaces max-tokens as the turn-end reason when the last step is
/// cut off" (`loop.spec.ts:1439`) and "does not dispatch tool calls from a
/// max-tokens-truncated step" (`loop.spec.ts:1530`).
#[tokio::test]
async fn an_answer_cut_off_by_the_limit_ends_the_turn_with_max_tokens() {
    // The cut-off call streams, and the assembler drops it.
    let harness = Harness::new(vec![answer(
        &[
            Block::Text("truncat".into()),
            Block::ToolCall(call("c1", "echo", r#"{"te"#)),
        ],
        FinishReason::MaxTokens,
    )])
    .await;
    harness.say("go").await;

    let mut expected = vec!["turn/start"];
    expected.extend(ANSWER_STEP);
    expected.push("turn/end");
    assert_eq!(harness.types(), expected);
    assert_eq!(harness.endings(), [TurnEndReason::MaxTokens]);
    assert_eq!(harness.model.requests().len(), 1);
    // The text is kept, the call isn't.
    let history = harness.sessions.messages(&id()).unwrap();
    assert_eq!(history[1].content, [Block::Text("truncat".into())]);
}

/// dsh: "a completed step after no max-tokens keeps the turn completed"
/// (`loop.spec.ts:1512`): the reason doesn't leak into the next turn.
#[tokio::test]
async fn max_tokens_ends_only_its_own_turn() {
    let harness = Harness::new(vec![
        answer(&[Block::Text("cut".into())], FinishReason::MaxTokens),
        text("clean"),
    ])
    .await;
    harness.say("first").await;
    harness.say("second").await;
    assert_eq!(
        harness.endings(),
        [TurnEndReason::MaxTokens, TurnEndReason::Completed]
    );
}

/// dsh: "records normalized model errors on the turn boundary"
/// (`loop.spec.ts:1861`).
#[tokio::test]
async fn a_model_error_ends_the_turn_with_a_structured_failure() {
    let mut busy = ModelError::new(ErrorKind::RateLimited, "Slow down.");
    busy.retry_after = Some(Duration::from_secs(2));
    let harness = Harness::new(vec![Reply::Events(vec![Ok(delta("Hel")), Err(busy)])]).await;
    harness.say("hi").await;

    // No assistant message: the partial answer belongs to a failed call.
    // An attempt records that the call was made (dsh: `agent.ts:489-494`).
    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/attempt",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Error(Failure {
            message: "Slow down.".into(),
            code: "RATE_LIMIT".into(),
            status: None,
            provider_retry_after_ms: Some(2000.into()),
            request_id: None,
            offload_images: None,
        })]
    );
}

/// dsh: a turn that ends with an error stops the driver (`kick`,
/// `agent.ts:252-265`). A message queued behind it stays queued, and the
/// next followup's driver runs it first, then the new one.
#[tokio::test]
async fn a_failed_turn_leaves_the_queue_for_the_next_followup() {
    let harness = Harness::new(vec![
        Reply::Events(vec![Err(ModelError::new(
            ErrorKind::Unavailable,
            "overloaded",
        ))]),
        text("second answer"),
        text("third answer"),
    ])
    .await;
    let agent = harness.agent();
    agent.followup(user("first")).unwrap();
    agent.followup(user("second")).unwrap();
    idle(agent.as_ref()).await;

    // Only the failed turn ran; "second" is still waiting.
    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("expected one failed turn, got {endings:?}");
    };
    assert_eq!(failure.code, "SERVER");
    assert_eq!(harness.model.requests().len(), 1);

    agent.followup(user("third")).unwrap();
    idle(agent.as_ref()).await;

    let endings = harness.endings();
    assert_eq!(endings.len(), 3);
    assert!(matches!(endings[0], TurnEndReason::Error(_)));
    assert_eq!(
        endings[1..],
        [TurnEndReason::Completed, TurnEndReason::Completed]
    );
    // The queued message ran before the new one.
    let said: Vec<String> = harness
        .events()
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::UserMessage { message, .. } => Some(message.id),
            _ => None,
        })
        .collect();
    assert_eq!(said, ["user-first", "user-second", "user-third"]);
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].messages,
        [
            Message::user("first"),
            Message::user("second"),
            Message {
                role: Role::Assistant,
                content: vec![Block::Text("second answer".into())]
            },
            Message::user("third"),
        ]
    );
}

/// dsh resolves the route when it calls the model, and fails the turn with
/// `NO_ADAPTER` if nothing serves it.
#[tokio::test]
async fn a_route_with_no_model_ends_the_turn_with_no_adapter() {
    let harness = Harness::with(Vec::new(), LoopConfig::new("nowhere", MODEL), Vec::new()).await;
    harness.say("hi").await;

    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("expected one failed turn, got {endings:?}");
    };
    assert_eq!(failure.code, "NO_ADAPTER");
    assert_eq!(
        failure.message,
        "no model is registered under the route \"nowhere\""
    );
    assert!(harness.model.requests().is_empty());
    // No model, so no provider to name in a header, and no call to leave an
    // attempt.
    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "step/end",
            "turn/end",
        ]
    );
}

/// dsh: "cancel() mid-stream ends the turn with reason aborted"
/// (`loop.spec.ts:1413`) and "cancel mid-stream finalizes the streamed
/// prefix onto the surface" (`cancel.spec.ts:571`).
#[tokio::test]
async fn a_cancel_mid_stream_keeps_the_partial_answer() {
    let waiting = Arc::new(Notify::new());
    let harness = Harness::new(vec![
        Reply::Hang(vec![delta("partial")], Arc::clone(&waiting)),
        text("after"),
    ])
    .await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    waiting.notified().await;
    agent.cancel(CancelCause::User);
    idle(agent.as_ref()).await;

    let mut expected = vec!["turn/start"];
    expected.extend(ANSWER_STEP);
    expected.push("turn/end");
    assert_eq!(harness.types(), expected);
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );

    let events = harness.events();
    let SessionEvent::AssistantMessage { data, op } = &events[6].event else {
        panic!("expected the partial answer, got {:?}", events[6].event);
    };
    assert!(data.interrupted);
    assert_eq!(*op, SurfaceOp::Append);
    assert_eq!(data.usage, None);
    assert_eq!(data.message.source.provider, PROVIDER);
    assert_eq!(data.message.blocks(), [Block::Text("partial".into())]);

    // The next request shows the model what the user saw.
    harness.say("continue").await;
    let requests = harness.model.requests();
    assert_eq!(
        requests[1].messages[1],
        Message {
            role: Role::Assistant,
            content: vec![Block::Text("partial".into())]
        }
    );
    assert_eq!(
        harness.endings(),
        [
            TurnEndReason::Aborted(CancelCause::User),
            TurnEndReason::Completed
        ]
    );
}

/// A cancel after the usage came keeps it on the partial answer, as dsh's
/// interrupted message carries `live.usage` (`agent.ts:480`).
#[tokio::test]
async fn a_cancel_after_the_usage_keeps_it_on_the_partial_answer() {
    let waiting = Arc::new(Notify::new());
    let mut events = vec![delta("partial")];
    events.push(Event::Usage { usage: usage() });
    let harness = Harness::new(vec![Reply::Hang(events, Arc::clone(&waiting))]).await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    waiting.notified().await;
    agent.cancel(CancelCause::User);
    idle(agent.as_ref()).await;

    let events = harness.events();
    let SessionEvent::AssistantMessage { data, .. } = &events[6].event else {
        panic!("expected the partial answer, got {:?}", events[6].event);
    };
    assert!(data.interrupted);
    assert_eq!(data.usage, Some(TokenUsage::from(usage())));
}

/// dsh: "cancel before any visible content finalizes nothing"
/// (`cancel.spec.ts:775`). Spaces aren't content, and a call that never ran
/// isn't kept.
#[tokio::test]
async fn a_cancel_before_any_text_keeps_no_answer() {
    let waiting = Arc::new(Notify::new());
    let harness = Harness::new(vec![Reply::Hang(
        vec![
            delta("   "),
            Event::BlockEnd {
                index: 1,
                block: Block::ToolCall(call("c1", "echo", "{}")),
            },
        ],
        Arc::clone(&waiting),
    )])
    .await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    waiting.notified().await;
    agent.cancel(CancelCause::User);
    idle(agent.as_ref()).await;

    // An attempt records the call that left nothing to keep (dsh:
    // `agent.ts:483-488`).
    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/attempt",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );
}

/// A cancel while a tool runs drops the tool. Its call started, so it gets
/// an "outcome unknown" result; the call after it never started. Both come
/// from `karaka-session`'s `ToolCallRecovery`, so the log stays valid.
#[tokio::test]
async fn a_cancel_during_a_tool_call_answers_every_call() {
    let harness = Harness::new(vec![calls(&[
        call("c1", "stuck", "{}"),
        call("c2", "echo", r#"{"text":"never"}"#),
    ])])
    .await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    harness.stuck.notified().await;
    agent.cancel(CancelCause::Hook {
        reason: "policy stopped this turn".into(),
    });
    idle(agent.as_ref()).await;

    assert!(
        harness.stuck_dropped.load(Ordering::SeqCst),
        "the tool was stopped"
    );
    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/message",
            "tool/call",
            "tool/result",
            "tool/result",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::Hook {
            reason: "policy stopped this turn".into()
        })]
    );
    let events = harness.events();
    let (started, sources, code) = tool_result(&events[8]);
    assert_eq!(started.call_id, "c1");
    assert!(started.is_error);
    assert_eq!(sources, [events[7].seq.0]);
    assert_eq!(code.as_deref(), Some(TOOL_OUTCOME_UNKNOWN));
    let (skipped, sources, code) = tool_result(&events[9]);
    assert_eq!(skipped.call_id, "c2");
    assert!(sources.is_empty());
    assert_eq!(code.as_deref(), Some(TOOL_NOT_STARTED));

    // Every call has its result, so the session can carry on.
    let history = harness.sessions.messages(&id()).unwrap();
    assert_eq!(history[2].content.len(), 2);
}

/// dsh: "cancel after waking send closes its synchronously opened turn
/// without a step" (`cancel.spec.ts:333`): the cancel empties the queue, so
/// the turn the followup opened has no input.
#[tokio::test]
async fn a_cancel_right_after_a_followup_ends_its_turn_without_a_step() {
    let harness = Harness::new(vec![text("should not run")]).await;
    let agent = harness.agent();
    agent.followup(user("drop me first")).unwrap();
    agent.followup(user("drop me second")).unwrap();
    agent.cancel(CancelCause::User);
    idle(agent.as_ref()).await;

    assert_eq!(harness.types(), ["turn/start", "turn/end"]);
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );
    assert!(harness.model.requests().is_empty());
}

/// dsh: "cancel() on an idle agent with nothing queued is a no-op; the next
/// prompt runs" (`cancel.spec.ts:147`).
#[tokio::test]
async fn a_cancel_while_idle_doesnt_stop_the_next_turn() {
    let harness = Harness::new(vec![text("reply")]).await;
    harness.agent().cancel(CancelCause::User);
    harness.say("real prompt").await;
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

/// Two followups sent at once run as two turns, one after the other, and
/// the second turn's request carries the first turn.
#[tokio::test]
async fn two_queued_followups_run_as_two_turns() {
    let harness = Harness::new(vec![text("one"), text("two")]).await;
    let agent = harness.agent();
    agent.followup(user("first")).unwrap();
    agent.followup(user("second")).unwrap();
    idle(agent.as_ref()).await;

    // The prompt is logged once: it hasn't changed by the second turn.
    let mut expected = vec!["turn/start"];
    expected.extend(ANSWER_STEP);
    expected.extend(["turn/end", "turn/start", "step/start", "user/message"]);
    expected.extend(["assistant/message", "step/end", "turn/end"]);
    assert_eq!(harness.types(), expected);
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Completed, TurnEndReason::Completed]
    );
    let turns: Vec<u64> = harness
        .events()
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::TurnStart(start) => Some(start.turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [1, 2]);

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages,
        [
            Message::user("first"),
            Message {
                role: Role::Assistant,
                content: vec![Block::Text("one".into())]
            },
            Message::user("second"),
        ]
    );
}

/// The same agent comes back for the same session, so two connections share
/// its turns.
#[tokio::test]
async fn opening_a_session_twice_gives_the_same_agent() {
    let harness = Harness::new(Vec::new()).await;
    let first = harness.agent();
    let second = harness.agent();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.id(), &id());
}

/// A session read back from storage continues where it stopped: its next
/// request is derived from the restored log, earlier tool results included,
/// and its turns carry on numbering.
#[tokio::test]
async fn a_reloaded_session_sends_its_earlier_tool_results() {
    let ping = call("c1", "echo", r#"{"text":"ping"}"#);
    let before = Harness::new(vec![calls(std::slice::from_ref(&ping)), text("done")]).await;
    before.say("use the tool").await;
    let stored = before.log();
    // What storage gives back is the JSON of each event.
    let stored: Vec<Entry> = stored
        .iter()
        .map(|entry| serde_json::from_str(&serde_json::to_string(entry).unwrap()).unwrap())
        .collect();

    let after = Harness::with(vec![text("again")], config(None), stored.clone()).await;
    after.say("and again").await;

    let requests = after.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].messages,
        [
            Message::user("use the tool"),
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolCall(ping)]
            },
            Message {
                role: Role::User,
                content: vec![Block::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    content: "echo: ping".into(),
                    is_error: false
                })]
            },
            Message {
                role: Role::Assistant,
                content: vec![Block::Text("done".into())]
            },
            Message::user("and again"),
        ]
    );
    let events = after.log();
    assert_eq!(&events[..stored.len()], stored.as_slice());
    let later: Vec<&Entry> = events[stored.len()..]
        .iter()
        .filter(|entry| !matches!(entry.event, SessionEvent::InboxSpliced(_)))
        .collect();
    assert_eq!(
        later[0].event,
        SessionEvent::turn_start(2),
        "the turns carry on numbering"
    );
    // A new loop's first request logs a `resume` header. The prompt and the
    // route haven't changed, so no system message and no context.
    let after_types: Vec<&str> = later.iter().map(|entry| entry.event.event_type()).collect();
    assert_eq!(
        after_types,
        [
            "turn/start",
            "step/start",
            "user/message",
            "request/header",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    let headers = after.headers();
    assert_eq!(
        headers.iter().map(|data| data.reason).collect::<Vec<_>>(),
        [RequestHeaderReason::Initial, RequestHeaderReason::Resume]
    );
    assert_eq!(headers[0].header, headers[1].header);
    assert_eq!(after.contexts().len(), 1);
}

/// A stored log whose last turn a crash left open is closed before the
/// agent runs anything on it, as dsh's resume repairs the log.
#[tokio::test]
async fn opening_a_session_a_crash_left_open_closes_its_turn() {
    let ping = call("c1", "echo", r#"{"text":"ping"}"#);
    let crashed = Harness::new(vec![calls(std::slice::from_ref(&ping)), text("done")]).await;
    crashed.say("use the tool").await;
    // Cut the log just after the call started, as a crash would. The two
    // inbox splices are the followup and the step's claim.
    let stored: Vec<Entry> = crashed.log().into_iter().take(10).collect();
    assert_eq!(stored[9].event.event_type(), "tool/call");

    let after = Harness::with(vec![text("recovered")], config(None), stored).await;
    let _agent = after.agent();
    assert_eq!(after.types()[8..], ["tool/result", "step/end", "turn/end"]);
    assert_eq!(after.endings(), [TurnEndReason::Interrupted]);

    after.say("again").await;
    assert_eq!(
        after.endings(),
        [TurnEndReason::Interrupted, TurnEndReason::Completed]
    );
}

/// A prompt changed since the session was stored is logged in place, after
/// the history, as dsh does for a route that takes prompts in its history.
#[tokio::test]
async fn a_changed_prompt_is_logged_where_it_changed() {
    let before = Harness::with(vec![text("one")], config(Some("Be brief.")), Vec::new()).await;
    before.say("first").await;

    let after = Harness::with(
        vec![text("two")],
        config(Some("Be thorough.")),
        before.log(),
    )
    .await;
    after.say("second").await;

    assert_eq!(
        after.types()[9..],
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    let request = &after.model.requests()[0];
    assert_eq!(request.system.as_deref(), Some("Be brief."));
    assert_eq!(request.messages[2], Message::system("Be thorough."));
    assert_eq!(request.messages[3], Message::user("second"));
}

/// A prompt taken away replaces every system message on the surface with
/// an empty one, as dsh clears a prompt.
#[tokio::test]
async fn a_removed_prompt_is_cleared_from_the_history() {
    let before = Harness::with(vec![text("one")], config(Some("Be brief.")), Vec::new()).await;
    before.say("first").await;

    let after = Harness::with(vec![text("two")], config(None), before.log()).await;
    after.say("second").await;

    let events = after.events();
    // The first system message held the prompt; 9 and 10 open the new
    // turn and its step.
    let prompt = events[2].seq;
    assert_eq!(events[2].event.event_type(), "system/message");
    let SessionEvent::SystemMessage { data, surface } = &events[11].event else {
        panic!("expected the cleared prompt, got {:?}", events[11].event);
    };
    assert!(data.message.content.is_empty());
    assert_eq!(
        surface.op,
        SurfaceOp::Replace {
            start: prompt,
            end: prompt
        }
    );
    assert_eq!(surface.sources, [prompt]);
    let request = &after.model.requests()[0];
    assert_eq!(request.system, None);
    assert!(
        request
            .messages
            .iter()
            .all(|message| message.role != Role::System)
    );
    // The replacement starts a new series, so the `resume` header says so.
    let headers = after.headers();
    assert_eq!(headers[1].reason, RequestHeaderReason::Resume);
    assert!(headers[1].starts_series);
}

/// Followups sent from threads outside the runtime, such as one reading
/// stdin, still run, each as a turn of its own, one turn at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn followups_from_other_threads_run_one_turn_at_a_time() {
    let harness = Harness::new((0..4).map(|n| text(&format!("answer {n}"))).collect()).await;
    let agent = harness.agent();
    std::thread::scope(|scope| {
        for n in 0..4 {
            let agent = Arc::clone(&agent);
            scope.spawn(move || agent.followup(user(&format!("question {n}"))));
        }
    });
    idle(agent.as_ref()).await;

    assert_eq!(harness.endings(), vec![TurnEndReason::Completed; 4]);
    // Every turn ends before the next one starts.
    let brackets: Vec<String> = harness
        .types()
        .into_iter()
        .filter(|kind| kind.starts_with("turn/"))
        .collect();
    assert_eq!(brackets, ["turn/start", "turn/end"].repeat(4));
    assert_eq!(harness.model.requests().len(), 4);
}

fn reasons(harness: &Harness) -> Vec<RequestHeaderReason> {
    harness.headers().iter().map(|data| data.reason).collect()
}

/// A loop over a stored log whose model differs logs a `resume` header for
/// the new model, and a new context. dsh logs `resume` for every loop's
/// first request, changed or not ("a fresh loop instance over a seeded log
/// anchors with a resume snapshot", `request-reconstruction.spec.ts:801`,
/// and the replacement model in its effort case, `:244-266`).
#[tokio::test]
async fn a_loop_with_another_model_resumes_with_its_own_header_and_context() {
    let before = Harness::new(vec![text("one")]).await;
    before.say("first").await;

    let after = Harness::with(
        vec![text("two")],
        LoopConfig::new(ROUTE, "mock-model-2"),
        before.log(),
    )
    .await;
    after.say("second").await;

    assert_eq!(
        after.types()[9..],
        [
            "turn/start",
            "step/start",
            "user/message",
            "request/header",
            "request/context",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(
        reasons(&after),
        [RequestHeaderReason::Initial, RequestHeaderReason::Resume]
    );
    assert_eq!(after.headers()[1].header.config.model, "mock-model-2");
    assert_eq!(after.contexts(), [context(MODEL), context("mock-model-2")]);
}

/// A header that differs from the one in force within one loop is logged
/// as a `change`. With one config per loop, what can change today is the
/// tool set: a tool registered between two turns. The route is the same,
/// so no new context. (dsh would also log a `developer/message` announcing
/// the tool; that isn't ported.)
#[tokio::test]
async fn a_tool_registered_between_turns_logs_a_change() {
    struct Later;

    impl Tool for Later {
        fn schema(&self) -> ToolSchema {
            schema("later")
        }

        fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
            Box::pin(async { ToolOutput::ok("late") })
        }
    }

    let harness = Harness::new(vec![text("one"), text("two"), text("three")]).await;
    harness.say("first").await;
    harness.tools.register(Arc::new(Later)).unwrap();
    harness.say("second").await;
    harness.say("third").await;

    // The third turn changed nothing, so it logs nothing.
    assert_eq!(
        reasons(&harness),
        [RequestHeaderReason::Initial, RequestHeaderReason::Change]
    );
    let tools = harness.headers()[1].header.tools.clone().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, ["broken", "echo", "later", "stuck"]);
    assert_eq!(harness.contexts().len(), 1);
}

/// With nothing changed, a request after part of the history was replaced
/// starts a new series, logged as `series`. Here the test replaces a
/// message itself, as compaction would.
#[tokio::test]
async fn a_request_after_a_replacement_logs_a_series() {
    let harness = Harness::new(vec![text("one"), text("two")]).await;
    harness.say("first").await;
    // The first user message.
    let first = harness.events()[3].seq;
    assert_eq!(harness.events()[3].event.event_type(), "user/message");
    let summary = UserMessage::text("summary", UserSource::user(), "first, in short");
    harness
        .sessions
        .append(
            &id(),
            SessionEvent::UserMessage {
                message: summary,
                surface: SurfaceIntent {
                    op: SurfaceOp::Replace {
                        start: first,
                        end: first,
                    },
                    sources: vec![first],
                },
            },
        )
        .unwrap();
    harness.say("second").await;

    let headers = harness.headers();
    assert_eq!(
        headers.iter().map(|data| data.reason).collect::<Vec<_>>(),
        [RequestHeaderReason::Initial, RequestHeaderReason::Series]
    );
    // `series` says it already, so it carries no marker.
    assert!(!headers[1].starts_series);
    assert_eq!(headers[0].header, headers[1].header);
}

/// dsh's `prepareRequest`: with no effort configured, a restarted loop
/// keeps the effort the header in force asked for, if that header was for
/// the same provider and model ("logs adapter defaults, supports per-turn
/// effort changes, and restores the effective value",
/// `request-reconstruction.spec.ts:210`).
#[tokio::test]
async fn a_resumed_loop_keeps_the_effort_the_session_ran_at() {
    let mut high = LoopConfig::new(ROUTE, MODEL);
    high.effort = Some(Effort::High);
    let before = Harness::with(vec![text("one")], high, Vec::new()).await;
    before.say("first").await;
    assert_eq!(
        before.headers()[0]
            .header
            .config
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
    assert_eq!(before.model.requests()[0].effort, Some(Effort::High));

    // The same model, no effort set: the session's effort carries on.
    let same = Harness::with(
        vec![text("two")],
        LoopConfig::new(ROUTE, MODEL),
        before.log(),
    )
    .await;
    same.say("second").await;
    assert_eq!(same.model.requests()[0].effort, Some(Effort::High));
    let resumed = &same.headers()[1];
    assert_eq!(resumed.reason, RequestHeaderReason::Resume);
    assert_eq!(
        resumed.header.config.reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(resumed.header, before.headers()[0].header);

    // Another model: the effort was the old model's, so it doesn't.
    let other = Harness::with(
        vec![text("two")],
        LoopConfig::new(ROUTE, "mock-model-2"),
        before.log(),
    )
    .await;
    other.say("second").await;
    assert_eq!(other.model.requests()[0].effort, None);
    assert_eq!(other.headers()[1].header.config.reasoning_effort, None);

    // An effort set in the config wins over the session's.
    let mut low = LoopConfig::new(ROUTE, MODEL);
    low.effort = Some(Effort::Low);
    let configured = Harness::with(vec![text("two")], low, before.log()).await;
    configured.say("second").await;
    assert_eq!(configured.model.requests()[0].effort, Some(Effort::Low));
    assert_eq!(
        configured.headers()[1]
            .header
            .config
            .reasoning_effort
            .as_deref(),
        Some("low")
    );
}

/// The effort carries on only from a header for the same provider. A
/// stored header from another provider, for a model of the same id, is
/// left alone.
#[tokio::test]
async fn an_effort_from_another_provider_doesnt_carry_on() {
    let before = Harness::new(vec![text("one")]).await;
    before.say("first").await;
    let mut stored = before.log();
    let mut config = CallConfig::new("someone-else", MODEL);
    config.reasoning_effort = Some("max".into());
    stored.push(Entry {
        seq: Seq(stored.len() as u64),
        time: stored[0].time,
        ignorable: false,
        event: SessionEvent::request_header(
            RequestHeader::new(config, &[]).unwrap(),
            RequestHeaderReason::Change,
        ),
    });

    let after = Harness::with(vec![text("two")], LoopConfig::new(ROUTE, MODEL), stored).await;
    after.say("second").await;
    assert_eq!(after.model.requests()[0].effort, None);
    let last = after.headers().pop().unwrap();
    assert_eq!(last.reason, RequestHeaderReason::Resume);
    assert_eq!(last.header.config.provider, PROVIDER);
    assert_eq!(last.header.config.reasoning_effort, None);
}

// The request-error hook. Ported from dsh's
// `agent-loop/tests/request-error.spec.ts`.

/// A handler that runs `decide` on each failed request.
struct Handler<F>(F);

impl<F> RequestErrorHandler for Handler<F>
where
    F: Fn(&RequestError) -> Result<Option<RequestErrorAction>, String> + Send + Sync,
{
    fn handle<'a>(&'a self, error: &'a RequestError, _next: Next<'a>) -> HookFuture<'a> {
        let decision = (self.0)(error).map_err(Into::into);
        Box::pin(async move { decision })
    }
}

fn failing(kind: ErrorKind, message: &str) -> Reply {
    Reply::Events(vec![Err(ModelError::new(kind, message))])
}

/// dsh: "lets each failed request return a retry action before its turn
/// closes" (`request-error.spec.ts:52`). Each failure leaves an attempt; the
/// retried request logs no new header, context or user message.
#[tokio::test]
async fn a_handler_can_retry_each_failed_request() {
    let harness = Harness::new(vec![
        failing(ErrorKind::RateLimited, "busy"),
        failing(ErrorKind::Unavailable, "unavailable"),
        text("ok"),
    ])
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&seen);
    harness
        .hooks
        .register(Arc::new(Handler(move |error: &RequestError| {
            noted.lock().unwrap().push((
                error.turn,
                error.step,
                error.provider.clone(),
                error.failure.code.clone(),
                error.retry_policy.clone(),
            ));
            Ok(Some(RequestErrorAction::Retry))
        })));
    harness.say("go").await;

    assert_eq!(
        harness.types(),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/attempt",
            "assistant/attempt",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
    let seen = seen.lock().unwrap();
    let codes: Vec<(u64, u64, &str, &str)> = seen
        .iter()
        .map(|(turn, step, provider, code, _)| (*turn, *step, provider.as_str(), code.as_str()))
        .collect();
    assert_eq!(
        codes,
        [(1, 1, PROVIDER, "RATE_LIMIT"), (1, 1, PROVIDER, "SERVER")]
    );
    assert!(
        seen.iter()
            .all(|(.., policy)| *policy == RetryPolicy::default())
    );
    // Every attempt asked the model with the same history.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.messages == [Message::user("go")])
    );
}

/// dsh: "lets cancellation win over a retry action"
/// (`request-error.spec.ts:105`).
#[tokio::test]
async fn a_cancel_wins_over_a_retry() {
    let harness = Harness::new(vec![
        failing(ErrorKind::RateLimited, "busy"),
        text("unused"),
    ])
    .await;
    let agents = Arc::clone(&harness.agents);
    harness
        .hooks
        .register(Arc::new(Handler(move |_error: &RequestError| {
            agents.open(id()).unwrap().cancel(CancelCause::User);
            Ok(Some(RequestErrorAction::Retry))
        })));
    harness.say("go").await;

    assert_eq!(harness.model.requests().len(), 1);
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );
    assert_eq!(
        harness.types()[6..],
        ["assistant/attempt", "step/end", "turn/end"]
    );
}

/// dsh: "does not retry when the recovery listener fails before returning
/// its action" (`request-error.spec.ts:125`). The turn ends with the
/// handler's error.
#[tokio::test]
async fn a_handler_that_fails_means_no_retry() {
    let harness = Harness::new(vec![
        failing(ErrorKind::RateLimited, "busy"),
        text("unused"),
    ])
    .await;
    harness
        .hooks
        .register(Arc::new(Handler(|_error: &RequestError| {
            Err("recovery failed".to_owned())
        })));
    harness.say("go").await;

    assert_eq!(harness.model.requests().len(), 1);
    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("expected one failed turn, got {endings:?}");
    };
    assert_eq!(
        (failure.message.as_str(), failure.code.as_str()),
        ("recovery failed", "UNKNOWN")
    );
}

/// A handler that panics fails its turn, not the agent: the next turn runs.
#[tokio::test]
async fn a_handler_that_panics_means_no_retry() {
    let harness = Harness::new(vec![failing(ErrorKind::RateLimited, "busy"), text("later")]).await;
    harness
        .hooks
        .register(Arc::new(Handler(|error: &RequestError| {
            if error.turn == 1 {
                panic!("a broken handler");
            }
            Ok(None)
        })));
    harness.say("go").await;
    harness.say("again").await;

    let endings = harness.endings();
    let TurnEndReason::Error(failure) = &endings[0] else {
        panic!("expected a failed turn, got {endings:?}");
    };
    assert_eq!(failure.message, "a request-error handler panicked");
    assert_eq!(endings[1], TurnEndReason::Completed);
}

/// The chain runs in the order handlers were registered. A handler that
/// calls `next` hands the decision on; here the second asks for a retry.
#[tokio::test]
async fn a_handler_can_hand_the_decision_on() {
    struct Passes(Arc<Mutex<Vec<&'static str>>>);

    impl RequestErrorHandler for Passes {
        fn handle<'a>(&'a self, _error: &'a RequestError, next: Next<'a>) -> HookFuture<'a> {
            Box::pin(async move {
                self.0.lock().unwrap().push("first");
                next.run().await
            })
        }
    }

    let harness = Harness::new(vec![failing(ErrorKind::Unavailable, "down"), text("ok")]).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    harness.hooks.register(Arc::new(Passes(Arc::clone(&order))));
    let noted = Arc::clone(&order);
    harness
        .hooks
        .register(Arc::new(Handler(move |_error: &RequestError| {
            noted.lock().unwrap().push("second");
            Ok(Some(RequestErrorAction::Retry))
        })));
    harness.say("go").await;

    assert_eq!(*order.lock().unwrap(), ["first", "second"]);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
    assert_eq!(harness.model.requests().len(), 2);
}

// Live frames. Ported from dsh's `loop.spec.ts` ("publishes one dense live
// attempt", "settles a failed attempt before retrying with a new dense
// attempt") and `cancel.spec.ts:1413`.

/// Every frame `agent` publishes from now on, until the subscription goes.
fn listen(agent: &dyn Agent) -> (Arc<Mutex<Vec<AssistantStreamFrame>>>, FrameSubscription) {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&frames);
    let subscription = agent.subscribe(Arc::new(move |frame| {
        noted.lock().unwrap().push(frame.clone());
    }));
    (frames, subscription)
}

/// A frame as text, without its time, so a sequence reads at a glance.
fn frame_line(frame: &AssistantStreamFrame) -> String {
    match frame {
        AssistantStreamFrame::Start {
            attempt_id,
            revision,
            turn,
            step,
        } => format!("{revision} start {attempt_id} turn {turn} step {step}"),
        AssistantStreamFrame::Chunk {
            attempt_id,
            revision,
            index,
            chunk,
            ..
        } => format!("{revision} chunk {attempt_id} #{index} {chunk:?}"),
        AssistantStreamFrame::End {
            attempt_id,
            revision,
            index,
            outcome,
        } => {
            let outcome = match outcome {
                StreamOutcome::Committed { event_type, seq } => {
                    format!("{} at {seq}", event_type.event_type())
                }
                StreamOutcome::Abandoned => "abandoned".into(),
            };
            format!("{revision} end {attempt_id} after {index}: {outcome}")
        }
    }
}

fn frame_lines(frames: &Mutex<Vec<AssistantStreamFrame>>) -> Vec<String> {
    frames.lock().unwrap().iter().map(frame_line).collect()
}

/// dsh: "publishes one dense live attempt and commits one v2 message with
/// the exact embedded stream" (`loop.spec.ts:76`). The `end` names the seq
/// the answer was logged at.
#[tokio::test]
async fn a_call_publishes_start_chunks_and_a_committed_end() {
    let harness = Harness::new(vec![text("hello there")]).await;
    let agent = harness.agent();
    let (frames, _subscription) = listen(agent.as_ref());
    harness.say("hi").await;

    assert_eq!(
        frame_lines(&frames),
        [
            "1 start a1:1 turn 1 step 1",
            "2 chunk a1:1 #0 Event(BlockStart { index: 0, block_type: Text })",
            r#"3 chunk a1:1 #1 Event(TextDelta { index: 0, text: "hello there" })"#,
            r#"4 chunk a1:1 #2 Event(BlockEnd { index: 0, block: Text("hello there") })"#,
            "5 chunk a1:1 #3 Event(Usage { usage: Usage { input_tokens: 10, output_tokens: 5, cache_read_tokens: 0, cache_write_tokens: 0 } })",
            "6 chunk a1:1 #4 Event(Finish { reason: Stop })",
            "7 end a1:1 after 5: assistant/message at 8",
        ]
    );
    assert_eq!(harness.events()[6].event.event_type(), "assistant/message");
    // Each chunk is stamped with when it arrived.
    let times: Vec<i64> = frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            AssistantStreamFrame::Chunk { time, .. } => Some(time.0),
            _ => None,
        })
        .collect();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]) && times[0] > 0);
}

/// dsh: "settles a failed attempt before retrying with a new dense attempt"
/// (`loop.spec.ts:185`). The failed attempt's last chunk is its failure,
/// as dsh's is a `finish` chunk of kind `error`; the retry has an id of its
/// own, and the revisions carry on.
#[tokio::test]
async fn a_retried_call_is_a_new_attempt() {
    let harness = Harness::new(vec![failing(ErrorKind::RateLimited, "busy"), text("ok")]).await;
    harness
        .hooks
        .register(Arc::new(Handler(|_error: &RequestError| {
            Ok(Some(RequestErrorAction::Retry))
        })));
    let agent = harness.agent();
    let (frames, _subscription) = listen(agent.as_ref());
    harness.say("go").await;

    let lines = frame_lines(&frames);
    assert_eq!(
        lines[..4],
        [
            "1 start a1:1 turn 1 step 1",
            r#"2 chunk a1:1 #0 Failed(ModelError { kind: RateLimited, message: "busy", retry_after: None, refusal: None })"#,
            "3 end a1:1 after 1: assistant/attempt at 8",
            "4 start a1:2 turn 1 step 1",
        ]
    );
    assert_eq!(lines[9], "10 end a1:2 after 5: assistant/message at 9");
    let failure = frames.lock().unwrap()[1].clone();
    assert!(matches!(
        failure,
        AssistantStreamFrame::Chunk {
            chunk: StreamChunk::Failed(ModelError {
                kind: ErrorKind::RateLimited,
                ..
            }),
            ..
        }
    ));
}

/// Each step's call is an attempt of its own.
#[tokio::test]
async fn each_step_publishes_its_own_attempt() {
    let ping = call("c1", "echo", r#"{"text":"ping"}"#);
    let harness = Harness::new(vec![calls(std::slice::from_ref(&ping)), text("done")]).await;
    let agent = harness.agent();
    let (frames, _subscription) = listen(agent.as_ref());
    harness.say("use the tool").await;

    let lines = frame_lines(&frames);
    let starts: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains(" start "))
        .collect();
    assert_eq!(
        starts,
        ["1 start a1:1 turn 1 step 1", "8 start a1:2 turn 1 step 2"]
    );
    assert_eq!(lines[6], "7 end a1:1 after 5: assistant/message at 8");
}

/// dsh: "cancel() mid-stream ends the turn with reason aborted"
/// (`loop.spec.ts:1413`): the last frame commits the interrupted answer. A
/// cancel before anything worth keeping commits an attempt instead.
#[tokio::test]
async fn a_cancelled_call_ends_with_what_it_settled_as() {
    for (events, settled) in [
        (vec![delta("partial")], "assistant/message"),
        (vec![delta("  ")], "assistant/attempt"),
    ] {
        let waiting = Arc::new(Notify::new());
        let harness = Harness::new(vec![Reply::Hang(events, Arc::clone(&waiting))]).await;
        let agent = harness.agent();
        let (frames, _subscription) = listen(agent.as_ref());
        agent.followup(user("go")).unwrap();
        waiting.notified().await;
        agent.cancel(CancelCause::User);
        idle(agent.as_ref()).await;

        let lines = frame_lines(&frames);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[2], format!("3 end a1:1 after 1: {settled} at 8"));
    }
}

/// A turn that never reaches a model publishes nothing.
#[tokio::test]
async fn a_call_that_never_starts_publishes_nothing() {
    let harness = Harness::with(Vec::new(), LoopConfig::new("nowhere", MODEL), Vec::new()).await;
    let agent = harness.agent();
    let (frames, _subscription) = listen(agent.as_ref());
    harness.say("hi").await;
    assert!(frames.lock().unwrap().is_empty());
}

/// A listener that panics is dropped, and the turn and the other listeners
/// carry on. A dropped subscription hears nothing.
#[tokio::test]
async fn a_broken_listener_doesnt_stop_the_turn() {
    let harness = Harness::new(vec![text("one"), text("two")]).await;
    let agent = harness.agent();
    let _broken = agent.subscribe(Arc::new(|_frame| panic!("a broken view")));
    let (frames, _subscription) = listen(agent.as_ref());
    let (gone, subscription) = listen(agent.as_ref());
    drop(subscription);
    harness.say("first").await;
    harness.say("second").await;

    assert_eq!(
        harness.endings(),
        [TurnEndReason::Completed, TurnEndReason::Completed]
    );
    // Two calls of 7 frames each: start, 5 chunks, end.
    assert_eq!(frames.lock().unwrap().len(), 14);
    assert!(gone.lock().unwrap().is_empty());
}

// The compact `stream` records each settlement embeds. dsh: `step()` in
// `agent-loop/src/agent.ts:465-545`, with `AssistantStreamAttempt`.

/// The `stream` of the settlement at `seq`, expanded.
fn expanded_stream(harness: &Harness, seq: usize) -> Vec<TimedChunk> {
    let events = harness.events();
    let stream = match &events[seq].event {
        SessionEvent::AssistantMessage { data, .. } => data.stream.clone(),
        SessionEvent::AssistantAttempt(data) => data.stream.clone(),
        other => panic!("expected a settlement at {seq}, got {other:?}"),
    };
    expand_stream(&stream).expect("the records are valid")
}

/// The chunks `frames` carried, with their times, as dsh's JSON.
fn framed_chunks(frames: &Mutex<Vec<AssistantStreamFrame>>) -> Vec<TimedChunk> {
    frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            AssistantStreamFrame::Chunk {
                time,
                chunk: StreamChunk::Event(event),
                ..
            } => Some(TimedChunk {
                time: *time,
                chunk: chunk_json(event),
            }),
            _ => None,
        })
        .collect()
}

/// dsh: "records exact replay chunks inside the durable assistant message"
/// (`loop.spec.ts:998`). The records expand to the very chunks the frames
/// carried, at the same times, and assemble to the logged answer, as
/// dsh's storage verifier checks (`generation.ts:553-577`).
#[tokio::test]
async fn an_answer_embeds_its_exact_stream() {
    let harness = Harness::new(vec![text("hello there")]).await;
    let agent = harness.agent();
    let (frames, _subscription) = listen(agent.as_ref());
    harness.say("hi").await;

    let expanded = expanded_stream(&harness, 6);
    assert_eq!(expanded, framed_chunks(&frames));
    let kinds: Vec<&str> = expanded
        .iter()
        .map(|chunk| chunk.chunk["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["block-start", "text-delta", "block-end", "usage", "finish"]
    );
    assert_eq!(
        expanded[4].chunk,
        json!({ "type": "finish", "reason": { "kind": "stop" } })
    );

    let mut assembled = BlockAssembler::new();
    for chunk in &expanded {
        assembled.push(&chunk_event(&chunk.chunk).unwrap());
    }
    let events = harness.events();
    let SessionEvent::AssistantMessage { data, .. } = &events[6].event else {
        panic!("expected the answer");
    };
    assert_eq!(assembled.blocks(), data.message.blocks());
    assert_eq!(assembled.usage().map(TokenUsage::from), data.usage);
}

/// An answer with a signature: the finish record carries the replay state
/// the message's source has, as a dsh adapter's finish carries it.
#[tokio::test]
async fn the_finish_record_carries_the_answers_replay_state() {
    let signed = Block::Reasoning {
        text: "think".into(),
        replay: Some(Replay::new(
            Origin::new("anthropic-messages", PROVIDER, MODEL),
            "sig",
        )),
    };
    let harness = Harness::new(vec![answer(
        &[signed, Block::Text("done".into())],
        FinishReason::Stop,
    )])
    .await;
    harness.say("hi").await;

    let events = harness.events();
    let SessionEvent::AssistantMessage { data, .. } = &events[6].event else {
        panic!("expected the answer");
    };
    let state = data
        .message
        .source
        .replay_state
        .clone()
        .expect("the answer has replay state");
    let finish = expanded_stream(&harness, 6).pop().unwrap().chunk;
    assert_eq!(
        finish,
        json!({ "type": "finish", "reason": { "kind": "stop" }, "replayState": state })
    );
}

/// A failed call's attempt ends its stream with dsh's `finish` of kind
/// `error`, the failure written as `turn/end` writes it.
#[tokio::test]
async fn a_failed_attempt_ends_its_stream_with_the_failure() {
    let mut busy = ModelError::new(ErrorKind::RateLimited, "Slow down.");
    busy.retry_after = Some(Duration::from_secs(2));
    let harness = Harness::new(vec![Reply::Events(vec![Ok(delta("Hel")), Err(busy)])]).await;
    harness.say("hi").await;

    let expanded = expanded_stream(&harness, 6);
    assert_eq!(
        expanded[0].chunk,
        json!({ "type": "text-delta", "index": 0, "text": "Hel" })
    );
    let endings = harness.endings();
    let [TurnEndReason::Error(failure)] = endings.as_slice() else {
        panic!("expected a failed turn, got {endings:?}");
    };
    assert_eq!(
        expanded[1].chunk,
        json!({ "type": "finish", "reason": { "kind": "error", "failure": serde_json::to_value(failure).unwrap() } })
    );
    assert_eq!(expanded.len(), 2);
}

/// A cancel leaves what streamed, with no finish: the interrupted answer
/// embeds its prefix, and an attempt with nothing worth keeping embeds its
/// chunks too.
#[tokio::test]
async fn a_cancelled_call_embeds_what_streamed() {
    for (events, kept) in [
        (vec![delta("partial")], "partial"),
        (vec![delta("  ")], "  "),
    ] {
        let waiting = Arc::new(Notify::new());
        let harness = Harness::new(vec![Reply::Hang(events, Arc::clone(&waiting))]).await;
        let agent = harness.agent();
        agent.followup(user("go")).unwrap();
        waiting.notified().await;
        agent.cancel(CancelCause::User);
        idle(agent.as_ref()).await;

        let expanded = expanded_stream(&harness, 6);
        assert_eq!(
            expanded
                .iter()
                .map(|chunk| &chunk.chunk)
                .collect::<Vec<_>>(),
            [&json!({ "type": "text-delta", "index": 0, "text": kept })]
        );
    }
}

// Runtime context. Ported from dsh's `agent-loop/tests/loop.spec.ts`
// (its runtime-context cases without a pre-step hook, which isn't ported)
// and `runtime-context.spec.ts`.

const SNAPSHOT: &str =
    "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.";
const CLEARED: &str =
    "Current runtime context: none. Earlier runtime-context snapshots no longer apply.";

/// The runtime-context messages in the log, with their seqs.
fn context_messages(events: &[Entry]) -> Vec<(Seq, UserMessage)> {
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::UserMessage { message, .. }
                if matches!(message.source, UserSource::RuntimeContext(_)) =>
            {
                Some((entry.seq, message.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The text of each runtime-context message in the log.
fn snapshots(harness: &Harness) -> Vec<String> {
    context_messages(&harness.events())
        .iter()
        .map(|(_, message)| {
            message
                .blocks()
                .iter()
                .filter_map(|block| match block {
                    Block::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}

/// Whether a request showed the model a runtime-context snapshot holding
/// `text`.
fn shows(request: &Request, text: &str) -> bool {
    request
        .messages
        .iter()
        .any(|message| message.role == Role::User && message.text().contains(text))
}

/// Replaces the surface node `seq` with a summary of another kind, as
/// compaction would.
fn compact(harness: &Harness, seq: Seq) {
    let summary = UserMessage::text(
        format!("summary-{seq}"),
        UserSource::Other(serde_json::json!({ "kind": "test-compaction" })),
        "compacted summary",
    );
    harness
        .sessions
        .append(
            &id(),
            SessionEvent::UserMessage {
                message: summary,
                surface: SurfaceIntent {
                    op: SurfaceOp::Replace {
                        start: seq,
                        end: seq,
                    },
                    sources: vec![seq],
                },
            },
        )
        .unwrap();
}

fn policy(harness: &Harness, mode: &Arc<Mutex<String>>) -> karaka_system_prompt::Registration {
    let mode = Arc::clone(mode);
    harness
        .prompt
        .context(
            None,
            PromptContext::new(
                "policy",
                0.0,
                Text::dynamic(move |_| format!("Mode: {}.", mode.lock().unwrap())),
            ),
        )
        .unwrap()
}

/// dsh: "materializes changed runtime context at the history tail without
/// rewriting the system header" (`loop.spec.ts:686`).
#[tokio::test]
async fn runtime_context_is_sent_when_it_changes_and_cleared_when_it_goes() {
    let replies = (0..5).map(|n| text(&format!("answer {n}"))).collect();
    let harness = Harness::with(replies, config(Some("Be brief.")), Vec::new()).await;
    let mode = Arc::new(Mutex::new("read-only".to_owned()));
    let registration = policy(&harness, &mode);

    harness.say("first").await;
    assert_eq!(
        snapshots(&harness),
        [format!("{SNAPSHOT}\n\nMode: read-only.")]
    );
    // After the prompt and the input, before the request's header: as dsh
    // admits it.
    let mut expected = vec![
        "turn/start",
        "step/start",
        "system/message",
        "user/message",
        "user/message",
    ];
    expected.extend([
        "request/header",
        "request/context",
        "assistant/message",
        "step/end",
        "turn/end",
    ]);
    assert_eq!(harness.types(), expected);
    let (_, message) = &context_messages(&harness.events())[0];
    assert_eq!(
        serde_json::to_value(&message.source).unwrap(),
        serde_json::json!({ "kind": "runtime-context", "form": "snapshot",
                            "sections": [{ "name": "policy", "text": "Mode: read-only." }] })
    );
    assert!(shows(&harness.model.requests()[0], "Mode: read-only."));

    harness.say("unchanged").await;
    assert_eq!(snapshots(&harness).len(), 1);

    *mode.lock().unwrap() = "danger-full-access".into();
    harness.say("changed").await;
    let sent = snapshots(&harness);
    assert_eq!(sent.len(), 2);
    assert!(sent[1].contains("danger-full-access"));

    registration.remove();
    harness.say("cleared").await;
    assert_eq!(snapshots(&harness).len(), 3);
    assert_eq!(snapshots(&harness)[2], CLEARED);
    let cleared = &context_messages(&harness.events())[2].1;
    assert_eq!(
        serde_json::to_value(&cleared.source).unwrap(),
        serde_json::json!({ "kind": "runtime-context" })
    );

    harness.say("still clear").await;
    assert_eq!(snapshots(&harness).len(), 3);
    // The prompt was never rewritten for it.
    let requests = harness.model.requests();
    assert!(
        requests
            .iter()
            .all(|request| request.system.as_deref() == Some("Be brief."))
    );
    assert_eq!(
        harness
            .types()
            .iter()
            .filter(|kind| *kind == "system/message")
            .count(),
        1
    );
    assert_eq!(reasons(&harness), [RequestHeaderReason::Initial]);
}

/// The registry's `include_runtime_context: false` keeps only required
/// context.
#[tokio::test]
async fn required_context_survives_with_runtime_context_off() {
    let mut prompt = quiet(None);
    prompt.include_runtime_context = false;
    let harness = Harness::with(
        vec![text("ok")],
        Setup {
            loop_config: LoopConfig::new(ROUTE, MODEL),
            prompt,
        },
        Vec::new(),
    )
    .await;
    let _ = harness
        .prompt
        .context(
            None,
            PromptContext::new("directory", 0.0, "Current working directory: /workspace.")
                .required(),
        )
        .unwrap();
    let _ = harness
        .prompt
        .context(None, PromptContext::new("optional", 1.0, "Mode: optional."))
        .unwrap();
    harness.say("hi").await;
    assert_eq!(
        snapshots(&harness),
        [format!(
            "{SNAPSHOT}\n\nCurrent working directory: /workspace."
        )]
    );
    assert!(!shows(&harness.model.requests()[0], "Mode: optional."));
}

/// dsh: "re-emits unchanged runtime context when a surface replacement
/// removed the retained snapshot" (`loop.spec.ts:743`).
#[tokio::test]
async fn a_snapshot_compacted_away_is_sent_again() {
    let harness = Harness::new(vec![text("one"), text("two")]).await;
    let _ = harness
        .prompt
        .context(None, PromptContext::new("policy", 0.0, "Mode: read-only."))
        .unwrap();
    harness.say("first").await;
    let (seq, _) = context_messages(&harness.events())[0].clone();
    compact(&harness, seq);
    harness.say("after compaction").await;
    assert_eq!(snapshots(&harness).len(), 2);
    assert!(shows(&harness.model.requests()[1], "Mode: read-only."));
}

/// dsh: "clears compacted runtime context after the active set becomes
/// empty" (`loop.spec.ts:921`).
#[tokio::test]
async fn compacted_context_that_went_away_is_cleared() {
    let harness = Harness::new(vec![text("one"), text("two")]).await;
    let registration = harness
        .prompt
        .context(None, PromptContext::new("policy", 0.0, "Mode: read-only."))
        .unwrap();
    harness.say("first").await;
    let (seq, _) = context_messages(&harness.events())[0].clone();
    compact(&harness, seq);
    registration.remove();
    harness.say("after compaction").await;
    assert!(shows(&harness.model.requests()[1], CLEARED));
}

/// dsh: "does not clear runtime context after an unrelated replacement"
/// (`loop.spec.ts:952`).
#[tokio::test]
async fn an_unrelated_replacement_sends_nothing() {
    let harness = Harness::new(vec![text("ok")]).await;
    // The agent first, as dsh's test creates it before appending.
    let _agent = harness.agent();
    let original = UserMessage::text(
        "old",
        UserSource::Other(serde_json::json!({ "kind": "test-context" })),
        "old context",
    );
    let seq = harness
        .sessions
        .append(&id(), SessionEvent::user(original))
        .unwrap();
    compact(&harness, seq);
    harness.say("after compaction").await;
    assert!(snapshots(&harness).is_empty());
    assert!(!shows(
        &harness.model.requests()[0],
        "Current runtime context"
    ));
}

/// dsh: "replaces a malformed retained runtime-context message with the
/// current complete snapshot" (`loop.spec.ts:974`).
#[tokio::test]
async fn a_malformed_retained_snapshot_is_replaced() {
    let harness = Harness::new(vec![text("ok")]).await;
    let _ = harness
        .prompt
        .context(None, PromptContext::new("policy", 0.0, "Mode: read-only."))
        .unwrap();
    let _agent = harness.agent();
    let mut broken = UserMessage::text(
        "broken",
        UserSource::RuntimeContext(Default::default()),
        "broken",
    );
    broken
        .content
        .push(karaka_session::ContentBlock::text("snapshot"));
    harness
        .sessions
        .append(&id(), SessionEvent::user(broken))
        .unwrap();
    harness.say("repair context").await;
    let sent = snapshots(&harness);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1], format!("{SNAPSHOT}\n\nMode: read-only."));
}

/// A resumed session works out the snapshot already in effect from its
/// log, as dsh's projection restores it (`runtime-context.ts:123-132`), so
/// an unchanged one isn't sent again; a changed one is.
#[tokio::test]
async fn a_resumed_session_doesnt_resend_its_snapshot() {
    let before = Harness::new(vec![text("one")]).await;
    let _ = before
        .prompt
        .context(None, PromptContext::new("policy", 0.0, "Mode: read-only."))
        .unwrap();
    before.say("first").await;
    let stored = before.log();

    let after = Harness::with(vec![text("two"), text("three")], config(None), stored).await;
    let mode = Arc::new(Mutex::new("read-only".to_owned()));
    let _registration = policy(&after, &mode);
    after.say("resumed").await;
    assert_eq!(snapshots(&after).len(), 1);
    *mode.lock().unwrap() = "strict".into();
    after.say("changed").await;
    assert_eq!(snapshots(&after).len(), 2);
}

/// dsh: "restores runtime context during a retry without admitting the
/// user batch twice" (`loop.spec.ts:877`).
#[tokio::test]
async fn a_retry_after_its_snapshot_was_compacted_sends_it_again_once() {
    let harness = Harness::new(vec![
        text("one"),
        failing(ErrorKind::ContextWindowExceeded, "overflow"),
        text("two"),
    ])
    .await;
    let _ = harness
        .prompt
        .context(
            None,
            PromptContext::new("directory", 0.0, "Current working directory: /workspace.")
                .required(),
        )
        .unwrap();
    harness.say("first").await;
    harness
        .hooks
        .register(Arc::new(Handler(|error: &RequestError| {
            let events = error.session.events();
            let (seq, _) = context_messages(&events)
                .last()
                .cloned()
                .expect("a retained snapshot");
            let summary = UserMessage::text(
                "checkpoint",
                UserSource::Other(serde_json::json!({ "kind": "test-compaction" })),
                "compacted checkpoint",
            );
            error
                .session
                .append(SessionEvent::UserMessage {
                    message: summary,
                    surface: SurfaceIntent {
                        op: SurfaceOp::Replace {
                            start: seq,
                            end: seq,
                        },
                        sources: vec![seq],
                    },
                })
                .map_err(|error| error.to_string())?;
            Ok(Some(RequestErrorAction::Retry))
        })));
    harness.say("second").await;
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 3);
    assert!(shows(&requests[2], "/workspace"));
    assert_eq!(snapshots(&harness).len(), 2);
    let people = harness
        .events()
        .iter()
        .filter(|entry| matches!(&entry.event, SessionEvent::UserMessage { message, .. } if matches!(message.source, UserSource::User(_))))
        .count();
    assert_eq!(people, 2);
}

/// dsh: "commits no pending user or prompt when a context provider cancels
/// admission" (`loop.spec.ts:856`): the context read again at the attempt
/// cancels, and nothing of the step is logged.
#[tokio::test]
async fn a_cancel_from_a_context_provider_commits_nothing() {
    let harness = Harness::with(Vec::new(), config(Some("Be brief.")), Vec::new()).await;
    let agent = harness.agent();
    let evaluations = Arc::new(AtomicUsize::new(0));
    let (counted, cancelled) = (Arc::clone(&evaluations), Arc::clone(&agent));
    let _ = harness
        .prompt
        .context(
            None,
            PromptContext::new(
                "cancel",
                0.0,
                Text::dynamic(move |_| {
                    if counted.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                        cancelled.cancel(CancelCause::User);
                    }
                    "pending".into()
                }),
            ),
        )
        .unwrap();
    agent.followup(user("do not commit")).unwrap();
    idle(agent.as_ref()).await;
    assert_eq!(evaluations.load(Ordering::SeqCst), 2);
    assert!(harness.model.requests().is_empty());
    let types = harness.types();
    assert!(
        !types
            .iter()
            .any(|kind| kind == "user/message" || kind == "system/message"),
        "{types:?}"
    );
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );
}

// The durable inbox: steer, inject, and what a cancel and a restore do to
// what is queued. (dsh: `agent.spec.ts`, `loop.spec.ts` and `inbox.spec.ts`.)

/// Every event, the inbox's splices included.
fn log_types(harness: &Harness) -> Vec<String> {
    harness
        .sessions
        .events(&id())
        .unwrap()
        .iter()
        .map(|entry| entry.event.event_type().to_owned())
        .collect()
}

/// Every `agent/inbox/spliced`, in order.
fn splices(harness: &Harness) -> Vec<InboxSplice> {
    harness
        .sessions
        .events(&id())
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::InboxSpliced(splice) => Some(splice),
            _ => None,
        })
        .collect()
}

/// The texts of the user messages the log admitted, in order.
fn admitted(harness: &Harness) -> Vec<String> {
    harness
        .sessions
        .events(&id())
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::UserMessage { message, .. }
                if !matches!(message.source, UserSource::RuntimeContext(_)) =>
            {
                Some(
                    message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                )
            }
            _ => None,
        })
        .collect()
}

/// The inbox as clients see it.
fn inbox(harness: &Harness) -> Value {
    let session = harness.sessions.get(&id()).unwrap();
    harness.projections.snapshot(&session, None).values["inbox"].clone()
}

fn notice(text: &str) -> UserMessage {
    UserMessage::text(
        format!("notice-{text}"),
        UserSource::Other(json!({ "kind": "watcher" })),
        text,
    )
}

fn splice(
    target: InboxTarget,
    start: u64,
    removed: Option<u64>,
    inserted: Vec<UserMessage>,
) -> InboxSplice {
    InboxSplice {
        target,
        start,
        removed_count: removed,
        inserted,
        outcome: None,
        extra: Extra::new(),
    }
}

fn entries(events: Vec<SessionEvent>) -> Vec<Entry> {
    events
        .into_iter()
        .enumerate()
        .map(|(seq, event)| Entry {
            seq: Seq(seq as u64),
            time: Time(seq as i64 + 1),
            ignorable: false,
            event,
        })
        .collect()
}

/// A tool that queues input on its agent while it runs: steering when
/// `steer`, else context.
struct Queuer {
    name: &'static str,
    agent: Arc<OnceLock<Arc<dyn Agent>>>,
    texts: Vec<&'static str>,
    steer: bool,
}

impl Tool for Queuer {
    fn schema(&self) -> ToolSchema {
        schema(self.name)
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            let agent = self.agent.get().expect("the agent is set");
            for text in &self.texts {
                let message = notice(text);
                if self.steer {
                    agent.steer(message).unwrap();
                } else {
                    agent.inject(message).unwrap();
                }
            }
            ToolOutput::ok("tool done")
        })
    }
}

fn queuer(harness: &Harness, name: &'static str, texts: Vec<&'static str>, steer: bool) {
    let agent = Arc::new(OnceLock::new());
    let _ = agent.set(harness.agent());
    harness
        .tools
        .register(Arc::new(Queuer {
            name,
            agent,
            texts,
            steer,
        }))
        .unwrap();
}

// "idle inject() durably stages context without opening a turn", and
// "inject() while idle durably stages context without opening a turn"
#[tokio::test]
async fn an_idle_inject_is_staged_durably_and_runs_nothing() {
    let harness = Harness::new(vec![text("ok")]).await;
    let agent = harness.agent();
    agent.inject(notice("file changed: a.ts")).unwrap();
    idle(agent.as_ref()).await;
    assert_eq!(log_types(&harness), ["agent/inbox/spliced"]);
    assert_eq!(
        splices(&harness),
        [splice(
            InboxTarget::NextStep,
            0,
            None,
            vec![notice("file changed: a.ts")]
        )]
    );
    assert!(harness.model.requests().is_empty());
    assert_eq!(
        inbox(&harness),
        json!({ "next-turn": [], "next-step": [serde_json::to_value(notice("file changed: a.ts")).unwrap()] })
    );

    // The next turn takes it, before its own prompt.
    harness.say("go").await;
    assert_eq!(admitted(&harness), ["file changed: a.ts", "go"]);
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert!(shows(&requests[0], "file changed: a.ts"));
    assert_eq!(inbox(&harness), json!({ "next-turn": [], "next-step": [] }));
}

// A followup is a splice into next-turn, and its turn's first step takes it
// with another, as dsh's send and claim do.
#[tokio::test]
async fn a_followup_is_queued_and_taken_as_splices() {
    let harness = Harness::new(vec![text("ok")]).await;
    harness.say("hi").await;
    let mut expected = vec!["agent/inbox/spliced", "turn/start", "agent/inbox/spliced"];
    expected.extend(ANSWER_STEP);
    expected.push("turn/end");
    assert_eq!(log_types(&harness), expected);
    assert_eq!(
        splices(&harness),
        [
            splice(InboxTarget::NextTurn, 0, None, vec![user("hi")]),
            // Taken, not dropped: no outcome.
            splice(InboxTarget::NextTurn, 0, Some(1), Vec::new()),
        ]
    );
}

// "defers inject() during tool execution until after the tool result"
#[tokio::test]
async fn an_inject_during_a_tool_is_taken_after_its_result() {
    let harness = Harness::new(vec![calls(&[call("c1", "noticer", "{}")]), text("done")]).await;
    queuer(
        &harness,
        "noticer",
        vec!["mid-turn notice", "second notice"],
        false,
    );
    harness.say("go").await;

    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
    let events = harness.events();
    let result = events
        .iter()
        .find(|entry| entry.event.event_type() == "tool/result")
        .unwrap()
        .seq;
    let notices: Vec<Seq> = context_like(&events, "watcher");
    assert_eq!(notices.len(), 2);
    assert!(notices.iter().all(|seq| *seq > result));
    assert_eq!(
        admitted(&harness),
        ["go", "mid-turn notice", "second notice"]
    );
    let second = &harness.model.requests()[1];
    let tool_at = second
        .messages
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, Block::ToolResult(_)))
        })
        .unwrap();
    let notice_at = second
        .messages
        .iter()
        .position(|message| message.text().contains("mid-turn notice"))
        .unwrap();
    assert!(notice_at > tool_at);
}

/// The seqs of the user messages from `kind`.
fn context_like(events: &[Entry], kind: &str) -> Vec<Seq> {
    events
        .iter()
        .filter(|entry| match &entry.event {
            SessionEvent::UserMessage { message, .. } => {
                matches!(&message.source, UserSource::Other(source) if source["kind"] == kind)
            }
            _ => false,
        })
        .map(|entry| entry.seq)
        .collect()
}

// "steer() while idle becomes a woken prompt turn"
#[tokio::test]
async fn an_idle_steer_starts_a_turn() {
    let harness = Harness::new(vec![text("ok")]).await;
    let agent = harness.agent();
    agent.steer(notice("steer idle")).unwrap();
    idle(agent.as_ref()).await;
    assert_eq!(admitted(&harness), ["steer idle"]);
    assert_eq!(harness.model.requests().len(), 1);
    assert_eq!(
        splices(&harness)[0],
        splice(InboxTarget::NextStep, 0, None, vec![notice("steer idle")])
    );
}

// "injects steering between steps and continues the turn"
#[tokio::test]
async fn steering_during_a_tool_enters_the_next_step() {
    let harness = Harness::new(vec![
        calls(&[call("c1", "slow", "{}")]),
        text("addressed the steering"),
    ])
    .await;
    queuer(&harness, "slow", vec!["change of plans"], true);
    harness.say("start").await;

    let events = harness.events();
    let steering = context_like(&events, "watcher")[0];
    let second_step = events
        .iter()
        .filter(|entry| entry.event.event_type() == "step/start")
        .nth(1)
        .unwrap()
        .seq;
    assert!(steering > second_step);
    assert!(shows(&harness.model.requests()[1], "change of plans"));
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

// "starts idle steering synchronously and enters later steering at the
// next step": steering that lands while a final answer streams keeps the
// turn going for one more step.
#[tokio::test]
async fn steering_during_a_final_answer_runs_another_step() {
    let agent = Arc::new(OnceLock::<Arc<dyn Agent>>::new());
    let steering = Arc::clone(&agent);
    let harness = Harness::new(vec![
        Reply::Before(
            Box::new(move || {
                steering
                    .get()
                    .unwrap()
                    .steer(notice("second steer"))
                    .unwrap()
            }),
            Box::new(text("first")),
        ),
        text("second"),
    ])
    .await;
    let _ = agent.set(harness.agent());
    harness.say("first prompt").await;

    let turns = log_types(&harness)
        .iter()
        .filter(|kind| *kind == "turn/start")
        .count();
    assert_eq!(turns, 1);
    assert_eq!(admitted(&harness), ["first prompt", "second steer"]);
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert!(!shows(&requests[0], "second steer"));
    assert!(shows(&requests[1], "second steer"));
}

// "stops after a throwing pre-step listener and retains later steering
// until a wakeup", with a failed model call for the throw: Karaka has no
// pre-step hook.
#[tokio::test]
async fn a_failed_turn_keeps_its_steering_until_the_next_wake() {
    let agent = Arc::new(OnceLock::<Arc<dyn Agent>>::new());
    let steering = Arc::clone(&agent);
    let harness = Harness::new(vec![
        Reply::Before(
            Box::new(move || {
                steering
                    .get()
                    .unwrap()
                    .steer(notice("pending steering"))
                    .unwrap()
            }),
            Box::new(failing(ErrorKind::InvalidRequest, "refused")),
        ),
        text("recovered"),
    ])
    .await;
    let _ = agent.set(harness.agent());
    harness.say("prompt").await;
    assert!(matches!(harness.endings()[..], [TurnEndReason::Error(_)]));
    assert_eq!(
        inbox(&harness)["next-step"].as_array().map(Vec::len),
        Some(1)
    );

    harness.say("resume").await;
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    assert!(shows(&requests[1], "pending steering"));
    assert!(shows(&requests[1], "resume"));
    assert_eq!(harness.endings().len(), 2);
}

// "clears both pending lists as durable cancellations", through a cancel
// of a running turn.
#[tokio::test]
async fn a_cancel_drops_both_queues_durably() {
    let waiting = Arc::new(Notify::new());
    let harness = Harness::new(vec![Reply::Hang(Vec::new(), Arc::clone(&waiting))]).await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    waiting.notified().await;
    agent.inject(notice("context")).unwrap();
    agent.followup(user("later")).unwrap();
    let before = splices(&harness).len();
    agent.cancel(CancelCause::User);
    idle(agent.as_ref()).await;

    let canceled = |target| InboxSplice {
        outcome: Some(SpliceOutcome::Canceled),
        ..splice(target, 0, Some(1), Vec::new())
    };
    assert_eq!(
        splices(&harness)[before..],
        [
            canceled(InboxTarget::NextStep),
            canceled(InboxTarget::NextTurn)
        ]
    );
    assert_eq!(inbox(&harness), json!({ "next-turn": [], "next-step": [] }));
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );
    // An empty inbox is cleared without a word.
    let count = log_types(&harness).len();
    agent.cancel(CancelCause::User);
    assert_eq!(log_types(&harness).len(), count);
}

// dsh's `send`: input that doesn't wake an agent whose turn was cancelled
// waits for the next wake; only waking input replays.
#[tokio::test]
async fn an_inject_after_a_cancel_waits_for_a_wake() {
    let waiting = Arc::new(Notify::new());
    let harness = Harness::new(vec![
        Reply::Hang(Vec::new(), Arc::clone(&waiting)),
        text("ok"),
    ])
    .await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    waiting.notified().await;
    agent.cancel(CancelCause::User);
    agent.inject(notice("after the cancel")).unwrap();
    idle(agent.as_ref()).await;
    assert_eq!(harness.endings().len(), 1);
    assert_eq!(
        inbox(&harness)["next-step"].as_array().map(Vec::len),
        Some(1)
    );
}

// A followup sent after a cancel, while the cancelled turn winds down, runs
// as the next turn. (dsh: the waking send latched behind an aborted
// activity.)
#[tokio::test]
async fn a_followup_after_a_cancel_runs_next() {
    let waiting = Arc::new(Notify::new());
    let harness = Harness::new(vec![
        Reply::Hang(Vec::new(), Arc::clone(&waiting)),
        text("ok"),
    ])
    .await;
    let agent = harness.agent();
    agent.followup(user("go")).unwrap();
    waiting.notified().await;
    agent.cancel(CancelCause::User);
    agent.steer(notice("after the cancel")).unwrap();
    idle(agent.as_ref()).await;
    assert_eq!(
        harness.endings(),
        [
            TurnEndReason::Aborted(CancelCause::User),
            TurnEndReason::Completed
        ]
    );
    assert!(shows(&harness.model.requests()[1], "after the cancel"));
}

// What is queued survives a restore, and waits for the next wake, which
// runs it first: dsh's resume doesn't wake on a pending inbox.
#[tokio::test]
async fn queued_input_survives_a_restore_and_waits_for_a_wake() {
    let restored = entries(vec![
        SessionEvent::InboxSpliced(splice(InboxTarget::NextTurn, 0, None, vec![user("queued")])),
        SessionEvent::InboxSpliced(splice(
            InboxTarget::NextStep,
            0,
            None,
            vec![notice("staged")],
        )),
    ]);
    let harness = Harness::with(vec![text("one"), text("two")], config(None), restored).await;
    let agent = harness.agent();
    idle(agent.as_ref()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(harness.model.requests().is_empty());
    assert_eq!(
        inbox(&harness)["next-turn"].as_array().map(Vec::len),
        Some(1)
    );

    harness.say("go").await;
    assert_eq!(admitted(&harness), ["staged", "queued", "go"]);
    assert_eq!(harness.endings().len(), 2);
    assert_eq!(inbox(&harness), json!({ "next-turn": [], "next-step": [] }));
}

// "rejects invalid durable coordinates and duplicate identities during
// reconstruction"
#[tokio::test]
async fn an_invalid_inbox_history_is_refused() {
    let out_of_range = entries(vec![SessionEvent::InboxSpliced(splice(
        InboxTarget::NextTurn,
        0,
        Some(1),
        Vec::new(),
    ))]);
    let harness = Harness::with(Vec::new(), config(None), out_of_range).await;
    let error = harness
        .agents
        .open(id())
        .err()
        .expect("the open is refused");
    assert_eq!(
        error.to_string(),
        "invalid persisted inbox splice at session seq 0"
    );
    let cause = std::error::Error::source(&error)
        .and_then(std::error::Error::source)
        .expect("a cause");
    assert_eq!(cause.to_string(), "invalid inbox splice");
    drop(harness);

    let duplicate = entries(vec![
        SessionEvent::InboxSpliced(splice(InboxTarget::NextTurn, 0, None, vec![user("x")])),
        SessionEvent::InboxSpliced(splice(InboxTarget::NextStep, 0, None, vec![user("x")])),
    ]);
    let harness = Harness::with(Vec::new(), config(None), duplicate).await;
    let error = harness
        .agents
        .open(id())
        .err()
        .expect("the open is refused");
    assert_eq!(
        error.to_string(),
        "invalid persisted inbox splice at session seq 1"
    );
    let cause = std::error::Error::source(&error)
        .and_then(std::error::Error::source)
        .expect("a cause");
    assert_eq!(cause.to_string(), "message \"user-x\" is already pending");
}

// "rejects duplicate identities": a message whose id is pending isn't
// queued twice.
#[tokio::test]
async fn a_pending_id_cant_be_queued_again() {
    let harness = Harness::new(Vec::new()).await;
    let agent = harness.agent();
    agent.inject(user("x")).unwrap();
    let error = agent.followup(user("x")).unwrap_err();
    assert_eq!(error, InboxError::AlreadyPending("user-x".into()));
    assert_eq!(error.to_string(), "message \"user-x\" is already pending");
    assert_eq!(splices(&harness).len(), 1);
    assert!(harness.model.requests().is_empty());
}

/// Records what the inbox hooks hear: `claimed id turn` and `discarded id`.
#[derive(Default)]
struct Heard(Mutex<Vec<String>>);

impl karaka_agent::InboxListener for Heard {
    fn claimed(&self, agent: &dyn Agent, message: &UserMessage, turn: u64) {
        assert_eq!(agent.id(), &id());
        self.0
            .lock()
            .unwrap()
            .push(format!("claimed {} {turn}", message.id));
    }

    fn discarded(&self, agent: &dyn Agent, message: &UserMessage) {
        assert_eq!(agent.id(), &id());
        self.0
            .lock()
            .unwrap()
            .push(format!("discarded {}", message.id));
    }
}

// dsh's `agent/inbox/claimed` and `agent/inbox/discarded`: each message a
// step takes, with its turn, after `turn/start`; and each message a cancel
// drops, next-step first.
#[tokio::test]
async fn the_inbox_hooks_hear_each_claim_and_each_discard() {
    let waiting = Arc::new(Notify::new());
    let harness = Harness::new(vec![
        text("one"),
        Reply::Hang(Vec::new(), Arc::clone(&waiting)),
    ])
    .await;
    let heard = Arc::new(Heard::default());
    let hooks = harness._runtime.get::<karaka_agent::InboxHooks>();
    hooks.register(Arc::clone(&heard) as Arc<dyn karaka_agent::InboxListener>);
    let agent = harness.agent();
    agent.inject(notice("context")).unwrap();
    harness.say("first").await;
    assert_eq!(
        *heard.0.lock().unwrap(),
        ["claimed notice-context 1", "claimed user-first 1"]
    );

    agent.followup(user("second")).unwrap();
    waiting.notified().await;
    agent.inject(notice("late")).unwrap();
    agent.followup(user("third")).unwrap();
    agent.cancel(CancelCause::User);
    idle(agent.as_ref()).await;
    assert_eq!(
        heard.0.lock().unwrap()[2..],
        [
            "claimed user-second 2",
            "discarded notice-late",
            "discarded user-third",
        ]
    );
}

// dsh's registry `get`: the live agent, or none, without making one; and
// every handle on one agent shares its instance.
#[tokio::test]
async fn the_registry_finds_live_agents_without_making_them() {
    let harness = Harness::new(Vec::new()).await;
    assert!(harness.agents.get(&id()).is_none());
    let agent = harness.agent();
    let found = harness.agents.get(&id()).expect("the agent is live");
    assert_eq!(found.instance(), agent.instance());
    let other = harness.agents.open(SessionId::new("b1")).unwrap();
    assert_ne!(other.instance(), agent.instance());
}

/// Adds `notice` to every step, after what the rest of the chain gives,
/// and keeps what it was proposed. (A dsh `agent/pre-step` listener.)
struct Adds(Mutex<Vec<(u64, u64, Vec<String>)>>);

impl karaka_agent::PreStepListener for Adds {
    fn decide<'a>(
        &'a self,
        step: &'a karaka_agent::PreStep,
        next: karaka_agent::PreStepNext<'a>,
    ) -> karaka_agent::PreStepFuture<'a> {
        Box::pin(async move {
            self.0.lock().unwrap().push((
                step.turn,
                step.step,
                step.messages
                    .iter()
                    .map(|message| message.id.clone())
                    .collect(),
            ));
            match next.run().await? {
                karaka_agent::PreStepDecision::Enter { mut messages } => {
                    messages.push(notice(&format!("added at {}", step.step)));
                    Ok(karaka_agent::PreStepDecision::Enter { messages })
                }
                reject => Ok(reject),
            }
        })
    }
}

// dsh's `agent/pre-step`: each step's claimed input is proposed, with its
// turn and step, and what the chain enters with is what the step logs and
// sends.
#[tokio::test]
async fn the_pre_step_hook_changes_what_enters_each_step() {
    let harness = Harness::new(vec![text("one")]).await;
    let heard = Arc::new(Adds(Mutex::new(Vec::new())));
    harness
        ._runtime
        .get::<karaka_agent::PreStepHooks>()
        .register(
            None,
            Arc::clone(&heard) as Arc<dyn karaka_agent::PreStepListener>,
        );
    harness.say("first").await;
    assert_eq!(
        *heard.0.lock().unwrap(),
        [(1, 1, vec!["user-first".to_owned()])]
    );
    let logged: Vec<String> = harness
        .events()
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::UserMessage { message, .. } => Some(message.id),
            _ => None,
        })
        .collect();
    assert!(
        logged.starts_with(&["user-first".to_owned(), "notice-added at 1".to_owned()]),
        "{logged:?}"
    );
    let request = &harness.model.requests()[0];
    assert!(shows(request, "added at 1"));
}

struct Rejects;

impl karaka_agent::PreStepListener for Rejects {
    fn decide<'a>(
        &'a self,
        _: &'a karaka_agent::PreStep,
        _: karaka_agent::PreStepNext<'a>,
    ) -> karaka_agent::PreStepFuture<'a> {
        Box::pin(async { Ok(karaka_agent::PreStepDecision::Reject) })
    }
}

// A rejected step ends its turn as `blocked`, with no step and no call.
#[tokio::test]
async fn a_rejected_step_blocks_the_turn() {
    let harness = Harness::new(Vec::new()).await;
    harness
        ._runtime
        .get::<karaka_agent::PreStepHooks>()
        .register(None, Arc::new(Rejects));
    harness.say("blocked").await;
    assert_eq!(harness.endings(), [TurnEndReason::Blocked]);
    assert!(!harness.types().iter().any(|kind| kind == "step/start"));
    assert!(harness.model.requests().is_empty());
}
