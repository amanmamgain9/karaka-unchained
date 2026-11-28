//! The `skill` tool, the session catalog and `/name` invocations, on a real
//! runtime with the filesystem provider over temp homes. (dsh:
//! `tool-skill/tests/tool-skill.spec.ts`.)
//!
//! dsh's `fireStep` runs the pre-step waterfall with no claimed messages and
//! appends what enters; `proposeStep` runs it over given messages. Here both
//! run the [`PreStepHooks`] the loop provides, whose end of the chain enters
//! with the claimed messages, as dsh's tests' does. Two tests at the end go
//! through the loop and a scripted model.
//!
//! Not ported:
//!
//! - "registers the skill tool schema and removes it on dispose", its
//!   dispose half and its `presentCall`: the kernel unloads no plugin, and
//!   Karaka's tools have no call presentation yet. The schema half is
//!   `the_tool_is_described_as_dshs_is`.
//! - "forwards the step abort signal to skill discovery": a pre-step has a
//!   cancel switch, not an abort signal, and the loop drops the chain on a
//!   cancel instead of signalling it.
//! - "rejects a missing event below the current Session length": a Karaka
//!   session's events are a list with no holes.
//! - "rejects an unknown resource-base kind at the canonical output
//!   boundary": a resource base is a Rust enum, so no unknown kind reaches
//!   the tool.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{
    Agent, Agents, PreStep, PreStepDecision, PreStepFuture, PreStepHooks, PreStepListener,
    PreStepNext, cancellation,
};
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_fs_local::{LocalFsConfig, LocalFsPlugin};
use karaka_kernel::Runtime;
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Message, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, Role, ToolCall, Usage, stream_of,
};
use karaka_scope::ScopeKey;
use karaka_session::{
    ContentBlock, Extra, ReasoningBlock, Seq, Session, SessionEvent, SessionHeader, SessionId,
    SessionStore, SessionsPlugin, SurfaceIntent, SurfaceOp, UserMessage, UserSource,
};
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_skill::{
    GetFuture, ListFuture, SkillCandidate, SkillDefinition, SkillInvocationPolicy,
    SkillLookupOptions, SkillProvider, SkillProviderControl, SkillProviderObservation,
    SkillRegistration, SkillRegistry, SkillResourceBase, SkillsPlugin,
};
use karaka_skill_filesystem::{SkillFilesystemConfig, SkillFilesystemPlugin};
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_skill::{
    CATALOG_KIND, DESCRIPTION, NAME, PARAMETERS, ToolSkillConfig, ToolSkillPlugin,
};
use karaka_tools::{
    Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRestriction, ToolRun, ToolsPlugin,
};
use karaka_working_directory::WorkingDirectoryPlugin;
use serde_json::{Value, json};

const ROUTE: &str = "mock-skill";

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "karaka-tool-skill-{}-{}-{}",
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

/// dsh's `writeSkill`, with optional policy lines in the frontmatter.
fn write_skill(root: &str, name: &str, description: &str, policy: &str, body: &str) {
    let dir = std::path::Path::new(root).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let policy = if policy.is_empty() {
        String::new()
    } else {
        format!("{policy}\n")
    };
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n{policy}---\n\n{body}\n"),
    )
    .unwrap();
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
    home: TempDir,
    tools: Arc<ToolRegistry>,
    agents: Arc<dyn Agents>,
    sessions: Arc<SessionStore>,
    skills: Arc<SkillRegistry>,
    hooks: Arc<PreStepHooks>,
    model: Arc<Scripted>,
    _runtime: Runtime,
}

/// The plugins of dsh's `setup`, around a loop: the filesystem provider's
/// homes are in `home`, and it doesn't watch.
fn plugins(home: &TempDir, config: ToolSkillConfig) -> Vec<Box<dyn karaka_kernel::DynPlugin>> {
    vec![
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
            cwd: home.root(),
            ..LocalFsConfig::default()
        })),
        Box::new(WorkingDirectoryPlugin::default()),
        Box::new(SkillsPlugin::default()),
        Box::new(SkillFilesystemPlugin::new(SkillFilesystemConfig {
            dsh_home: Some(home.at(".dsh")),
            agents_home: Some(home.at(".agents")),
            bundled_skill_dir: Some(home.at("bundled")),
            watch: false,
            ..SkillFilesystemConfig::default()
        })),
        Box::new(ToolSkillPlugin::new(config)),
    ]
}

async fn fixture_in(home: TempDir, config: ToolSkillConfig, replies: Vec<Vec<Block>>) -> Fixture {
    let runtime = Runtime::start(plugins(&home, config)).await.unwrap();
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
        skills: runtime.get::<SkillRegistry>(),
        hooks: runtime.get::<PreStepHooks>(),
        model,
        home,
        _runtime: runtime,
    }
}

async fn fixture() -> Fixture {
    fixture_in(TempDir::new(), ToolSkillConfig::default(), Vec::new()).await
}

impl Fixture {
    /// An agent whose session is `id`, working in `cwd`. (dsh:
    /// `agentForCwd` and `sessionAgent`.)
    fn agent_in(&self, id: &str, cwd: &str) -> Arc<dyn Agent> {
        let id = SessionId::new(id);
        if self.sessions.get(&id).is_none() {
            self.sessions
                .create_with(SessionHeader::new(id.clone(), 1).with_cwd(cwd))
                .unwrap();
        }
        self.agents.open(id).unwrap()
    }

    /// An agent working in the home.
    fn agent(&self, id: &str) -> Arc<dyn Agent> {
        let cwd = self.home.root();
        self.agent_in(id, &cwd)
    }

    fn session(&self, agent: &Arc<dyn Agent>) -> Session {
        self.sessions.get(agent.id()).unwrap()
    }

    /// A runtime skill. (dsh: `ctx.skills.register`.)
    fn register(&self, name: &str, description: &str, content: &str) -> karaka_skill::SkillHandle {
        self.skills
            .register(
                None,
                SkillRegistration::new(name, description, "runtime", content),
            )
            .unwrap()
    }

    /// dsh's `proposeStep`: the chain over `messages`.
    async fn propose(&self, agent: &Arc<dyn Agent>, messages: Vec<UserMessage>) -> PreStepDecision {
        let (_switch, cancel) = cancellation();
        let step = PreStep {
            agent: Arc::clone(agent),
            messages,
            turn: 1,
            step: 1,
            cancel,
        };
        self.hooks.run(&step).await.unwrap()
    }

    /// dsh's `fireStep`: the chain over no messages, and what enters is
    /// appended to the session. It gives what entered.
    async fn fire_step(&self, agent: &Arc<dyn Agent>) -> Vec<UserMessage> {
        let PreStepDecision::Enter { messages } = self.propose(agent, Vec::new()).await else {
            return Vec::new();
        };
        let session = self.session(agent);
        for message in &messages {
            session.append(SessionEvent::user(message.clone())).unwrap();
        }
        messages
    }

    /// dsh's `catalogMessages`.
    fn catalogs(&self, agent: &Arc<dyn Agent>) -> Vec<(Seq, UserMessage)> {
        self.session(agent)
            .events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                SessionEvent::UserMessage { message, .. } if kind(&message) == CATALOG_KIND => {
                    Some((entry.seq, message))
                }
                _ => None,
            })
            .collect()
    }

    async fn call(&self, name: &str, agent: Option<Arc<dyn Agent>>) -> ToolOutput {
        let call = ToolCall {
            id: "skill-call".into(),
            name: NAME.into(),
            arguments: json!({ "name": name }).to_string(),
        };
        self.tools.execute(&call, agent).await
    }
}

/// dsh's `openMessageTurn`.
fn open_message_turn(session: &Session) {
    session.append(SessionEvent::turn_start(1)).unwrap();
    session
        .append(SessionEvent::user(prompt("turn 1")))
        .unwrap();
}

fn prompt(text: &str) -> UserMessage {
    UserMessage::text(format!("prompt-{text}"), UserSource::user(), text)
}

fn kind(message: &UserMessage) -> String {
    match &message.source {
        UserSource::User(_) => "user".into(),
        UserSource::RuntimeContext(_) => "runtime-context".into(),
        UserSource::Other(source) => source["kind"].as_str().unwrap_or_default().into(),
    }
}

fn text(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn source(message: &UserMessage) -> Value {
    serde_json::to_value(&message.source).unwrap()
}

fn entered(decision: PreStepDecision) -> Vec<UserMessage> {
    match decision {
        PreStepDecision::Enter { messages } => messages,
        PreStepDecision::Reject => panic!("expected the step to enter"),
    }
}

/// A catalog message as a seed or a proposal holds it. (dsh:
/// `catalogContent` and its source.)
fn catalog_with(id: &str, entries: Value, lines: &[&str]) -> UserMessage {
    let text = ["<system-reminder>", "<available_skills>"]
        .into_iter()
        .chain(lines.iter().copied())
        .chain(["</available_skills>", "</system-reminder>"])
        .collect::<Vec<_>>()
        .join("\n");
    UserMessage::text(
        id,
        UserSource::Other(json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": entries })),
        text,
    )
}

/// Adds a message after whatever the rest of the chain enters with, as
/// dsh's test registers a `later-contribution` listener.
struct LaterContribution;

impl PreStepListener for LaterContribution {
    fn decide<'a>(&'a self, _: &'a PreStep, next: PreStepNext<'a>) -> PreStepFuture<'a> {
        Box::pin(async move {
            let PreStepDecision::Enter { mut messages } = next.run().await? else {
                return Ok(PreStepDecision::Reject);
            };
            messages.push(UserMessage::text(
                "later",
                UserSource::Other(json!({ "kind": "later-contribution" })),
                "later contribution",
            ));
            Ok(PreStepDecision::Enter { messages })
        })
    }
}

/// Rejects every step, at the end of the chain.
struct Rejects;

impl PreStepListener for Rejects {
    fn decide<'a>(&'a self, _: &'a PreStep, _: PreStepNext<'a>) -> PreStepFuture<'a> {
        Box::pin(async { Ok(PreStepDecision::Reject) })
    }
}

// The schema half of "registers the skill tool schema and removes it on
// dispose".
#[tokio::test]
async fn the_tool_is_described_as_dshs_is() {
    let f = fixture().await;
    let schemas = f.tools.schemas();
    assert_eq!(
        schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["skill"]
    );
    assert_eq!(
        schemas[0].description,
        "Load the full instructions for a skill. Call it before acting on a task that names or \
         clearly matches a skill in the session skill catalog."
    );
    assert_eq!(schemas[0].description, DESCRIPTION);
    assert_eq!(
        schemas[0].parameters,
        r#"{"type":"object","properties":{"name":{"type":"string","description":"The exact skill name from the available skills list."}},"required":["name"]}"#
    );
    assert_eq!(schemas[0].parameters, PARAMETERS);
    let _kept = f.register("lifecycle-skill", "Lifecycle", "body");
    assert_eq!(f.fire_step(&f.agent("schema")).await.len(), 1);
}

// "injects a stable durable name-and-description catalog at the first step"
#[tokio::test]
async fn the_first_catalog_lists_names_and_capped_descriptions() {
    let f = fixture_in(
        TempDir::new(),
        ToolSkillConfig {
            catalog_description_max_length: 50,
        },
        Vec::new(),
    )
    .await;
    let mut z = SkillRegistration::new(
        "z-skill",
        "Long   description ".repeat(5),
        "secret-source",
        "Secret body.",
    );
    z.when_to_use = Some("Never render this routing hint.".into());
    z.provider = Some("runtime".into());
    z.resource_base = Some(SkillResourceBase::Directory {
        path: "/secret/path".into(),
    });
    let _kept = f.skills.register(None, z).unwrap();
    let _kept = f.register(
        "a-skill",
        "Use {{placeholder}} <safely> & carefully.",
        "A body.",
    );
    let mut model_only = SkillRegistration::new(
        "model-only-skill",
        "Model-only skill.",
        "runtime",
        "Model-only body.",
    );
    model_only.invocation = Some(SkillInvocationPolicy {
        model_invocable: true,
        user_invocable: false,
    });
    let _kept = f.skills.register(None, model_only).unwrap();
    let mut user_only = SkillRegistration::new(
        "user-only-skill",
        "User-only skill.",
        "runtime",
        "User-only body.",
    );
    user_only.invocation = Some(SkillInvocationPolicy {
        model_invocable: false,
        user_invocable: true,
    });
    let _kept = f.skills.register(None, user_only).unwrap();
    f.hooks.register(None, Arc::new(LaterContribution));

    let prefix = f.fire_step(&f.agent("catalog")).await;

    assert_eq!(prefix.len(), 2);
    assert_eq!(text(&prefix[0]), "later contribution");
    assert_eq!(kind(&prefix[0]), "later-contribution");
    let catalog = &prefix[1];
    assert_eq!(
        source(catalog),
        json!({
            "kind": "skill-catalog",
            "form": "catalog",
            "entries": [
                { "name": "a-skill", "description": "Use {{placeholder}} <safely> & carefully." },
                { "name": "model-only-skill", "description": "Model-only skill." },
                { "name": "z-skill", "description": "Long description Long description Long descript..." },
            ],
        })
    );
    assert_eq!(
        text(catalog),
        [
            "<system-reminder>",
            "A skill is a reusable set of task-specific instructions. The following skills are available in this session:",
            "",
            "<available_skills>",
            "- `a-skill`: Use {{placeholder}} &lt;safely&gt; &amp; carefully.",
            "- `model-only-skill`: Model-only skill.",
            "- `z-skill`: Long description Long description Long descript...",
            "</available_skills>",
            "",
            "If the user names a skill, or the task clearly matches a skill's description, call the `skill` tool with the exact skill name before taking task actions. Load all applicable skills, then follow their full instructions. This catalog contains summaries only; do not infer or follow a skill's instructions until it has been loaded.",
            "A user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill.",
            "</system-reminder>",
        ]
        .join("\n")
    );
    let rendered = serde_json::to_string(catalog).unwrap();
    for hidden in [
        "whenToUse",
        "routing hint",
        "secret-source",
        "/secret/path",
        "Secret body",
        "user-only-skill",
    ] {
        assert!(!rendered.contains(hidden), "{hidden}");
    }
}

// "does not inject a catalog when no model-invocable skills are available"
#[tokio::test]
async fn no_catalog_without_model_invocable_skills() {
    let f = fixture().await;
    let mut user_only = SkillRegistration::new(
        "user-only-skill",
        "User-only skill",
        "runtime",
        "User-only body.",
    );
    user_only.invocation = Some(SkillInvocationPolicy {
        model_invocable: false,
        user_invocable: true,
    });
    let _kept = f.skills.register(None, user_only).unwrap();
    let agent = f.agent("empty-catalog");
    assert!(f.fire_step(&agent).await.is_empty());
    assert!(f.fire_step(&agent).await.is_empty());
}

/// A provider whose listing fails until `failing` is cleared, and lists
/// nothing after.
struct Recovering {
    failing: Arc<AtomicBool>,
}

impl SkillProvider for Recovering {
    fn name(&self) -> &str {
        "recovering"
    }

    fn list<'a>(&'a self, _: &'a SkillLookupOptions) -> ListFuture<'a> {
        let failing = self.failing.load(Ordering::SeqCst);
        Box::pin(async move {
            if failing {
                return Err("temporarily unavailable".into());
            }
            Ok(SkillProviderObservation::complete(Vec::new()))
        })
    }

    fn get<'a>(&'a self, _: &'a SkillCandidate, _: &'a SkillLookupOptions) -> GetFuture<'a> {
        Box::pin(async { Ok(None) })
    }
}

// "omits an incomplete initial catalog and retries on a later request
// boundary"
#[tokio::test]
async fn an_incomplete_first_catalog_is_left_out_and_retried() {
    let f = fixture().await;
    let failing = Arc::new(AtomicBool::new(true));
    let control: Arc<Mutex<Option<SkillProviderControl>>> = Arc::default();
    let registration = {
        let failing = Arc::clone(&failing);
        let control = Arc::clone(&control);
        f.skills
            .register_provider(None, move |given| {
                *control.lock().unwrap() = Some(given);
                Arc::new(Recovering { failing }) as Arc<dyn SkillProvider>
            })
            .unwrap()
    };
    let agent = f.agent("incomplete-prefix");
    open_message_turn(&f.session(&agent));

    f.fire_step(&agent).await;
    assert!(f.catalogs(&agent).is_empty());
    failing.store(false, Ordering::SeqCst);
    control.lock().unwrap().as_ref().unwrap().invalidate();
    f.fire_step(&agent).await;
    assert!(f.catalogs(&agent).is_empty());
    drop(registration);
}

// "records an empty baseline across repeated step observations"
#[tokio::test]
async fn an_empty_catalog_stays_unpublished() {
    let f = fixture().await;
    let agent = f.agent("empty-step");
    open_message_turn(&f.session(&agent));
    f.fire_step(&agent).await;
    f.fire_step(&agent).await;
    assert!(f.catalogs(&agent).is_empty());
}

// "deduplicates or replaces a catalog already proposed for the same step"
#[tokio::test]
async fn a_proposed_catalog_is_dropped_when_seen_or_replaced_when_stale() {
    let f = fixture().await;
    let first = f.register("first-skill", "First skill", "First body.");
    let agent = f.agent("proposed-catalog");
    open_message_turn(&f.session(&agent));
    f.fire_step(&agent).await;
    let initial = f.catalogs(&agent)[0].1.clone();

    let duplicate = entered(f.propose(&agent, vec![initial.clone()]).await);
    assert!(duplicate.is_empty());

    let _kept = f.register("second-skill", "Second skill", "Second body.");
    let companion = prompt("keep this message");
    let replaced = entered(
        f.propose(&agent, vec![companion.clone(), initial.clone()])
            .await,
    );
    assert_eq!(replaced.len(), 2);
    assert_eq!(replaced[0], companion);
    assert_ne!(replaced[1].id, initial.id);
    assert!(text(&replaced[1]).contains("second-skill"));
    first.remove();
}

// "removes a stale proposed catalog before the first empty baseline"
#[tokio::test]
async fn a_stale_proposal_is_dropped_before_any_catalog_was_published() {
    let f = fixture().await;
    let agent = f.agent("proposed-empty-catalog");
    let malformed = UserMessage::text(
        "malformed",
        UserSource::Other(json!({ "kind": CATALOG_KIND, "form": "catalog" })),
        "preserve unreadable claimed context",
    );
    let stale = catalog_with(
        "stale",
        json!([{ "name": "stale-skill", "description": "Stale skill" }]),
        &["- `stale-skill`: Stale skill"],
    );
    let decision = f.propose(&agent, vec![malformed.clone(), stale]).await;
    assert_eq!(
        decision,
        PreStepDecision::Enter {
            messages: vec![malformed]
        }
    );
}

// "keeps a proposed catalog that already matches the current snapshot"
#[tokio::test]
async fn a_matching_proposal_is_kept() {
    let f = fixture().await;
    let _kept = f.register("first-skill", "First skill", "First body.");
    let agent = f.agent("matching-proposal");
    let proposed = catalog_with(
        "proposed",
        json!([{ "name": "first-skill", "description": "First skill" }]),
        &["- `first-skill`: First skill"],
    );
    let decision = f.propose(&agent, vec![proposed.clone()]).await;
    assert_eq!(
        decision,
        PreStepDecision::Enter {
            messages: vec![proposed]
        }
    );
}

// "injects complete replacement catalogs for additions and an empty
// tombstone for removals"
#[tokio::test]
async fn changes_publish_whole_replacements_and_an_empty_tombstone() {
    let f = fixture().await;
    let first = f.register("first-skill", "First skill", "First body.");
    let agent = f.agent("dynamic-catalog");
    open_message_turn(&f.session(&agent));

    f.fire_step(&agent).await;
    assert_eq!(f.catalogs(&agent).len(), 1);
    assert!(text(&f.catalogs(&agent)[0].1).contains("first-skill"));
    f.fire_step(&agent).await;
    assert_eq!(f.catalogs(&agent).len(), 1);

    let second = f.register("second-skill", "Second skill", "Second body.");
    f.fire_step(&agent).await;
    let addition = f.catalogs(&agent)[1].1.clone();
    assert!(text(&addition).contains("first-skill"));
    assert!(text(&addition).contains("second-skill"));
    assert_eq!(source(&addition)["update"], json!(true));
    assert_eq!(
        text(&addition),
        [
            "<system-reminder>",
            "The available skill catalog changed. This complete catalog replaces every earlier available-skills list in this session:",
            "",
            "<available_skills>",
            "- `first-skill`: First skill",
            "- `second-skill`: Second skill",
            "</available_skills>",
            "",
            "Use only names in this replacement catalog. If the user names a listed skill, or the task clearly matches its description, call the `skill` tool with the exact name before acting.",
            "A user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill.",
            "</system-reminder>",
        ]
        .join("\n")
    );

    second.remove();
    first.remove();
    f.fire_step(&agent).await;
    let removal = f.catalogs(&agent)[2].1.clone();
    assert_eq!(
        text(&removal),
        [
            "<system-reminder>",
            "The available skill catalog changed. This complete catalog replaces every earlier available-skills list in this session:",
            "",
            "<available_skills>",
            "</available_skills>",
            "",
            "No skills are currently available through the `skill` tool. Do not use names from earlier skill catalogs.",
            "A user may still invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool for it.",
            "</system-reminder>",
        ]
        .join("\n")
    );
    assert_eq!(
        source(&removal),
        json!({ "kind": "skill-catalog", "form": "catalog", "update": true, "entries": [] })
    );

    f.fire_step(&agent).await;
    assert_eq!(f.catalogs(&agent).len(), 3);
}

// "resumes from the durable entries of the latest visible catalog"
#[tokio::test]
async fn a_resumed_session_is_compared_by_its_catalogs_entries() {
    let f = fixture().await;
    let _kept = f.register("resumed-skill", "Resumed skill", "Resumed body.");
    let agent = f.agent("catalog-resume");
    let session = f.session(&agent);
    open_message_turn(&session);
    session
        .append(SessionEvent::user(UserMessage::text(
            "seeded",
            UserSource::Other(json!({
                "kind": CATALOG_KIND,
                "form": "catalog",
                "entries": [{ "name": "old-skill", "description": "Old skill" }],
            })),
            "prose a reader cannot rely on",
        )))
        .unwrap();
    let lookalike = catalog_with(
        "lookalike",
        json!([]),
        &["- `resumed-skill`: Resumed skill"],
    );
    session
        .append(SessionEvent::user(UserMessage {
            source: UserSource::Other(json!({ "kind": "dsh-tool-skill" })),
            ..lookalike
        }))
        .unwrap();

    f.fire_step(&agent).await;

    let catalogs = f.catalogs(&agent);
    assert_eq!(catalogs.len(), 2);
    let latest = &catalogs[1].1;
    assert_eq!(
        source(latest),
        json!({
            "kind": "skill-catalog",
            "form": "catalog",
            "update": true,
            "entries": [{ "name": "resumed-skill", "description": "Resumed skill" }],
        })
    );
    assert!(text(latest).contains("resumed-skill"));

    f.fire_step(&agent).await;
    assert_eq!(f.catalogs(&agent).len(), 2);
}

// "treats a malformed durable catalog as unrecognizable instead of failing
// the step"
#[tokio::test]
async fn malformed_catalogs_in_the_log_are_not_catalogs() {
    let f = fixture().await;
    let _kept = f.register("live-skill", "Live skill", "Live body.");
    let agent = f.agent("catalog-malformed");
    let session = f.session(&agent);
    open_message_turn(&session);
    for (index, source) in [
        json!({ "kind": CATALOG_KIND, "form": "catalog" }),
        json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": null }),
        json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": "not-an-array" }),
        json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": [null] }),
        json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": [{ "name": "x" }] }),
        json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": [{ "description": "no name" }] }),
        json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": [{ "name": "", "description": "empty" }] }),
    ]
    .into_iter()
    .enumerate()
    {
        session
            .append(SessionEvent::user(UserMessage::text(
                format!("malformed-{index}"),
                UserSource::Other(source),
                "unreadable catalog",
            )))
            .unwrap();
    }

    f.fire_step(&agent).await;

    let published: Vec<UserMessage> = f
        .catalogs(&agent)
        .into_iter()
        .map(|(_, message)| message)
        .filter(|message| source(message)["entries"].is_array())
        .filter(|message| text(message).contains("live-skill"))
        .collect();
    assert_eq!(published.len(), 1);
    assert_eq!(source(&published[0]).get("update"), None);
}

// "re-establishes the current catalog after compaction hides its durable
// message"
#[tokio::test]
async fn a_compacted_catalog_is_published_again() {
    let f = fixture().await;
    let _kept = f.register("first-skill", "First skill", "First body.");
    let agent = f.agent("catalog-compaction");
    let session = f.session(&agent);
    open_message_turn(&session);
    f.fire_step(&agent).await;
    let (seq, _) = f.catalogs(&agent)[0].clone();
    session
        .append(SessionEvent::UserMessage {
            message: UserMessage::text(
                "checkpoint",
                UserSource::Other(
                    json!({ "kind": "compact-checkpoint", "compactionId": "skill-compaction" }),
                ),
                "compacted history",
            ),
            surface: SurfaceIntent {
                op: SurfaceOp::Replace {
                    start: seq,
                    end: seq,
                },
                sources: vec![seq],
            },
        })
        .unwrap();

    f.fire_step(&agent).await;

    let catalogs = f.catalogs(&agent);
    assert_eq!(catalogs.len(), 2);
    assert!(text(&catalogs[1].1).contains("first-skill"));
    // It was published before, so this is a replacement.
    assert_eq!(source(&catalogs[1].1)["update"], json!(true));
}

// "keeps body-only edits out of the catalog and loads the latest body on
// demand"
#[tokio::test]
async fn a_body_edit_changes_no_catalog_and_loads_fresh() {
    let f = fixture().await;
    let root = f.home.at(".dsh/skills");
    write_skill(&root, "body-skill", "Stable description", "", "First body.");
    let agent = f.agent("body-refresh");
    open_message_turn(&f.session(&agent));

    let first = f.fire_step(&agent).await;
    assert!(text(&first[0]).contains("Stable description"));
    write_skill(
        &root,
        "body-skill",
        "Stable description",
        "",
        "Second body.",
    );
    f.fire_step(&agent).await;
    assert_eq!(f.catalogs(&agent).len(), 1);

    let result = f.call("body-skill", Some(agent)).await;
    assert!(!result.is_error);
    assert!(result.content.contains("Second body."));
    assert!(!result.content.contains("First body."));
}

// "resolves the layered registry as the calling agent sees it"
#[tokio::test]
async fn lookups_are_the_calling_agents() {
    let f = fixture().await;
    let scoped = f.agent("scoped-agent");
    let other = f.agent("other-agent");
    let _kept = f
        .skills
        .register(
            Some(&ScopeKey::from(scoped.id().clone())),
            SkillRegistration::new(
                "preset-only-skill",
                "Visible to the scoped agent alone",
                "preset",
                "Preset-only body.",
            ),
        )
        .unwrap();

    assert!(
        f.fire_step(&scoped)
            .await
            .iter()
            .any(|message| text(message).contains("preset-only-skill"))
    );
    assert!(
        !f.fire_step(&other)
            .await
            .iter()
            .any(|message| text(message).contains("preset-only-skill"))
    );

    let loaded = f.call("preset-only-skill", Some(scoped)).await;
    assert!(!loaded.is_error);
    assert!(loaded.content.contains("Preset-only body."));
    let foreign = f.call("preset-only-skill", Some(other)).await;
    assert_eq!(
        foreign,
        ToolOutput::error("Error: skill \"preset-only-skill\" is unknown or no longer available")
    );
}

/// A provider whose listing always fails.
struct Failing;

impl SkillProvider for Failing {
    fn name(&self) -> &str {
        "failing"
    }

    fn list<'a>(&'a self, _: &'a SkillLookupOptions) -> ListFuture<'a> {
        Box::pin(async { Err("temporarily unavailable".into()) })
    }

    fn get<'a>(&'a self, _: &'a SkillCandidate, _: &'a SkillLookupOptions) -> GetFuture<'a> {
        Box::pin(async { Ok(None) })
    }
}

// "retains the last-good catalog while any provider discovery is
// incomplete"
#[tokio::test]
async fn the_last_good_catalog_stands_while_discovery_is_incomplete() {
    let f = fixture().await;
    let stable = f.register("stable-skill", "Stable skill", "Stable body.");
    let agent = f.agent("incomplete-catalog");
    open_message_turn(&f.session(&agent));
    assert!(text(&f.fire_step(&agent).await[0]).contains("stable-skill"));

    let _failing = f
        .skills
        .register_provider(None, |_| Arc::new(Failing) as Arc<dyn SkillProvider>)
        .unwrap();
    stable.remove();
    f.fire_step(&agent).await;
    assert_eq!(f.catalogs(&agent).len(), 1);
}

// "validates the catalog description cap"
#[tokio::test]
async fn the_description_cap_is_at_least_three() {
    let home = TempDir::new();
    let error = Runtime::start(plugins(
        &home,
        ToolSkillConfig {
            catalog_description_max_length: 2,
        },
    ))
    .await
    .err()
    .expect("the start fails");
    assert!(
        error.to_string().contains(
            "tool-skill: catalogDescriptionMaxLength must be an integer greater than or equal to 3"
        ),
        "{error}"
    );
    // Three is enough.
    Runtime::start(plugins(
        &home,
        ToolSkillConfig {
            catalog_description_max_length: 3,
        },
    ))
    .await
    .unwrap();
}

// "loads a skill for the calling agent cwd"
#[tokio::test]
async fn a_skill_loads_from_the_calling_agents_directory() {
    let f = fixture().await;
    let project = TempDir::new();
    std::fs::create_dir(project.0.join(".git")).unwrap();
    write_skill(
        &project.at(".dsh/skills"),
        "project-skill",
        "Project skill",
        "",
        "Project instructions.",
    );
    let agent = f.agent_in("project-agent", &project.root());

    let result = f.call("project-skill", Some(agent)).await;

    assert_eq!(
        result,
        ToolOutput::ok(
            [
                "<skill_content name=\"project-skill\">".to_owned(),
                "<skill_resources>".to_owned(),
                format!(
                    "Base directory for this skill: {}",
                    project.at(".dsh/skills/project-skill")
                ),
                "Resolve relative paths mentioned by this skill against the base directory before \
                 using them. Load referenced resources only as needed."
                    .to_owned(),
                "</skill_resources>".to_owned(),
                String::new(),
                "<skill_instructions>".to_owned(),
                "Project instructions.".to_owned(),
                "</skill_instructions>".to_owned(),
                "</skill_content>".to_owned(),
            ]
            .join("\n")
        )
    );
}

// "renders provider-managed resource hints for non-local skills"
#[tokio::test]
async fn resource_hints_follow_the_resource_base() {
    let f = fixture().await;
    for (name, base) in [
        (
            "opaque-skill",
            Some(SkillResourceBase::Opaque {
                description: "runtime memory".into(),
            }),
        ),
        (
            "url-skill",
            Some(SkillResourceBase::Url {
                url: "https://skills.example.test/url-skill".into(),
            }),
        ),
        ("provider-skill", None),
    ] {
        let mut skill = SkillRegistration::new(name, "A skill", "runtime", "Instructions.");
        skill.provider = Some("runtime".into());
        skill.resource_base = base;
        let _kept = f.skills.register(None, skill).unwrap();
    }

    let opaque = f.call("opaque-skill", None).await;
    let url = f.call("url-skill", None).await;
    let provider = f.call("provider-skill", None).await;

    assert!(opaque.content.contains(
        "<skill_resources>\nResources for this skill: runtime memory\nLoad referenced resources \
         only as needed.\n</skill_resources>"
    ));
    assert!(url.content.contains(
        "<skill_resources>\nBase URL for this skill: https://skills.example.test/url-skill\n\
         Resolve relative URLs mentioned by this skill against the base URL before using them. \
         Load referenced resources only as needed.\n</skill_resources>"
    ));
    assert!(provider.content.contains(
        "<skill_resources>\nResources for this skill are managed by provider \"runtime\".\nLoad \
         referenced resources only as needed.\n</skill_resources>"
    ));
}

// "returns isError for unknown, invalid, and model-disabled skills"
#[tokio::test]
async fn unknown_invalid_and_model_disabled_skills_are_errors() {
    let f = fixture().await;
    write_skill(
        &f.home.at(".dsh/skills"),
        "hidden-skill",
        "Hidden skill",
        "disable-model-invocation: true",
        "Hidden instructions.",
    );
    let mut model_only = SkillRegistration::new(
        "model-only-skill",
        "Model-only skill",
        "runtime",
        "Model-only instructions.",
    );
    model_only.invocation = Some(SkillInvocationPolicy {
        model_invocable: true,
        user_invocable: false,
    });
    let _kept = f.skills.register(None, model_only).unwrap();

    assert_eq!(
        f.call("missing", None).await,
        ToolOutput::error("Error: skill \"missing\" is unknown or no longer available")
    );
    assert_eq!(
        f.call("Bad_Name", None).await,
        ToolOutput::error("Error: invalid skill name \"Bad_Name\"")
    );
    assert_eq!(
        f.call("hidden-skill", Some(f.agent("errors"))).await,
        ToolOutput::error("Error: skill \"hidden-skill\" is not available for model invocation")
    );
    assert!(!f.call("model-only-skill", None).await.is_error);
    // The registry checks the arguments first, in dsh's words.
    let call = ToolCall {
        id: "no-name".into(),
        name: NAME.into(),
        arguments: "{}".into(),
    };
    assert!(f.tools.execute(&call, None).await.is_error);
}

/// dsh's `policy-probe`: three candidates, the bodies of which all come
/// back model-disabled, or not at all.
struct PolicyProbe {
    gets: Mutex<Vec<String>>,
}

fn probe_candidate(name: &str, model_invocable: bool) -> SkillCandidate {
    SkillCandidate {
        path: None,
        name: name.into(),
        description: format!("{name} description"),
        when_to_use: None,
        invocation: SkillInvocationPolicy {
            model_invocable,
            user_invocable: true,
        },
        source: "test".into(),
        provider: "policy-probe".into(),
        resource_base: None,
        rank: 1.0,
        locator: Arc::new(name.to_owned()),
        metadata: None,
    }
}

impl SkillProvider for PolicyProbe {
    fn name(&self) -> &str {
        "policy-probe"
    }

    fn list<'a>(&'a self, _: &'a SkillLookupOptions) -> ListFuture<'a> {
        Box::pin(async {
            Ok(SkillProviderObservation::complete(vec![
                probe_candidate("denied-skill", false),
                probe_candidate("policy-race-skill", true),
                probe_candidate("vanishing-skill", true),
            ]))
        })
    }

    fn get<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        _: &'a SkillLookupOptions,
    ) -> GetFuture<'a> {
        self.gets.lock().unwrap().push(candidate.name.clone());
        Box::pin(async move {
            if candidate.name == "vanishing-skill" {
                return Ok(None);
            }
            Ok(Some(SkillDefinition {
                path: None,
                name: candidate.name.clone(),
                description: candidate.description.clone(),
                when_to_use: None,
                invocation: SkillInvocationPolicy {
                    model_invocable: false,
                    user_invocable: true,
                },
                source: candidate.source.clone(),
                provider: candidate.provider.clone(),
                resource_base: None,
                content: "Instructions must not be disclosed.".into(),
                metadata: None,
            }))
        })
    }
}

// "checks model policy before provider loading and rechecks the loaded
// definition"
#[tokio::test]
async fn the_policy_is_checked_before_loading_and_on_what_loaded() {
    let f = fixture().await;
    let probe = Arc::new(PolicyProbe {
        gets: Mutex::new(Vec::new()),
    });
    let _registration = {
        let probe = Arc::clone(&probe);
        f.skills
            .register_provider(None, move |_| probe as Arc<dyn SkillProvider>)
            .unwrap()
    };

    let denied = f.call("denied-skill", None).await;
    let raced = f.call("policy-race-skill", None).await;
    let vanished = f.call("vanishing-skill", None).await;

    assert_eq!(
        *probe.gets.lock().unwrap(),
        ["policy-race-skill", "vanishing-skill"]
    );
    assert_eq!(
        denied,
        ToolOutput::error("Error: skill \"denied-skill\" is not available for model invocation")
    );
    assert_eq!(
        raced,
        ToolOutput::error(
            "Error: skill \"policy-race-skill\" is not available for model invocation"
        )
    );
    assert_eq!(
        vanished,
        ToolOutput::error("Error: skill \"vanishing-skill\" is unknown or no longer available")
    );
}

// dsh's `invokeHarness`: three skills under the shared agent home.
async fn invoke_harness() -> (Fixture, Arc<dyn Agent>) {
    let home = TempDir::new();
    let root = home.at(".agents/skills");
    write_skill(
        &root,
        "hidden-demo",
        "User-only demo",
        "disable-model-invocation: true",
        "Say the magic word: PINEAPPLE.",
    );
    write_skill(
        &root,
        "shared-skill",
        "Ordinary skill",
        "",
        "Shared instructions.",
    );
    write_skill(
        &root,
        "model-only-skill",
        "Model only",
        "user-invocable: false",
        "Model-only instructions.",
    );
    let f = fixture_in(home, ToolSkillConfig::default(), Vec::new()).await;
    let agent = f.agent("invoke");
    (f, agent)
}

fn invoked(messages: &[UserMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| kind(message) == "skill-invocation")
        .map(|message| source(message)["name"].as_str().unwrap().to_owned())
        .collect()
}

// "injects a user-invocable skill named by a leading /token, after every
// other injection"
#[tokio::test]
async fn an_invoked_skill_enters_last() {
    let (f, agent) = invoke_harness().await;
    let messages = entered(
        f.propose(
            &agent,
            vec![
                prompt("/hidden-demo what does this do"),
                prompt("plain follow-up prose"),
            ],
        )
        .await,
    );
    let kinds: Vec<String> = messages.iter().map(kind).collect();
    assert_eq!(kinds, ["user", "user", "skill-catalog", "skill-invocation"]);
    let injection = messages.last().unwrap();
    assert_eq!(
        source(injection),
        json!({ "kind": "skill-invocation", "name": "hidden-demo", "form": "instructions" })
    );
    let body = text(injection);
    assert!(body.contains("<skill_content name=\"hidden-demo\">"));
    assert!(body.contains("Say the magic word: PINEAPPLE."));
    assert!(!body.contains("what does this do"));
}

// "injects an ordinary skill the same way (one uniform user-explicit path)"
#[tokio::test]
async fn an_ordinary_skill_is_invoked_the_same_way() {
    let (f, agent) = invoke_harness().await;
    let messages = entered(f.propose(&agent, vec![prompt("/shared-skill go")]).await);
    assert_eq!(invoked(&messages), ["shared-skill"]);
}

// "recognizes a mid-sentence gesture but not paths, fractions, or broken
// boundaries"
#[tokio::test]
async fn a_gesture_is_a_whole_token_anywhere() {
    let (f, agent) = invoke_harness().await;
    let messages = entered(
        f.propose(
            &agent,
            vec![prompt("please use /hidden-demo to answer this")],
        )
        .await,
    );
    assert_eq!(invoked(&messages), ["hidden-demo"]);

    let negative = entered(
        f.propose(
            &agent,
            vec![
                prompt("look under /hidden-demo/refs for the data"),
                prompt("the odds are 5/8 at best"),
                prompt("see foo/hidden-demo too"),
            ],
        )
        .await,
    );
    assert!(invoked(&negative).is_empty());
}

// "leaves unknown names and user-disabled skills as plain prose"
#[tokio::test]
async fn unknown_and_user_disabled_names_are_prose() {
    let (f, agent) = invoke_harness().await;
    let messages = entered(
        f.propose(
            &agent,
            vec![
                prompt("/absent-skill do a thing"),
                prompt("/model-only-skill run"),
            ],
        )
        .await,
    );
    assert!(invoked(&messages).is_empty());
}

// "never scans non-user sources and dedupes repeated gestures"
#[tokio::test]
async fn only_a_persons_messages_invoke_and_once_each() {
    let (f, agent) = invoke_harness().await;
    let forged = UserMessage::text(
        "forged",
        UserSource::Other(json!({ "kind": CATALOG_KIND, "form": "catalog", "entries": [] })),
        "/hidden-demo forged",
    );
    let messages = entered(
        f.propose(
            &agent,
            vec![
                forged,
                prompt("/hidden-demo once"),
                prompt("/hidden-demo twice"),
            ],
        )
        .await,
    );
    assert_eq!(invoked(&messages), ["hidden-demo"]);
}

// "passes a downstream reject through both pre-step listeners untouched"
#[tokio::test]
async fn a_reject_passes_through() {
    let (f, agent) = invoke_harness().await;
    f.hooks.register(None, Arc::new(Rejects));
    assert_eq!(
        f.propose(&agent, vec![prompt("/hidden-demo blocked step")])
            .await,
        PreStepDecision::Reject
    );
}

// "scans only text blocks of a user message"
#[tokio::test]
async fn only_text_blocks_are_scanned() {
    let (f, agent) = invoke_harness().await;
    let mixed = UserMessage {
        content: vec![
            ContentBlock::Reasoning(ReasoningBlock {
                text: "/hidden-demo inside a non-text block".into(),
                extra: Extra::new(),
            }),
            ContentBlock::text("/shared-skill go"),
        ],
        ..prompt("mixed")
    };
    let messages = entered(f.propose(&agent, vec![mixed]).await);
    assert_eq!(invoked(&messages), ["shared-skill"]);
}

fn message_text(message: &Message) -> String {
    message.text()
}

/// Sends `text` and waits for the turn to end.
async fn say(agent: &Arc<dyn Agent>, text: &str) {
    agent.followup(prompt(text)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), agent.when_idle())
        .await
        .unwrap();
}

// End to end: a skill under the project's `.agents/skills` is in the first
// request's catalog, after the prompt and before the runtime context, not in
// the system prompt; the model loads it with the tool, and the body is the
// tool's result in the next request.
#[tokio::test]
async fn a_model_reads_the_catalog_and_loads_a_skill() {
    let home = TempDir::new();
    write_skill(
        &home.at(".agents/skills"),
        "demo",
        "Demo skill",
        "",
        "Demo instructions.",
    );
    let f = fixture_in(
        home,
        ToolSkillConfig::default(),
        vec![
            vec![Block::ToolCall(ToolCall {
                id: "c1".into(),
                name: NAME.into(),
                arguments: r#"{"name":"demo"}"#.into(),
            })],
            vec![Block::Text("done".into())],
        ],
    )
    .await;
    let agent = f.agent("loop");
    say(&agent, "help me").await;

    let requests = f.model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert!(
        !first
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("<available_skills>")
    );
    let texts: Vec<String> = first
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(message_text)
        .collect();
    let prompt_at = texts.iter().position(|text| text == "help me").unwrap();
    let catalog_at = texts
        .iter()
        .position(|text| text.contains("<available_skills>\n- `demo`: Demo skill\n"))
        .expect("the catalog is in the first request");
    let context_at = texts
        .iter()
        .position(|text| text.starts_with("Current runtime context."))
        .unwrap();
    assert!(
        prompt_at < catalog_at && catalog_at < context_at,
        "{texts:?}"
    );

    let result = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Block::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        result.starts_with("<skill_content name=\"demo\">"),
        "{result}"
    );
    assert!(result.contains("Demo instructions."));
    // The catalog is logged once: the second step saw it on the surface.
    assert_eq!(f.catalogs(&agent).len(), 1);
}

// End to end: `/demo` in a prompt puts the skill's body in that step's
// request, after the prompt and the catalog.
#[tokio::test]
async fn a_person_invokes_a_skill_by_name() {
    let home = TempDir::new();
    write_skill(
        &home.at(".agents/skills"),
        "demo",
        "Demo skill",
        "",
        "Demo instructions.",
    );
    let f = fixture_in(
        home,
        ToolSkillConfig::default(),
        vec![vec![Block::Text("done".into())]],
    )
    .await;
    let agent = f.agent("invoke-loop");
    say(&agent, "/demo now").await;

    let requests = f.model.requests.lock().unwrap().clone();
    let texts: Vec<String> = requests[0]
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(message_text)
        .collect();
    let prompt_at = texts.iter().position(|text| text == "/demo now").unwrap();
    let catalog_at = texts
        .iter()
        .position(|text| text.contains("<available_skills>"))
        .unwrap();
    let body_at = texts
        .iter()
        .position(|text| text.contains("<skill_instructions>\nDemo instructions."))
        .expect("the invoked body is in the request");
    assert!(prompt_at < catalog_at && catalog_at < body_at, "{texts:?}");
    let logged: Vec<String> = f
        .session(&agent)
        .events()
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::UserMessage { message, .. } => Some(kind(&message)),
            _ => None,
        })
        .collect();
    assert!(
        logged.starts_with(&[
            "user".into(),
            "skill-catalog".into(),
            "skill-invocation".into()
        ]),
        "{logged:?}"
    );
}

// "omits catalog guidance when the calling agent restricts away the
// shipped skill tool"; and a catalog published before the restriction is
// withdrawn with the empty replacement, as dsh's README says.
#[tokio::test]
async fn an_agent_restricted_from_the_tool_gets_no_catalog() {
    let f = fixture().await;
    let _kept = f.register("listed-skill", "Listed", "body");
    let agent = f.agent("restricted-catalog");
    open_message_turn(&f.session(&agent));
    let _restriction = f
        .tools
        .restrict(
            &agent.scope(),
            ToolRestriction {
                allow: None,
                deny: Some(vec![NAME.into()]),
            },
        )
        .unwrap();

    assert!(f.tools.get(NAME, &agent.scope()).is_none());
    f.fire_step(&agent).await;
    f.fire_step(&agent).await;
    assert!(f.catalogs(&agent).is_empty());
    // Another agent still gets one.
    assert_eq!(f.fire_step(&f.agent("free")).await.len(), 1);

    let published = f.agent("published");
    open_message_turn(&f.session(&published));
    f.fire_step(&published).await;
    assert_eq!(f.catalogs(&published).len(), 1);
    let _later = f
        .tools
        .restrict(
            &published.scope(),
            ToolRestriction {
                allow: None,
                deny: Some(vec![NAME.into()]),
            },
        )
        .unwrap();
    f.fire_step(&published).await;
    let catalogs = f.catalogs(&published);
    assert_eq!(catalogs.len(), 2);
    assert_eq!(
        source(&catalogs[1].1),
        json!({ "kind": "skill-catalog", "form": "catalog", "update": true, "entries": [] })
    );
}

/// A tool named `skill` with other semantics.
struct Shadow;

impl Tool for Shadow {
    fn schema(&self) -> karaka_model::ToolSchema {
        karaka_model::ToolSchema {
            name: NAME.into(),
            description: "A scoped tool with unrelated semantics.".into(),
            parameters: r#"{"type":"object","properties":{}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::ok("shadow") })
    }
}

// "does not attach shipped catalog guidance to a scoped same-name tool
// shadow"
#[tokio::test]
async fn a_scoped_tool_named_skill_gets_no_catalog() {
    let f = fixture().await;
    let _kept = f.register("listed-skill", "Listed", "body");
    let agent = f.agent("shadowed");
    f.tools
        .register_in(Some(&ScopeKey::from(agent.id().clone())), Arc::new(Shadow))
        .unwrap();

    assert!(f.fire_step(&agent).await.is_empty());
    assert_eq!(f.fire_step(&f.agent("unshadowed")).await.len(), 1);
    assert_eq!(
        f.call("listed-skill", Some(agent)).await,
        ToolOutput::ok("shadow")
    );
}
