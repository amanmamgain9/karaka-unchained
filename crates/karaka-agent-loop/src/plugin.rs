//! The loop's config, the [`AgentLoop`] that keeps one agent per session,
//! and [`AgentLoopPlugin`], which provides it as the `Agents` port.
//! (dsh: `AgentLoop` in `agent-loop/src/index.ts`, and its `AgentFactory`.)

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use karaka_agent::{
    Agent, AgentComposition, Agents, InboxHooks, OpenError, PreStepHooks, RequestErrorHooks,
};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::{Effort, ModelRegistry};
use karaka_session::{SessionId, SessionStore};
use karaka_session_projection::SessionProjections;
use karaka_system_prompt::SystemPrompt;
use karaka_tools::ToolRegistry;

use crate::agent::LoopAgent;
use crate::inbox::{Inbox, InboxProjection};

/// How every agent of a loop asks its model. (dsh: `AgentOptions`, in
/// `agent/src/runtime-types.ts`.)
///
/// One config serves every session for now. dsh lets each agent have its
/// own options, and lets plugins change the request per step through the
/// `agent/request` hook; neither is ported.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopConfig {
    /// The route in the model registry that requests go to.
    /// (dsh: `provider`.)
    pub route: String,
    /// The model's id at that route. (dsh: `model`.)
    pub model: String,
    /// The most the model may write in one step. (dsh: `maxTokens`.)
    pub max_tokens: Option<u32>,
    /// (dsh: `reasoningEffort`.)
    pub effort: Option<Effort>,
}

impl LoopConfig {
    /// A config with the provider's own output limit and effort. The
    /// system prompt comes from the `SystemPrompt` registry.
    pub fn new(route: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            model: model.into(),
            max_tokens: None,
            effort: None,
        }
    }
}

/// What every agent of one loop uses.
pub(crate) struct Shared {
    pub(crate) config: LoopConfig,
    pub(crate) models: Arc<ModelRegistry>,
    pub(crate) tools: Arc<ToolRegistry>,
    sessions: Arc<SessionStore>,
    pub(crate) hooks: Arc<RequestErrorHooks>,
    /// Who hears messages leave an agent's inbox.
    pub(crate) inbox_hooks: Arc<InboxHooks>,
    /// Run before each step starts.
    pub(crate) pre_step: Arc<PreStepHooks>,
    /// What composes each agent from its session: its preset.
    pub(crate) composition: Arc<AgentComposition>,
    /// Each step's prompt and tool list are assembled from it.
    pub(crate) prompt: Arc<SystemPrompt>,
    /// Where each agent's inbox is folded, as the `inbox` projection.
    pub(crate) projections: Arc<SessionProjections>,
}

/// The default loop's [`Agents`]: one agent per session, made the first
/// time the session is opened, and kept for as long as the loop lives.
///
/// Its agents run their turns on Tokio tasks, so [`open`](Agents::open)
/// must be called from within a Tokio runtime. After that, an agent can be
/// used from any thread.
pub struct AgentLoop {
    shared: Arc<Shared>,
    /// A `std::sync` lock: nothing waits while holding it.
    agents: Mutex<HashMap<SessionId, Arc<LoopAgent>>>,
}

/// The extension points a loop runs: the request-error hook, and the
/// inbox signals.
#[derive(Clone, Debug, Default)]
pub struct LoopHooks {
    /// Run when a model request fails.
    pub request_error: Arc<RequestErrorHooks>,
    /// Told when a message leaves an agent's inbox.
    pub inbox: Arc<InboxHooks>,
    /// Run before each step starts.
    pub pre_step: Arc<PreStepHooks>,
    /// Where an agent composer is set.
    pub composition: Arc<AgentComposition>,
}

impl AgentLoop {
    /// A loop that asks the models in `models`, runs the tools in `tools`,
    /// keeps its sessions in `sessions`, runs `hooks`, assembles each
    /// step's prompt and tools from `prompt`, and keeps each agent's
    /// pending input as the `inbox` projection of `projections`, which must
    /// have [`InboxProjection`] registered.
    pub fn new(
        config: LoopConfig,
        models: Arc<ModelRegistry>,
        tools: Arc<ToolRegistry>,
        sessions: Arc<SessionStore>,
        hooks: LoopHooks,
        prompt: Arc<SystemPrompt>,
        projections: Arc<SessionProjections>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                models,
                tools,
                sessions,
                hooks: hooks.request_error,
                inbox_hooks: hooks.inbox,
                pre_step: hooks.pre_step,
                composition: hooks.composition,
                prompt,
                projections,
            }),
            agents: Mutex::new(HashMap::new()),
        }
    }
}

impl Agents for AgentLoop {
    /// # Panics
    ///
    /// If called outside a Tokio runtime, when the agent is new.
    fn open(&self, id: SessionId) -> Result<Arc<dyn Agent>, OpenError> {
        // Held while the agent is made, so two connections opening one new
        // session at once get the same agent.
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(agent) = agents.get(&id) {
            return Ok(Arc::clone(agent) as Arc<dyn Agent>);
        }
        // A new session names the preset it is composed from, and a session
        // that can't be composed refuses to open, before anything runs on it.
        let composer = self.shared.composition.composer();
        let session = match &composer {
            Some(composer) => self
                .shared
                .sessions
                .open_or_create_with(id.clone(), |header| composer.prepare(header)),
            None => self.shared.sessions.open_or_create(id.clone()),
        };
        if let Some(composer) = &composer {
            composer.check(&session)?;
        }
        // Nothing runs on a session before its agent exists, so a turn left
        // open here was left by a crash, and closing it is safe.
        session.repair()?;
        // A history of queued input that doesn't fold can't be continued:
        // dsh refuses to read it.
        Inbox {
            projections: &self.shared.projections,
            session: &session,
        }
        .current()?;
        let agent = Arc::new(LoopAgent::new(session, Arc::clone(&self.shared)));
        agents.insert(id, Arc::clone(&agent));
        Ok(agent)
    }

    fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>> {
        let agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        agents
            .get(id)
            .map(|agent| Arc::clone(agent) as Arc<dyn Agent>)
    }
}

impl fmt::Debug for AgentLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sessions: Vec<SessionId> = self
            .agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        f.debug_struct("AgentLoop")
            .field("config", &self.shared.config)
            .field("sessions", &sessions)
            .finish_non_exhaustive()
    }
}

/// Provides the `Agents` port with an [`AgentLoop`]. (dsh: the
/// `dsh-agent-loop` plugin, which registers its factory with the agent
/// registry.)
///
/// It requires the model and tool registries, the session store, the
/// session projections and the system prompt. It registers the `inbox`
/// projection, which holds each agent's pending input, and the prompt
/// variables `provider` (its route) and `model`, as dsh's loop does. It provides the [`RequestErrorHooks`]
/// [`InboxHooks`], [`PreStepHooks`] and [`AgentComposition`] ports beside
/// `Agents`, empty: the loop
/// runs the hooks, so it owns the lists. A plugin that registers a handler,
/// such as `karaka-retry`, requires that port, so it starts after this one.
///
/// It doesn't check its route at setup: a route may be registered later, and a
/// turn whose route has no model ends with `NO_ADAPTER`, as in dsh.
#[derive(Debug)]
pub struct AgentLoopPlugin {
    config: LoopConfig,
}

impl AgentLoopPlugin {
    pub fn new(config: LoopConfig) -> Self {
        Self { config }
    }
}

impl Plugin for AgentLoopPlugin {
    fn name(&self) -> &str {
        "agent-loop"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![
            PortId::of::<dyn Agents>(),
            PortId::of::<RequestErrorHooks>(),
            PortId::of::<InboxHooks>(),
            PortId::of::<PreStepHooks>(),
            PortId::of::<AgentComposition>(),
        ]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ModelRegistry>(),
            PortId::of::<ToolRegistry>(),
            PortId::of::<SessionStore>(),
            PortId::of::<SessionProjections>(),
            PortId::of::<SystemPrompt>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let hooks = LoopHooks::default();
        let prompt = ctx.get::<SystemPrompt>();
        let projections = ctx.get::<SessionProjections>();
        // For the life of the runtime, as dsh's loop holds it for its own.
        let _ = projections.register(InboxProjection)?;
        // dsh reads them off each agent's options; one config serves every
        // agent here. They stay for the life of the runtime.
        let route = self.config.route.clone();
        let _ = prompt.variable(None, "provider", move |_| Some(route.clone()))?;
        let model = self.config.model.clone();
        let _ = prompt.variable(None, "model", move |_| Some(model.clone()))?;
        let agents: Arc<dyn Agents> = Arc::new(AgentLoop::new(
            self.config.clone(),
            ctx.get::<ModelRegistry>(),
            ctx.get::<ToolRegistry>(),
            ctx.get::<SessionStore>(),
            hooks.clone(),
            prompt,
            projections,
        ));
        ctx.provide(agents);
        ctx.provide(hooks.request_error);
        ctx.provide(hooks.inbox);
        ctx.provide(hooks.pre_step);
        ctx.provide(hooks.composition);
        Ok(())
    }
}
