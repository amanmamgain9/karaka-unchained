//! The loop's config, the [`AgentLoop`] that keeps one agent per session,
//! and [`AgentLoopPlugin`], which provides it as the `Agents` port.
//! (dsh: `AgentLoop` in `agent-loop/src/index.ts`, and its `AgentFactory`.)

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use karaka_agent::{Agent, Agents, RequestErrorHooks};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::{Effort, ModelRegistry};
use karaka_session::{SessionError, SessionId, SessionStore};
use karaka_tools::ToolRegistry;

use crate::agent::LoopAgent;

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
    /// The system prompt, as plain text. dsh assembles its prompt from
    /// sections in the system-prompt plugin; that isn't ported, so the
    /// whole prompt is given here.
    pub system: Option<String>,
    /// The most the model may write in one step. (dsh: `maxTokens`.)
    pub max_tokens: Option<u32>,
    /// (dsh: `reasoningEffort`.)
    pub effort: Option<Effort>,
}

impl LoopConfig {
    /// A config with no system prompt, and the provider's own output limit
    /// and effort.
    pub fn new(route: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            model: model.into(),
            system: None,
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

impl AgentLoop {
    /// A loop that asks the models in `models`, runs the tools in `tools`,
    /// keeps its sessions in `sessions`, and runs `hooks` when a request
    /// fails.
    pub fn new(
        config: LoopConfig,
        models: Arc<ModelRegistry>,
        tools: Arc<ToolRegistry>,
        sessions: Arc<SessionStore>,
        hooks: Arc<RequestErrorHooks>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                models,
                tools,
                sessions,
                hooks,
            }),
            agents: Mutex::new(HashMap::new()),
        }
    }
}

impl Agents for AgentLoop {
    /// # Panics
    ///
    /// If called outside a Tokio runtime, when the agent is new.
    fn open(&self, id: SessionId) -> Result<Arc<dyn Agent>, SessionError> {
        // Held while the agent is made, so two connections opening one new
        // session at once get the same agent.
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(agent) = agents.get(&id) {
            return Ok(Arc::clone(agent) as Arc<dyn Agent>);
        }
        let session = self.shared.sessions.open_or_create(id.clone());
        // Nothing runs on a session before its agent exists, so a turn left
        // open here was left by a crash, and closing it is safe.
        session.repair()?;
        let agent = Arc::new(LoopAgent::new(session, Arc::clone(&self.shared)));
        agents.insert(id, Arc::clone(&agent));
        Ok(agent)
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
/// It requires the model and tool registries and the session store, and
/// provides the [`RequestErrorHooks`] port beside `Agents`, empty: the loop
/// runs the hook, so it owns the list. A plugin that registers a handler,
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
        ]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ModelRegistry>(),
            PortId::of::<ToolRegistry>(),
            PortId::of::<SessionStore>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let hooks = Arc::new(RequestErrorHooks::new());
        let agents: Arc<dyn Agents> = Arc::new(AgentLoop::new(
            self.config.clone(),
            ctx.get::<ModelRegistry>(),
            ctx.get::<ToolRegistry>(),
            ctx.get::<SessionStore>(),
            Arc::clone(&hooks),
        ));
        ctx.provide(agents);
        ctx.provide(hooks);
        Ok(())
    }
}
