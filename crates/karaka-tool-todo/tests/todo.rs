//! The `todo_write` tool, the `todo/write` event, the `todos` projection,
//! and the tool through the loop. (dsh: `tool-todo/tests/tool-todo.spec.ts`,
//! `projection.spec.ts`, `integration.spec.ts` and
//! `loader-composition.spec.ts`.)
//!
//! Not ported:
//! - `presentCall` (Karaka has no presentation layer);
//! - unregistering on fiber disposal, in `tool-todo.spec` and
//!   `projection.spec` (the kernel unloads no plugin);
//! - the namespace-export shape and the cordis.yml boot of
//!   `loader-composition.spec`, which are Cordis's; its config checks are
//!   kept through `TodoConfig`, and a missing or non-boolean flag can't be
//!   written in Rust;
//! - "isolates the log from later mutation", which an owned Rust value can't
//!   break.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{
    Agent, Agents, FrameListener, FrameListeners, FrameSubscription, InboxError, WhenIdle,
};
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_kernel::Runtime;
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, ToolCall, Usage, stream_of,
};
use karaka_session::{
    CancelCause, Entry, Session, SessionEvent, SessionId, SessionStore, SessionsPlugin,
    TurnEndReason, UserMessage, UserSource,
};
use karaka_session_projection::{SessionProjections, SessionProjectionsPlugin};
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_todo::{
    NAME, TodoConfig, TodoItem, TodoStatus, TodoToolPlugin, TodosProjection, description,
};
use karaka_tools::{ToolOutput, ToolRegistry, ToolsPlugin};
use serde_json::{Value, json};

/// An agent that only has an id; its session is in the store.
struct Owner {
    id: SessionId,
    frames: FrameListeners,
}

impl Agent for Owner {
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

struct Bench {
    tools: Arc<ToolRegistry>,
    sessions: Arc<SessionStore>,
    projections: Arc<SessionProjections>,
    _runtime: Runtime,
}

async fn setup(allow_parallel_in_progress: bool) -> Bench {
    bench(Some(allow_parallel_in_progress)).await
}

async fn bench(todo: Option<bool>) -> Bench {
    let mut plugins: Vec<Box<dyn karaka_kernel::DynPlugin>> = vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::default()),
        Box::new(ToolsPlugin),
    ];
    if let Some(allow_parallel_in_progress) = todo {
        plugins.push(Box::new(TodoToolPlugin::new(TodoConfig {
            allow_parallel_in_progress,
        })));
    }
    let runtime = Runtime::start(plugins).await.unwrap();
    Bench {
        tools: runtime.get::<ToolRegistry>(),
        sessions: runtime.get::<SessionStore>(),
        projections: runtime.get::<SessionProjections>(),
        _runtime: runtime,
    }
}

static CALLS: AtomicUsize = AtomicUsize::new(0);

impl Bench {
    /// An agent whose session `id` is in the store.
    fn owner(&self, id: &str) -> Arc<dyn Agent> {
        let id = SessionId::new(id);
        if self.sessions.get(&id).is_none() {
            self.sessions.create(id.clone()).unwrap();
        }
        Arc::new(Owner {
            id,
            frames: FrameListeners::new(),
        })
    }

    async fn call(&self, arguments: Value, agent: Option<Arc<dyn Agent>>) -> ToolOutput {
        let call = ToolCall {
            id: format!("call-{}", CALLS.fetch_add(1, Ordering::SeqCst)),
            name: NAME.into(),
            arguments: arguments.to_string(),
        };
        self.tools.execute(&call, agent).await
    }

    async fn write(&self, arguments: Value) -> ToolOutput {
        self.call(arguments, Some(self.owner("parent-1"))).await
    }

    fn session(&self, id: &str) -> Session {
        self.sessions.get(&SessionId::new(id)).unwrap()
    }

    fn todos_of(&self, session: &Session) -> Value {
        self.projections.snapshot(session, None).values["todos"].clone()
    }
}

/// The lists every `todo/write` logged, in order.
fn writes(session: &Session) -> Vec<Value> {
    session
        .events()
        .iter()
        .filter(|entry| entry.event.event_type() == "todo/write")
        .map(|entry| serde_json::to_value(entry).unwrap()["data"]["todos"].clone())
        .collect()
}

fn item(content: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        content: content.into(),
        status,
    }
}

fn write_event(todos: Value) -> SessionEvent {
    SessionEvent::Passthrough(karaka_session::Passthrough {
        event_type: "todo/write".into(),
        data: json!({ "todos": todos }),
        surface_op: None,
        source_event_seqs: None,
    })
}

// "registers a `todo_write` tool whose schema is an array of
// {content,status}", to the byte: dsh's `defineTool` compiles each
// `required: true` into its parent's `required`, keeps the item's
// `additionalProperties: false`, and leaves the root open.
#[tokio::test]
async fn the_tool_has_dshs_exact_schema() {
    let bench = setup(true).await;
    let schema = bench
        .tools
        .schemas()
        .into_iter()
        .find(|schema| schema.name == "todo_write")
        .expect("the tool is registered");
    assert_eq!(
        schema.parameters,
        concat!(
            r#"{"type":"object","properties":{"todos":{"type":"array","#,
            r#""description":"The COMPLETE task list, replacing any previous list.","#,
            r#""items":{"type":"object","additionalProperties":false,"properties":{"#,
            r#""content":{"type":"string","description":"What the task is — a short imperative line."},"#,
            r#""status":{"type":"string","#,
            r#""description":"pending (not started) | in_progress (now) | completed (done).","#,
            r#""enum":["pending","in_progress","completed"]}},"#,
            r#""required":["content","status"]}}},"required":["todos"]}"#
        )
    );
}

// "instructs the model to keep at most one active, while true instructs
// parallel", and the descriptions whole.
#[tokio::test]
async fn the_description_follows_the_parallel_flag() {
    let head = "Record and update a task list to plan multi-step work and show progress; skip \
                it for trivial single-step tasks. Add one todo per concrete step before you \
                start. ";
    let tail = "Mark each todo `completed` as soon as it is done.";
    assert_eq!(
        description(false),
        format!("{head}While work remains, keep exactly one todo `in_progress`. {tail}")
    );
    assert_eq!(
        description(true),
        format!(
            "{head}While work remains, keep the todos being worked on `in_progress`, several \
             only when work runs in parallel. {tail}"
        )
    );
    for (allow, wanted, unwanted) in [
        (false, "keep exactly one todo `in_progress`", "several only"),
        (
            true,
            "several only when work runs in parallel",
            "exactly one",
        ),
    ] {
        let bench = setup(allow).await;
        let schema = bench
            .tools
            .schemas()
            .into_iter()
            .find(|schema| schema.name == "todo_write")
            .unwrap();
        assert!(schema.description.contains(wanted));
        assert!(!schema.description.contains(unwanted));
    }
}

// "appends a todo/write event carrying the whole list to the calling
// session"
#[tokio::test]
async fn a_write_logs_the_whole_list_and_counts_it() {
    let bench = setup(true).await;
    let todos = json!([
        { "content": "plan", "status": "in_progress" },
        { "content": "build", "status": "pending" },
    ]);
    let output = bench
        .call(json!({ "todos": todos }), Some(bench.owner("writer")))
        .await;
    assert_eq!(
        output,
        ToolOutput::ok("Updated todo list: 1 pending, 1 in progress, 0 completed.")
    );
    assert_eq!(writes(&bench.session("writer")), [todos]);
}

// "stores the trimmed content (the dedupe/length key), not the raw input"
#[tokio::test]
async fn the_content_is_stored_trimmed() {
    let bench = setup(true).await;
    let output = bench
        .call(
            json!({ "todos": [{ "content": "  plan the work  ", "status": "pending" }] }),
            Some(bench.owner("trim")),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(
        writes(&bench.session("trim")),
        [json!([{ "content": "plan the work", "status": "pending" }])]
    );
}

// "replaces the list on a second call (last-write-wins on the log)"
#[tokio::test]
async fn a_second_write_replaces_the_list() {
    let bench = setup(true).await;
    let owner = bench.owner("writer-2");
    bench
        .call(
            json!({ "todos": [{ "content": "a", "status": "pending" }] }),
            Some(Arc::clone(&owner)),
        )
        .await;
    let second = json!([
        { "content": "a", "status": "completed" },
        { "content": "b", "status": "in_progress" },
    ]);
    bench.call(json!({ "todos": second }), Some(owner)).await;
    let session = bench.session("writer-2");
    assert_eq!(writes(&session).last(), Some(&second));
    assert_eq!(bench.todos_of(&session), second);
}

// "rejects a malformed status before execute runs", "rejects a non-array
// todos argument", "unknown item keys": dsh's registry checks the arguments
// against the schema first, in its validator's words.
#[tokio::test]
async fn arguments_that_dont_fit_the_schema_are_refused_in_dshs_words() {
    let bench = setup(true).await;
    for (arguments, message) in [
        (json!({}), r#"missing required property "todos""#),
        (json!({ "todos": "nope" }), r#""todos" must be an array"#),
        (json!({ "todos": null }), r#""todos" must be an array"#),
        (
            json!({ "todos": [{ "content": "x", "status": "doing" }] }),
            r#""todos[0].status" must be one of ["pending","in_progress","completed"]"#,
        ),
        (
            json!({ "todos": [{ "content": "a", "status": "pending", "children": [] }] }),
            r#""todos[0].children" is not a declared property (additionalProperties: false)"#,
        ),
        (json!({ "todos": ["a"] }), r#""todos[0]" must be an object"#),
        (
            json!({ "todos": [{ "content": 1, "status": "pending" }] }),
            r#""todos[0].content" must be a string"#,
        ),
        // Missing keys first, then each declared property's, then the
        // undeclared ones, as dsh's walk orders them.
        (
            json!({ "todos": [
                { "extra": 1, "status": 7 },
                { "content": "b", "status": "pending" },
            ] }),
            concat!(
                r#"missing required property "todos[0].content"; "#,
                r#""todos[0].status" must be a string; "#,
                r#""todos[0].extra" is not a declared property (additionalProperties: false)"#
            ),
        ),
    ] {
        let output = bench.write(arguments.clone()).await;
        assert_eq!(
            output,
            ToolOutput::error(format!("Error: invalid arguments: {message}")),
            "{arguments}"
        );
    }
    // The root is open: other keys are ignored.
    let output = bench.write(json!({ "todos": [], "note": "ignored" })).await;
    assert_eq!(
        output,
        ToolOutput::ok("Updated todo list: 0 pending, 0 in progress, 0 completed.")
    );
    assert_eq!(writes(&bench.session("parent-1")).len(), 1);
}

// "accepts several in_progress items at once (parallel work)", and the
// `allowParallelInProgress` cases, with loader-composition's.
#[tokio::test]
async fn the_parallel_flag_decides_how_many_may_be_in_progress() {
    let parallel = json!([
        { "content": "run subagent a", "status": "in_progress" },
        { "content": "run subagent b", "status": "in_progress" },
    ]);
    let single = setup(false).await;
    let output = single
        .call(
            json!({ "todos": parallel }),
            Some(single.owner("single-active")),
        )
        .await;
    assert_eq!(
        output,
        ToolOutput::error("Error: invalid todos: at most one task may be in_progress (got 2)")
    );
    assert!(writes(&single.session("single-active")).is_empty());
    let one = single
        .write(json!({ "todos": [
            { "content": "run subagent a", "status": "in_progress" },
            { "content": "run subagent b", "status": "pending" },
        ] }))
        .await;
    assert!(!one.is_error);

    let concurrent = setup(true).await;
    let mut three = parallel.as_array().unwrap().clone();
    three.push(json!({ "content": "merge results", "status": "pending" }));
    let output = concurrent
        .call(
            json!({ "todos": three }),
            Some(concurrent.owner("parallel")),
        )
        .await;
    assert_eq!(
        output,
        ToolOutput::ok("Updated todo list: 1 pending, 2 in progress, 0 completed.")
    );
    assert_eq!(
        writes(&concurrent.session("parallel")),
        [Value::Array(three)]
    );
}

// "rejects empty content / duplicate content as an isError result"
#[tokio::test]
async fn empty_and_duplicate_content_are_refused() {
    let bench = setup(true).await;
    assert_eq!(
        bench
            .write(json!({ "todos": [{ "content": "   ", "status": "pending" }] }))
            .await,
        ToolOutput::error("Error: invalid todo: `content` must be a non-empty string")
    );
    assert_eq!(
        bench
            .write(json!({ "todos": [
                { "content": "dup", "status": "pending" },
                { "content": " dup ", "status": "completed" },
            ] }))
            .await,
        ToolOutput::error("Error: invalid todos: duplicate content \"dup\"")
    );
    assert!(writes(&bench.session("parent-1")).is_empty());
}

// "rejects a non-agent caller (the list has no owning session)". The list
// is checked first, as dsh checks it before the agent.
#[tokio::test]
async fn a_call_without_an_agent_is_refused_after_the_list_is_checked() {
    let bench = setup(true).await;
    assert_eq!(
        bench
            .call(
                json!({ "todos": [{ "content": "a", "status": "pending" }] }),
                None
            )
            .await,
        ToolOutput::error("Error: todo_write requires an owning agent session")
    );
    assert_eq!(
        bench
            .call(
                json!({ "todos": [{ "content": " ", "status": "pending" }] }),
                None
            )
            .await,
        ToolOutput::error("Error: invalid todo: `content` must be a non-empty string")
    );
}

// todo/write event: "does not add a derived message or surface node", and
// "round-trips through a seeded replay identically without surface
// metadata"
#[tokio::test]
async fn a_write_is_log_only_and_round_trips() {
    let bench = setup(true).await;
    let session = bench.sessions.create(SessionId::new("t3")).unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    session
        .append(SessionEvent::user(UserMessage::text(
            "q",
            UserSource::user(),
            "q",
        )))
        .unwrap();
    let messages = session.messages().len();
    let surface = session.surface_nodes();
    let seq = session
        .append(write_event(
            json!([{ "content": "a task", "status": "pending" }]),
        ))
        .unwrap();
    assert_eq!(session.messages().len(), messages);
    assert_eq!(session.surface_nodes(), surface);
    assert!(!session.surface_nodes().contains(&seq));
    session
        .append(SessionEvent::turn_end(1, TurnEndReason::Completed))
        .unwrap();

    let stored: Vec<Entry> = session
        .events()
        .iter()
        .map(|entry| serde_json::from_str(&serde_json::to_string(entry).unwrap()).unwrap())
        .collect();
    assert_eq!(stored, session.events());
    let written = serde_json::to_value(&stored[2]).unwrap();
    assert_eq!(
        written,
        json!({ "type": "todo/write", "seq": 2, "time": written["time"],
                "data": { "todos": [{ "content": "a task", "status": "pending" }] } })
    );
    let replayed = bench
        .sessions
        .restore(SessionId::new("t3-replay"), stored)
        .unwrap();
    assert_eq!(writes(&replayed), writes(&session));
    assert_eq!(replayed.events(), session.events());
}

// projection.spec: "serves null before the first todo/write"
#[tokio::test]
async fn the_list_is_null_before_the_first_write() {
    let bench = setup(true).await;
    let session = bench.sessions.create(SessionId::new("bench")).unwrap();
    session
        .append(SessionEvent::user(UserMessage::text(
            "hi",
            UserSource::user(),
            "hi",
        )))
        .unwrap();
    let snapshot = bench.projections.snapshot(&session, None);
    assert_eq!(snapshot.values["todos"], Value::Null);
    assert_eq!(
        snapshot.as_of_seq.map(|seq| seq.0),
        Some(session.len() as u64 - 1)
    );
}

// "serves the latest whole list after writes, asOfSeq = last event seq",
// and "clears the standing plan on the next turn/start (turn/end keeps it)"
#[tokio::test]
async fn the_list_is_the_latest_write_until_the_next_turn_starts() {
    let bench = setup(true).await;
    let session = bench.sessions.create(SessionId::new("bench")).unwrap();
    let first = json!([{ "content": "a", "status": "pending" }]);
    let second = json!([
        { "content": "a", "status": "completed" },
        { "content": "b", "status": "in_progress" },
    ]);
    session.append(SessionEvent::turn_start(1)).unwrap();
    session.append(write_event(first)).unwrap();
    session.append(write_event(second.clone())).unwrap();
    let snapshot = bench.projections.snapshot(&session, None);
    assert_eq!(snapshot.values["todos"], second);
    assert_eq!(
        snapshot.as_of_seq.map(|seq| seq.0),
        Some(session.len() as u64 - 1)
    );
    session
        .append(SessionEvent::turn_end(1, TurnEndReason::Completed))
        .unwrap();
    assert_eq!(bench.todos_of(&session), second);
    session.append(SessionEvent::turn_start(2)).unwrap();
    assert_eq!(bench.todos_of(&session), Value::Null);
    let state = bench
        .projections
        .state_of::<TodosProjection>(&session)
        .unwrap();
    assert_eq!(*state, None);
}

// "has no todos key when tool-todo is not composed"
#[tokio::test]
async fn without_the_tool_there_is_no_todos_key() {
    let bench = bench(None).await;
    let session = bench.sessions.create(SessionId::new("bench")).unwrap();
    session
        .append(SessionEvent::user(UserMessage::text(
            "hi",
            UserSource::user(),
            "hi",
        )))
        .unwrap();
    let snapshot = bench.projections.snapshot(&session, None);
    assert!(!snapshot.values.contains_key("todos"));
}

#[test]
fn an_item_reads_and_writes_as_dshs() {
    let parsed: Vec<TodoItem> = serde_json::from_value(json!([
        { "content": "a", "status": "pending" },
        { "content": "b", "status": "in_progress" },
        { "content": "c", "status": "completed" },
    ]))
    .unwrap();
    assert_eq!(
        parsed,
        [
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::InProgress),
            item("c", TodoStatus::Completed),
        ]
    );
}

// The loop.

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

fn todo_call(id: &str, todos: Value) -> Block {
    Block::ToolCall(ToolCall {
        id: id.into(),
        name: NAME.into(),
        arguments: json!({ "todos": todos }).to_string(),
    })
}

async fn looped(
    replies: Vec<Vec<Block>>,
) -> (
    Arc<SessionStore>,
    Arc<SessionProjections>,
    Arc<dyn Agent>,
    Runtime,
) {
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new("mock-work", "mock"))),
        Box::new(TodoToolPlugin::new(TodoConfig {
            allow_parallel_in_progress: true,
        })),
    ])
    .await
    .unwrap();
    runtime
        .get::<ModelRegistry>()
        .register(
            "mock-work",
            Arc::new(Scripted {
                replies: Mutex::new(replies.into()),
            }) as Arc<dyn Model>,
        )
        .unwrap();
    let agent = runtime
        .get::<dyn Agents>()
        .open(SessionId::new("it-todo"))
        .unwrap();
    (
        runtime.get::<SessionStore>(),
        runtime.get::<SessionProjections>(),
        agent,
        runtime,
    )
}

async fn say(agent: &dyn Agent, text: &str) {
    agent
        .followup(UserMessage::text(
            format!("user-{text}"),
            UserSource::user(),
            text,
        ))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), agent.when_idle())
        .await
        .unwrap();
}

// integration.spec: "model calls todo_write: a tool/call, a non-error
// tool/result, and a todo/write snapshot land"; and the projection holds
// the list until the next turn's `turn/start` clears it.
#[tokio::test]
async fn a_model_plans_with_todo_write_and_the_next_turn_clears_it() {
    let todos = json!([
        { "content": "read the code", "status": "in_progress" },
        { "content": "write the fix", "status": "pending" },
    ]);
    let (sessions, projections, agent, _runtime) = looped(vec![
        vec![
            Block::Text("Planning the work.".into()),
            todo_call("call-1", todos.clone()),
        ],
        vec![Block::Text("Plan recorded.".into())],
        vec![Block::Text("Next.".into())],
    ])
    .await;
    say(agent.as_ref(), "plan a two-step task").await;

    let session = sessions.get(agent.id()).unwrap();
    let events = session.events();
    let call = events
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::ToolCall(call) => Some(call.name.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(call, "todo_write");
    let result = events
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::ToolResult { data, .. } => Some(data.message.to_model()),
            _ => None,
        })
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        "Updated todo list: 1 pending, 1 in progress, 0 completed."
    );
    assert_eq!(writes(&session), std::slice::from_ref(&todos));
    // The write is logged after the call started and before its result.
    let at = |kind: &str| {
        events
            .iter()
            .position(|entry| entry.event.event_type() == kind)
            .unwrap()
    };
    assert!(at("tool/call") < at("todo/write") && at("todo/write") < at("tool/result"));
    assert_eq!(projections.snapshot(&session, None).values["todos"], todos);

    say(agent.as_ref(), "next").await;
    assert_eq!(
        projections.snapshot(&session, None).values["todos"],
        Value::Null
    );
}

// "a second todo_write replaces the list (last-write-wins on the log)"
#[tokio::test]
async fn a_second_write_in_the_loop_replaces_the_list() {
    let second = json!([
        { "content": "step one", "status": "completed" },
        { "content": "step two", "status": "in_progress" },
    ]);
    let (sessions, projections, agent, _runtime) = looped(vec![
        vec![todo_call(
            "call-1",
            json!([{ "content": "step one", "status": "in_progress" }]),
        )],
        vec![todo_call("call-2", second.clone())],
        vec![Block::Text("Done planning.".into())],
    ])
    .await;
    say(agent.as_ref(), "plan then update").await;
    let session = sessions.get(agent.id()).unwrap();
    assert_eq!(writes(&session).len(), 2);
    assert_eq!(writes(&session).last(), Some(&second));
    assert_eq!(projections.snapshot(&session, None).values["todos"], second);
}
