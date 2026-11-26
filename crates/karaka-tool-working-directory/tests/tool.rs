//! The `working_directory` tool, through the registry and through the loop.
//! (dsh: "exposes reading and changing through the same model tool" in
//! `working-directory/tests/working-directory.spec.ts`, without its
//! unregistering on disposal: the kernel unloads no plugin.)

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{Agent, Agents};
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_fs_local::{LocalFsConfig, LocalFsPlugin};
use karaka_kernel::Runtime;
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Message, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, Role, ToolCall, Usage, stream_of,
};
use karaka_session::{
    SessionEvent, SessionHeader, SessionId, SessionStore, SessionsPlugin, UserMessage, UserSource,
};
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_working_directory::{DESCRIPTION, NAME, WorkingDirectoryToolPlugin};
use karaka_tools::{ToolOutput, ToolRegistry, ToolsPlugin};
use karaka_working_directory::WorkingDirectoryPlugin;
use serde_json::{Value, json};

const ROUTE: &str = "mock-work";

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "karaka-tool-working-directory-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn at(&self, relative: &str) -> String {
        self.0.join(relative).to_str().unwrap().to_owned()
    }

    fn root(&self) -> String {
        self.0.to_str().unwrap().to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Plays one scripted reply per call, and keeps every request.
struct Scripted {
    replies: Mutex<VecDeque<Vec<Block>>>,
    requests: Mutex<Vec<Request>>,
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        "mock"
    }

    fn stream(&self, request: Request) -> EventStream {
        self.requests.lock().unwrap().push(request);
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

struct Fixture {
    dir: TempDir,
    tools: Arc<ToolRegistry>,
    agents: Arc<dyn Agents>,
    sessions: Arc<SessionStore>,
    model: Arc<Scripted>,
    _runtime: Runtime,
}

async fn fixture(replies: Vec<Vec<Block>>) -> Fixture {
    let dir = TempDir::new();
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig {
            include_harness_identity: false,
            ..SystemPromptConfig::default()
        })),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new(ROUTE, "mock-model"))),
        Box::new(LocalFsPlugin::new(LocalFsConfig {
            cwd: dir.root(),
            ..LocalFsConfig::default()
        })),
        Box::new(WorkingDirectoryPlugin::default()),
        Box::new(WorkingDirectoryToolPlugin),
    ])
    .await
    .unwrap();
    let model = Arc::new(Scripted {
        replies: Mutex::new(replies.into()),
        requests: Mutex::new(Vec::new()),
    });
    runtime
        .get::<ModelRegistry>()
        .register(ROUTE, Arc::clone(&model) as Arc<dyn Model>)
        .unwrap();
    Fixture {
        tools: runtime.get::<ToolRegistry>(),
        agents: runtime.get::<dyn Agents>(),
        sessions: runtime.get::<SessionStore>(),
        model,
        dir,
        _runtime: runtime,
    }
}

impl Fixture {
    fn agent(&self) -> Arc<dyn Agent> {
        let id = SessionId::new("directory-owner");
        if self.sessions.get(&id).is_none() {
            self.sessions
                .create_with(SessionHeader::new(id.clone(), 1).with_cwd(self.dir.root()))
                .unwrap();
        }
        self.agents.open(id).unwrap()
    }

    async fn call(&self, arguments: Value, agent: Option<Arc<dyn Agent>>) -> ToolOutput {
        let call = ToolCall {
            id: "directory-call".into(),
            name: NAME.into(),
            arguments: arguments.to_string(),
        };
        self.tools.execute(&call, agent).await
    }
}

// dsh's exact schema: `defineTool` compiles `cd` into an open object root,
// with no `required` and no `additionalProperties`.
#[tokio::test]
async fn the_tool_is_described_as_dshs_is() {
    let f = fixture(Vec::new()).await;
    let schema = f
        .tools
        .schemas()
        .into_iter()
        .find(|schema| schema.name == "working_directory")
        .expect("the tool is registered");
    assert_eq!(
        schema.description,
        "Read the current working directory, or change it with cd. Relative paths use the \
         current directory. Existing shells and running processes keep their own directories."
    );
    assert_eq!(schema.description, DESCRIPTION);
    assert_eq!(
        schema.parameters,
        r#"{"type":"object","properties":{"cd":{"type":"string","description":"Existing directory to enter. Omit to read the current directory."}}}"#
    );
}

// "exposes reading and changing through the same model tool"
#[tokio::test]
async fn the_tool_reads_and_changes_the_directory() {
    let f = fixture(Vec::new()).await;
    std::fs::create_dir(f.dir.0.join("child")).unwrap();
    let agent = f.agent();
    assert_eq!(
        f.call(json!({}), Some(Arc::clone(&agent))).await,
        ToolOutput::ok(f.dir.root())
    );
    assert_eq!(
        f.call(json!({ "cd": "child" }), Some(Arc::clone(&agent)))
            .await,
        ToolOutput::ok(f.dir.at("child"))
    );
    assert_eq!(
        f.call(json!({}), None).await,
        ToolOutput::error("Error: working_directory requires an agent Session")
    );
}

// dsh's typed tools refuse arguments that don't fit their schema with a
// `ToolArgsError`, shown as `Error: <message>`; a thrown error is shown the
// same way.
#[tokio::test]
async fn bad_arguments_and_failures_are_error_results() {
    let f = fixture(Vec::new()).await;
    let agent = f.agent();
    assert_eq!(
        f.call(json!({ "cd": 7 }), Some(Arc::clone(&agent))).await,
        ToolOutput::error("Error: invalid arguments: \"cd\" must be a string")
    );
    assert_eq!(
        f.call(json!({ "cd": "missing" }), Some(Arc::clone(&agent)))
            .await,
        ToolOutput::error("Error: working-directory: directory does not exist: \"missing\"")
    );
    assert_eq!(
        f.call(json!({ "cd": "" }), Some(Arc::clone(&agent))).await,
        ToolOutput::error("Error: working-directory: cd must not be empty")
    );
    // The object is open, as dsh's root is: other keys are ignored.
    assert_eq!(
        f.call(json!({ "other": true }), Some(agent)).await,
        ToolOutput::ok(f.dir.root())
    );
}

fn text_of(message: &Message) -> String {
    message.text()
}

// End to end: the model changes the directory with the tool; the change is
// logged, its notice reaches the next step's request as a user message,
// and that step's runtime context shows the new directory.
#[tokio::test]
async fn a_model_changes_the_directory_and_its_next_step_sees_it() {
    let f = fixture(vec![
        vec![Block::ToolCall(ToolCall {
            id: "c1".into(),
            name: NAME.into(),
            arguments: r#"{"cd":"child"}"#.into(),
        })],
        vec![Block::Text("moved".into())],
    ])
    .await;
    std::fs::create_dir(f.dir.0.join("child")).unwrap();
    let child = f.dir.at("child");
    let agent = f.agent();
    agent
        .followup(UserMessage::text(
            "prompt",
            UserSource::user(),
            "go to child",
        ))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), agent.when_idle())
        .await
        .unwrap();

    let session = f.sessions.get(agent.id()).unwrap();
    let changes: Vec<Value> = session
        .events()
        .iter()
        .filter(|entry| entry.event.event_type() == "working-directory/change")
        .map(|entry| serde_json::to_value(entry).unwrap()["data"].clone())
        .collect();
    assert_eq!(changes, [json!({ "cwd": child })]);
    let result = session
        .events()
        .into_iter()
        .find_map(|entry| match entry.event {
            SessionEvent::ToolResult { data, .. } => Some(data.message.to_model().content),
            _ => None,
        })
        .unwrap();
    assert_eq!(result, child);

    let requests = f.model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let quoted = |path: &str| serde_json::to_string(path).unwrap();
    let notice = format!(
        "The working directory changed from {} to {}.",
        quoted(&f.dir.root()),
        quoted(&child)
    );
    let second = &requests[1].messages;
    let tool_at = second
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, Block::ToolResult(_)))
        })
        .unwrap();
    let notice_at = second
        .iter()
        .position(|message| message.role == Role::User && text_of(message).contains(&notice))
        .expect("the notice reaches the next request");
    assert!(notice_at > tool_at);
    let context = second
        .iter()
        .rev()
        .map(text_of)
        .find(|text| text.starts_with("Current runtime context."))
        .unwrap();
    assert!(
        context.contains(&format!("Current working directory: {}.", quoted(&child))),
        "{context}"
    );
    // The first request showed the project.
    let first = requests[0]
        .messages
        .iter()
        .rev()
        .map(text_of)
        .find(|text| text.starts_with("Current runtime context."))
        .unwrap();
    assert!(first.contains(&format!(
        "Current working directory: {}.",
        quoted(&f.dir.root())
    )));
}
