//! [`ToolsPlugin`], which puts an empty [`ToolRegistry`] in a Karaka
//! runtime as a port, and makes it a tool source of the system prompt.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_system_prompt::{SystemPrompt, ToolProviderResult};

use crate::ToolRegistry;

/// Provides the [`ToolRegistry`] port, empty. Plugins with tools require it
/// and register them during setup, so the runtime starts this plugin before
/// any of them. (dsh: the `tools` service.)
///
/// It requires the [`SystemPrompt`] port, and registers the registry there
/// as a source of tool schemas, so each assembly lists the tools its agent
/// sees by then, as dsh's tool runtime does (`core/tools/src/index.ts:857`).
///
/// The port is the concrete registry, not a trait, for the same reason as
/// the model registry: there is one way to keep a list of tools, and the
/// tools are what vary.
#[derive(Debug, Default)]
pub struct ToolsPlugin;

impl Plugin for ToolsPlugin {
    fn name(&self) -> &str {
        "tools"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<SystemPrompt>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let registry = Arc::new(ToolRegistry::new());
        let source = Arc::clone(&registry);
        // For the life of the runtime.
        let _ = ctx.get::<SystemPrompt>().tools(None, move |context| {
            // The tools the assembly's agent sees, and every name it
            // knows, as dsh's provider gives `wireSchemas(scope)`.
            let (schemas, known) = source.wire(&context.chain);
            ToolProviderResult {
                schemas,
                known_names: Some(known),
            }
        });
        ctx.provide(registry);
        Ok(())
    }
}
