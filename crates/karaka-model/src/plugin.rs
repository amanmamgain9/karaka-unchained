//! [`ModelsPlugin`], which puts an empty [`ModelRegistry`] in a Karaka
//! runtime as a port.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::ModelRegistry;

/// Provides the [`ModelRegistry`] port, empty. Provider plugins require it
/// and register their models in it, so the runtime starts this plugin
/// before any of them.
///
/// The port is the concrete registry, not a trait: there is one way to keep
/// a list of routes, and the providers are what vary.
#[derive(Debug, Default)]
pub struct ModelsPlugin;

impl Plugin for ModelsPlugin {
    fn name(&self) -> &str {
        "models"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<ModelRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(ModelRegistry::new()));
        Ok(())
    }
}
