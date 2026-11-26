//! The `working_directory` tool: read the calling session's working
//! directory, or change it with `cd`. (dsh:
//! `@deepseek-ai/dsh-tool-working-directory`, 0.2.1-rc.2.)
//!
//! [`WorkingDirectoryToolPlugin`] registers it, over the
//! `karaka-working-directory` service. Without `cd` the tool checks the
//! directory ([`ensure`](WorkingDirectory::ensure)); with it, it changes it
//! ([`set`](WorkingDirectory::set)), which queues the change's notice for
//! the agent's next step. Its result is the directory.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::ToolSchema;
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun};
use karaka_working_directory::WorkingDirectory;
use serde_json::Value;

/// The tool's name. (dsh: `name`.)
pub const NAME: &str = "working_directory";

/// What the model is told the tool does. (dsh: `description`.)
pub const DESCRIPTION: &str = "Read the current working directory, or change it with cd. \
                               Relative paths use the current directory. Existing shells and \
                               running processes keep their own directories.";

/// The parameters, as dsh's `defineTool` compiles `{ cd }`: an open object
/// whose one property is optional, so there is no `required` and no
/// `additionalProperties`.
const PARAMETERS: &str = r#"{"type":"object","properties":{"cd":{"type":"string","description":"Existing directory to enter. Omit to read the current directory."}}}"#;

/// The tool. (dsh: the `defineTool` its `apply` registers.)
pub struct WorkingDirectoryTool {
    directories: Arc<WorkingDirectory>,
}

impl WorkingDirectoryTool {
    pub fn new(directories: Arc<WorkingDirectory>) -> Self {
        Self { directories }
    }
}

/// A thrown error, as dsh's tools layer shows it (`toolErrorResult`).
fn thrown(message: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::error(format!("Error: {message}"))
}

impl Tool for WorkingDirectoryTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: NAME.into(),
            description: DESCRIPTION.into(),
            parameters: PARAMETERS.into(),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            // dsh's argument check comes first: an absent `cd` is fine,
            // anything but a string isn't (`validateArgs`'s wording).
            let cd = match arguments.get("cd") {
                None => None,
                Some(Value::String(cd)) => Some(cd.clone()),
                Some(_) => return thrown("invalid arguments: \"cd\" must be a string"),
            };
            let Some(agent) = run.agent else {
                return thrown("working_directory requires an agent Session");
            };
            // dsh passes the call's signal; a dropped call is the cancel here.
            let cwd = match &cd {
                None => self.directories.ensure(agent.as_ref(), None).await,
                Some(cd) => self.directories.set(agent.as_ref(), cd, None).await,
            };
            match cwd {
                // dsh's `render`: the directory, as text.
                Ok(cwd) => ToolOutput::ok(cwd),
                Err(error) => thrown(error),
            }
        })
    }
}

/// Registers the tool. It requires the tool registry and the
/// working-directory service. (dsh: the plugin's `apply`, with `inject` of
/// `tools` and `workingDirectory`.)
///
/// dsh's registration is undone when its plugin unloads. Karaka's kernel
/// unloads no plugin, and the registry has no removal, so the tool stays for
/// the life of the runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkingDirectoryToolPlugin;

impl Plugin for WorkingDirectoryToolPlugin {
    fn name(&self) -> &str {
        "tool-working-directory"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ToolRegistry>(),
            PortId::of::<WorkingDirectory>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let tool = WorkingDirectoryTool::new(ctx.get::<WorkingDirectory>());
        ctx.get::<ToolRegistry>().register(Arc::new(tool))?;
        Ok(())
    }
}
