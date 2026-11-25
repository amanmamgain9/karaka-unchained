//! [`LocalFsPlugin`], which provides the host filesystem as the
//! `dyn FileSystem` port.

use std::sync::Arc;

use karaka_fs::FileSystem;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::{LocalFileSystem, LocalFsConfig};

/// Provides the [`FileSystem`] port: the host's files. (dsh: mounting
/// `dsh-fs-local`, which registers `ctx.fs`.)
///
/// It fails at setup when the config is refused.
#[derive(Clone, Debug, Default)]
pub struct LocalFsPlugin {
    config: LocalFsConfig,
}

impl LocalFsPlugin {
    pub fn new(config: LocalFsConfig) -> Self {
        Self { config }
    }
}

impl Plugin for LocalFsPlugin {
    fn name(&self) -> &str {
        "fs-local"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn FileSystem>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new(self.config.clone())?);
        ctx.provide(fs);
        Ok(())
    }
}
