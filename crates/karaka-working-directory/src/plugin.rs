//! [`WorkingDirectoryPlugin`]: the service as a port, its runtime context,
//! and the listener that checks the directory before every assembly.
//! (dsh: `WorkingDirectoryService`'s constructor.)

use std::sync::Arc;

use karaka_agent::Agents;
use karaka_fs::FileSystem;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::{SessionId, SessionStore};
use karaka_session_projection::SessionProjections;
use karaka_system_prompt::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, AssembledContext,
    ContextOrder, PromptAssembly, PromptContext, PromptError, SystemPrompt, Text,
};

use crate::{CONTEXT_NAME, WorkingDirectory, WorkingDirectoryConfig};

/// Provides the [`WorkingDirectory`] port. (dsh: the
/// `dsh-working-directory` plugin, `ctx.workingDirectory`.)
///
/// It requires the filesystem, the session store and projections, the
/// system prompt and the agents. At setup it registers the
/// `workingDirectory` projection, the required, literal
/// `working-directory:current` context at
/// [`ContextOrder::WorkingDirectory`], and an assembly listener that runs
/// before every other: it checks the agent's directory, recovering it if it
/// is gone, and once the rest of the chain has run, puts the context back
/// if a later listener took it out or changed it.
#[derive(Clone, Debug, Default)]
pub struct WorkingDirectoryPlugin {
    config: WorkingDirectoryConfig,
}

impl WorkingDirectoryPlugin {
    pub fn new(config: WorkingDirectoryConfig) -> Self {
        Self { config }
    }
}

impl Plugin for WorkingDirectoryPlugin {
    fn name(&self) -> &str {
        "working-directory"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<WorkingDirectory>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<dyn FileSystem>(),
            PortId::of::<SessionStore>(),
            PortId::of::<SessionProjections>(),
            PortId::of::<SystemPrompt>(),
            PortId::of::<dyn Agents>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let sessions = ctx.get::<SessionStore>();
        let service = Arc::new(WorkingDirectory::new(
            self.config.clone(),
            ctx.get::<dyn FileSystem>(),
            ctx.get::<SessionProjections>(),
            Arc::clone(&sessions),
        )?);
        let prompt = ctx.get::<SystemPrompt>();
        let shown = Arc::clone(&service);
        let read = Arc::clone(&sessions);
        // For the life of the runtime, as dsh's service holds them for its
        // own.
        let _ = prompt.context(
            None,
            PromptContext::new(
                CONTEXT_NAME,
                ContextOrder::WorkingDirectory.order(),
                Text::dynamic(move |context| {
                    session_of(&read, context)
                        .map(|session| shown.context_text(&session))
                        .unwrap_or_default()
                }),
            )
            .literal()
            .required(),
        )?;
        let _ = prompt.on_assemble_first(
            None,
            Arc::new(EnsureFirst {
                service: Arc::clone(&service),
                sessions,
                agents: ctx.get::<dyn Agents>(),
            }),
        );
        ctx.provide(service);
        Ok(())
    }
}

fn session_of(
    sessions: &SessionStore,
    context: &AssembleContext,
) -> Option<karaka_session::Session> {
    let scope = context.scope.as_ref()?;
    sessions.get(&SessionId::new(scope.as_str()))
}

/// Checks the directory before the assembly, and keeps its context after.
/// (dsh: the service's prepended `system-prompt/assemble` listener.)
struct EnsureFirst {
    service: Arc<WorkingDirectory>,
    sessions: Arc<SessionStore>,
    agents: Arc<dyn Agents>,
}

impl AssembleListener for EnsureFirst {
    fn assemble<'a>(
        &'a self,
        assembly: PromptAssembly,
        context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        Box::pin(async move {
            let Some(scope) = &context.scope else {
                return next.run(assembly).await;
            };
            let agent = self
                .agents
                .open(SessionId::new(scope.as_str()))
                .map_err(|error| PromptError::new(error.to_string()))?;
            self.service
                .ensure(agent.as_ref(), None)
                .await
                .map_err(|error| PromptError::new(error.to_string()))?;
            let mut assembly = next.run(assembly).await?;
            if self.service.is_stopped() {
                return Err(PromptError::new(
                    crate::WorkingDirectoryError::Disposed.to_string(),
                ));
            }
            let Some(session) = session_of(&self.sessions, context) else {
                return Ok(assembly);
            };
            let text = self.service.context_text(&session);
            match assembly
                .contexts
                .iter_mut()
                .find(|entry| entry.name == CONTEXT_NAME)
            {
                Some(entry) => {
                    entry.text = text;
                    entry.interpolate = false;
                }
                None => assembly.contexts.insert(
                    0,
                    AssembledContext {
                        name: CONTEXT_NAME.into(),
                        text,
                        interpolate: false,
                    },
                ),
            }
            Ok(assembly)
        })
    }
}
