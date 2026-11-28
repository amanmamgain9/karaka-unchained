//! [`SkillFilesystemPlugin`], which registers the provider. (dsh: the
//! `skill-filesystem` plugin's `apply`.)

use std::sync::Arc;

use karaka_fs::FileSystem;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_skill::{SkillProvider, SkillRegistry};

use crate::provider::{FileSystemSkillProvider, SkillFilesystemConfig};

/// Registers a [`FileSystemSkillProvider`] in the registry's global layer,
/// for the life of the runtime. It requires the skill registry and the
/// filesystem.
///
/// dsh's plugin also invalidates on `fs/observed` after a first-party write
/// or edit; Karaka has no such event yet, so that is
/// [`FileSystemSkillProvider::observe_host_mutation`], for a caller to
/// call. An agent preset mounts dsh's plugin in the preset's own layer;
/// this plugin registers globally. A preset plugin (`karaka-agent-presets`)
/// can register a [`FileSystemSkillProvider`] in its preset's scope.
#[derive(Clone, Debug, Default)]
pub struct SkillFilesystemPlugin {
    config: SkillFilesystemConfig,
}

impl SkillFilesystemPlugin {
    pub fn new(config: SkillFilesystemConfig) -> Self {
        Self { config }
    }
}

impl Plugin for SkillFilesystemPlugin {
    fn name(&self) -> &str {
        "skill-filesystem"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<SkillRegistry>(),
            PortId::of::<dyn FileSystem>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        // Checked first, so the provider is made inside its registration,
        // with its control, and can't fail there.
        self.config.validate()?;
        let fs = ctx.get::<dyn FileSystem>();
        let config = self.config.clone();
        // For the life of the runtime.
        let _ = ctx
            .get::<SkillRegistry>()
            .register_provider(None, |control| {
                FileSystemSkillProvider::new(fs, control, config).expect("the config was checked")
                    as Arc<dyn SkillProvider>
            })?;
        Ok(())
    }
}
