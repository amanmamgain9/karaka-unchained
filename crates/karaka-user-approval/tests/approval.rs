//! The approval service. Ported from dsh's
//! `user-approval/tests/approval.spec.ts`, where its cases fit.
//!
//! Not ported:
//! - the two cases where a `session/event` observer throws after an
//!   append: a Karaka `SessionListener` can't fail;
//! - "propagates an append failure that prevented audit log growth": a
//!   passthrough always fits the log, so nothing outside the session crate
//!   can make that append fail;
//! - "normalizes a rogue non-vocabulary answer": a Rust answerer returns
//!   the closed `ApprovalOutcome`;
//! - "drops a disposed plugin listener", and "disposes the runtime-context
//!   contribution with the service": the kernel unloads no plugin. A
//!   removed registration stands in for the first.
//!
//! dsh's synchronously throwing answerer is a panicking one here.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_agent::{Agent, FrameListener, FrameListeners, FrameSubscription, InboxError, WhenIdle};
use karaka_fs::{AbortController, AbortSignal};
use karaka_kernel::Runtime;
use karaka_model::{ToolCall, ToolSchema};
use karaka_session::{
    CancelCause, Entry, SessionEvent, SessionId, SessionStore, SessionsPlugin, TurnEnd,
    TurnEndReason, TurnStart, UserMessage, UserSource,
};
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_system_prompt::{
    AssembleContext, Scope, SystemPrompt, SystemPromptConfig, SystemPromptPlugin,
};
use karaka_tools::{
    PreExecuteFuture, PreExecuteListener, PreExecuteNext, PreToolDecision, Tool, ToolExecution,
    ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin,
};
use karaka_user_approval::{
    ASK_SENTENCE, AnswererError, ApprovalAnswerer, ApprovalConfig, ApprovalError, ApprovalFuture,
    ApprovalNext, ApprovalOutcome, ApprovalPlugin, ApprovalPolicy, ApprovalRequest,
    ApprovalService, CONTEXT_NAME, NEVER_SENTENCE, OUTSIDE_TURN, set_approval_policy,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// An agent that only keeps what it is injected with.
struct Stub {
    id: SessionId,
    frames: FrameListeners,
    injected: Mutex<Vec<UserMessage>>,
}

impl Agent for Stub {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn followup(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn steer(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn inject(&self, message: UserMessage) -> Result<(), InboxError> {
        self.injected.lock().unwrap().push(message);
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

struct Bench {
    service: Arc<ApprovalService>,
    tools: Arc<ToolRegistry>,
    sessions: Arc<SessionStore>,
    prompt: Arc<SystemPrompt>,
    _runtime: Runtime,
}

async fn mounted(policy: ApprovalPolicy) -> Bench {
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(ApprovalPlugin::new(ApprovalConfig { policy })),
    ])
    .await
    .expect("the runtime starts");
    Bench {
        service: runtime.get::<ApprovalService>(),
        tools: runtime.get::<ToolRegistry>(),
        sessions: runtime.get::<SessionStore>(),
        prompt: runtime.get::<SystemPrompt>(),
        _runtime: runtime,
    }
}

fn turn_start() -> SessionEvent {
    SessionEvent::TurnStart(TurnStart {
        turn: 1,
        extra: Default::default(),
    })
}

fn turn_end() -> SessionEvent {
    SessionEvent::TurnEnd(TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
        extra: Default::default(),
    })
}

impl Bench {
    /// An agent whose session holds `seed`. dsh's `fakeAgent` is seeded
    /// inside an open turn by default; [`Self::agent`] does that.
    fn agent_with(&self, id: &str, seed: Vec<SessionEvent>) -> Arc<Stub> {
        let session = self.sessions.create(SessionId::new(id)).unwrap();
        for event in seed {
            session.append(event).unwrap();
        }
        Arc::new(Stub {
            id: SessionId::new(id),
            frames: FrameListeners::new(),
            injected: Mutex::new(Vec::new()),
        })
    }

    fn agent(&self, id: &str) -> Arc<Stub> {
        self.agent_with(id, vec![turn_start()])
    }

    fn events(&self, agent: &Stub) -> Vec<Entry> {
        self.sessions.events(&agent.id).unwrap()
    }

    /// The approval events, as `(type, data)`.
    fn audit(&self, agent: &Stub) -> Vec<(String, Value)> {
        self.events(agent)
            .into_iter()
            .filter_map(|entry| match entry.event {
                SessionEvent::Passthrough(event) if event.event_type.starts_with("approval/") => {
                    Some((event.event_type, event.data))
                }
                _ => None,
            })
            .collect()
    }

    fn register(&self, answerer: impl ApprovalAnswerer + 'static) {
        let _ = self.service.answerer(None, Arc::new(answerer));
    }

    async fn context_for(&self, scope: Option<&str>) -> Option<String> {
        self.prompt
            .assemble(&AssembleContext {
                scope: scope.map(Scope::new),
            })
            .await
            .unwrap()
            .contexts
            .into_iter()
            .find(|context| context.name == CONTEXT_NAME)
            .map(|context| context.text)
    }
}

fn request_of(agent: &Arc<Stub>) -> ApprovalRequest {
    ApprovalRequest::new(Arc::clone(agent) as Arc<dyn Agent>, "echo")
}

/// An answerer made of a closure.
struct With<F>(F);

impl<F> ApprovalAnswerer for With<F>
where
    F: for<'a> Fn(&'a ApprovalRequest, ApprovalNext<'a>) -> ApprovalFuture<'a> + Send + Sync,
{
    fn answer<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        next: ApprovalNext<'a>,
    ) -> ApprovalFuture<'a> {
        (self.0)(request, next)
    }
}

fn with<F>(answer: F) -> With<F>
where
    F: for<'a> Fn(&'a ApprovalRequest, ApprovalNext<'a>) -> ApprovalFuture<'a> + Send + Sync,
{
    With(answer)
}

fn answering(outcome: ApprovalOutcome) -> impl ApprovalAnswerer + 'static {
    with(move |_, _| Box::pin(async move { Ok(outcome) }))
}

/// Counts its calls, and grants.
fn counting(calls: &Arc<AtomicUsize>) -> impl ApprovalAnswerer + 'static {
    let calls = Arc::clone(calls);
    with(move |_, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ApprovalOutcome::AllowedOnce) })
    })
}

// ApprovalService.request

// "throws before appending anything when no turn has ever opened (idle
// ask)", and "throws between turns"
#[tokio::test]
async fn a_request_outside_an_open_turn_fails_before_logging() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    for (id, seed) in [
        ("idle", Vec::new()),
        ("between", vec![turn_start(), turn_end()]),
    ] {
        let agent = bench.agent_with(id, seed);
        let before = bench.events(&agent).len();
        let error = bench
            .service
            .request(&request_of(&agent))
            .await
            .unwrap_err();
        assert_eq!(error, ApprovalError::OutsideTurn);
        assert_eq!(error.to_string(), OUTSIDE_TURN);
        assert!(error.to_string().contains("outside an open turn"));
        assert_eq!(bench.events(&agent).len(), before);
    }
}

// "fails closed to unavailable when nobody listens, auditing the
// asked/decided pair"
#[tokio::test]
async fn with_no_answerer_a_request_fails_closed_and_is_audited() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("unanswered");
    let request = ApprovalRequest {
        call_id: Some("call-1".into()),
        reason: Some("hook says ask".into()),
        ..request_of(&agent)
    };
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::Unavailable)
    );
    let audit = bench.audit(&agent);
    let types: Vec<&str> = audit.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(types, ["approval/asked", "approval/decided"]);
    assert_eq!(
        audit[0].1,
        json!({ "id": audit[0].1["id"], "toolName": "echo", "callId": "call-1", "reason": "hook says ask" })
    );
    assert_eq!(audit[1].1["outcome"], "unavailable");
    assert_eq!(audit[1].1["id"], audit[0].1["id"]);
    // The events are dsh's, with no surface op.
    let asked = bench
        .events(&agent)
        .into_iter()
        .find(|entry| entry.event.event_type() == "approval/asked")
        .unwrap();
    let envelope = serde_json::to_value(&asked).unwrap();
    assert!(envelope.get("surfaceOp").is_none(), "{envelope}");
}

// "omits absent optional fields from the asked audit event", and
// `displayReason` is never logged
#[tokio::test]
async fn the_asked_event_has_only_what_was_given() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("bare");
    let request = ApprovalRequest {
        display_reason: Some(karaka_user_approval::DisplayReason::en("Shown only.")),
        ..request_of(&agent)
    };
    bench.service.request(&request).await.unwrap();
    let asked = &bench.audit(&agent)[0].1;
    let mut keys: Vec<&String> = asked.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["id", "toolName"]);
}

// "borrows the exact readonly request for scoped dispatch and audit"
#[tokio::test]
async fn a_scoped_answerer_gets_the_very_request() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("scoped");
    let received = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&received);
    let _ = bench.service.answerer(
        Some(&agent.id),
        Arc::new(with(move |request, _| {
            *seen.lock().unwrap() = Some((
                std::ptr::from_ref(request) as usize,
                request.agent.instance(),
            ));
            Box::pin(async { Ok(ApprovalOutcome::AllowedOnce) })
        })),
    );
    let request = ApprovalRequest {
        tool_name: "scoped-tool".into(),
        call_id: Some("scoped-call".into()),
        reason: Some("scoped reason".into()),
        ..request_of(&agent)
    };
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::AllowedOnce)
    );
    assert_eq!(
        *received.lock().unwrap(),
        Some((std::ptr::from_ref(&request) as usize, agent.instance()))
    );
    let audit = bench.audit(&agent);
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].1["toolName"], "scoped-tool");
    assert_eq!(audit[0].1["callId"], "scoped-call");
    assert_eq!(audit[0].1["reason"], "scoped reason");
    assert_eq!(audit[1].1["outcome"], "allowed-once");
    assert_eq!(audit[1].1["id"], audit[0].1["id"]);
}

// "returns the first answering listener outcome (single decision slot)"
#[tokio::test]
async fn the_first_answer_decides() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("first");
    bench.register(answering(ApprovalOutcome::AllowedOnce));
    let second = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&second));
    assert_eq!(
        bench.service.request(&request_of(&agent)).await,
        Ok(ApprovalOutcome::AllowedOnce)
    );
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

// "lets a non-owning listener delegate via next() down to the fail-closed
// default"
#[tokio::test]
async fn a_delegating_answerer_reaches_the_fail_closed_end() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("delegating");
    bench.register(with(|_, next| next.run()));
    assert_eq!(
        bench.service.request(&request_of(&agent)).await,
        Ok(ApprovalOutcome::Unavailable)
    );
}

// "dispatches to global and matching agent-scoped listeners, never a
// foreign scope", and "keys the scoped dispatch carrier to the exact
// request agent"
#[tokio::test]
async fn an_agents_answerers_hear_only_its_requests() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let a = bench.agent("agent-a");
    let b = bench.agent("agent-b");
    let heard: Arc<Mutex<Vec<String>>> = Arc::default();
    let note = |label: &'static str| {
        let heard = Arc::clone(&heard);
        Arc::new(with(move |request: &ApprovalRequest, next| {
            heard
                .lock()
                .unwrap()
                .push(format!("{label}:{}", request.agent.id()));
            next.run()
        }))
    };
    let _ = bench.service.answerer(None, note("global"));
    let _ = bench.service.answerer(Some(&a.id), note("scoped"));
    let _ = bench.service.answerer(Some(&b.id), note("scoped"));
    for agent in [&a, &b] {
        assert_eq!(
            bench.service.request(&request_of(agent)).await,
            Ok(ApprovalOutcome::Unavailable)
        );
    }
    assert_eq!(
        *heard.lock().unwrap(),
        [
            "global:agent-a",
            "scoped:agent-a",
            "global:agent-b",
            "scoped:agent-b"
        ]
    );
}

#[derive(Debug)]
struct Plain(&'static str);

impl std::fmt::Display for Plain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Plain {}

// "contains a throwing answerer as unavailable", and "contains an answerer
// that throws SYNCHRONOUSLY as unavailable": here, one that fails, one
// that panics in its future, and one that panics before it.
#[tokio::test]
async fn a_failing_or_panicking_answerer_is_unavailable() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let answerers: Vec<Arc<dyn ApprovalAnswerer>> = vec![
        Arc::new(with(|_, _| {
            Box::pin(async { Err(Box::new(Plain("transport died")) as AnswererError) })
        })),
        Arc::new(with(|_, _| Box::pin(async { panic!("async bug") }))),
        Arc::new(with(|_, _| panic!("sync bug"))),
    ];
    for (index, answerer) in answerers.into_iter().enumerate() {
        let agent = bench.agent(&format!("failing-{index}"));
        let registration = bench.service.answerer(None, answerer);
        assert_eq!(
            bench.service.request(&request_of(&agent)).await,
            Ok(ApprovalOutcome::Unavailable)
        );
        assert_eq!(bench.audit(&agent)[1].1["outcome"], "unavailable");
        registration.remove();
    }
}

// "settles cancelled immediately on an already-aborted signal without
// asking anyone"
#[tokio::test]
async fn an_aborted_signal_cancels_without_asking() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("aborted");
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let request = ApprovalRequest {
        signal: Some(AbortSignal::aborted()),
        ..request_of(&agent)
    };
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::Cancelled)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let audit = bench.audit(&agent);
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[1].1["outcome"], "cancelled");
}

// "resolves cancelled when the signal aborts mid-question and discards the
// late answer", and "discards a late REJECTION after abort without an
// unhandled rejection": the answerer's future is dropped, so its late
// answer has nowhere to go.
#[tokio::test]
async fn a_cancel_mid_question_wins_and_the_late_answer_is_dropped() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("mid-question");
    let (settle, settled) = oneshot::channel::<ApprovalOutcome>();
    let settled = Mutex::new(Some(settled));
    let started = Arc::new(tokio::sync::Notify::new());
    let starting = Arc::clone(&started);
    bench.register(with(move |_, _| {
        let settled = settled.lock().unwrap().take().expect("asked once");
        starting.notify_one();
        Box::pin(async move { Ok(settled.await.unwrap_or(ApprovalOutcome::Unavailable)) })
    }));
    let controller = AbortController::new();
    let request = ApprovalRequest {
        signal: Some(controller.signal()),
        ..request_of(&agent)
    };
    let service = Arc::clone(&bench.service);
    let pending = tokio::spawn(async move { service.request(&request).await });
    started.notified().await;
    controller.abort();
    assert_eq!(pending.await.unwrap(), Ok(ApprovalOutcome::Cancelled));
    // The answerer's future is gone: its answer can't be delivered.
    assert!(settle.send(ApprovalOutcome::AllowedOnce).is_err());
    let decided: Vec<Value> = bench
        .audit(&agent)
        .into_iter()
        .filter(|(kind, _)| kind == "approval/decided")
        .map(|(_, data)| data)
        .collect();
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["outcome"], "cancelled");
}

// "resolves the answer when the signal never aborts"
#[tokio::test]
async fn an_answer_wins_when_the_signal_never_fires() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("unaborted");
    bench.register(answering(ApprovalOutcome::Rejected));
    let controller = AbortController::new();
    let request = ApprovalRequest {
        signal: Some(controller.signal()),
        ..request_of(&agent)
    };
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::Rejected)
    );
}

// "issues a fresh id per request"
#[tokio::test]
async fn each_request_has_a_fresh_id() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("ids");
    bench.service.request(&request_of(&agent)).await.unwrap();
    bench.service.request(&request_of(&agent)).await.unwrap();
    let ids: Vec<Value> = bench
        .audit(&agent)
        .into_iter()
        .filter(|(kind, _)| kind == "approval/asked")
        .map(|(_, data)| data["id"].clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}

// "drops a disposed plugin listener from the chain (HMR safety)": here, a
// removed registration.
#[tokio::test]
async fn a_removed_answerer_is_out_of_the_chain() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("removed");
    let registration = bench
        .service
        .answerer(None, Arc::new(answering(ApprovalOutcome::AllowedOnce)));
    assert_eq!(
        bench.service.request(&request_of(&agent)).await,
        Ok(ApprovalOutcome::AllowedOnce)
    );
    registration.remove();
    assert_eq!(
        bench.service.request(&request_of(&agent)).await,
        Ok(ApprovalOutcome::Unavailable)
    );
}

// approval policy (the approval/policy fold)

// "folds to the last event, or undefined without one"
#[tokio::test]
async fn the_override_is_the_last_policy_event() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("fold");
    let session = bench.sessions.get(&agent.id).unwrap();
    assert_eq!(bench.service.override_of(&session), None);
    set_approval_policy(&session, ApprovalPolicy::Never).unwrap();
    set_approval_policy(&session, ApprovalPolicy::Ask).unwrap();
    assert_eq!(
        bench.service.override_of(&session),
        Some(ApprovalPolicy::Ask)
    );
    assert_eq!(
        bench.audit(&agent).last().unwrap(),
        &("approval/policy".to_owned(), json!({ "policy": "ask" }))
    );
}

// "rejects a policy outside the closed vocabulary before appending": a
// string is checked as it becomes a policy, with dsh's message.
#[test]
fn a_policy_outside_the_vocabulary_is_refused() {
    let error = "sometimes".parse::<ApprovalPolicy>().unwrap_err();
    assert_eq!(
        error.to_string(),
        "approval policy must be one of \"ask\" or \"never\""
    );
    assert_eq!("never".parse(), Ok(ApprovalPolicy::Never));
    assert!(serde_json::from_value::<ApprovalPolicy>(json!("sometimes")).is_err());
}

// "defaults a schema-less construction to ask"
#[tokio::test]
async fn the_default_policy_is_ask() {
    assert_eq!(ApprovalConfig::default().policy, ApprovalPolicy::Ask);
    let bench = mounted(ApprovalConfig::default().policy).await;
    let agent = bench.agent("bare-config");
    bench.register(answering(ApprovalOutcome::AllowedOnce));
    assert_eq!(
        bench.service.request(&request_of(&agent)).await,
        Ok(ApprovalOutcome::AllowedOnce)
    );
}

// "a never config rejects deterministically without consulting any
// answerer", "the gate decides FIRST even against an answerer registered
// before the service", and "never is unbypassable even by an answerer
// PREPENDED after the service mounts": no answerer runs at all.
#[tokio::test]
async fn never_rejects_without_asking_anyone() {
    let bench = mounted(ApprovalPolicy::Never).await;
    let calls = Arc::new(AtomicUsize::new(0));
    bench.register(counting(&calls));
    let agent = bench.agent("never");
    let _ = bench
        .service
        .answerer(Some(&agent.id), Arc::new(counting(&calls)));
    let request = ApprovalRequest::new(Arc::clone(&agent) as Arc<dyn Agent>, "bash");
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::Rejected)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // The audit pair still lands on the log.
    let types: Vec<String> = bench
        .audit(&agent)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(types, ["approval/asked", "approval/decided"]);
}

// "a session override outranks the configured default, in both directions"
#[tokio::test]
async fn a_session_override_outranks_the_default() {
    let bench = mounted(ApprovalPolicy::Never).await;
    bench.register(answering(ApprovalOutcome::AllowedOnce));
    let agent = bench.agent("override");
    let session = bench.sessions.get(&agent.id).unwrap();
    let request = ApprovalRequest::new(Arc::clone(&agent) as Arc<dyn Agent>, "bash");
    assert_eq!(bench.service.override_of(&session), None);
    set_approval_policy(&session, ApprovalPolicy::Ask).unwrap();
    assert_eq!(
        bench.service.override_of(&session),
        Some(ApprovalPolicy::Ask)
    );
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::AllowedOnce)
    );
    set_approval_policy(&session, ApprovalPolicy::Never).unwrap();
    assert_eq!(
        bench.service.request(&request).await,
        Ok(ApprovalOutcome::Rejected)
    );
}

// "queues a live policy switch for the next model step"
#[tokio::test]
async fn a_live_switch_queues_its_notice_once() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("notice");
    let session = bench.sessions.get(&agent.id).unwrap();
    bench
        .service
        .set_policy(agent.as_ref(), ApprovalPolicy::Never)
        .unwrap();
    bench
        .service
        .set_policy(agent.as_ref(), ApprovalPolicy::Never)
        .unwrap();
    assert_eq!(
        bench.service.override_of(&session),
        Some(ApprovalPolicy::Never)
    );
    let injected = agent.injected.lock().unwrap();
    assert_eq!(injected.len(), 1);
    assert_eq!(
        injected[0].source,
        UserSource::Other(json!({ "kind": "user-approval" }))
    );
    assert_eq!(
        serde_json::to_value(&injected[0].content).unwrap(),
        json!([{
            "type": "text",
            "text": "The approval policy changed from \"ask\" to \"never\" (changed by the user).",
        }])
    );
    // One policy event: the second switch changed nothing.
    let policies = bench
        .audit(&agent)
        .into_iter()
        .filter(|(kind, _)| kind == "approval/policy")
        .count();
    assert_eq!(policies, 1);
}

// "contributes the complete current ask or never policy as cache-safe
// context"
#[tokio::test]
async fn the_context_states_the_policy() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let _ask = bench.agent("sect-ask");
    let never = bench.agent("sect-never");
    set_approval_policy(
        &bench.sessions.get(&never.id).unwrap(),
        ApprovalPolicy::Never,
    )
    .unwrap();
    assert_eq!(
        bench.context_for(Some("sect-ask")).await.as_deref(),
        Some(ASK_SENTENCE)
    );
    assert_eq!(
        bench.context_for(Some("sect-never")).await.as_deref(),
        Some(NEVER_SENTENCE)
    );
    // An assembly for no agent has no session to state.
    assert_eq!(bench.context_for(None).await.as_deref(), Some(""));
    assert_eq!(
        ASK_SENTENCE,
        "Approval policy: ask. Operations that require approval may ask through the configured \
         answerers; without an available answerer, the request fails closed."
    );
    assert_eq!(
        NEVER_SENTENCE,
        "Approval prompts are disabled in this session: actions that require approval are \
         rejected automatically — do not request sandbox escalation (do not set \
         `sandbox_permissions`)."
    );
}

// "reflects the latest durable switch in cache-safe context and stays
// byte-stable while unchanged"
#[tokio::test]
async fn the_context_follows_the_latest_switch() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let agent = bench.agent("context-switch");
    let session = bench.sessions.get(&agent.id).unwrap();
    let scope = Some("context-switch");
    assert_eq!(
        bench.context_for(scope).await.as_deref(),
        Some(ASK_SENTENCE)
    );
    assert_eq!(
        bench.context_for(scope).await.as_deref(),
        Some(ASK_SENTENCE)
    );
    for policy in [
        ApprovalPolicy::Never,
        ApprovalPolicy::Ask,
        ApprovalPolicy::Never,
    ] {
        set_approval_policy(&session, policy).unwrap();
    }
    assert_eq!(
        bench.context_for(scope).await.as_deref(),
        Some(NEVER_SENTENCE)
    );
    assert_eq!(
        bench.context_for(scope).await.as_deref(),
        Some(NEVER_SENTENCE)
    );
}

// A restored session keeps its override: the fold reads the stored log.
#[tokio::test]
async fn a_restored_session_keeps_its_override() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    let entries: Vec<Entry> = serde_json::from_value(json!([
        { "type": "turn/start", "seq": 0, "time": 0, "data": { "turn": 1 } },
        { "type": "approval/policy", "seq": 1, "time": 0, "data": { "policy": "never" } },
    ]))
    .unwrap();
    let session = bench
        .sessions
        .restore(SessionId::new("restored"), entries)
        .unwrap();
    assert_eq!(
        bench.service.override_of(&session),
        Some(ApprovalPolicy::Never)
    );
    assert_eq!(
        bench.service.effective_policy(&session),
        ApprovalPolicy::Never
    );
}

// The tool registry's `ask`, through the service: the plugin makes it the
// registry's approval seam. (dsh: tools.spec's "ask routing through
// ctx.approval", with the real service.)

/// Answers `done`.
struct Done;

impl Tool for Done {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "echo".into(),
            description: "Says done.".into(),
            parameters: r#"{"type":"object","properties":{}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::ok("done") })
    }
}

/// Asks before every call.
struct AskFirst;

impl PreExecuteListener for AskFirst {
    fn decide<'a>(&'a self, _: &'a ToolExecution, _: PreExecuteNext<'a>) -> PreExecuteFuture<'a> {
        Box::pin(async {
            Ok(PreToolDecision::Ask {
                reason: Some("hook wants a human".into()),
                display_reason: None,
            })
        })
    }
}

impl Bench {
    fn gated(&self) {
        self.tools.register(Arc::new(Done)).unwrap();
        let _ = self.tools.on_pre_execute(None, Arc::new(AskFirst));
    }

    async fn call(&self, agent: &Arc<Stub>) -> ToolOutput {
        self.tools
            .execute(
                &ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: "{}".into(),
                },
                Some(Arc::clone(agent) as Arc<dyn Agent>),
            )
            .await
    }
}

#[tokio::test]
async fn a_tool_calls_ask_is_this_services_request() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    bench.gated();
    bench.register(answering(ApprovalOutcome::AllowedOnce));
    let agent = bench.agent("gated");
    assert_eq!(bench.call(&agent).await, ToolOutput::ok("done"));
    let audit = bench.audit(&agent);
    assert_eq!(audit[0].1["toolName"], "echo");
    assert_eq!(audit[0].1["callId"], "c1");
    assert_eq!(audit[0].1["reason"], "hook wants a human");
    assert_eq!(audit[1].1["outcome"], "allowed-once");
}

#[tokio::test]
async fn a_tool_calls_ask_under_never_or_with_no_answerer_is_denied() {
    let bench = mounted(ApprovalPolicy::Never).await;
    bench.gated();
    let agent = bench.agent("never-gated");
    assert_eq!(
        bench.call(&agent).await,
        ToolOutput::error("Error: the user rejected tool \"echo\"")
    );
    let bench = mounted(ApprovalPolicy::Ask).await;
    bench.gated();
    let agent = bench.agent("unanswered-gated");
    assert_eq!(
        bench.call(&agent).await,
        ToolOutput::error(
            "Error: tool \"echo\" requires approval, but no approval channel is available"
        )
    );
}

// An ask outside an open turn fails the call with dsh's message, as its
// registry turns the thrown error into the result.
#[tokio::test]
async fn a_tool_calls_ask_outside_a_turn_fails_the_call() {
    let bench = mounted(ApprovalPolicy::Ask).await;
    bench.gated();
    let agent = bench.agent_with("idle-gated", Vec::new());
    assert_eq!(
        bench.call(&agent).await,
        ToolOutput::error(format!("Error: {OUTSIDE_TURN}"))
    );
    assert!(bench.audit(&agent).is_empty());
}
