//! The `ask_user_question` tool, run through the registry. Ported from
//! dsh's `tool-ask-user/tests/tool-ask-user.spec.ts`.
//!
//! Not ported:
//! - "passes the tool abort signal to the user-questions request": a
//!   Karaka tool has no signal; dropping its future cancels it.
//! - "rejects a live runtime-owned agent with a structured
//!   DELEGATED_CALLER error": Karaka has no child agents.
//! - "unregisters the tool when its plugin fiber is disposed": the kernel
//!   unloads no plugin.
//! - The `error.info` name and code of the structured-error cases: a
//!   Karaka result is text only. Their text is checked instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use karaka_agent::{
    Agent, Agents, FrameListener, FrameListeners, FrameSubscription, InboxError, InboxHooks,
    OpenError, WhenIdle,
};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId, Runtime};
use karaka_model::ToolCall;
use karaka_session::{
    CancelCause, Entry, SessionHeader, SessionId, SessionStore, SessionsPlugin, UserMessage,
};
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_ask_user::{AskUserConfig, AskUserPlugin, Mode, NAME, PENDING_NOTICE};
use karaka_tools::{ToolOutput, ToolRegistry, ToolsPlugin};
use karaka_user_questions::{
    ASK_TIMED_OUT, AnswerFuture, AnswerNext, AskUserQuestionAnswer, AskUserQuestionAnswerItem,
    AskUserQuestionItem, AskUserQuestionOption, QuestionAnswerer, QuestionRequest, QuestionWait,
    UserQuestionError, UserQuestionState, UserQuestions, UserQuestionsPlugin, fold_user_questions,
    is_timed_ask_user_question_schema,
};
use serde_json::{Value, json};

// A stand-in registry, whose agents do nothing.

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

#[derive(Default)]
struct Registry(Mutex<HashMap<SessionId, Arc<dyn Agent>>>);

impl Agents for Registry {
    fn open(&self, id: SessionId) -> Result<Arc<dyn Agent>, OpenError> {
        Ok(self.get(&id).expect("tests enter their agents"))
    }

    fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>> {
        self.0.lock().unwrap().get(id).cloned()
    }
}

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

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let registry = Arc::new(Registry::default());
        ctx.provide(Arc::clone(&registry) as Arc<dyn Agents>);
        ctx.provide(registry);
        ctx.provide(Arc::new(InboxHooks::new()));
        Ok(())
    }
}

struct Bench {
    tools: Arc<ToolRegistry>,
    service: Arc<UserQuestions>,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    _runtime: Runtime,
}

/// dsh's `setup`: the timed tool with no window, unless `config` says
/// otherwise.
async fn setup(config: AskUserConfig) -> Bench {
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(RegistryPlugin),
        Box::new(UserQuestionsPlugin),
        Box::new(AskUserPlugin::new(config)),
    ])
    .await
    .expect("the runtime starts");
    Bench {
        tools: runtime.get::<ToolRegistry>(),
        service: runtime.get::<UserQuestions>(),
        registry: runtime.get::<Registry>(),
        sessions: runtime.get::<SessionStore>(),
        _runtime: runtime,
    }
}

fn timed(timeout: i64) -> AskUserConfig {
    AskUserConfig {
        mode: Mode::Timed,
        timeout,
    }
}

fn legacy() -> AskUserConfig {
    AskUserConfig::default()
}

impl Bench {
    /// A live agent, with a session of its own.
    fn live(&self, id: &str) -> Arc<dyn Agent> {
        self.sessions.create(SessionId::new(id)).unwrap();
        self.enter(id)
    }

    fn enter(&self, id: &str) -> Arc<dyn Agent> {
        let agent: Arc<dyn Agent> = Arc::new(Stub {
            id: SessionId::new(id),
            frames: FrameListeners::new(),
        });
        self.registry
            .0
            .lock()
            .unwrap()
            .insert(agent.id().clone(), Arc::clone(&agent));
        agent
    }

    /// Records every request, and answers with `answer`.
    fn answer_with(
        &self,
        answer: impl Fn(&QuestionRequest) -> Result<AskUserQuestionAnswer, UserQuestionError>
        + Send
        + Sync
        + 'static,
    ) -> Arc<Mutex<Vec<QuestionRequest>>> {
        let seen: Arc<Mutex<Vec<QuestionRequest>>> = Arc::default();
        let _ = self.service.answerer(
            None,
            Arc::new(Answerer {
                seen: Arc::clone(&seen),
                answer: Box::new(answer),
            }),
        );
        seen
    }

    async fn execute(
        &self,
        call_id: &str,
        arguments: Value,
        agent: Option<Arc<dyn Agent>>,
    ) -> ToolOutput {
        self.tools
            .execute(
                &ToolCall {
                    id: call_id.into(),
                    name: NAME.into(),
                    arguments: arguments.to_string(),
                },
                agent,
            )
            .await
    }

    fn schema(&self) -> (String, Value) {
        let schema = self
            .tools
            .schemas()
            .into_iter()
            .find(|tool| tool.name == NAME)
            .expect("the tool is registered");
        (
            schema.description,
            serde_json::from_str(&schema.parameters).unwrap(),
        )
    }

    /// The tool's schema as a request header logs it.
    fn header_tools(&self) -> Vec<Value> {
        self.tools
            .schemas()
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": serde_json::from_str::<Value>(&tool.parameters).unwrap(),
                })
            })
            .collect()
    }
}

type Answer =
    dyn Fn(&QuestionRequest) -> Result<AskUserQuestionAnswer, UserQuestionError> + Send + Sync;

struct Answerer {
    seen: Arc<Mutex<Vec<QuestionRequest>>>,
    answer: Box<Answer>,
}

impl QuestionAnswerer for Answerer {
    fn answer<'a>(&'a self, request: &'a QuestionRequest, _: AnswerNext<'a>) -> AnswerFuture<'a> {
        self.seen.lock().unwrap().push(request.clone());
        let answer = (self.answer)(request).map_err(Into::into);
        Box::pin(async move { answer })
    }
}

fn item(id: &str, selected: &[&str], custom: Option<&str>) -> AskUserQuestionAnswerItem {
    AskUserQuestionAnswerItem {
        id: id.into(),
        selected: selected.iter().map(|label| (*label).to_owned()).collect(),
        custom: custom.map(str::to_owned),
    }
}

fn answers(items: Vec<AskUserQuestionAnswerItem>) -> AskUserQuestionAnswer {
    AskUserQuestionAnswer { answers: items }
}

fn continue_question() -> Value {
    json!({ "questions": [{ "id": "continue", "question": "Continue?" }] })
}

fn ok(text: &str) -> ToolOutput {
    ToolOutput::ok(text)
}

fn event(seq: u64, event_type: &str, data: Value) -> Entry {
    let mut envelope = json!({ "type": event_type, "seq": seq, "time": seq, "data": data });
    if event_type == "tool/result" {
        envelope["surfaceOp"] = json!("append");
    }
    serde_json::from_value(envelope).unwrap()
}

fn header_event(tools: Vec<Value>) -> Entry {
    event(
        0,
        "request/header",
        json!({
            "header": { "config": { "provider": "deepseek-official", "model": "deepseek-v4-flash" }, "tools": tools },
            "reason": "initial",
        }),
    )
}

fn asked_event(call_id: &str, arguments: &str) -> Entry {
    event(
        1,
        "tool/call",
        json!({ "turn": 1, "step": 1, "callId": call_id, "name": NAME, "arguments": arguments }),
    )
}

fn resulted_event(call_id: &str, text: &str) -> Entry {
    event(
        2,
        "tool/result",
        json!({
            "turn": 1, "step": 1,
            "message": {
                "id": format!("tool-{call_id}"), "role": "tool",
                "source": { "kind": "tool", "callId": call_id },
                "toolCallId": call_id,
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            },
        }),
    )
}

// "keeps the blocking legacy tool when apply receives no config"
#[tokio::test]
async fn the_default_is_the_blocking_legacy_tool() {
    let bench = setup(AskUserConfig::default()).await;
    let (description, parameters) = bench.schema();
    assert!(parameters["properties"].get("timeout").is_none());
    assert!(description.contains("Ask the user a concise question"));
}

// "registers the timed tool only when the Cordis row opts in", and "falls
// back to the 120 second wait when apply receives a row without a timeout"
#[tokio::test]
async fn the_timed_tool_is_opted_into() {
    let bench = setup(AskUserConfig {
        mode: Mode::Timed,
        ..AskUserConfig::default()
    })
    .await;
    let (description, parameters) = bench.schema();
    assert!(parameters["properties"].get("timeout").is_some());
    assert!(description.contains("pending"));
    assert!(
        parameters
            .to_string()
            .contains("Wait seconds for the entire batch (default 120)")
    );
}

// "returns the answer batch when the Client answers inside the configured
// wait"
#[tokio::test]
async fn an_answer_inside_the_window_is_the_result() {
    let bench = setup(timed(30)).await;
    bench.answer_with(|_| Ok(answers(vec![item("continue", &["yes"], None)])));
    let agent = bench.live("timed-in-time");
    let result = bench
        .execute("ask-in-time", continue_question(), Some(agent))
        .await;
    assert_eq!(
        result,
        ok(r#"{"answers":[{"id":"continue","selected":["yes"]}]}"#)
    );
}

// "describes the configured default timeout to the model"
#[tokio::test]
async fn the_schema_names_the_default_window() {
    let bench = setup(timed(45)).await;
    let text = bench.schema().1.to_string();
    assert!(text.contains(
        "Wait seconds for the entire batch (default 45); omit unless the user specifies a \
         duration. Use -1 only when an answer is required before proceeding."
    ));
    assert!(!text.contains("Wait seconds for the entire batch (default 120)"));
}

// "registers a model-facing tool schema": here, to the byte, as dsh's
// `defineTool` compiles it.
#[tokio::test]
async fn the_timed_schema_is_dshs() {
    let bench = setup(timed(-1)).await;
    let (description, parameters) = bench.schema();
    assert!(description.contains(
        "A submitted skipped question is an answer item with empty selected and no custom"
    ));
    assert_eq!(
        parameters.to_string(),
        concat!(
            r#"{"type":"object","properties":{"questions":{"type":"array","description":"Questions to ask the user.","#,
            r#""items":{"type":"object","additionalProperties":true,"properties":{"#,
            r#""id":{"type":"string","description":"Stable id for this question; echoed in the answer."},"#,
            r#""question":{"type":"string","description":"The specific question to ask the user."},"#,
            r#""header":{"type":"string","description":"Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\"."},"#,
            r#""options":{"type":"array","description":"Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.","#,
            r#""items":{"type":"object","additionalProperties":true,"properties":{"#,
            r#""label":{"type":"string","description":"Short user-facing option label."},"#,
            r#""description":{"type":"string","description":"One sentence explaining the tradeoff or impact."}},"#,
            r#""required":["label"]}},"#,
            r#""multi_select":{"type":"boolean","description":"Whether the user may select more than one option. Defaults to false."}},"#,
            r#""required":["id","question"]}},"#,
            r#""timeout":{"type":"integer","description":"Wait seconds for the entire batch (default -1); omit unless the user specifies a duration. Use -1 only when an answer is required before proceeding."}},"#,
            r#""required":["questions"]}"#
        )
    );
    let options = &parameters["properties"]["questions"]["items"]["properties"]["options"]["items"]
        ["properties"];
    for absent in ["value", "recommended", "preview"] {
        assert!(options.get(absent).is_none(), "{absent}");
    }
}

// "lets Cordis select the exact legacy blocking tool instead of the timed
// tool"
#[tokio::test]
async fn the_legacy_tool_blocks_and_passes_every_field() {
    let bench = setup(legacy()).await;
    let seen = bench.answer_with(|request| {
        Ok(if request.questions[0].id == "pkg" {
            answers(vec![item("pkg", &[], Some("bun"))])
        } else {
            answers(vec![item("continue", &["yes"], None)])
        })
    });
    let (description, parameters) = bench.schema();
    assert_eq!(
        description,
        "Ask the user a concise question when you need confirmation, a choice, or missing \
         information before proceeding."
    );
    assert_eq!(
        parameters["properties"]["questions"]["description"],
        "Questions to ask the user before continuing."
    );
    assert!(parameters["properties"].get("timeout").is_none());
    // The projection tells the two tools apart by this schema alone.
    assert!(!is_timed_ask_user_question_schema(NAME, &parameters));
    assert_eq!(
        fold_user_questions(&[
            header_event(bench.header_tools()),
            asked_event(
                "ask-legacy",
                r#"{"questions":[{"id":"continue","question":"Continue?"}]}"#
            ),
        ]),
        Default::default()
    );

    let result = bench.execute("ask-legacy", continue_question(), None).await;
    assert_eq!(
        result,
        ok(r#"{"answers":[{"id":"continue","selected":["yes"]}]}"#)
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].wait, None);
        assert!(seen[0].agent.is_none());
        assert_eq!(
            seen[0].questions,
            [AskUserQuestionItem::new("continue", "Continue?")]
        );
    }

    let agent = bench.live("legacy-agent");
    let detailed = bench
        .execute(
            "ask-legacy-detailed",
            json!({ "questions": [{
                "id": "pkg", "question": "Which package manager?", "header": "Setup", "multi_select": false,
                "options": [{ "label": "pnpm (Recommended)", "description": "Workspace default." }, { "label": "npm" }],
            }] }),
            Some(Arc::clone(&agent)),
        )
        .await;
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[1].agent.as_ref().map(|agent| agent.instance()),
        Some(agent.instance())
    );
    assert_eq!(
        seen[1].questions,
        [AskUserQuestionItem {
            header: Some("Setup".into()),
            multi_select: Some(false),
            options: Some(vec![
                AskUserQuestionOption {
                    label: "pnpm (Recommended)".into(),
                    description: Some("Workspace default.".into()),
                },
                AskUserQuestionOption::new("npm"),
            ]),
            ..AskUserQuestionItem::new("pkg", "Which package manager?")
        }]
    );
    assert_eq!(
        detailed,
        ok(r#"{"answers":[{"id":"pkg","selected":[],"custom":"bun"}]}"#)
    );
}

// "asks the registered user-questions provider and projects structured
// answers to text"
#[tokio::test]
async fn an_unwindowed_timed_ask_is_keyed_by_its_call() {
    let bench = setup(timed(-1)).await;
    let seen = bench.answer_with(|_| Ok(answers(vec![item("pkg", &["pnpm"], None)])));
    let result = bench
        .execute(
            "ask-1",
            json!({ "questions": [{
                "id": "pkg", "question": "Which package manager should I use?",
                "options": [{ "label": "pnpm", "description": "Use pnpm workspaces." }],
            }] }),
            None,
        )
        .await;
    assert_eq!(
        result,
        ok(r#"{"answers":[{"id":"pkg","selected":["pnpm"]}]}"#)
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].questions[0].options,
        Some(vec![AskUserQuestionOption {
            label: "pnpm".into(),
            description: Some("Use pnpm workspaces.".into()),
        }])
    );
    // Keyed by the call with no deadline.
    assert_eq!(
        seen[0].wait,
        Some(QuestionWait {
            call_id: "ask-1".into(),
            timed: false,
        })
    );
}

// "passes recommended option labels through without adding schema fields"
#[tokio::test]
async fn recommended_labels_pass_through() {
    let bench = setup(timed(-1)).await;
    let seen = bench.answer_with(|_| Ok(answers(vec![item("pkg", &["pnpm (Recommended)"], None)])));
    bench
        .execute(
            "ask-recommended",
            json!({ "questions": [{
                "id": "pkg", "question": "Which package manager should I use?",
                "options": [{ "label": "pnpm (Recommended)" }, { "label": "npm" }],
            }] }),
            None,
        )
        .await;
    assert_eq!(
        seen.lock().unwrap()[0].questions[0].options,
        Some(vec![
            AskUserQuestionOption::new("pnpm (Recommended)"),
            AskUserQuestionOption::new("npm"),
        ])
    );
}

// "projects custom answers and multi-select choices"
#[tokio::test]
async fn custom_answers_and_multi_select_come_back_verbatim() {
    let bench = setup(timed(-1)).await;
    bench.answer_with(|_| {
        Ok(answers(vec![
            item("targets", &["tests", "docs"], Some("release notes")),
            item("labels-only", &["tests"], None),
            item("notes", &[], Some("ship today")),
            item("optional", &[], None),
        ]))
    });
    let result = bench
        .execute(
            "ask-multi",
            json!({ "questions": [
                { "id": "targets", "question": "What should I update?", "options": [{ "label": "tests" }, { "label": "docs" }], "multi_select": true },
                { "id": "labels-only", "question": "Which labels should I keep?", "options": [{ "label": "tests" }, { "label": "docs" }], "multi_select": true },
                { "id": "notes", "question": "Any note?" },
                { "id": "optional", "question": "Anything else?" },
            ] }),
            None,
        )
        .await;
    assert_eq!(
        result,
        ok(concat!(
            r#"{"answers":[{"id":"targets","selected":["tests","docs"],"custom":"release notes"},"#,
            r#"{"id":"labels-only","selected":["tests"]},{"id":"notes","selected":[],"custom":"ship today"},"#,
            r#"{"id":"optional","selected":[]}]}"#
        ))
    );
}

// "passes optional header and a resumed runtime root through to the
// user-questions request"
#[tokio::test]
async fn a_header_and_a_resumed_root_pass_through() {
    let bench = setup(timed(-1)).await;
    let seen = bench.answer_with(|_| Ok(answers(vec![item("continue", &["ok"], None)])));
    bench
        .sessions
        .create_with(SessionHeader {
            delegation_depth: 1,
            ..SessionHeader::new(SessionId::new("resumed-root"), 0)
        })
        .unwrap();
    let agent = bench.enter("resumed-root");
    let result = bench
        .execute(
            "ask-3",
            json!({ "questions": [{ "id": "continue", "header": "Confirm", "question": "Continue?" }] }),
            Some(agent),
        )
        .await;
    assert_eq!(
        result,
        ok(r#"{"answers":[{"id":"continue","selected":["ok"]}]}"#)
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].questions,
        [AskUserQuestionItem {
            header: Some("Confirm".into()),
            ..AskUserQuestionItem::new("continue", "Continue?")
        }]
    );
    assert_eq!(
        seen[0].agent.as_ref().map(|agent| agent.id().as_str()),
        Some("resumed-root")
    );
}

// "reports a pending question when the Client settles the configured
// deadline"
#[tokio::test]
async fn a_client_timeout_reports_the_question_pending() {
    let bench = setup(timed(1)).await;
    let seen = bench.answer_with(|_| {
        Err(UserQuestionError::new(
            ASK_TIMED_OUT,
            "client countdown ended",
        ))
    });
    let agent = bench.live("timed-root");
    let settled = bench
        .execute("ask-timeout", continue_question(), Some(agent))
        .await;
    {
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].wait,
            Some(QuestionWait {
                call_id: "ask-timeout".into(),
                timed: true,
            })
        );
    }
    assert!(!settled.is_error);
    // One JSON object, instruction included.
    let payload: Value = serde_json::from_str(&settled.content).unwrap();
    assert_eq!(
        payload,
        json!({ "pending": true, "callId": "ask-timeout", "message": PENDING_NOTICE })
    );
    assert!(
        settled
            .content
            .starts_with(r#"{"pending":true,"callId":"ask-timeout","message":"#)
    );
    assert!(
        settled
            .content
            .contains("This is pending, not a skipped answer.")
    );
    assert!(
        settled
            .content
            .contains("No answer batch arrived before the timeout.")
    );
    assert!(settled.content.contains("Do not treat this as permission."));
    // The projection reads the same text, and keeps the question
    // answerable.
    assert!(is_timed_ask_user_question_schema(NAME, &bench.schema().1));
    let view = fold_user_questions(&[
        header_event(bench.header_tools()),
        asked_event(
            "ask-timeout",
            r#"{"questions":[{"id":"continue","question":"Continue?"}]}"#,
        ),
        resulted_event("ask-timeout", &settled.content),
    ]);
    assert_eq!(view.active.len(), 1);
    assert_eq!(view.active[0].call_id, "ask-timeout");
    assert_eq!(
        view.active[0].questions,
        [AskUserQuestionItem::new("continue", "Continue?")]
    );
    assert_eq!(view.active[0].state, UserQuestionState::Continued);
}

// "holds an unclaimed timed question until the deadline and then reports it
// pending"
#[tokio::test(start_paused = true)]
async fn an_unclaimed_question_is_pending_after_its_window() {
    let bench = setup(timed(1)).await;
    let agent = bench.live("timed-unclaimed");
    let started = tokio::time::Instant::now();
    let result = bench
        .execute("ask-unclaimed", continue_question(), Some(agent))
        .await;
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(1));
    let payload: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(payload["pending"], true);
    assert_eq!(payload["callId"], "ask-unclaimed");
}

// "requires a live agent for the timed path"
#[tokio::test]
async fn the_timed_path_needs_an_agent() {
    let bench = setup(AskUserConfig {
        mode: Mode::Timed,
        ..AskUserConfig::default()
    })
    .await;
    let result = bench
        .execute("ask-no-agent", continue_question(), None)
        .await;
    assert_eq!(
        result,
        ToolOutput::error("Error: timed questions require a live agent")
    );
}

// "rejects invalid timeout 0, 1.5 and 2147484". 1.5 is refused by the
// registry's check, the others by the tool's.
#[tokio::test]
async fn a_window_out_of_range_is_refused() {
    let bench = setup(timed(-1)).await;
    for (timeout, message) in [
        (
            json!(0),
            "Error: timeout must be -1 or a positive integer up to 2147483 seconds",
        ),
        (
            json!(1.5),
            "Error: invalid arguments: \"timeout\" must be an integer",
        ),
        (
            json!(2_147_484),
            "Error: timeout must be -1 or a positive integer up to 2147483 seconds",
        ),
    ] {
        let mut arguments = continue_question();
        arguments["timeout"] = timeout;
        let result = bench.execute("ask-invalid", arguments, None).await;
        assert_eq!(result, ToolOutput::error(message));
    }
}

// "rejects duplicate question ids before dispatching a timed call"
#[tokio::test]
async fn duplicate_ids_are_refused_before_asking() {
    let bench = setup(timed(-1)).await;
    let seen = bench.answer_with(|_| Ok(answers(Vec::new())));
    let result = bench
        .execute(
            "ask-duplicate-ids",
            json!({ "questions": [
                { "id": "scope", "question": "Choose a scope." },
                { "id": "scope", "question": "Confirm the scope." },
            ], "timeout": -1 }),
            None,
        )
        .await;
    assert_eq!(
        result,
        ToolOutput::error("Error: question id \"scope\" must be unique within this call")
    );
    assert!(seen.lock().unwrap().is_empty());
}

// "allows separate timed calls to reuse a question id"
#[tokio::test]
async fn separate_calls_may_reuse_an_id() {
    let bench = setup(timed(-1)).await;
    let seen = bench.answer_with(|_| Ok(answers(vec![item("scope", &["Tools"], None)])));
    for call_id in ["first-scope", "second-scope"] {
        let result = bench
            .execute(
                call_id,
                json!({ "questions": [{ "id": "scope", "question": "Choose a scope." }], "timeout": -1 }),
                None,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
}

// "returns structured user-questions errors through tool execution": the
// text dsh's tool layer gives.
#[tokio::test]
async fn a_service_error_is_the_tools_error() {
    let bench = setup(timed(-1)).await;
    let result = bench
        .execute("ask-no-provider", continue_question(), None)
        .await;
    assert_eq!(
        result,
        ToolOutput::error("Error: no user-questions answerer accepted the request")
    );
}

// "returns a structured error for empty question batches"
#[tokio::test]
async fn an_empty_batch_is_the_services_error() {
    let bench = setup(timed(-1)).await;
    let result = bench
        .execute("ask-empty", json!({ "questions": [] }), None)
        .await;
    assert_eq!(
        result,
        ToolOutput::error("Error: ask_user_question requires at least one question")
    );
}

// The config's window is checked when the tool is made, as dsh's
// `registerTimedAskUser` validates it.
#[tokio::test]
async fn a_default_window_out_of_range_fails_the_start() {
    let error = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(RegistryPlugin),
        Box::new(UserQuestionsPlugin),
        Box::new(AskUserPlugin::new(timed(0))),
    ])
    .await
    .err()
    .expect("the start fails");
    assert!(
        error
            .to_string()
            .contains("timeout must be -1 or a positive integer up to 2147483 seconds"),
        "{error}"
    );
}

// The arguments are checked by the registry, in dsh's words.
#[tokio::test]
async fn arguments_that_dont_fit_are_refused_by_the_registry() {
    let bench = setup(legacy()).await;
    let result = bench
        .execute("ask-bad", json!({ "questions": [{ "id": "x" }] }), None)
        .await;
    assert_eq!(
        result,
        ToolOutput::error(
            "Error: invalid arguments: missing required property \"questions[0].question\""
        )
    );
}
