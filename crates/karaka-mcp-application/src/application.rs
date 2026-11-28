//! One application's MCP endpoint: its tools, published only to agents of
//! sessions the application owns, each call authorized again and carrying
//! the owner. (Karaka: `mcp-application/src/index.ts`, `apply`.)

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use karaka_agent::{Agent, AgentOpenHooks, Agents};
use karaka_identity::{ApplicationId, SessionOwners};
use karaka_mcp_client::{
    BridgeExtensions, ConnectionConfig, ConnectionHandle, DEFAULT_MAX_INSTRUCTION_BYTES,
    DEFAULT_TOOL_CALL_TIMEOUT_MS, Disposer, HeaderSource, Level, Log, McpError, McpStartError,
    McpTool, MetadataFuture, ToolBridge, resolve_reconnect_policy, start_connection,
};
use karaka_scope::{ScopeChain, ScopeKey};
use karaka_server_auth::ServerAuth;
use karaka_tools::{Tool, ToolRegistration, ToolRegistry, ToolRun};
use serde_json::{Value, json};

use crate::catalog::{ApplicationCatalog, CatalogEndpoint};
use crate::policy::{ApplicationPolicies, PolicyWatch};
use crate::tools::prepare_application_tool;
use crate::transport::application_transport;

/// The plugin's name, which prefixes its reconnect errors. (Karaka:
/// `name`.)
pub const NAME: &str = "karaka-mcp-application";

/// One endpoint's settings. (Karaka: `Config`.)
#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationConfig {
    /// The trusted application whose agents may get this catalog.
    pub application_id: String,
    /// The tool namespace: 1 to 32 ASCII letters, digits, `_` or `-`.
    pub server_name: String,
    /// The HTTP or HTTPS endpoint.
    pub url: String,
    /// Sent with every request; the current `authorization` from
    /// [`ServerAuth`] overrides one of the same name.
    pub headers: Vec<(String, String)>,
    /// The longest one `tools/call` may take. Default 60000.
    pub tool_call_timeout_ms: u64,
    /// A failed first connection or catalog sync fails the start. Default
    /// `true`.
    pub fail_on_startup_error: bool,
    /// The raw `reconnect` mapping; `None` is the supervisor's defaults.
    pub reconnect: Option<Value>,
    /// Only these public names reach the application's agents.
    pub allow: Vec<String>,
    /// Refused even where `allow` lists them.
    pub deny: Vec<String>,
}

impl ApplicationConfig {
    /// The defaults, as Karaka's schema fills them: nothing allowed yet.
    pub fn new(
        application_id: impl Into<String>,
        server_name: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            application_id: application_id.into(),
            server_name: server_name.into(),
            url: url.into(),
            headers: Vec::new(),
            tool_call_timeout_ms: DEFAULT_TOOL_CALL_TIMEOUT_MS,
            fail_on_startup_error: true,
            reconnect: None,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }

    /// Reads Karaka's row config: `applicationId`, `serverName` and `url`
    /// required; `headers`, `toolCallTimeoutMs`, `failOnStartupError`,
    /// `allow`, `deny` and `reconnect` optional, with the defaults of
    /// [`new`](Self::new). A key it doesn't know is refused. `serverName`'s
    /// pattern and `reconnect` are checked when the endpoint starts.
    ///
    /// # Errors
    ///
    /// A missing or misshapen field, said with `at`.
    pub fn from_json(value: &Value, at: &str) -> Result<Self, String> {
        const KEYS: [&str; 9] = [
            "applicationId",
            "serverName",
            "url",
            "headers",
            "toolCallTimeoutMs",
            "failOnStartupError",
            "allow",
            "deny",
            "reconnect",
        ];
        let Value::Object(map) = value else {
            return Err(format!("{at} must be a mapping"));
        };
        if let Some(key) = map.keys().find(|key| !KEYS.contains(&key.as_str())) {
            return Err(format!("{at}: unknown key \"{key}\""));
        }
        let required = |key: &str| match map.get(key) {
            Some(Value::String(text)) => Ok(text.clone()),
            None | Some(Value::Null) => Err(format!("{at} needs \"{key}\"")),
            Some(_) => Err(format!("{at}: \"{key}\" must be a string")),
        };
        let strings = |key: &str| -> Result<Vec<String>, String> {
            let wrong = || format!("{at}: \"{key}\" must be a list of strings");
            match map.get(key) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned).ok_or_else(wrong))
                    .collect(),
                Some(_) => Err(wrong()),
            }
        };
        let mut config = Self::new(
            required("applicationId")?,
            required("serverName")?,
            required("url")?,
        );
        match map.get("headers") {
            None | Some(Value::Null) => {}
            Some(Value::Object(entries)) => {
                for (name, value) in entries {
                    let value = value
                        .as_str()
                        .ok_or(format!("{at}: \"headers\" must map names to strings"))?;
                    config.headers.push((name.clone(), value.to_owned()));
                }
            }
            Some(_) => return Err(format!("{at}: \"headers\" must map names to strings")),
        }
        match map.get("toolCallTimeoutMs") {
            None | Some(Value::Null) => {}
            Some(value) => {
                config.tool_call_timeout_ms = value.as_u64().filter(|ms| *ms > 0).ok_or(
                    format!("{at}: \"toolCallTimeoutMs\" must be a positive whole number"),
                )?;
            }
        }
        match map.get("failOnStartupError") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(fail)) => config.fail_on_startup_error = *fail,
            Some(_) => {
                return Err(format!(
                    "{at}: \"failOnStartupError\" must be true or false"
                ));
            }
        }
        config.allow = strings("allow")?;
        config.deny = strings("deny")?;
        config.reconnect = map
            .get("reconnect")
            .filter(|value| !value.is_null())
            .cloned();
        Ok(config)
    }
}

/// The ports an endpoint reads. (Karaka: `inject`, with `agents` and the
/// `agent/created` event as [`Agents`] and [`AgentOpenHooks`].)
#[derive(Clone)]
pub struct ApplicationPorts {
    pub tools: Arc<ToolRegistry>,
    pub owners: Arc<dyn SessionOwners>,
    pub auth: Arc<dyn ServerAuth>,
    pub catalog: Arc<ApplicationCatalog>,
    pub policies: Arc<ApplicationPolicies>,
    pub agents: Arc<dyn Agents>,
    pub opened: Arc<AgentOpenHooks>,
    /// Told the supervisor's lines; `None` drops them.
    pub log: Option<Log>,
}

/// A started endpoint. Dropping it keeps it running; [`dispose`] stops it.
///
/// [`dispose`]: Application::dispose
pub struct Application {
    shared: Arc<Shared>,
    connection: ConnectionHandle,
    endpoint: CatalogEndpoint,
    watch: PolicyWatch,
}

impl Application {
    /// The supervised connection.
    pub fn connection(&self) -> &ConnectionHandle {
        &self.connection
    }

    /// Disconnects, and takes back every registration and the catalog's
    /// contributions, then frees the catalog's server.
    pub async fn dispose(self) {
        self.shared.lock().closed = true;
        self.watch.remove();
        self.connection.dispose().await;
        self.shared.clear_agents();
        self.endpoint.dispose();
    }
}

/// Starts the endpoint: claims its server in the catalog, publishes its
/// tools to every owned agent, now and as they open, and settles the
/// catalog once the first connection has. (Karaka: `apply`.)
///
/// # Errors
///
/// A bad `serverName` or `reconnect`; the catalog's refusal of the server;
/// and `Application MCP <server> could not start`, with its cause, when the
/// URL isn't HTTP or HTTPS, or the first connection or catalog sync failed
/// with `fail_on_startup_error`.
pub async fn start_application(
    ports: ApplicationPorts,
    config: ApplicationConfig,
) -> Result<Application, McpStartError> {
    let refuse = |message: String| McpStartError {
        message,
        cause: None,
    };
    let server = config.server_name.clone();
    let valid_name = (1..=32).contains(&server.len())
        && server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid_name {
        return Err(refuse(format!(
            "{NAME}: \"serverName\" must match /^[A-Za-z0-9_-]{{1,32}}$/, not \"{server}\""
        )));
    }
    let endpoint = ports.catalog.endpoint(&server).map_err(refuse)?;
    let could_not_start = |cause: McpError| McpStartError {
        message: format!("Application MCP {server} could not start"),
        cause: Some(Box::new(cause)),
    };
    let policy = match resolve_reconnect_policy(config.reconnect.as_ref(), NAME) {
        Ok(policy) => policy,
        Err(message) => {
            endpoint.dispose();
            return Err(refuse(message));
        }
    };
    let application_id = ApplicationId::new(&config.application_id);
    let auth = Arc::clone(&ports.auth);
    let id = application_id.clone();
    let credentials: HeaderSource = Arc::new(move || {
        let (auth, id) = (Arc::clone(&auth), id.clone());
        Box::pin(async move {
            Ok(vec![(
                "authorization".to_owned(),
                auth.authorize_tools(&id).await?,
            )])
        })
    });
    let factory = match application_transport(&config.url, config.headers.clone(), credentials) {
        Ok(factory) => factory,
        Err(message) => {
            endpoint.dispose();
            return Err(could_not_start(McpError::other(message)));
        }
    };
    let shared = Arc::new_cyclic(|me| Shared {
        application_id,
        allowed: config.allow.iter().cloned().collect(),
        denied: config.deny.iter().cloned().collect(),
        tools: Arc::clone(&ports.tools),
        owners: Arc::clone(&ports.owners),
        catalog: Arc::clone(&ports.catalog),
        policies: Arc::clone(&ports.policies),
        log: ports.log.clone(),
        state: Mutex::default(),
        me: me.clone(),
    });
    // A policy change re-registers every attached agent's catalog.
    let watched = Arc::downgrade(&shared);
    let watch = ports.policies.watch(Arc::new(move || {
        watched
            .upgrade()
            .map_or(Ok(()), |shared| shared.reregister())
    }));
    // Agents opened from now on, then those already open; one opened in
    // between is attached once.
    let opened = Arc::downgrade(&shared);
    ports.opened.register(Arc::new(move |agent| {
        if let Some(shared) = opened.upgrade() {
            shared.attach(agent);
        }
    }));
    for agent in ports.agents.list() {
        shared.attach(&agent);
    }
    let log = ports.log.clone().unwrap_or_else(|| Arc::new(|_, _| {}));
    let bridge = Arc::new(
        ToolBridge::new(
            server.clone(),
            None,
            Arc::clone(&ports.tools),
            Duration::from_millis(config.tool_call_timeout_ms),
            log,
        )
        .with_extensions(Arc::clone(&shared) as Arc<dyn BridgeExtensions>),
    );
    let connection = start_connection(
        ConnectionConfig {
            server_name: server.clone(),
            tool_call_timeout_ms: config.tool_call_timeout_ms,
            fail_on_startup_error: config.fail_on_startup_error,
            max_instruction_bytes: DEFAULT_MAX_INSTRUCTION_BYTES,
        },
        policy,
        factory,
        bridge,
    );
    let application = Application {
        shared,
        connection,
        endpoint,
        watch,
    };
    if let Err(cause) = application.connection.ready().await
        && config.fail_on_startup_error
    {
        application.dispose().await;
        return Err(could_not_start(cause));
    }
    application.endpoint.settle();
    Ok(application)
}

/// What the endpoint's bridge, open hook and policy watch share. (Karaka:
/// `apply`'s closure, and its `ApplicationBridge`.)
struct Shared {
    application_id: ApplicationId,
    allowed: HashSet<String>,
    denied: HashSet<String>,
    tools: Arc<ToolRegistry>,
    owners: Arc<dyn SessionOwners>,
    catalog: Arc<ApplicationCatalog>,
    policies: Arc<ApplicationPolicies>,
    log: Option<Log>,
    state: Mutex<State>,
    me: Weak<Shared>,
}

#[derive(Default)]
struct State {
    /// Disposed: no agent is attached again.
    closed: bool,
    /// The published tools, by public name. (Karaka: `catalog`.)
    catalog: Vec<(String, Arc<dyn Tool>)>,
    /// The owned agents, and what each was given. (Karaka: `agents`.)
    agents: Vec<Attached>,
}

struct Attached {
    instance: usize,
    agent: Weak<dyn Agent>,
    registrations: Vec<(String, ToolRegistration)>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the endpoint's lists and the agent's policies admit `name`.
    fn admitted(&self, chain: &ScopeChain, name: &str) -> bool {
        self.allowed.contains(name)
            && !self.denied.contains(name)
            && self.policies.allows(chain, name)
    }

    /// Registers `tool` in `agent`'s own scope, if admitted. (Karaka:
    /// `registerFor`.)
    fn register_for(
        &self,
        agent: &Arc<dyn Agent>,
        registrations: &mut Vec<(String, ToolRegistration)>,
        name: &str,
        tool: &Arc<dyn Tool>,
    ) -> Result<(), String> {
        if !self.admitted(&agent.scope(), name) {
            return Ok(());
        }
        let scope = ScopeKey::Session(agent.id().clone());
        let registration = self
            .tools
            .register_in(Some(&scope), Arc::clone(tool))
            .map_err(|error| error.to_string())?;
        registrations.push((name.to_owned(), registration));
        Ok(())
    }

    /// Gives `agent` the catalog, if its session's owner is this
    /// application. (Karaka: `attach`.)
    fn attach(&self, agent: &Arc<dyn Agent>) {
        let mut state = self.lock();
        if state.closed
            || state
                .agents
                .iter()
                .any(|known| known.instance == agent.instance() && known.agent.strong_count() > 0)
        {
            return;
        }
        let owner = match self.owners.owner_of_cached(agent.id()) {
            Ok(owner) => owner,
            Err(error) => {
                self.say(Level::Warn, &error.message);
                return;
            }
        };
        if owner.map(|owner| owner.application_id) != Some(self.application_id.clone()) {
            return;
        }
        let mut registrations = Vec::new();
        for (name, tool) in &state.catalog {
            if let Err(error) = self.register_for(agent, &mut registrations, name, tool) {
                self.say(Level::Warn, &error);
            }
        }
        state.agents.push(Attached {
            instance: agent.instance(),
            agent: Arc::downgrade(agent),
            registrations,
        });
    }

    /// Registers every attached agent's catalog again, after a policy
    /// change. (Karaka: the `watchPolicy` listener.)
    fn reregister(&self) -> Result<(), String> {
        let mut state = self.lock();
        let State {
            catalog, agents, ..
        } = &mut *state;
        for attached in agents.iter_mut() {
            for (_, registration) in attached.registrations.drain(..) {
                registration.remove();
            }
            let Some(agent) = attached.agent.upgrade() else {
                continue;
            };
            for (name, tool) in catalog.iter() {
                self.register_for(&agent, &mut attached.registrations, name, tool)?;
            }
        }
        Ok(())
    }

    /// Takes `name` out of the catalog and from every agent. (Karaka:
    /// `register`'s `remove`.)
    fn remove(&self, name: &str) {
        let mut state = self.lock();
        state.catalog.retain(|(known, _)| known != name);
        for attached in &mut state.agents {
            let (gone, kept) = attached
                .registrations
                .drain(..)
                .partition(|(known, _)| known == name);
            attached.registrations = kept;
            for (_, registration) in gone {
                registration.remove();
            }
        }
    }

    /// Takes back every agent's registrations, and forgets the agents.
    fn clear_agents(&self) {
        let mut state = self.lock();
        for attached in state.agents.drain(..) {
            for (_, registration) in attached.registrations {
                registration.remove();
            }
        }
    }

    fn say(&self, level: Level, message: &str) {
        if let Some(log) = &self.log {
            log(level, &format!("{NAME}: {message}"));
        }
    }
}

impl BridgeExtensions for Shared {
    fn prepare(&self, tool: McpTool, _definition: &Value) -> Result<Arc<dyn Tool>, String> {
        prepare_application_tool(tool)
    }

    /// The call's owner, checked again now: a cached registration is not
    /// trusted. (Karaka: `ApplicationBridge.metadata`.)
    fn metadata<'a>(&'a self, run: &'a ToolRun) -> MetadataFuture<'a> {
        Box::pin(async move {
            let Some(agent) = &run.agent else {
                return Err("Application tool requires an Agent".into());
            };
            let owner = self
                .owners
                .owner_of(agent.id())
                .await
                .map_err(|error| error.message)?;
            let Some(owner) = owner.filter(|owner| owner.application_id == self.application_id)
            else {
                return Err("Application tool owner does not match endpoint".into());
            };
            if !self.admitted(&agent.scope(), &run.name) {
                return Err("Application tool is not allowed".into());
            }
            Ok(Some(json!({
                "karaka": {
                    "applicationId": owner.application_id.as_str(),
                    "tenantId": owner.tenant_id.as_str(),
                    "userId": owner.user_id.as_str(),
                    "chatId": agent.id().as_str(),
                }
            })))
        })
    }

    /// Adds `tool` to the catalog and gives it to every admitted agent; a
    /// refused registration takes it all back. (Karaka:
    /// `ApplicationBridge.register`.)
    fn register(&self, tool: Arc<dyn Tool>) -> Option<Result<Disposer, String>> {
        let name = tool.schema().name;
        let contribution = self.catalog.register_tool(&name);
        let failed = {
            let mut state = self.lock();
            state.catalog.retain(|(known, _)| *known != name);
            state.catalog.push((name.clone(), Arc::clone(&tool)));
            let mut failed = None;
            for attached in &mut state.agents {
                let Some(agent) = attached.agent.upgrade() else {
                    continue;
                };
                if let Err(error) =
                    self.register_for(&agent, &mut attached.registrations, &name, &tool)
                {
                    failed = Some(error);
                    break;
                }
            }
            failed
        };
        if let Some(error) = failed {
            self.remove(&name);
            contribution.remove();
            return Some(Err(error));
        }
        let me = self.me.clone();
        Some(Ok(Box::new(move || {
            if let Some(shared) = me.upgrade() {
                shared.remove(&name);
            }
            contribution.remove();
        })))
    }
}
