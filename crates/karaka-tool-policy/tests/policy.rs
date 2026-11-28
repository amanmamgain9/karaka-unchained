//! The tool policy on a real runtime: global tools, then presets whose
//! policies restrict what their agents inherit.

use std::sync::Arc;

use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_agent_presets::{
    AgentPresets, AgentPresetsConfig, AgentPresetsPlugin, PresetDefinition,
};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId, Runtime};
use karaka_model::{ModelsPlugin, ToolSchema};
use karaka_scope::{PresetId, ScopeChain};
use karaka_session::SessionsPlugin;
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_policy::{ToolMode, ToolPolicy};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin};
use serde_json::Value;

struct Named(&'static str);

impl Tool for Named {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.0.into(),
            description: format!("tool {}", self.0),
            parameters: r#"{"type":"object","properties":{}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move { ToolOutput::ok(self.0) })
    }
}

/// Registers `read`, `bash` and `web` globally.
struct GlobalTools;

impl Plugin for GlobalTools {
    fn name(&self) -> &str {
        "global-tools"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let tools = ctx.get::<ToolRegistry>();
        for name in ["read", "bash", "web"] {
            tools.register(Arc::new(Named(name)))?;
        }
        Ok(())
    }
}

fn policy(allow: Option<&[&str]>, deny: Option<&[&str]>) -> ToolPolicy {
    let list = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    ToolPolicy {
        mode: ToolMode::Native,
        allow: allow.map(list),
        deny: deny.map(list),
    }
}

fn preset(id: &str, policy: ToolPolicy) -> PresetDefinition {
    PresetDefinition::new(id, vec![Box::new(policy)])
}

async fn runtime() -> Runtime {
    Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new("mock", "mock-model"))),
        Box::new(GlobalTools),
        Box::new(
            AgentPresetsPlugin::new(AgentPresetsConfig::new("open"))
                .preset(preset("open", policy(None, None)))
                .preset(preset("reader", policy(Some(&["read"]), None)))
                .preset(preset("safe", policy(None, Some(&["bash"]))))
                .preset(preset(
                    "both",
                    policy(Some(&["read", "bash"]), Some(&["bash"])),
                ))
                .preset(preset("ghostly", policy(Some(&["ghost"]), None))),
        ),
    ])
    .await
    .unwrap()
}

fn names(runtime: &Runtime, preset: &str) -> Vec<String> {
    let mut names: Vec<String> = runtime
        .get::<ToolRegistry>()
        .schemas_for(&ScopeChain::new(Some(PresetId::new(preset)), None))
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn each_presets_lists_restrict_what_its_agents_inherit() {
    let runtime = runtime().await;
    // No lists: nothing masked.
    assert_eq!(names(&runtime, "open"), ["bash", "read", "web"]);
    assert_eq!(names(&runtime, "reader"), ["read"]);
    // An omitted allow list stays omitted: only the denied name goes.
    assert_eq!(names(&runtime, "safe"), ["read", "web"]);
    // Deny wins over allow.
    assert_eq!(names(&runtime, "both"), ["read"]);
    // The global view is untouched.
    assert_eq!(runtime.get::<ToolRegistry>().schemas().len(), 3);
}

#[tokio::test]
async fn a_name_no_tool_has_breaks_the_preset() {
    let runtime = runtime().await;
    let ghostly = runtime
        .get::<AgentPresets>()
        .resolve(Some("ghostly"))
        .unwrap();
    assert_eq!(
        ghostly.broken.as_deref(),
        Some(
            "tool-policy (tool-policy): tools.restrict() names unknown global tool \"ghost\"; \
             known global tools: bash, read, web"
        )
    );
    assert_eq!(
        runtime
            .get::<AgentPresets>()
            .resolve(Some("reader"))
            .unwrap()
            .broken,
        None
    );
}
