//! The `skill` tool: the model loads one skill's full instructions. (dsh:
//! the `defineTool` of `tool-skill/src/index.ts`.)

use std::sync::Arc;

use karaka_model::ToolSchema;
use karaka_skill::{
    Invocable, SkillLookupOptions, SkillRegistry, is_skill_name, render_skill_content,
};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRun};
use karaka_working_directory::WorkingDirectory;
use serde_json::Value;

/// The tool's name. (dsh: `name: 'skill'`.)
pub const NAME: &str = "skill";

/// What the model is told the tool does. (dsh: `description`.)
pub const DESCRIPTION: &str = "Load the full instructions for a skill. Call it before acting on a \
                               task that names or clearly matches a skill in the session skill \
                               catalog.";

/// The parameters, as dsh's `defineTool` compiles `{ name }`.
pub const PARAMETERS: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"The exact skill name from the available skills list."}},"required":["name"]}"#;

/// The tool. Its lookups are the calling agent's: its working directory,
/// and its scope in the registry.
pub struct SkillTool {
    skills: Arc<SkillRegistry>,
    directories: Arc<WorkingDirectory>,
}

impl SkillTool {
    pub fn new(skills: Arc<SkillRegistry>, directories: Arc<WorkingDirectory>) -> Self {
        Self {
            skills,
            directories,
        }
    }

    /// The rendered skill, or the message of the error dsh throws.
    async fn load(&self, arguments: &Value, run: &ToolRun) -> Result<String, String> {
        // The registry has checked that `name` is a string.
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_skill_name(name) {
            return Err(format!("invalid skill name \"{name}\""));
        }
        let cwd = match &run.agent {
            Some(agent) => Some(
                self.directories
                    .ensure(agent.as_ref(), None)
                    .await
                    .map_err(|error| error.to_string())?,
            ),
            None => None,
        };
        let lookup = SkillLookupOptions {
            cwd,
            signal: None,
            scope: run.agent.as_ref().map(|agent| agent.id().clone()),
        };
        let unknown = || format!("skill \"{name}\" is unknown or no longer available");
        let not_for_models = || format!("skill \"{name}\" is not available for model invocation");
        // The policy is checked before the body loads, and again on what
        // loaded.
        let listed = self
            .skills
            .list(&lookup)
            .await
            .map_err(|error| error.to_string())?;
        let summary = listed
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(unknown)?;
        if !summary.is_model_invocable() {
            return Err(not_for_models());
        }
        let skill = self
            .skills
            .get(name, &lookup)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(unknown)?;
        if !skill.is_model_invocable() {
            return Err(not_for_models());
        }
        // dsh's `render`.
        Ok(render_skill_content(
            &skill.name,
            &skill.provider,
            skill.resource_base.as_ref(),
            &skill.content,
        ))
    }
}

impl Tool for SkillTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: NAME.into(),
            description: DESCRIPTION.into(),
            parameters: PARAMETERS.into(),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            match self.load(&arguments, &run).await {
                Ok(text) => ToolOutput::ok(text),
                // dsh's tool layer shows a thrown error as `Error: <message>`.
                Err(message) => ToolOutput::error(format!("Error: {message}")),
            }
        })
    }
}
