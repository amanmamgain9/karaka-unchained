//! Karaka's application chat routes: how an application server, such as
//! Play This Life's, opens chats for its users and talks to them over HTTP.
//! (Karaka's own: `@karaka-ai/transport-http`, `transport-http/src/index.ts`,
//! `application.ts`, `application-state.ts` and `http.ts`; the wire
//! contract is `@karaka-ai/sdk`'s `protocol.ts`.)
//!
//! Under `path` (default `/v1`), every request passes the startup gate,
//! then server authentication, then:
//!
//! - `GET /agents`: the usable presets;
//! - `POST /chats`: create or reopen a chat under a preset;
//! - `POST /chats/:id/messages`: admit a prompt once per request id;
//! - `POST /chats/:id/history`: the chat's transcript;
//! - `POST /chats/:id/cancel`: stop the running turn, keeping the queue;
//! - `POST /chats/:id/model`: select the chat's model;
//! - `POST /chats/:id/responses`: answer a pending question;
//! - `POST /chats/:id/stream`: an SSE follow, its snapshot first.
//!
//! The browser routes and socket (`browser*.ts`) are HTTP ingress step 4.

mod controller;
mod error;
mod hub;
mod ids;
mod questions;
mod routes;
mod state;
mod wire;

use std::sync::{Arc, Mutex, PoisonError};

use karaka_agent::{AgentOpenHooks, Agents, AssistantStreamFrame, FrameSubscription, StreamChunk};
use karaka_agent_default_model::AgentDefaultModel;
use karaka_agent_presets::AgentPresets;
use karaka_identity::KarakaIdentity;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::{Event, ModelRegistry};
use karaka_server_auth::ServerAuth;
use karaka_session::{SessionListener, SessionStore};
use karaka_session_persistence::DurableSessions;
use karaka_session_query::SessionQuery;
use karaka_startup::KarakaStartup;
use karaka_user_questions::UserQuestions;
use karaka_webserver::{RouteKind, WebRoute, WebServer};

pub use controller::{Address, ApplicationChats, ControllerPorts, Follow, FollowFrame};
pub use error::{AppError, Exposed, expose};
pub use hub::Hub;
pub use ids::random_uuid;
pub use questions::{ApplicationAnswerer, Interactions};
pub use routes::Routes;
pub use state::{ApplicationState, application_state, fold};
pub use wire::{
    Address as AddressRequest, AnswerItem, Content, CreateChat, Identity,
    KARAKA_APPLICATION_API_PATH, ModelRequest, Prompt, Respond, project, sse,
};

/// The default largest body, 1 MiB. (Karaka: `maxBodyBytes`'s default.)
pub const DEFAULT_MAX_BODY_BYTES: usize = 1_048_576;

/// (Karaka: `Config`, without the browser fields, which are step 4.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportConfig {
    /// Answer owned chats' questions through `responses`. Default `true`.
    pub handle_questions: bool,
    /// The route prefix. Default `/v1`.
    pub path: String,
    /// The largest JSON body taken. Default 1 MiB.
    pub max_body_bytes: usize,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            handle_questions: true,
            path: KARAKA_APPLICATION_API_PATH.into(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

/// Checks a route prefix. (Karaka: `normalizeBase`.)
///
/// # Errors
///
/// In Karaka's words.
pub fn normalize_base(path: &str) -> Result<&str, String> {
    if !path.starts_with('/') || path == "/" || path.ends_with('/') {
        return Err("transport-http path must start with / and have no trailing slash".into());
    }
    Ok(path)
}

/// Mounts the routes. It requires the webserver, server auth, the
/// identity, the agents and their open hooks, the session store, durable
/// sessions, the session query, the default model, the model registry, the
/// startup gate, the question service when it answers questions, and the
/// presets when the runtime has them. (Karaka: the `application-http`
/// row.)
pub struct TransportHttpPlugin {
    config: TransportConfig,
    presets: bool,
    /// Each opened agent's text-delta listener, for the runtime's life.
    frames: Arc<Mutex<Vec<FrameSubscription>>>,
}

impl TransportHttpPlugin {
    /// # Errors
    ///
    /// A bad path, in Karaka's words.
    pub fn new(config: TransportConfig, presets: bool) -> Result<Self, String> {
        normalize_base(&config.path)?;
        Ok(Self {
            config,
            presets,
            frames: Arc::default(),
        })
    }
}

impl Plugin for TransportHttpPlugin {
    fn name(&self) -> &str {
        "application-http"
    }

    fn requires(&self) -> Vec<PortId> {
        let mut ports = vec![
            PortId::of::<WebServer>(),
            PortId::of::<dyn ServerAuth>(),
            PortId::of::<KarakaIdentity>(),
            PortId::of::<dyn Agents>(),
            PortId::of::<AgentOpenHooks>(),
            PortId::of::<SessionStore>(),
            PortId::of::<DurableSessions>(),
            PortId::of::<SessionQuery>(),
            PortId::of::<AgentDefaultModel>(),
            PortId::of::<ModelRegistry>(),
            PortId::of::<KarakaStartup>(),
        ];
        if self.config.handle_questions {
            ports.push(PortId::of::<UserQuestions>());
        }
        if self.presets {
            ports.push(PortId::of::<AgentPresets>());
        }
        ports
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let store = ctx.get::<SessionStore>();
        let hub = Hub::new(&store);
        store.listen(Arc::clone(&hub) as Arc<dyn SessionListener>);
        // Every agent's streamed text reaches its chat's follows.
        let frames = Arc::clone(&self.frames);
        let streamed = Arc::clone(&hub);
        let subscribe = move |agent: &Arc<dyn karaka_agent::Agent>| {
            let hub = Arc::clone(&streamed);
            let chat = agent.id().clone();
            let subscription = agent.subscribe(Arc::new(move |frame: &AssistantStreamFrame| {
                if let AssistantStreamFrame::Chunk {
                    chunk: StreamChunk::Event(Event::TextDelta { text, .. }),
                    ..
                } = frame
                {
                    hub.text_delta(&chat, text);
                }
            }));
            frames
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(subscription);
        };
        let agents = ctx.get::<dyn Agents>();
        for agent in agents.list() {
            subscribe(&agent);
        }
        ctx.get::<AgentOpenHooks>().register(Arc::new(subscribe));

        let identity = ctx.get::<KarakaIdentity>();
        let chats = ApplicationChats::new(
            ControllerPorts {
                identity: Arc::clone(&identity),
                agents,
                store: Arc::clone(&store),
                durable: ctx.get::<DurableSessions>(),
                query: ctx.get::<SessionQuery>(),
                presets: self.presets.then(|| ctx.get::<AgentPresets>()),
                default_model: ctx.get::<AgentDefaultModel>(),
                models: ctx.get::<ModelRegistry>(),
            },
            Arc::clone(&hub),
        );
        let interactions = Interactions::new(Arc::clone(&hub));
        if self.config.handle_questions {
            // For the runtime's life.
            let _ = ctx.get::<UserQuestions>().answerer(
                None,
                Arc::new(ApplicationAnswerer {
                    interactions: Arc::clone(&interactions),
                    identity,
                    store,
                }),
            );
        }
        let routes = Arc::new(Routes {
            base: self.config.path.clone(),
            max_body_bytes: self.config.max_body_bytes,
            chats,
            auth: ctx.get::<dyn ServerAuth>(),
            startup: ctx.get::<KarakaStartup>(),
            interactions,
            hub,
        });
        let _ = ctx.get::<WebServer>().register(WebRoute::new(
            RouteKind::Prefix,
            self.config.path.clone(),
            move |request| {
                let routes = Arc::clone(&routes);
                async move { Ok(routes.handle(request).await) }
            },
        ))?;
        Ok(())
    }
}
