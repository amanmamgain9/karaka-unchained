//! [`ToolsPlugin`], which puts an empty [`ToolRegistry`] in a Karaka
//! runtime as a port.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::ToolRegistry;

/// Provides the [`ToolRegistry`] port, empty. Plugins with tools require it
/// and register them during setup, so the runtime starts this plugin before
/// any of them. (dsh: the `tools` service.)
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

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(ToolRegistry::new()));
        Ok(())
    }
}
