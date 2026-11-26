//! The `todo_write` tool: the agent's whole task list, replaced on each call.
//! (dsh: `@deepseek-ai/dsh-tool-todo`, 0.2.1-rc.2.)
//!
//! Each call appends a `todo/write { todos }` snapshot to the calling
//! agent's session, so replaying the log is last-write-wins, and UIs render
//! from the `todos` projection. A call outside any agent has no list of its
//! own, and is refused. [`TodoToolPlugin`] registers the tool and the
//! projection.
//!
//! `todo/write` is a v4 event type the session crate knows but doesn't
//! model, as v4 leaves its payload to its plugin: it is stored as a
//! passthrough, its shape [`TodoItem`]s.
//!
//! - `schema.rs`: the argument checks dsh's registry makes, in its words.

mod schema;

use std::sync::{Arc, LazyLock};

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::ToolSchema;
use karaka_session::{Entry, Passthrough, SessionEvent, SessionHeader, SessionStore};
use karaka_session_projection::{ClientView, Projection, SessionProjections};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The tool's name. (dsh: `name: 'todo_write'`.)
pub const NAME: &str = "todo_write";

/// The event a write is logged as.
pub const WRITE_EVENT: &str = "todo/write";

/// One entry of the list: a short line and its state. No id: the list is
/// replaced whole on every write. (dsh: `TodoItem`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// A todo's state. (dsh: `'pending' | 'in_progress' | 'completed'`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// The deployment's todo policy. (dsh: `Config`.) It has no default: the
/// deployment chooses, as dsh's config requires the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TodoConfig {
    /// Whether several todos may be `in_progress` at once. True suits agents
    /// that run work concurrently, and the description then asks the model
    /// to mark every task it is working on; false asks for exactly one, and
    /// a call marking more is refused. (dsh: `allowParallelInProgress`.)
    pub allow_parallel_in_progress: bool,
}

const DESCRIPTION_HEAD: &str = "Record and update a task list to plan multi-step work and show \
                                progress; skip it for trivial single-step tasks. Add one todo \
                                per concrete step before you start. ";
const DESCRIPTION_PARALLEL: &str = "While work remains, keep the todos being worked on \
                                    `in_progress`, several only when work runs in parallel. ";
const DESCRIPTION_SINGLE: &str = "While work remains, keep exactly one todo `in_progress`. ";
const DESCRIPTION_TAIL: &str = "Mark each todo `completed` as soon as it is done.";

/// The model-facing description: only the active-status clause depends on
/// the policy. (dsh: `describe`.)
pub fn description(allow_parallel: bool) -> String {
    let clause = if allow_parallel {
        DESCRIPTION_PARALLEL
    } else {
        DESCRIPTION_SINGLE
    };
    format!("{DESCRIPTION_HEAD}{clause}{DESCRIPTION_TAIL}")
}

/// The parameters, as dsh's `defineTool` compiles its spec: each
/// `required: true` becomes its parent's `required`, the item keeps
/// `additionalProperties: false`, and the root stays open.
static PARAMETERS: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "description": "The COMPLETE task list, replacing any previous list.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "What the task is — a short imperative line.",
                        },
                        "status": {
                            "type": "string",
                            "description":
                                "pending (not started) | in_progress (now) | completed (done).",
                            "enum": ["pending", "in_progress", "completed"],
                        },
                    },
                    "required": ["content", "status"],
                },
            },
        },
        "required": ["todos"],
    })
});

/// The canonical list: content trimmed and not empty, no content twice,
/// and at most one `in_progress` unless parallel work is allowed. (dsh:
/// `toTodoList`.)
fn to_todo_list(raw: Vec<TodoItem>, allow_parallel: bool) -> Result<Vec<TodoItem>, String> {
    let mut todos: Vec<TodoItem> = Vec::with_capacity(raw.len());
    let mut active = 0;
    for item in raw {
        let content = item.content.trim().to_owned();
        if content.is_empty() {
            return Err("invalid todo: `content` must be a non-empty string".into());
        }
        if todos.iter().any(|todo| todo.content == content) {
            return Err(format!(
                "invalid todos: duplicate content {}",
                Value::String(content)
            ));
        }
        if item.status == TodoStatus::InProgress {
            active += 1;
        }
        todos.push(TodoItem {
            content,
            status: item.status,
        });
    }
    if !allow_parallel && active > 1 {
        return Err(format!(
            "invalid todos: at most one task may be in_progress (got {active})"
        ));
    }
    Ok(todos)
}

/// The `todos` projection: the latest written list, cleared by the next
/// `turn/start` (a `turn/end` keeps the finished list in view); `None`
/// before the first write. Clients see the state. (dsh: the `todos` unit,
/// state version 2.)
pub struct TodosProjection;

impl Projection for TodosProjection {
    const KEY: &'static str = "todos";
    type State = Option<Vec<TodoItem>>;

    fn state_version(&self) -> u64 {
        2
    }

    fn init(&self, _: &SessionHeader, _: u64) -> Self::State {
        None
    }

    fn apply(&self, _: &Self::State, entry: &Entry) -> Option<Self::State> {
        match &entry.event {
            SessionEvent::Passthrough(event) if event.event_type == WRITE_EVENT => {
                let todos = event.data.get("todos")?;
                serde_json::from_value(todos.clone()).ok().map(Some)
            }
            SessionEvent::TurnStart(_) => Some(None),
            _ => None,
        }
    }
}

impl ClientView for TodosProjection {
    type View = Option<Vec<TodoItem>>;

    fn view(&self, state: &Self::State) -> Self::View {
        state.clone()
    }
}

/// The tool. (dsh: the `defineTool` its `apply` registers.)
pub struct TodoWriteTool {
    allow_parallel: bool,
    sessions: Arc<SessionStore>,
}

impl TodoWriteTool {
    pub fn new(config: TodoConfig, sessions: Arc<SessionStore>) -> Self {
        Self {
            allow_parallel: config.allow_parallel_in_progress,
            sessions,
        }
    }

    fn write(&self, arguments: &Value, run: &ToolRun) -> Result<String, String> {
        // dsh's registry checks the arguments against the schema before the
        // tool runs.
        let violations = schema::violations(&PARAMETERS, arguments);
        if !violations.is_empty() {
            return Err(format!("invalid arguments: {}", violations.join("; ")));
        }
        let raw: Vec<TodoItem> = serde_json::from_value(arguments["todos"].clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let todos = to_todo_list(raw, self.allow_parallel)?;
        // The list belongs to an agent's session; a call outside any agent
        // has nowhere to put it.
        let Some(agent) = &run.agent else {
            return Err("todo_write requires an owning agent session".into());
        };
        let session = self
            .sessions
            .get(agent.id())
            .ok_or_else(|| format!("todo_write: no session \"{}\"", agent.id()))?;
        let count = |status| todos.iter().filter(|todo| todo.status == status).count();
        let (pending, in_progress, completed) = (
            count(TodoStatus::Pending),
            count(TodoStatus::InProgress),
            count(TodoStatus::Completed),
        );
        session
            .append(SessionEvent::Passthrough(Passthrough {
                event_type: WRITE_EVENT.into(),
                data: json!({ "todos": todos }),
                surface_op: None,
                source_event_seqs: None,
            }))
            .map_err(|error| error.to_string())?;
        // dsh's `render`.
        Ok(format!(
            "Updated todo list: {pending} pending, {in_progress} in progress, {completed} \
             completed."
        ))
    }
}

impl Tool for TodoWriteTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: NAME.into(),
            description: description(self.allow_parallel),
            parameters: PARAMETERS.to_string(),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            match self.write(&arguments, &run) {
                Ok(text) => ToolOutput::ok(text),
                // dsh's tool layer shows a thrown error as `Error: <message>`.
                Err(message) => ToolOutput::error(format!("Error: {message}")),
            }
        })
    }
}

/// Registers the `todos` projection and the `todo_write` tool. It requires
/// the tool registry, the session projections and the session store. (dsh:
/// the plugin's `apply`, with `inject` of `tools` and `sessionProjections`.)
///
/// dsh's registrations are undone when its plugin unloads. Karaka's kernel
/// unloads no plugin, so they stay for the runtime's life.
#[derive(Clone, Copy, Debug)]
pub struct TodoToolPlugin {
    config: TodoConfig,
}

impl TodoToolPlugin {
    pub fn new(config: TodoConfig) -> Self {
        Self { config }
    }
}

impl Plugin for TodoToolPlugin {
    fn name(&self) -> &str {
        "tool-todo"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ToolRegistry>(),
            PortId::of::<SessionProjections>(),
            PortId::of::<SessionStore>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let _ = ctx.get::<SessionProjections>().register(TodosProjection)?;
        let tool = TodoWriteTool::new(self.config, ctx.get::<SessionStore>());
        ctx.get::<ToolRegistry>().register(Arc::new(tool))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    #[test]
    fn the_list_rules_are_dshs() {
        assert_eq!(
            to_todo_list(vec![item(" a ", TodoStatus::Pending)], false),
            Ok(vec![item("a", TodoStatus::Pending)])
        );
        assert_eq!(
            to_todo_list(
                vec![
                    item("a", TodoStatus::InProgress),
                    item("b", TodoStatus::InProgress),
                    item("c", TodoStatus::InProgress),
                ],
                false
            ),
            Err("invalid todos: at most one task may be in_progress (got 3)".into())
        );
        // A duplicate is reported before the count of active ones.
        assert_eq!(
            to_todo_list(
                vec![
                    item("a", TodoStatus::InProgress),
                    item("a", TodoStatus::InProgress),
                ],
                false
            ),
            Err("invalid todos: duplicate content \"a\"".into())
        );
    }
}
