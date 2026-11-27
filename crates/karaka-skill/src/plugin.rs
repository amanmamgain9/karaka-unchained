//! [`SkillsPlugin`], which provides the registry as a port. (dsh: the
//! `SkillRegistry` plugin, `ctx.skills`.)

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::registry::{SkillConfig, SkillRegistry};

/// Provides an empty [`SkillRegistry`]. Provider plugins, such as the
/// filesystem's, require it and register into it.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkillsPlugin {
    config: SkillConfig,
}

impl SkillsPlugin {
    pub fn new(config: SkillConfig) -> Self {
        Self { config }
    }
}

impl Plugin for SkillsPlugin {
    fn name(&self) -> &str {
        "skill"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<SkillRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(SkillRegistry::new(self.config)?));
        Ok(())
    }
}
