//! [`ApprovalPlugin`], which provides the service as a port. (dsh: the
//! `ApprovalService` plugin, `ctx.approval`.)

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::{SessionId, SessionStore};
use karaka_session_projection::SessionProjections;
use karaka_system_prompt::{ContextOrder, PromptContext, SystemPrompt, Text};
use karaka_tools::{ToolApproval, ToolRegistry};

use crate::service::{ApprovalService, ApprovalStateProjection};
use crate::types::ApprovalPolicy;

/// The runtime context's name. (dsh: `approval:policy`.)
pub const CONTEXT_NAME: &str = "approval:policy";

/// The plugin's settings. (dsh: `Config`.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApprovalConfig {
    /// The policy of a session with no `approval/policy` of its own: `ask`
    /// by default.
    pub policy: ApprovalPolicy,
}

/// Provides [`ApprovalService`], registers the projection it reads, and
/// adds the `approval:policy` runtime context, at
/// [`ContextOrder::ApprovalPolicy`], for the life of the runtime.
///
/// The context states the agent's current policy, after the cached
/// history, so a switch doesn't change the system prompt. An assembly for
/// no agent gets an empty line. It is optional runtime context, as dsh's
/// is: neither literal nor required.
///
/// It makes the service the tool registry's approval seam, so a
/// `tools/pre-execute` listener's `ask` is decided here. dsh's registry
/// takes the approval service when one is mounted; the kernel has no
/// optional port, so this plugin requires the registry instead.
///
/// It requires the session store, the session projections, the system
/// prompt and the tool registry.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApprovalPlugin {
    config: ApprovalConfig,
}

impl ApprovalPlugin {
    pub fn new(config: ApprovalConfig) -> Self {
        Self { config }
    }
}

impl Plugin for ApprovalPlugin {
    fn name(&self) -> &str {
        "user-approval"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<ApprovalService>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<SessionStore>(),
            PortId::of::<SessionProjections>(),
            PortId::of::<SystemPrompt>(),
            PortId::of::<ToolRegistry>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let projections = ctx.get::<SessionProjections>();
        let _ = projections.register_host_only(ApprovalStateProjection)?;
        let service = Arc::new(ApprovalService::new(
            ctx.get::<SessionStore>(),
            projections,
            self.config.policy,
        ));
        let shown = Arc::clone(&service);
        let _ = ctx.get::<SystemPrompt>().context(
            None,
            PromptContext::new(
                CONTEXT_NAME,
                ContextOrder::ApprovalPolicy.order(),
                Text::dynamic(move |context| {
                    let id = context
                        .scope
                        .as_ref()
                        .map(|scope| SessionId::new(scope.as_str()));
                    shown.context_text(id.as_ref())
                }),
            ),
        )?;
        ctx.get::<ToolRegistry>()
            .set_approval(Arc::clone(&service) as Arc<dyn ToolApproval>);
        ctx.provide(service);
        Ok(())
    }
}
