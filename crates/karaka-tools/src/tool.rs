use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use karaka_agent::Agent;

use karaka_model::{ToolResult, ToolSchema};
use serde_json::Value;

/// What a tool implements. (dsh: `ToolDefinition` in `tools/src/index.ts`.)
///
/// Tools of different types share one registry as `Arc<dyn Tool>`, so the
/// trait has to work as `dyn Tool`. A trait with an `async fn` can't, so
/// [`execute`](Self::execute) returns its future boxed instead, as the
/// kernel's `DynPlugin::setup_boxed` does. Write the body as
/// `Box::pin(async move { ... })`.
pub trait Tool: Send + Sync {
    /// How the model sees the tool: its name, when to use it, and a JSON
    /// Schema for its arguments. The registry reads it once, when the tool
    /// is registered.
    fn schema(&self) -> ToolSchema;

    /// Whether the registry checks the arguments against the parameters
    /// before the tool runs, as dsh checks every tool made with
    /// `defineTool`. Such a tool's parameters must be in dsh's enforced
    /// JSON Schema subset (`json_schema.rs`), which the registry checks when
    /// the tool is registered.
    ///
    /// A tool whose schema comes from elsewhere, such as an MCP server's,
    /// says `false`, as dsh's raw `ToolDefinition`s are unchecked: its
    /// parameters go to the model as they are, in any JSON Schema, and the
    /// tool checks its own arguments.
    fn registry_checks_arguments(&self) -> bool {
        true
    }

    /// Runs one call. `arguments` is the model's JSON, already parsed. The
    /// registry has checked it is an object that fits the parameters,
    /// unless the tool opted out with
    /// [`registry_checks_arguments`](Self::registry_checks_arguments); such
    /// a tool gets any JSON value, as dsh hands its raw definitions one.
    ///
    /// Dropping the future cancels the call, as dropping a model's stream
    /// does. dsh passes an `AbortSignal` instead and waits for the tool to
    /// notice it (`ToolExecutionInput.signal`). A dropped future runs no more
    /// code, so a tool that must clean up after a cancel, such as killing a
    /// child process, does it in a `Drop` impl.
    ///
    /// A failure is an output with `is_error` set, not a panic: the model
    /// reads it and can try again. A panic is a bug, and nothing catches it.
    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a>;
}

/// The future [`Tool::execute`] returns. It may borrow the tool, and it must
/// be `Send` so a multi-threaded executor can run it on any thread.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

/// Which call a run is for, and who made it. (dsh: the identity fields of
/// `ToolRunContext`, `callId` and `name`, and its `agent`.)
#[derive(Clone)]
pub struct ToolRun {
    /// The id the model gave the call. Its result repeats it.
    pub call_id: String,
    /// The tool's name, as the model called it.
    pub name: String,
    /// The agent whose turn made the call; `None` for a call made outside
    /// any agent. The run holds it only while the call runs. (dsh:
    /// `agent`.)
    pub agent: Option<Arc<dyn Agent>>,
}

impl fmt::Debug for ToolRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRun")
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field("agent", &self.agent.as_ref().map(|agent| agent.id()))
            .finish()
    }
}

/// What a call returned, as the model reads it. (dsh: the `content` and
/// `isError` of `ToolExecutionResult`.)
///
/// It holds exactly what a [`ToolResult`] can carry back to the model: text,
/// and whether the call failed. dsh's results can also hold images, a
/// structured error code, presentation data and context for the next step;
/// those come later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    /// Whether the call failed. The model still reads `content`, so it can
    /// see what went wrong.
    pub is_error: bool,
}

impl ToolOutput {
    /// A call that worked.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A call that failed, with what the model should read about it.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }

    /// The result to send back to the model for the call `call_id`.
    pub fn into_result(self, call_id: impl Into<String>) -> ToolResult {
        ToolResult {
            call_id: call_id.into(),
            content: self.content,
            is_error: self.is_error,
        }
    }
}
