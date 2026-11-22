//! The retry plugin, with the real loop over a scripted model, on paused
//! time: every wait passes at once, and exactly. Ported from dsh's
//! `llm-retry/tests/retry.spec.ts` and `persistence.spec.ts`.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{
    Agent, Agents, HookFuture, Next, RequestError, RequestErrorAction, RequestErrorHandler,
    RequestErrorHooks, cancellation,
};
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_kernel::Runtime;
use karaka_model::{
    Backoff, Block, ErrorKind, Event, EventStream, FinishReason, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, RetryPolicy, ToolCall, ToolSchema, Usage, stream_of,
};
use karaka_retry::{RETRY, RETRY_STARTED, RetryHandler, RetryPlugin, policy_key};
use karaka_session::{
    CancelCause, Entry, Failure, SessionEvent, SessionId, SessionStore, SessionsPlugin,
    TurnEndReason, UserMessage, UserSource,
};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin};
use serde_json::{Value, json};
use tokio::time::Instant;

const ROUTE: &str = "mock-route";
const PROVIDER: &str = "mock";

/// Plays one reply per call. (dsh: `ScriptedAdapter`.)
struct Scripted {
    policy: RetryPolicy,
    replies: Mutex<VecDeque<Vec<Result<Event, ModelError>>>>,
    calls: Mutex<usize>,
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        PROVIDER
    }

    fn retry_policy(&self) -> RetryPolicy {
        self.policy.clone()
    }

    fn stream(&self, _request: Request) -> EventStream {
        *self.calls.lock().unwrap() += 1;
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![Err(ModelError::new(
                ErrorKind::InvalidRequest,
                "the script is exhausted",
            ))]
        });
        Box::pin(Playback(reply.into()))
    }
}

struct Playback(VecDeque<Result<Event, ModelError>>);

impl Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

fn text(answer: &str) -> Vec<Result<Event, ModelError>> {
    stream_of(
        &[Block::Text(answer.into())],
        FinishReason::Stop,
        Usage::default(),
    )
    .into_iter()
    .map(Ok)
    .collect()
}

fn fail(kind: ErrorKind, message: &str) -> Vec<Result<Event, ModelError>> {
    vec![Err(ModelError::new(kind, message))]
}

/// A failure that came with a `Retry-After` of `millis`.
fn fail_after(kind: ErrorKind, message: &str, millis: u64) -> Vec<Result<Event, ModelError>> {
    let mut error = ModelError::new(kind, message);
    error.retry_after = Some(Duration::from_millis(millis));
    vec![Err(error)]
}

fn echo_call() -> Vec<Result<Event, ModelError>> {
    let call = Block::ToolCall(ToolCall {
        id: "c1".into(),
        name: "echo".into(),
        arguments: "{}".into(),
    });
    stream_of(&[call], FinishReason::ToolCalls, Usage::default())
        .into_iter()
        .map(Ok)
        .collect()
}

struct Echo;

impl Tool for Echo {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "echo".into(),
            description: "Echoes.".into(),
            parameters: r#"{"type":"object"}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::ok("echoed") })
    }
}

/// dsh's test policies: two retries, no jitter.
/// (`normalConfig` in `retry.spec.ts`.)
fn normal(codes: &[&str], max_retries: u64, backoff: Backoff) -> RetryPolicy {
    RetryPolicy::Normal {
        max_retries,
        retryable_codes: codes.iter().map(|code| (*code).to_owned()).collect(),
        backoff,
    }
}

fn backoff(initial: f64, max: f64, jitter: f64) -> Backoff {
    Backoff {
        initial_delay_ms: initial,
        max_delay_ms: max,
        jitter_ratio: jitter,
    }
}

fn dsh_backoff() -> Backoff {
    backoff(500.0, 10_000.0, 0.0)
}

struct Harness {
    model: Arc<Scripted>,
    sessions: Arc<SessionStore>,
    agents: Arc<dyn Agents>,
    hooks: Arc<RequestErrorHooks>,
    _runtime: Runtime,
}

impl Harness {
    /// The loop and the retry plugin, whose jitter draws `samples` in turn
    /// and then 0.5, as dsh's tests fix `random`.
    async fn new(
        policy: RetryPolicy,
        replies: Vec<Vec<Result<Event, ModelError>>>,
        samples: Vec<f64>,
    ) -> Self {
        let samples = Mutex::new(VecDeque::from(samples));
        let runtime = Runtime::start(vec![
            Box::new(ModelsPlugin),
            Box::new(ToolsPlugin),
            Box::new(SessionsPlugin),
            Box::new(AgentLoopPlugin::new(LoopConfig::new(ROUTE, "mock-model"))),
            Box::new(RetryPlugin::with_random(move || {
                samples.lock().unwrap().pop_front().unwrap_or(0.5)
            })),
        ])
        .await
        .expect("the runtime starts");
        let model = Arc::new(Scripted {
            policy,
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(0),
        });
        runtime
            .get::<ModelRegistry>()
            .register(ROUTE, Arc::clone(&model) as Arc<dyn Model>)
            .unwrap();
        runtime
            .get::<ToolRegistry>()
            .register(Arc::new(Echo))
            .unwrap();
        Self {
            model,
            sessions: runtime.get::<SessionStore>(),
            agents: runtime.get::<dyn Agents>(),
            hooks: runtime.get::<RequestErrorHooks>(),
            _runtime: runtime,
        }
    }

    fn agent(&self) -> Arc<dyn Agent> {
        self.agents.open(id()).unwrap()
    }

    async fn say(&self, text: &str) {
        let agent = self.agent();
        agent.followup(user(text));
        agent.when_idle().await;
    }

    fn calls(&self) -> usize {
        *self.model.calls.lock().unwrap()
    }

    fn events(&self) -> Vec<Entry> {
        self.sessions.events(&id()).unwrap()
    }

    fn types(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|entry| entry.event.event_type().to_owned())
            .collect()
    }

    /// The data of every event of type `event_type`.
    fn data(&self, event_type: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|entry| entry.event.event_type() == event_type)
            .map(|entry| serde_json::to_value(&entry).unwrap()["data"].clone())
            .collect()
    }

    fn endings(&self) -> Vec<TurnEndReason> {
        self.events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                SessionEvent::TurnEnd(end) => Some(end.reason),
                _ => None,
            })
            .collect()
    }
}

fn id() -> SessionId {
    SessionId::new("retry")
}

fn user(text: &str) -> UserMessage {
    UserMessage::text(format!("user-{text}"), UserSource::user(), text)
}

/// The events of a first step up to its request.
const OPENING: [&str; 6] = [
    "turn/start",
    "step/start",
    "system/message",
    "user/message",
    "request/header",
    "request/context",
];

fn sequence(rest: &[&str]) -> Vec<String> {
    OPENING
        .iter()
        .chain(rest)
        .map(|kind| (*kind).to_owned())
        .collect()
}

fn error_ending(message: &str, code: &str) -> TurnEndReason {
    TurnEndReason::Error(Failure {
        message: message.into(),
        code: code.into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
        offload_images: None,
    })
}

/// dsh: "records the scheduled delay before retrying the request"
/// (`retry.spec.ts:202`). The plan is logged before the wait, which lasts
/// exactly the delay, and the retry stays in the same step.
#[tokio::test(start_paused = true)]
async fn one_retry_then_success() {
    let harness = Harness::new(
        normal(&["SERVER", "RATE_LIMIT"], 2, dsh_backoff()),
        vec![fail(ErrorKind::RateLimited, "busy"), text("done")],
        Vec::new(),
    )
    .await;
    let start = Instant::now();
    harness.say("go").await;
    assert_eq!(start.elapsed(), Duration::from_millis(500));

    assert_eq!(
        harness.types(),
        sequence(&[
            "assistant/attempt",
            RETRY,
            RETRY_STARTED,
            "assistant/message",
            "step/end",
            "turn/end",
        ])
    );
    assert_eq!(
        harness.data(RETRY),
        [json!({
            "retryId": "retry-7",
            "turn": 1,
            "step": 1,
            "provider": PROVIDER,
            "mode": "normal",
            "policyKey": r#"["normal",2,["RATE_LIMIT","SERVER"],500,10000,0]"#,
            "retry": 1,
            "maxRetries": 2,
            "delayMs": 500,
            "failure": { "message": "busy", "code": "RATE_LIMIT" },
        })]
    );
    assert_eq!(
        harness.data(RETRY_STARTED),
        [json!({ "retryId": "retry-7", "turn": 1, "step": 1, "retry": 1 })]
    );
    // The key order is dsh's too.
    let written = serde_json::to_string(&harness.events()[7]).unwrap();
    assert!(
        written.contains(r#""data":{"retryId":"retry-7","turn":1,"step":1,"provider":"mock","mode":"normal","policyKey":"#),
        "{written}"
    );
    assert_eq!(harness.calls(), 2);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

/// dsh: "applies bounded exponential jitter and stops after the configured
/// budget" (`retry.spec.ts:336`). Two retries, 450 ms then 1,100 ms, then
/// the third failure ends the turn.
#[tokio::test(start_paused = true)]
async fn retries_stop_when_the_budget_is_spent() {
    let harness = Harness::new(
        normal(&["SERVER"], 2, backoff(500.0, 10_000.0, 0.1)),
        vec![
            fail(ErrorKind::Unavailable, "busy one"),
            fail(ErrorKind::Unavailable, "busy two"),
            fail(ErrorKind::Unavailable, "busy three"),
        ],
        vec![0.0, 1.0],
    )
    .await;
    let start = Instant::now();
    harness.say("go").await;
    assert_eq!(start.elapsed(), Duration::from_millis(1_550));

    assert_eq!(
        harness.types(),
        sequence(&[
            "assistant/attempt",
            RETRY,
            RETRY_STARTED,
            "assistant/attempt",
            RETRY,
            RETRY_STARTED,
            "assistant/attempt",
            "step/end",
            "turn/end",
        ])
    );
    let retries = harness.data(RETRY);
    let delays: Vec<&Value> = retries.iter().map(|data| &data["delayMs"]).collect();
    assert_eq!(delays, [&json!(450), &json!(1_100)]);
    // One chain, one id.
    assert_eq!(retries[0]["retryId"], retries[1]["retryId"]);
    assert_eq!(retries[1]["retry"], 2);
    assert_eq!(harness.calls(), 3);
    assert_eq!(harness.endings(), [error_ending("busy three", "SERVER")]);
}

/// dsh: "delegates non-transient failures without scheduling a timer"
/// (`retry.spec.ts:449`).
#[tokio::test(start_paused = true)]
async fn a_code_the_policy_doesnt_list_is_handed_on() {
    let harness = Harness::new(
        RetryPolicy::default(),
        vec![fail(ErrorKind::Auth, "bad key"), text("unused")],
        Vec::new(),
    )
    .await;
    let start = Instant::now();
    harness.say("go").await;
    assert_eq!(start.elapsed(), Duration::ZERO);

    assert_eq!(
        harness.types(),
        sequence(&["assistant/attempt", "step/end", "turn/end"])
    );
    assert_eq!(harness.endings(), [error_ending("bad key", "AUTH")]);
}

/// dsh: "uses a bounded provider Retry-After verbatim and delegates an
/// over-cap instruction" (`retry.spec.ts:392`).
#[tokio::test(start_paused = true)]
async fn a_providers_retry_after_is_honoured_up_to_the_longest_wait() {
    let accepted = Harness::new(
        normal(&["RATE_LIMIT"], 2, backoff(500.0, 10_000.0, 1.0)),
        vec![
            fail_after(ErrorKind::RateLimited, "wait", 2_000),
            text("done"),
        ],
        Vec::new(),
    )
    .await;
    let start = Instant::now();
    accepted.say("go").await;
    assert_eq!(start.elapsed(), Duration::from_millis(2_000));
    assert_eq!(accepted.data(RETRY)[0]["delayMs"], 2_000);
    assert_eq!(
        accepted.data(RETRY)[0]["failure"],
        json!({ "message": "wait", "code": "RATE_LIMIT", "providerRetryAfterMs": 2000 })
    );
    assert_eq!(accepted.calls(), 2);

    let refused = Harness::new(
        RetryPolicy::default(),
        vec![
            fail_after(ErrorKind::RateLimited, "wait too long", 10_001),
            text("unused"),
        ],
        Vec::new(),
    )
    .await;
    refused.say("go").await;
    assert_eq!(
        refused.types(),
        sequence(&["assistant/attempt", "step/end", "turn/end"])
    );
    assert_eq!(refused.calls(), 1);
}

/// dsh: "uses local jittered backoff when always mode receives an over-cap
/// Retry-After" (`retry.spec.ts:423`). Always mode retries a code no
/// policy lists, too.
#[tokio::test(start_paused = true)]
async fn always_mode_backs_off_on_its_own_past_the_longest_wait() {
    let harness = Harness::new(
        RetryPolicy::Always {
            backoff: backoff(2.0, 4.0, 0.5),
        },
        vec![
            fail_after(ErrorKind::Auth, "wait too long", 10),
            text("done"),
        ],
        vec![1.0],
    )
    .await;
    let start = Instant::now();
    harness.say("go").await;
    assert_eq!(start.elapsed(), Duration::from_millis(3));

    let retry = &harness.data(RETRY)[0];
    assert_eq!(retry["delayMs"], 3);
    assert_eq!(retry["mode"], "always");
    assert_eq!(retry["policyKey"], r#"["always",2,4,0.5]"#);
    assert!(retry.get("maxRetries").is_none());
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

/// dsh: "keeps always mode unbounded while preserving cancellable jittered
/// backoff" (`retry.spec.ts:679`): more failures than any normal budget.
#[tokio::test(start_paused = true)]
async fn always_mode_has_no_limit() {
    let mut replies: Vec<_> = (0..8)
        .map(|n| fail(ErrorKind::Auth, &format!("no {n}")))
        .collect();
    replies.push(text("finally"));
    let harness = Harness::new(
        RetryPolicy::Always {
            backoff: backoff(1.0, 4.0, 0.0),
        },
        replies,
        Vec::new(),
    )
    .await;
    let start = Instant::now();
    harness.say("go").await;
    // 1, 2, then 4 ms for each of the other six.
    assert_eq!(start.elapsed(), Duration::from_millis(27));

    let retries = harness.data(RETRY);
    let numbers: Vec<u64> = retries
        .iter()
        .map(|data| data["retry"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, (1..=8).collect::<Vec<_>>());
    assert_eq!(harness.calls(), 9);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

/// dsh: "lets downstream specialized recovery run before always fallback"
/// (`retry.spec.ts:752`). A handler after the plugin that asks for a retry
/// gets it, with no wait and no `llm/retry`.
#[tokio::test(start_paused = true)]
async fn always_mode_asks_the_rest_of_the_chain_first() {
    struct Specialist(Arc<Mutex<usize>>);

    impl RequestErrorHandler for Specialist {
        fn handle<'a>(&'a self, _error: &'a RequestError, _next: Next<'a>) -> HookFuture<'a> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async { Ok(Some(RequestErrorAction::Retry)) })
        }
    }

    let harness = Harness::new(
        RetryPolicy::Always {
            backoff: dsh_backoff(),
        },
        vec![fail(ErrorKind::Auth, "no"), text("done")],
        Vec::new(),
    )
    .await;
    let asked = Arc::new(Mutex::new(0));
    harness
        .hooks
        .register(Arc::new(Specialist(Arc::clone(&asked))));
    harness.say("go").await;

    assert_eq!(*asked.lock().unwrap(), 1);
    assert!(harness.data(RETRY).is_empty());
    assert_eq!(harness.calls(), 2);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

/// dsh: "lets turn cancellation win during backoff without opening another
/// step" (`retry.spec.ts:977`). The plan is logged; the wait is cut short,
/// so no `llm/retry-started`, and the turn ends aborted.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_wait_ends_the_turn() {
    let harness = Harness::new(
        RetryPolicy::Always {
            backoff: dsh_backoff(),
        },
        vec![fail(ErrorKind::Auth, "permanent"), text("must not run")],
        Vec::new(),
    )
    .await;
    let agent = harness.agent();
    agent.followup(user("go"));
    // Each 1 ms sleep is the earliest timer, so paused time moves only that
    // far, and the retry's 500 ms wait is still running when it is seen.
    while harness.data(RETRY).is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    agent.cancel(CancelCause::User);
    agent.when_idle().await;

    assert_eq!(
        harness.types(),
        sequence(&["assistant/attempt", RETRY, "step/end", "turn/end"])
    );
    assert_eq!(
        harness.endings(),
        [TurnEndReason::Aborted(CancelCause::User)]
    );
    assert_eq!(harness.calls(), 1);
}

/// dsh: "lets an earlier recovery listener cancel before normal retry
/// policy runs" (`retry.spec.ts:1006`).
#[tokio::test(start_paused = true)]
async fn a_cancel_from_an_earlier_handler_stops_the_retry() {
    struct Canceller(Arc<dyn Agents>);

    impl RequestErrorHandler for Canceller {
        fn handle<'a>(&'a self, _error: &'a RequestError, next: Next<'a>) -> HookFuture<'a> {
            self.0.open(id()).unwrap().cancel(CancelCause::User);
            next.run()
        }
    }

    // The canceller has to come first, so it is registered on a runtime of
    // its own, before the retry handler.
    let runtime = Runtime::start(vec![
        Box::new(ModelsPlugin),
        Box::new(ToolsPlugin),
        Box::new(SessionsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new(ROUTE, "mock-model"))),
    ])
    .await
    .unwrap();
    let model = Arc::new(Scripted {
        policy: RetryPolicy::default(),
        replies: Mutex::new(vec![fail(ErrorKind::Unavailable, "temporary"), text("no")].into()),
        calls: Mutex::new(0),
    });
    runtime
        .get::<ModelRegistry>()
        .register(ROUTE, Arc::clone(&model) as Arc<dyn Model>)
        .unwrap();
    let agents = runtime.get::<dyn Agents>();
    let hooks = runtime.get::<RequestErrorHooks>();
    hooks.register(Arc::new(Canceller(Arc::clone(&agents))));
    hooks.register(Arc::new(RetryHandler::new(Arc::new(|| 0.5))));
    let agent = agents.open(id()).unwrap();
    agent.followup(user("go"));
    agent.when_idle().await;

    let events = runtime.get::<SessionStore>().events(&id()).unwrap();
    assert!(!events.iter().any(|entry| entry.event.event_type() == RETRY));
    let last = &events.last().unwrap().event;
    assert_eq!(
        *last,
        SessionEvent::turn_end(1, TurnEndReason::Aborted(CancelCause::User))
    );
    assert_eq!(*model.calls.lock().unwrap(), 1);
}

/// The count is per step: a new step starts it again, with a new id. With
/// one retry allowed, the second step's failure is still retried.
#[tokio::test(start_paused = true)]
async fn the_count_starts_again_at_the_next_step() {
    let harness = Harness::new(
        normal(&["SERVER"], 1, dsh_backoff()),
        vec![
            fail(ErrorKind::Unavailable, "first step"),
            echo_call(),
            fail(ErrorKind::Unavailable, "second step"),
            text("done"),
        ],
        Vec::new(),
    )
    .await;
    harness.say("go").await;

    let retries = harness.data(RETRY);
    let steps: Vec<(&Value, &Value)> = retries
        .iter()
        .map(|data| (&data["step"], &data["retry"]))
        .collect();
    assert_eq!(steps, [(&json!(1), &json!(1)), (&json!(2), &json!(1))]);
    assert_ne!(retries[0]["retryId"], retries[1]["retryId"]);
    assert_eq!(harness.endings(), [TurnEndReason::Completed]);
}

/// The count comes from the log, so a step read back from storage keeps
/// counting where the log left off, under the same id. Here the handler is
/// run on its own over such a log.
#[tokio::test(start_paused = true)]
async fn a_log_read_back_mid_step_keeps_counting() {
    let policy = normal(&["SERVER"], 3, dsh_backoff());
    let stored: Vec<Entry> = serde_json::from_value(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        { "type": "step/start", "seq": 1, "time": 1, "data": { "turn": 1, "step": 1 } },
        { "type": "assistant/attempt", "seq": 2, "time": 1,
          "data": { "turn": 1, "step": 1, "stream": [] } },
        { "type": "llm/retry", "seq": 3, "time": 1, "data": {
            "retryId": "retry-from-before", "turn": 1, "step": 1, "provider": PROVIDER,
            "mode": "normal", "policyKey": policy_key(&policy), "retry": 1, "maxRetries": 3,
            "delayMs": 500, "failure": { "message": "busy", "code": "SERVER" } } },
        { "type": "llm/retry-started", "seq": 4, "time": 1,
          "data": { "retryId": "retry-from-before", "turn": 1, "step": 1, "retry": 1 } },
        { "type": "assistant/attempt", "seq": 5, "time": 1,
          "data": { "turn": 1, "step": 1, "stream": [] } },
    ]))
    .unwrap();
    let store = SessionStore::new();
    let session = store.restore(id(), stored).unwrap();
    let hooks = RequestErrorHooks::new();
    hooks.register(Arc::new(RetryHandler::new(Arc::new(|| 0.5))));
    let error = RequestError {
        session: session.clone(),
        turn: 1,
        step: 1,
        provider: PROVIDER.into(),
        failure: Failure {
            message: "busy again".into(),
            code: "SERVER".into(),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
            offload_images: None,
        },
        retry_policy: policy,
        cancel: cancellation().1,
    };

    let decision = hooks.run(&error).await.unwrap();
    assert_eq!(decision, Some(RequestErrorAction::Retry));
    let added: Vec<Value> = session
        .events_since(karaka_session::Seq(6))
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap()["data"].clone())
        .collect();
    assert_eq!(added[0]["retry"], 2);
    assert_eq!(added[0]["retryId"], "retry-from-before");
    // The second retry waits twice as long.
    assert_eq!(added[0]["delayMs"], 1_000);
    assert_eq!(
        added[1],
        json!({ "retryId": "retry-from-before", "turn": 1, "step": 1, "retry": 2 })
    );
}
