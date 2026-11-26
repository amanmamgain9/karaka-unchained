//! [`UserQuestionsPlugin`], which provides the service as a port. (dsh:
//! the `UserQuestionService` plugin, `ctx.userQuestions`.)

use std::sync::Arc;

use karaka_agent::{Agents, InboxHooks};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::SessionStore;
use karaka_session_projection::SessionProjections;

use crate::projection::UserQuestionsProjection;
use crate::service::UserQuestions;

/// Provides the [`UserQuestions`] port, and registers the `userQuestions`
/// projection, for the life of the runtime.
///
/// It requires the session store, the session projections, the `Agents`
/// registry that vouches for a calling agent, and the loop's
/// [`InboxHooks`], which tell it when a queued reply is claimed or
/// dropped. dsh's service takes the projections when there are some and
/// the registry when there is one; here both must be there.
///
/// The kernel has no teardown yet, so dsh's disposal, which ends every
/// live timed wait, is [`UserQuestions::close`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UserQuestionsPlugin;

impl Plugin for UserQuestionsPlugin {
    fn name(&self) -> &str {
        "user-questions"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<UserQuestions>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<SessionStore>(),
            PortId::of::<SessionProjections>(),
            PortId::of::<dyn Agents>(),
            PortId::of::<InboxHooks>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let projections = ctx.get::<SessionProjections>();
        // For the life of the runtime, as dsh's service holds it.
        let _ = projections.register(UserQuestionsProjection)?;
        let service = UserQuestions::new(
            ctx.get::<dyn Agents>(),
            ctx.get::<SessionStore>(),
            projections,
            &ctx.get::<InboxHooks>(),
        );
        ctx.provide(Arc::new(service));
        Ok(())
    }
}
