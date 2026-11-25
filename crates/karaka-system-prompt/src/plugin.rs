//! [`SystemPromptPlugin`], which puts a [`SystemPrompt`] in a Karaka
//! runtime as a port.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::{SystemPrompt, SystemPromptConfig};

/// Provides the [`SystemPrompt`] port. (dsh: the `dsh-system-prompt`
/// plugin, `ctx.systemPrompt`.)
///
/// The port is the concrete registry, as the session store is: there is
/// one way to keep and assemble prompt contributions, and the
/// contributions are what vary. Plugins with something to say require the
/// port and register into it during their setup.
///
/// It fails at setup when the configured tool order lists a name twice or
/// lacks its rest entry.
#[derive(Debug, Default)]
pub struct SystemPromptPlugin {
    config: SystemPromptConfig,
}

impl SystemPromptPlugin {
    pub fn new(config: SystemPromptConfig) -> Self {
        Self { config }
    }
}

impl Plugin for SystemPromptPlugin {
    fn name(&self) -> &str {
        "system-prompt"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<SystemPrompt>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(SystemPrompt::new(self.config.clone())?));
        Ok(())
    }
}
