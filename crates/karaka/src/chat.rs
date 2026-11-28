//! `karaka chat`: a line-by-line conversation with the agent loop.
//!
//! Each line read from stdin is a followup to one session's agent. Once the
//! agent is idle, the turn's tool calls are printed one per line, then the
//! model's final text, all read back from the session's log.
//!
//! With `KARAKA_SESSIONS` set, the session is kept in that directory: a
//! chat in a session already stored there carries on its conversation.
//! The session is made durable after each turn, and given up when stdin
//! ends.

use std::collections::HashMap;
use std::io::{self, BufReader, IsTerminal, Write};
use std::sync::Arc;

use karaka_agent::{AgentComposition, Agents, ModelSelection};
use karaka_agent_default_model::AgentDefaultModelPlugin;
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_agent_presets::AgentPresets;
use karaka_browser_auth::BrowserAuthPlugin;
use karaka_credentials::CredentialsPlugin;
use karaka_fs_local::LocalFsPlugin;
use karaka_identity::{
    ApplicationOwner, IdentityError, KarakaIdentity, KarakaIdentityPlugin, Reservation,
};
use karaka_kernel::{DynPlugin, Runtime};
use karaka_mcp_application::{ApplicationCatalogPlugin, McpApplicationPlugin};
use karaka_mcp_client::{Level, Log, McpClientsPlugin};
use karaka_model::Block;
use karaka_model::Effort;
use karaka_retry::RetryPlugin;
use karaka_server_auth::ServerAuthPlugin;
use karaka_session::{
    Entry, Seq, SessionEvent, SessionId, SessionStore, SessionsPlugin, TurnEndReason, UserMessage,
    UserSource,
};
use karaka_session_jsonl::{JsonlConfig, JsonlPlugin};
use karaka_session_persistence::{DurableSessions, DurableSessionsPlugin};
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_session_query::SessionQueryPlugin;
use karaka_skill::SkillsPlugin;
use karaka_skill_filesystem::{SkillFilesystemConfig, SkillFilesystemPlugin};
use karaka_startup::KarakaStartupPlugin;
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_ask_user::{AskUserConfig, AskUserPlugin};
use karaka_tool_skill::ToolSkillPlugin;
use karaka_tool_todo::{TodoConfig, TodoToolPlugin};
use karaka_tool_working_directory::WorkingDirectoryToolPlugin;
use karaka_tools::{
    PreExecuteFuture, PreExecuteListener, PreExecuteNext, PreToolDecision, ToolExecution,
    ToolRegistry, ToolsPlugin,
};
use karaka_transport_http::TransportHttpPlugin;
use karaka_user_approval::{ApprovalConfig, ApprovalPlugin, ApprovalPolicy, ApprovalService};
use karaka_user_questions::{UserQuestions, UserQuestionsPlugin};
use karaka_webserver::{WebServerConfig, WebServerPlugin};
use karaka_working_directory::WorkingDirectoryPlugin;

use crate::config::Config;
use crate::current_time::CurrentTimePlugin;

/// dsh's name for an effort level, as its adapters write `reasoningEffort`.
fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}
use crate::plugins::GlobalPlugins;
use crate::questions::{Lines, TerminalAnswerer, TerminalApprover};

/// The session a chat runs in, unless `KARAKA_SESSION` names another.
const SESSION: &str = "chat";

/// How much of a tool's result its line shows.
const SHORT_RESULT: usize = 80;

/// The plugins `chat` and `ask` both run: the model provider, the system
/// prompt, the tools and sessions, the sessions' projections, the host's
/// files and each session's working directory with its tool, the todo
/// tool, the user-questions service with the `ask_user_question` tool, the
/// skills with the `skill` tool, the agent loop, and the retry plugin,
/// which retries a failed request as the provider's policy says before the
/// turn gives up.
///
/// `chat` says the plugins are for `karaka chat`, which has the
/// `current_time` tool, and watches the skill roots for changes, as dsh
/// watches them by default: a chat's next step sees a skill added
/// meanwhile. A single question has no next step to see one.
///
/// The global plugins from `KARAKA_PLUGINS` follow (`plugins.rs`): the
/// MCP servers every session sees, each waiting for its first connection,
/// with the port a preset's `mcp-client` rows use; the sessions' owners;
/// the server auth with its credentials, read from the environment; the
/// application catalog, empty without a row; and each application
/// endpoint, waiting for its first connection, so its catalog has settled
/// before the presets' policies read it.
///
/// With `KARAKA_PRESETS`, the agent preset registry goes last, so every
/// tool a preset's policy names is registered before it is set up. The
/// presets then own skill discovery: the global `skill-filesystem` isn't
/// mounted, and a preset gives its agents skills with a `skill-filesystem`
/// row, as dsh's web bundle disables the host row when presets own local
/// discovery (`bundle/web-app/cordis.patch.yml`).
///
/// # Errors
///
/// A presets or plugins file that doesn't read or fit.
pub fn loop_plugins(
    config: Config,
    approvals: &Approvals,
    mode: Mode,
) -> Result<Vec<Box<dyn DynPlugin>>, String> {
    let chat = mode == Mode::Chat;
    let watch_skills = chat;
    // `ask` keeps nothing; `chat` and `serve` keep their sessions in
    // `KARAKA_SESSIONS` when it is set.
    let persist = mode != Mode::Ask && config.sessions.is_some();
    let presets = match &config.presets {
        Some(path) => Some(crate::presets::load(
            path,
            config.preset.as_deref(),
            watch_skills,
        )?),
        None => None,
    };
    let global = match &config.plugins {
        Some(path) => crate::plugins::load(path)?,
        None => GlobalPlugins::default(),
    };
    // An application tool needs an explicit allow from a preset's
    // tool-policy (`karaka-mcp-application`'s policies), so without presets
    // an endpoint would offer nothing.
    if presets.is_none()
        && let (Some(path), Some(endpoint)) = (&config.plugins, global.applications.first())
    {
        return Err(format!(
            "KARAKA_PLUGINS ({}): the mcp-application row for \"{}\" needs KARAKA_PRESETS: its tools reach only sessions whose preset's tool-policy allows them",
            path.display(),
            endpoint.server_name
        ));
    }
    // The model an agent uses until a chat selects another. (dsh: the
    // `agent-default-model` row.)
    let default_model = ModelSelection {
        provider: config.provider.route().to_owned(),
        model: config.model.clone(),
        reasoning_effort: config.effort.map(|effort| effort_name(effort).to_owned()),
    };
    let mut loop_config = LoopConfig::new(config.provider.route(), config.model);
    loop_config.max_tokens = config.max_tokens;
    loop_config.effort = config.effort;

    // `KARAKA_SYSTEM` is the deployment's persona: after the identity line,
    // before anything else.
    let prompt = SystemPromptConfig {
        persona_prefix: config.system.unwrap_or_default(),
        ..SystemPromptConfig::default()
    };
    let mut plugins: Vec<Box<dyn DynPlugin>> = config.provider.plugins();
    plugins.push(Box::new(SystemPromptPlugin::new(prompt)));
    plugins.push(Box::new(ToolsPlugin));
    plugins.push(Box::new(SessionsPlugin));
    plugins.push(Box::new(SessionProjectionsPlugin));
    // A chat's sessions kept as JSONL files under `KARAKA_SESSIONS`. Early
    // in the list, so what requires the persistence port, the authority
    // among them, starts in its place, and the application endpoints
    // still settle before the presets.
    if persist && let Some(root) = &config.sessions {
        plugins.push(Box::new(JsonlPlugin::new(JsonlConfig::new(root.clone()))));
        plugins.push(Box::new(DurableSessionsPlugin));
    }
    // The host's files, relative paths from the process's directory.
    plugins.push(Box::new(LocalFsPlugin::default()));
    // Each session's working directory, shown to the model as runtime
    // context: the session's project, or the process's directory.
    plugins.push(Box::new(WorkingDirectoryPlugin::default()));
    // The model's tool for reading and changing it.
    plugins.push(Box::new(WorkingDirectoryToolPlugin));
    // The model's task list. Parallel work may be in progress at once, as
    // every dsh bundle configures it (`bundle/base/cordis.patch.yml`).
    plugins.push(Box::new(TodoToolPlugin::new(TodoConfig {
        allow_parallel_in_progress: true,
    })));
    // Questions the model asks the user, with the blocking legacy tool,
    // which is what dsh's bundles mount (`tool-ask-user` with no config in
    // `bundle/web-app/presets/*.patch.yml`).
    plugins.push(Box::new(UserQuestionsPlugin));
    // Approvals, which a tool call may need before it runs, under the
    // policy `--approval-policy` chose.
    plugins.push(Box::new(ApprovalPlugin::new(ApprovalConfig {
        policy: approvals.policy,
    })));
    plugins.push(Box::new(AskUserPlugin::new(AskUserConfig::default())));
    // Skills, found on disk under dsh's default roots: the project's
    // `.dsh/skills` and `.agents/skills`, then `~/.dsh/skills` and
    // `~/.agents/skills`, as dsh's base bundle mounts `skill-filesystem`
    // with no config (`bundle/base/cordis.patch.yml`). `DSH_HOME`,
    // `DSH_AGENTS_HOME` and `DSH_BUNDLED_SKILL_DIR` move the homes, and add
    // a bundled root. The model loads one with the `skill` tool, from the
    // catalog each step is given, or a person with `/name` in a message.
    plugins.push(Box::new(SkillsPlugin::default()));
    if presets.is_none() {
        plugins.push(Box::new(SkillFilesystemPlugin::new(
            SkillFilesystemConfig {
                watch: watch_skills,
                ..SkillFilesystemConfig::default()
            },
        )));
    }
    plugins.push(Box::new(ToolSkillPlugin::default()));
    plugins.push(Box::new(AgentLoopPlugin::new(loop_config)));
    plugins.push(Box::new(RetryPlugin::new()));
    if chat {
        plugins.push(Box::new(CurrentTimePlugin));
    }
    // MCP servers: the global ones every session sees, and the port a
    // preset's `mcp-client` rows start theirs through. Their failures are
    // told on stderr, as the loop's warnings would be logged.
    let log: Log = Arc::new(|level, line| {
        if level != Level::Info {
            eprintln!("karaka: {line}");
        }
    });
    plugins.push(Box::new(global.mcp_servers.into_iter().fold(
        McpClientsPlugin::new().on_log(Arc::clone(&log)),
        McpClientsPlugin::server,
    )));
    // Karaka's application authority: who owns each session.
    // `KARAKA_APPLICATION_ID` and the rest admit the owner of the one this
    // process opens (`admit`). A chat kept in `KARAKA_SESSIONS` has durable
    // storage, so its chats are acknowledged and restored through it.
    plugins.push(Box::new(if persist {
        KarakaIdentityPlugin::with_persistence()
    } else {
        KarakaIdentityPlugin::new()
    }));
    if let Some(auth) = global.server_auth {
        plugins.push(Box::new(CredentialsPlugin::new()));
        plugins.push(Box::new(ServerAuthPlugin::bearer(auth)));
    }
    // The application catalog a preset's tool policy splits its lists by.
    // (Karaka: the `application-catalog` row.)
    plugins.push(Box::new(ApplicationCatalogPlugin::new(global.catalog)));
    for endpoint in global.applications {
        plugins.push(Box::new(
            McpApplicationPlugin::new(endpoint).on_log(Arc::clone(&log)),
        ));
    }
    let has_presets = presets.is_some();
    if let Some(presets) = presets {
        plugins.push(Box::new(presets));
    }
    if let Mode::Serve(listen) = mode {
        plugins.push(Box::new(
            WebServerPlugin::new(listen).on_log(Arc::new(|line: &str| eprintln!("karaka: {line}"))),
        ));
        // The ingress admits requests only once every plugin has started.
        plugins.push(Box::new(KarakaStartupPlugin));
        if let Some(auth) = global.browser_auth {
            plugins.push(Box::new(BrowserAuthPlugin::new(auth)));
        }
        if let Some(transport) = global.transport {
            if !persist {
                return Err(format!(
                    "{}: the application-http row needs KARAKA_SESSIONS: a chat is acknowledged once it is stored",
                    global.at
                ));
            }
            plugins.push(Box::new(SessionQueryPlugin::with_persistence()));
            plugins.push(Box::new(AgentDefaultModelPlugin::new(default_model)));
            plugins.push(Box::new(TransportHttpPlugin::new(transport, has_presets)?));
        }
    }
    Ok(plugins)
}

/// Which command the plugins are for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `karaka ask`: one question, nothing kept, nothing watched.
    Ask,
    /// `karaka chat`: a conversation, its skills watched.
    Chat,
    /// `karaka serve`: the webserver on this address, and the
    /// application routes when the plugins file has their row.
    Serve(WebServerConfig),
}

/// What the command line says: approvals, and the preset to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub approvals: Approvals,
    /// `--preset`: the preset the session runs, selected before its first
    /// turn.
    pub preset: Option<String>,
}

/// Makes the session `id` run `preset`, if one was asked for, through the
/// registry's `select`: only while the session is blank.
///
/// # Errors
///
/// `--preset` without `KARAKA_PRESETS`, or the registry's refusal, in dsh's
/// words.
pub fn select_preset(
    runtime: &Runtime,
    id: &SessionId,
    preset: Option<&str>,
) -> Result<(), String> {
    let Some(preset) = preset else {
        return Ok(());
    };
    if !runtime.plugins().contains(&"agent-preset-registry") {
        return Err(format!(
            "--preset {preset} needs KARAKA_PRESETS: without it there are no presets"
        ));
    }
    let session = runtime
        .get::<SessionStore>()
        .get(id)
        .ok_or("the session wasn't started")?;
    runtime
        .get::<AgentPresets>()
        .select(&session, preset)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Admits `owner` to the session `id` through Karaka's authority, before
/// the session's agent opens, which is when an application endpoint offers
/// the agent its tools. (Karaka: `transport-http`'s create path, under
/// `withChatLock`.)
///
/// A session already stored is restored to `owner`: the operator who set
/// the variables is trusted, where the ingress trusts the application's
/// verified access. Otherwise the chat is reserved, its session made as the
/// loop or the durable store makes it, and bound; with durable storage it
/// is then acknowledged once flushed.
///
/// # Errors
///
/// The authority's refusal, in Karaka's words, or the durable store's.
pub async fn admit(
    runtime: &Runtime,
    id: &SessionId,
    owner: &ApplicationOwner,
    durable: Option<&DurableSessions>,
    cwd: Option<String>,
) -> Result<(), String> {
    let identity = runtime.get::<KarakaIdentity>();
    let refused = |error: IdentityError| format!("the session's owner: {error}");
    identity
        .with_chat_lock(id, async {
            if let Some(durable) = durable {
                let stored = durable
                    .persistence()
                    .stat(id)
                    .await
                    .map_err(|error| error.to_string())?;
                if stored.is_some() {
                    identity.restore(id, owner).await.map_err(refused)?;
                    return Ok(());
                }
            }
            if identity.reserve(id, owner).await.map_err(refused)? == Reservation::Existing {
                return Ok(());
            }
            let session = match durable {
                Some(durable) => durable
                    .open(id.clone(), cwd)
                    .await
                    .map_err(|error| error.to_string())?,
                // As the loop's `open` makes it, named for its preset.
                None => match runtime.get::<AgentComposition>().composer() {
                    Some(composer) => runtime
                        .get::<SessionStore>()
                        .open_or_create_with(id.clone(), |header| composer.prepare(header)),
                    None => runtime.get::<SessionStore>().open_or_create(id.clone()),
                },
            };
            identity.bind(&session, owner).await.map_err(refused)?;
            if durable.is_some() {
                identity
                    .mark_ready(&session, owner)
                    .await
                    .map_err(refused)?;
            }
            Ok(())
        })
        .await
        .map_err(refused)?
}

/// What the command line says about approvals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Approvals {
    /// `--approval-policy`: `ask` by default.
    pub policy: ApprovalPolicy,
    /// `--ask-before`: the tools that need the user's approval each time.
    pub ask_before: Vec<String>,
}

/// Puts the model's questions, and the approvals its tool calls need, to
/// the user on the terminal: written to stderr, answered from `lines`.
/// The tools `approvals` names ask before each call.
pub fn answer_on_the_terminal(runtime: &Runtime, lines: &Arc<Lines>, approvals: &Approvals) {
    let answerer = TerminalAnswerer::new(Arc::clone(lines), Box::new(io::stderr()));
    // For the runtime's life.
    let _ = runtime
        .get::<UserQuestions>()
        .answerer(None, Arc::new(answerer));
    let approver = TerminalApprover::new(Arc::clone(lines), Box::new(io::stderr()));
    let _ = runtime
        .get::<ApprovalService>()
        .answerer(None, Arc::new(approver));
    if !approvals.ask_before.is_empty() {
        let _ = runtime.get::<ToolRegistry>().on_pre_execute(
            None,
            Arc::new(AskBefore {
                tools: approvals.ask_before.clone(),
            }),
        );
    }
}

/// Asks before each call to one of `tools`. (`--ask-before`.)
struct AskBefore {
    tools: Vec<String>,
}

impl PreExecuteListener for AskBefore {
    fn decide<'a>(
        &'a self,
        call: &'a ToolExecution,
        next: PreExecuteNext<'a>,
    ) -> PreExecuteFuture<'a> {
        if !self.tools.contains(&call.name) {
            return next.run();
        }
        let reason = format!("karaka was started with --ask-before {}", call.name);
        Box::pin(async move {
            Ok(PreToolDecision::Ask {
                reason: Some(reason),
                display_reason: None,
            })
        })
    }
}

/// Runs the chat until stdin ends.
pub async fn chat(config: Config, options: Options) -> Result<(), String> {
    let Options { approvals, preset } = options;
    let id = SessionId::new(config.session.as_deref().unwrap_or(SESSION));
    let sessions = config.sessions.clone();
    let owner = config.owner.clone();
    // With `KARAKA_SESSIONS`, the plugins keep the sessions durable.
    let plugins = loop_plugins(config, &approvals, Mode::Chat)?;
    let runtime = Runtime::start(plugins)
        .await
        .map_err(|error| error.to_string())?;
    // stdin's one reader, shared by the chat's messages and the model's
    // questions (see `questions.rs`).
    let lines = Arc::new(Lines::new(BufReader::new(io::stdin())));
    answer_on_the_terminal(&runtime, &lines, &approvals);

    // A stored session is resumed, and repaired if a crash left a turn
    // open, before the agent sees it. Its owner, if one is set, is admitted
    // first.
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.to_str().map(str::to_owned));
    let durable = sessions.map(|_| runtime.get::<DurableSessions>());
    if let Some(owner) = &owner {
        admit(&runtime, &id, owner, durable.as_deref(), cwd.clone()).await?;
    }
    let durable = match durable {
        Some(durable) => {
            durable
                .open(id.clone(), cwd)
                .await
                .map_err(|error| error.to_string())?;
            Some(durable)
        }
        None => None,
    };
    let result = converse(&runtime, &id, &lines, preset.as_deref()).await;
    // Whatever happened, what the chat said is written before it exits.
    if let Some(durable) = durable {
        durable.close().await.map_err(|error| error.to_string())?;
    }
    result
}

/// Reads lines and answers them until stdin ends.
async fn converse(
    runtime: &Runtime,
    id: &SessionId,
    lines: &Lines,
    preset: Option<&str>,
) -> Result<(), String> {
    let store = runtime.get::<SessionStore>();
    let agent = runtime
        .get::<dyn Agents>()
        .open(id.clone())
        .map_err(|error| error.to_string())?;
    select_preset(runtime, id, preset)?;
    // Opening the agent started the session, so it is there to read.
    let session = store.get(id).ok_or("the chat's session wasn't started")?;

    let interactive = io::stdin().is_terminal();
    if interactive {
        if !session.is_empty() {
            eprintln!("Carrying on session \"{id}\".");
        }
        eprintln!("Type a message and press Enter. Ctrl-D ends the chat.");
    }
    loop {
        if interactive {
            eprint!("> ");
        }
        // Read only now, with the agent idle: during a turn, the lines are a
        // question's to read.
        let Some(line) = lines
            .next()
            .await
            .map_err(|error| format!("couldn't read the message: {error}"))?
        else {
            return Ok(());
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The turn's events start here. The message is named after this
        // seq, as the loop names its own messages.
        let start = Seq(session.len() as u64);
        let message = UserMessage::text(format!("user-{start}"), UserSource::user(), line);
        agent
            .followup(message)
            .map_err(|error| format!("couldn't send the message: {error}"))?;
        agent.when_idle().await;
        // The turn is durable before its answer shows. Without storage,
        // there is nothing to wait for.
        store
            .flush(id)
            .await
            .map_err(|error| format!("couldn't store the turn: {error}"))?;
        print_turn(&session.events_since(start))
            .map_err(|error| format!("couldn't write the answer: {error}"))?;
    }
}

/// Prints one turn: a line per tool call, then the model's last text on
/// stdout, and how the turn ended on stderr if it didn't complete.
fn print_turn(events: &[Entry]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut names = HashMap::new();
    let mut answer = String::new();
    for entry in events {
        match &entry.event {
            SessionEvent::ToolCall(call) => {
                names.insert(call.call_id.clone(), call.name.clone());
            }
            SessionEvent::ToolResult { data, .. } => {
                let result = data.message.to_model();
                let name = names.get(&result.call_id).map_or("a tool", String::as_str);
                let verb = if result.is_error { "failed" } else { "gave" };
                writeln!(stdout, "[{name} {verb}: {}]", short(&result.content))?;
            }
            SessionEvent::AssistantMessage { data, .. } => {
                let text: String = data
                    .message
                    .blocks()
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.trim().is_empty() {
                    answer = text;
                }
            }
            SessionEvent::TurnEnd(end) => match &end.reason {
                TurnEndReason::Completed => {}
                TurnEndReason::MaxTokens => eprintln!("(the answer reached the token limit)"),
                TurnEndReason::Aborted(_) => eprintln!("(the turn was cancelled)"),
                TurnEndReason::Error(failure) => {
                    eprintln!("karaka: {} ({})", failure.message, failure.code);
                }
                other => eprintln!("(the turn ended: {other:?})"),
            },
            _ => {}
        }
    }
    if !answer.is_empty() {
        writeln!(stdout, "{answer}")?;
    }
    stdout.flush()
}

/// The first line of `text`, cut to [`SHORT_RESULT`] characters.
fn short(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= SHORT_RESULT && line.len() == text.len() {
        return line.to_owned();
    }
    let cut: String = line.chars().take(SHORT_RESULT).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::short;

    #[test]
    fn a_short_result_is_shown_whole() {
        assert_eq!(
            short("Sat, 21 Nov 2026 09:30:00 GMT"),
            "Sat, 21 Nov 2026 09:30:00 GMT"
        );
    }

    #[test]
    fn a_long_or_many_line_result_is_cut() {
        assert_eq!(short("first\nsecond"), "first…");
        let long = "x".repeat(100);
        assert_eq!(short(&long), format!("{}…", "x".repeat(80)));
    }
}
