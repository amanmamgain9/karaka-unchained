//! The gate before a tool runs: `tools/pre-execute` and `ask`. Ported from
//! dsh's `tools/tests/tools.spec.ts`, its pre-execute and "ask routing
//! through ctx.approval" cases, with a stand-in approval seam;
//! `karaka-user-approval`'s tests run the real one.
//!
//! Not ported: the structured `error.info` a denial or failure carries
//! (results are text only); the signal cases, whose cancel here is the
//! call's future being dropped (one case shows that); and the rogue
//! outcome of a non-conforming approval stand-in, which a closed enum
//! rules out.

use std::future::pending;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_agent::{Agent, FrameListener, FrameListeners, FrameSubscription, InboxError, WhenIdle};
use karaka_model::{ToolCall, ToolSchema};
use karaka_session::{CancelCause, SessionId, UserMessage};
use karaka_tools::{
    DisplayReason, PreExecuteFuture, PreExecuteListener, PreExecuteNext, PreToolDecision, Tool,
    ToolApproval, ToolApprovalFuture, ToolApprovalOutcome, ToolApprovalRequest, ToolExecution,
    ToolFuture, ToolOutput, ToolRegistry, ToolRun,
};
use serde_json::{Value, json};

struct Stub {
    id: SessionId,
    frames: FrameListeners,
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

    fn inject(&self, _: UserMessage) -> Result<(), InboxError> {
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

fn agent(id: &str) -> Arc<dyn Agent> {
    Arc::new(Stub {
        id: SessionId::new(id),
        frames: FrameListeners::new(),
    })
}

/// Answers with its `text` argument, and counts its runs. (dsh:
/// `echoTool`.)
struct Echo(Arc<AtomicUsize>);

impl Tool for Echo {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "echo".into(),
            description: "Echoes its text.".into(),
            parameters: r#"{"type":"object","properties":{"text":{"type":"string"}}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, _: ToolRun) -> ToolFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            ToolOutput::ok(arguments["text"].as_str().unwrap_or_default().to_owned())
        })
    }
}

/// A registry with `echo`, and how many times it ran.
fn registry() -> (ToolRegistry, Arc<AtomicUsize>) {
    let registry = ToolRegistry::new();
    let runs = Arc::new(AtomicUsize::new(0));
    registry
        .register(Arc::new(Echo(Arc::clone(&runs))))
        .unwrap();
    (registry, runs)
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

/// A listener made of a closure.
struct With<F>(F);

impl<F> PreExecuteListener for With<F>
where
    F: for<'a> Fn(&'a ToolExecution, PreExecuteNext<'a>) -> PreExecuteFuture<'a> + Send + Sync,
{
    fn decide<'a>(
        &'a self,
        call: &'a ToolExecution,
        next: PreExecuteNext<'a>,
    ) -> PreExecuteFuture<'a> {
        (self.0)(call, next)
    }
}

fn with<F>(decide: F) -> Arc<With<F>>
where
    F: for<'a> Fn(&'a ToolExecution, PreExecuteNext<'a>) -> PreExecuteFuture<'a> + Send + Sync,
{
    Arc::new(With(decide))
}

fn deciding(decision: PreToolDecision) -> Arc<dyn PreExecuteListener> {
    with(move |_, _| {
        let decision = decision.clone();
        Box::pin(async move { Ok(decision) })
    })
}

fn ask(reason: Option<&str>) -> PreToolDecision {
    PreToolDecision::Ask {
        reason: reason.map(str::to_owned),
        display_reason: None,
    }
}

/// A stand-in approval seam: answers `outcome`, and keeps what it was asked.
struct Approval {
    outcome: Result<ToolApprovalOutcome, String>,
    seen: Mutex<Vec<ToolApprovalRequest>>,
}

impl ToolApproval for Approval {
    fn request<'a>(&'a self, request: ToolApprovalRequest) -> ToolApprovalFuture<'a> {
        self.seen.lock().unwrap().push(request);
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

fn approval(outcome: Result<ToolApprovalOutcome, String>) -> Arc<Approval> {
    Arc::new(Approval {
        outcome,
        seen: Mutex::new(Vec::new()),
    })
}

// "lets a tools/pre-execute listener deny a call (permission pattern)"
#[tokio::test]
async fn a_listener_can_deny_a_call() {
    let (registry, runs) = registry();
    let _ = registry.on_pre_execute(
        None,
        with(|call, next| {
            if call.name == "echo" {
                return Box::pin(async {
                    Ok(PreToolDecision::Deny {
                        reason: "denied by policy".into(),
                    })
                });
            }
            next.run()
        }),
    );
    let output = registry
        .execute(&call("echo", json!({ "text": "hi" })), None)
        .await;
    assert_eq!(output, ToolOutput::error("Error: denied by policy"));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

// A listener that hands on reaches the end of the chain, which allows.
#[tokio::test]
async fn the_end_of_the_chain_allows() {
    let (registry, runs) = registry();
    let _ = registry.on_pre_execute(None, with(|_, next| next.run()));
    let output = registry
        .execute(&call("echo", json!({ "text": "hi" })), None)
        .await;
    assert_eq!(output, ToolOutput::ok("hi"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// "an ask decision degrades to deny when no approval seam is mounted", and
// "an ask decision with no reason degrades to deny with a default message"
#[tokio::test]
async fn an_ask_with_no_approval_seam_is_denied() {
    for (reason, text) in [
        (Some("needs approval"), "Error: needs approval"),
        (
            None,
            "Error: tool \"echo\" requires approval (not yet supported)",
        ),
    ] {
        let (registry, runs) = registry();
        let _ = registry.on_pre_execute(None, deciding(ask(reason)));
        let output = registry
            .execute(&call("echo", json!({ "text": "hi" })), Some(agent("a")))
            .await;
        assert_eq!(output, ToolOutput::error(text));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
}

// "dispatches the tool when the answerer grants allowed-once, forwarding
// the ask fields"
#[tokio::test]
async fn a_granted_ask_runs_the_call_and_forwards_its_fields() {
    let (registry, runs) = registry();
    let seam = approval(Ok(ToolApprovalOutcome::AllowedOnce));
    registry.set_approval(Arc::clone(&seam) as Arc<dyn ToolApproval>);
    let display = DisplayReason {
        en: "Allow it?".into(),
        others: [("zh".to_owned(), "允许吗？".to_owned())].into(),
    };
    let _ = registry.on_pre_execute(
        None,
        deciding(PreToolDecision::Ask {
            reason: Some("hook wants a human".into()),
            display_reason: Some(display.clone()),
        }),
    );
    let caller = agent("asker");
    let output = registry
        .execute(
            &call("echo", json!({ "text": "hi" })),
            Some(Arc::clone(&caller)),
        )
        .await;
    assert_eq!(output, ToolOutput::ok("hi"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let seen = seam.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].agent.instance(), caller.instance());
    assert_eq!(seen[0].tool_name, "echo");
    assert_eq!(seen[0].call_id, "c1");
    assert_eq!(seen[0].reason.as_deref(), Some("hook wants a human"));
    assert_eq!(seen[0].display_reason, Some(display));
}

// "denies with the user-rejection reason on rejected", "... the
// cancellation reason on cancelled", and "... the no-channel reason when
// the seam is mounted but nobody answers"
#[tokio::test]
async fn each_withheld_grant_denies_with_its_own_reason() {
    for (outcome, text) in [
        (
            ToolApprovalOutcome::Rejected,
            "Error: the user rejected tool \"echo\"",
        ),
        (
            ToolApprovalOutcome::Cancelled,
            "Error: approval for tool \"echo\" was cancelled",
        ),
        (
            ToolApprovalOutcome::Unavailable,
            "Error: tool \"echo\" requires approval, but no approval channel is available",
        ),
    ] {
        let (registry, runs) = registry();
        registry.set_approval(approval(Ok(outcome)));
        let _ = registry.on_pre_execute(None, deciding(ask(None)));
        let output = registry
            .execute(&call("echo", json!({})), Some(agent("a")))
            .await;
        assert_eq!(output, ToolOutput::error(text));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
}

// "denies an agent-less execution without asking"
#[tokio::test]
async fn an_ask_with_no_agent_is_denied_without_asking() {
    let (registry, _) = registry();
    let seam = approval(Ok(ToolApprovalOutcome::AllowedOnce));
    registry.set_approval(Arc::clone(&seam) as Arc<dyn ToolApproval>);
    let _ = registry.on_pre_execute(None, deciding(ask(None)));
    let output = registry.execute(&call("echo", json!({})), None).await;
    assert_eq!(
        output,
        ToolOutput::error(
            "Error: tool \"echo\" requires approval, but the call has no agent to route it through"
        )
    );
    assert!(seam.seen.lock().unwrap().is_empty());
}

// An approval that fails, such as one asked outside an open turn, fails the
// call with its message. (dsh: `toolErrorResult` of the thrown error.)
#[tokio::test]
async fn a_failed_approval_fails_the_call() {
    let (registry, runs) = registry();
    registry.set_approval(approval(Err("approval broke".into())));
    let _ = registry.on_pre_execute(None, deciding(ask(None)));
    let output = registry
        .execute(&call("echo", json!({})), Some(agent("a")))
        .await;
    assert_eq!(output, ToolOutput::error("Error: approval broke"));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

// "maps an explicit pre-execute cancellation to the canonical
// before-dispatch result"
#[tokio::test]
async fn a_cancel_decision_is_the_before_dispatch_result() {
    let (registry, runs) = registry();
    let _ = registry.on_pre_execute(None, deciding(PreToolDecision::Cancel));
    let output = registry.execute(&call("echo", json!({})), None).await;
    assert_eq!(
        output,
        ToolOutput::error("Error: tool call aborted before dispatch")
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[derive(Debug)]
struct Plain(&'static str);

impl std::fmt::Display for Plain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Plain {}

// "returns an isError result when a tools/pre-execute listener throws"
#[tokio::test]
async fn a_failing_listener_fails_the_call() {
    let (registry, runs) = registry();
    let _ = registry.on_pre_execute(
        None,
        with(|_, _| Box::pin(async { Err(Box::new(Plain("permission hook broke")) as _) })),
    );
    let output = registry
        .execute(&call("echo", json!({ "text": "hi" })), None)
        .await;
    assert_eq!(output, ToolOutput::error("Error: permission hook broke"));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

// dsh's order: the gate sees every call that reaches the registry, an
// unknown name's and unfit arguments' too, before either is refused.
#[tokio::test]
async fn the_gate_runs_before_the_unknown_name_and_argument_checks() {
    let (registry, runs) = registry();
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let noted = Arc::clone(&seen);
    let _ = registry.on_pre_execute(
        None,
        with(move |call, next| {
            noted
                .lock()
                .unwrap()
                .push((call.name.clone(), call.arguments.clone()));
            next.run()
        }),
    );
    let ghost = registry.execute(&call("ghost", json!({})), None).await;
    assert_eq!(ghost, ToolOutput::error("Error: unknown tool \"ghost\""));
    let unfit = registry
        .execute(&call("echo", json!({ "text": 1 })), None)
        .await;
    assert_eq!(
        unfit,
        ToolOutput::error("Error: invalid arguments: \"text\" must be a string")
    );
    let not_json = registry
        .execute(
            &ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: "not json".into(),
            },
            None,
        )
        .await;
    assert_eq!(
        not_json,
        ToolOutput::error("Error: invalid arguments: \"arguments\" must be an object")
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            ("ghost".to_owned(), json!({})),
            ("echo".to_owned(), json!({ "text": 1 })),
            ("echo".to_owned(), json!("not json")),
        ]
    );
    // And a denial wins over the argument check, which never runs.
    let (registry, _) = self::registry();
    let _ = registry.on_pre_execute(
        None,
        deciding(PreToolDecision::Deny {
            reason: "nope".into(),
        }),
    );
    let output = registry
        .execute(&call("echo", json!({ "text": 1 })), None)
        .await;
    assert_eq!(output, ToolOutput::error("Error: nope"));
}

// Scoped like the registry's other ports: a call runs the global listeners
// and its agent's, in the order they were added; a call with no agent runs
// only the global ones. (dsh: scope-filtered dispatch.)
#[tokio::test]
async fn an_agents_listeners_hear_only_its_calls() {
    let (registry, _) = registry();
    let heard: Arc<Mutex<Vec<String>>> = Arc::default();
    let note = |label: &'static str| {
        let heard = Arc::clone(&heard);
        with(move |call: &ToolExecution, next| {
            let who = call
                .agent
                .as_ref()
                .map_or("none".to_owned(), |agent| agent.id().to_string());
            heard.lock().unwrap().push(format!("{label}:{who}"));
            next.run()
        })
    };
    let a = agent("a");
    let b = agent("b");
    let _ = registry.on_pre_execute(None, note("global"));
    let _ = registry.on_pre_execute(Some(a.id()), note("scoped"));
    let _ = registry.on_pre_execute(Some(b.id()), note("scoped"));
    for caller in [Some(a), Some(b), None] {
        registry.execute(&call("echo", json!({})), caller).await;
    }
    assert_eq!(
        *heard.lock().unwrap(),
        [
            "global:a",
            "scoped:a",
            "global:b",
            "scoped:b",
            "global:none"
        ]
    );
}

// "skips dispatch when caller cancellation arrives while pre-execute
// awaits": the cancel drops the call's future, and the tool never runs.
#[tokio::test]
async fn a_call_dropped_in_the_gate_never_runs() {
    let (registry, runs) = registry();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entering = Arc::clone(&entered);
    let _ = registry.on_pre_execute(
        None,
        with(move |_, next| {
            let entering = Arc::clone(&entering);
            Box::pin(async move {
                entering.notify_one();
                pending::<()>().await;
                next.run().await
            })
        }),
    );
    let mut future = registry.execute(&call("echo", json!({})), None);
    tokio::select! {
        _ = &mut future => panic!("the gate never decides"),
        () = entered.notified() => {}
    }
    drop(future);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

// A removed listener is out of the chain. (dsh: the disposer.)
#[tokio::test]
async fn a_removed_listener_is_out_of_the_chain() {
    let (registry, runs) = registry();
    let registration = registry.on_pre_execute(None, deciding(PreToolDecision::Cancel));
    registration.remove();
    let output = registry
        .execute(&call("echo", json!({ "text": "hi" })), None)
        .await;
    assert_eq!(output, ToolOutput::ok("hi"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}
