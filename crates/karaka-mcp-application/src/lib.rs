//! Karaka's application MCP: an application server's own MCP endpoint,
//! whose tools reach only the agents of sessions that application owns.
//! (Karaka's own: `@karaka-ai/mcp-application`,
//! `karaka/packages/karaka/mcp-application`.) It is how Play This Life
//! gets its `mcp__play_this_life__*` tools.
//!
//! - [`ApplicationCatalogPlugin`] provides the [`ApplicationCatalog`], the
//!   endpoints a runtime expects and the names they contribute, and the
//!   [`ApplicationPolicies`] presets file their application allow lists in
//!   (`catalog.ts`, `policy.ts`).
//! - [`McpApplicationPlugin`] starts one endpoint ([`start_application`]):
//!   a Streamable HTTP connection under dsh's supervisor, whose requests
//!   carry [`ServerAuth`]'s current authorization and never follow a
//!   redirect (`transport.ts`); each tool's input checked before anything
//!   else (`tools.ts`); each tool registered in the own scope of every
//!   agent whose session the application owns, as the endpoint's and the
//!   agent's policies admit it; and each call authorized again against the
//!   session's owner, then sent with that owner and the chat id as
//!   `_meta.karaka` (`index.ts`).
//!
//! The server's instructions aren't a prompt section, as Karaka's
//! `startConnection` call registers none.
//!
//! [`ServerAuth`]: karaka_server_auth::ServerAuth

mod application;
mod catalog;
mod policy;
mod tools;
mod transport;

use std::sync::{Mutex, PoisonError};

use karaka_agent::{AgentOpenHooks, Agents};
use karaka_identity::SessionOwners;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_mcp_client::Log;
use karaka_server_auth::ServerAuth;
use karaka_tools::ToolRegistry;

pub use application::{Application, ApplicationConfig, ApplicationPorts, NAME, start_application};
pub use catalog::{ApplicationCatalog, CatalogEndpoint, ToolContribution};
pub use policy::{
    ApplicationPolicies, ApplicationToolPolicy, PolicyListener, PolicyRegistration, PolicyWatch,
};
pub use tools::{ApplicationTool, prepare_application_tool};
pub use transport::application_transport;

/// Provides the [`ApplicationCatalog`] expecting `servers`, and the
/// [`ApplicationPolicies`]. Every runtime with Karaka's tool policy mounts
/// it, with no servers where there are no applications. (Karaka: the
/// `application-catalog` row.)
#[derive(Debug, Default)]
pub struct ApplicationCatalogPlugin {
    servers: Vec<String>,
}

impl ApplicationCatalogPlugin {
    pub fn new(servers: Vec<String>) -> Self {
        Self { servers }
    }
}

impl Plugin for ApplicationCatalogPlugin {
    fn name(&self) -> &str {
        "application-catalog"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ApplicationCatalog>(),
            PortId::of::<ApplicationPolicies>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(ApplicationCatalog::new(self.servers.clone())?);
        ctx.provide(ApplicationPolicies::new());
        Ok(())
    }
}

/// Starts one application endpoint, and waits for its first connection.
/// Start it before the presets, so their policies see its tools. (Karaka:
/// an `mcp-application` row.)
pub struct McpApplicationPlugin {
    name: String,
    config: ApplicationConfig,
    log: Option<Log>,
    started: Mutex<Vec<Application>>,
}

impl McpApplicationPlugin {
    pub fn new(config: ApplicationConfig) -> Self {
        Self {
            // Names are unique in a runtime, and a server is in one
            // catalog once.
            name: format!("{NAME}({})", config.server_name),
            config,
            log: None,
            started: Mutex::new(Vec::new()),
        }
    }

    /// Tells `log` the supervisor's lines.
    #[must_use]
    pub fn on_log(mut self, log: Log) -> Self {
        self.log = Some(log);
        self
    }
}

impl Plugin for McpApplicationPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ToolRegistry>(),
            PortId::of::<dyn SessionOwners>(),
            PortId::of::<dyn ServerAuth>(),
            PortId::of::<ApplicationCatalog>(),
            PortId::of::<ApplicationPolicies>(),
            PortId::of::<dyn Agents>(),
            PortId::of::<AgentOpenHooks>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let ports = ApplicationPorts {
            tools: ctx.get::<ToolRegistry>(),
            owners: ctx.get::<dyn SessionOwners>(),
            auth: ctx.get::<dyn ServerAuth>(),
            catalog: ctx.get::<ApplicationCatalog>(),
            policies: ctx.get::<ApplicationPolicies>(),
            agents: ctx.get::<dyn Agents>(),
            opened: ctx.get::<AgentOpenHooks>(),
            log: self.log.clone(),
        };
        let application = start_application(ports, self.config.clone()).await?;
        // For the runtime's life.
        self.started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(application);
        Ok(())
    }
}
