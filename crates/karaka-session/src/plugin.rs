//! [`SessionsPlugin`], which puts an empty [`SessionStore`] in a Karaka
//! runtime as a port.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::SessionStore;

/// Provides the [`SessionStore`] port: every session, in memory.
/// (dsh: the `sessions` service.)
///
/// The port is the concrete store, not a trait. There is only one store, and
/// durable backends won't replace it: they will be plugins that save its
/// events, as dsh's persistence plugins do. How a backend hears about new
/// events (in dsh, the `session/event` feed) is left until the first one is
/// built.
#[derive(Debug, Default)]
pub struct SessionsPlugin;

impl Plugin for SessionsPlugin {
    fn name(&self) -> &str {
        "sessions"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<SessionStore>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(SessionStore::default()));
        Ok(())
    }
}
