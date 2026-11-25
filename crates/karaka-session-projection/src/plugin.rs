//! [`SessionProjectionsPlugin`], which puts a [`SessionProjections`]
//! registry in a Karaka runtime as a port.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::SessionStore;

use crate::SessionProjections;

/// Provides the [`SessionProjections`] port, following the
/// [`SessionStore`] port's sessions. (dsh: the `dsh-session-projection`
/// plugin, `ctx.sessionProjections`.)
///
/// The port is the concrete registry, as the session store is: the units
/// are what vary. A domain with state to project requires the port and
/// registers its unit during its setup.
#[derive(Debug, Default)]
pub struct SessionProjectionsPlugin;

impl Plugin for SessionProjectionsPlugin {
    fn name(&self) -> &str {
        "session-projections"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<SessionProjections>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<SessionStore>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let store = ctx.get::<SessionStore>();
        ctx.provide(Arc::new(SessionProjections::new(&store)));
        Ok(())
    }
}
