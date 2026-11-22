//! [`JsonlPlugin`], which provides the backend as the
//! [`SessionPersistence`] port.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session_persistence::SessionPersistence;

use crate::{JsonlConfig, JsonlPersistence};

/// Provides the [`SessionPersistence`] port with a [`JsonlPersistence`].
/// (dsh: the `session-persistence-jsonl` plugin.)
///
/// It fails at setup when the root exists but isn't a readable directory.
#[derive(Debug)]
pub struct JsonlPlugin {
    config: JsonlConfig,
}

impl JsonlPlugin {
    pub fn new(config: JsonlConfig) -> Self {
        Self { config }
    }
}

impl Plugin for JsonlPlugin {
    fn name(&self) -> &str {
        "session-persistence-jsonl"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn SessionPersistence>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let persistence: Arc<dyn SessionPersistence> =
            Arc::new(JsonlPersistence::new(self.config.clone())?);
        ctx.provide(persistence);
        Ok(())
    }
}
