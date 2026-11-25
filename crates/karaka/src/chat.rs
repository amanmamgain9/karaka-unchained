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
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use karaka_agent::Agents;
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_kernel::{DynPlugin, Runtime};
use karaka_model::Block;
use karaka_retry::RetryPlugin;
use karaka_session::{
    Entry, Seq, SessionEvent, SessionId, SessionStore, SessionsPlugin, TurnEndReason, UserMessage,
    UserSource,
};
use karaka_session_jsonl::{JsonlConfig, JsonlPlugin};
use karaka_session_persistence::{DurableSessions, DurableSessionsPlugin};
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tools::{ToolRegistry, ToolsPlugin};

use crate::config::Config;
use crate::current_time::CurrentTime;

/// The session a chat runs in, unless `KARAKA_SESSION` names another.
const SESSION: &str = "chat";

/// How much of a tool's result its line shows.
const SHORT_RESULT: usize = 80;

/// The plugins `chat` and `ask` both run: the model provider, the system
/// prompt, the tools and sessions, the agent loop, and the retry plugin,
/// which retries a failed
/// request as the provider's policy says before the turn gives up.
pub fn loop_plugins(config: Config) -> Vec<Box<dyn DynPlugin>> {
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
    plugins.push(Box::new(AgentLoopPlugin::new(loop_config)));
    plugins.push(Box::new(RetryPlugin::new()));
    plugins
}

/// Runs the chat until stdin ends.
pub async fn chat(config: Config) -> Result<(), String> {
    let id = SessionId::new(config.session.as_deref().unwrap_or(SESSION));
    let sessions = config.sessions.clone();
    let mut plugins = loop_plugins(config);
    if let Some(root) = sessions.clone() {
        // The store's sessions kept as JSONL files under the root.
        plugins.push(Box::new(JsonlPlugin::new(JsonlConfig::new(root))));
        plugins.push(Box::new(DurableSessionsPlugin));
    }
    let runtime = Runtime::start(plugins)
        .await
        .map_err(|error| error.to_string())?;
    runtime
        .get::<ToolRegistry>()
        .register(Arc::new(CurrentTime))
        .map_err(|error| error.to_string())?;

    // A stored session is resumed, and repaired if a crash left a turn
    // open, before the agent sees it.
    let durable = match sessions {
        Some(_) => {
            let durable = runtime.get::<DurableSessions>();
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|dir| dir.to_str().map(str::to_owned));
            durable
                .open(id.clone(), cwd)
                .await
                .map_err(|error| error.to_string())?;
            Some(durable)
        }
        None => None,
    };
    let result = converse(&runtime, &id).await;
    // Whatever happened, what the chat said is written before it exits.
    if let Some(durable) = durable {
        durable.close().await.map_err(|error| error.to_string())?;
    }
    result
}

/// Reads lines and answers them until stdin ends.
async fn converse(runtime: &Runtime, id: &SessionId) -> Result<(), String> {
    let store = runtime.get::<SessionStore>();
    let agent = runtime
        .get::<dyn Agents>()
        .open(id.clone())
        .map_err(|error| error.to_string())?;
    // Opening the agent started the session, so it is there to read.
    let session = store.get(id).ok_or("the chat's session wasn't started")?;

    let interactive = io::stdin().is_terminal();
    if interactive {
        if !session.is_empty() {
            eprintln!("Carrying on session \"{id}\".");
        }
        eprintln!("Type a message and press Enter. Ctrl-D ends the chat.");
    }
    let mut lines = io::stdin().lock().lines();
    loop {
        if interactive {
            eprint!("> ");
        }
        // This blocks the thread, but the agent is idle, and its turns run
        // on the runtime's other threads anyway.
        let Some(line) = lines.next() else {
            return Ok(());
        };
        let line = line.map_err(|error| format!("couldn't read the message: {error}"))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The turn's events start here. The message is named after this
        // seq, as the loop names its own messages.
        let start = Seq(session.len() as u64);
        let message = UserMessage::text(format!("user-{start}"), UserSource::user(), line);
        agent.followup(message);
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
