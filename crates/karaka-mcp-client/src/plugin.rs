//! The plugin: one supervised connection per configured server, its tools
//! synced into the tool registry, and its instructions in the system
//! prompt. (dsh: `mcp-client/src/index.ts` and `server-context.ts`.)
//!
//! dsh loads one plugin instance per server, globally or inside an agent
//! preset, and keeps the live `serverName`s per registration scope in a
//! module-level map. Here [`McpClientsPlugin`] provides the [`McpClients`]
//! port, which holds the reservations and the servers' log lines, and
//! starts the global servers it was given; a preset mounts a server with
//! [`McpServerPresetPlugin`], which starts it in the preset's scope.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use karaka_agent_presets::{PresetContext, PresetFuture, PresetPlugin};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_scope::ScopeKey;
use karaka_system_prompt::{Registration, Section, SystemPrompt, Text};
use karaka_tools::ToolRegistry;
use serde_json::Value;

use crate::connection::{
    ConnectionConfig, ConnectionHandle, DEFAULT_MAX_INSTRUCTION_BYTES,
    DEFAULT_TOOL_CALL_TIMEOUT_MS, Level, TransportConfig, TransportFactory,
    resolve_reconnect_policy, start_connection, transport_factory,
};
use crate::http::HttpParams;
use crate::stdio::StdioParams;
use crate::tools::{Log, ToolBridge};

/// Where a server's instructions go among the prompt's sections. (dsh:
/// `SECTION_ORDERS.MCP_SERVERS`.)
pub const MCP_SERVERS_SECTION_ORDER: f64 = 3100.0;

/// How many log lines the port keeps.
const KEPT_LINES: usize = 256;

/// One server's config. (dsh: `Config`, `StdioConfig` or
/// `StreamableHttpConfig`, with Schemastery's defaults.)
#[derive(Clone, Debug, PartialEq)]
pub struct McpServerConfig {
    /// The namespace of its public tool names, `[A-Za-z0-9_-]{1,32}`, and
    /// unique among the live servers of one scope.
    pub server_name: String,
    pub transport: TransportConfig,
    /// Each tool call's timeout. (60 000 by default.)
    pub tool_call_timeout_ms: u64,
    /// A failed first connection, or first tool sync, fails the plugin.
    pub fail_on_startup_error: bool,
    /// The attributed instructions' byte cap. (32 768 by default.)
    pub max_instruction_bytes: usize,
    /// The raw `reconnect` config, resolved when the server starts.
    pub reconnect: Option<Value>,
}

impl McpServerConfig {
    /// A server of `server_name` over `transport`, with dsh's defaults.
    pub fn new(server_name: impl Into<String>, transport: TransportConfig) -> Self {
        Self {
            server_name: server_name.into(),
            transport,
            tool_call_timeout_ms: DEFAULT_TOOL_CALL_TIMEOUT_MS,
            fail_on_startup_error: false,
            max_instruction_bytes: DEFAULT_MAX_INSTRUCTION_BYTES,
            reconnect: None,
        }
    }

    /// The config dsh's plugin row takes, as JSON: `transport` (`stdio` or
    /// `streamable-http`), `serverName`, `command`, `args`, `env` and `cwd`
    /// for stdio, `url` and `headers` for HTTP, `toolCallTimeoutMs`,
    /// `failOnStartupError`, `maxInstructionBytes` and `reconnect`. A key
    /// it doesn't know is refused.
    ///
    /// # Errors
    ///
    /// A missing or misshapen field, said with `at`.
    pub fn from_json(value: &Value, at: &str) -> Result<Self, String> {
        let Value::Object(map) = value else {
            return Err(format!("{at} must be a mapping"));
        };
        let transport = map.get("transport").and_then(Value::as_str);
        let common = [
            "transport",
            "serverName",
            "toolCallTimeoutMs",
            "failOnStartupError",
            "maxInstructionBytes",
            "reconnect",
        ];
        let own: &[&str] = match transport {
            Some("stdio") => &["command", "args", "env", "cwd"],
            Some("streamable-http") => &["url", "headers"],
            _ => {
                return Err(format!(
                    "{at}: \"transport\" must be \"stdio\" or \"streamable-http\""
                ));
            }
        };
        if let Some(key) = map
            .keys()
            .find(|key| !common.contains(&key.as_str()) && !own.contains(&key.as_str()))
        {
            return Err(format!("{at}: unknown key \"{key}\""));
        }
        let string = |key: &str| -> Result<Option<String>, String> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.clone())),
                Some(_) => Err(format!("{at}: \"{key}\" must be a string")),
            }
        };
        let strings = |key: &str| -> Result<Vec<String>, String> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("{at}: \"{key}\" must be a list of strings"))
                    })
                    .collect(),
                Some(_) => Err(format!("{at}: \"{key}\" must be a list of strings")),
            }
        };
        let dictionary = |key: &str| -> Result<BTreeMap<String, String>, String> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(BTreeMap::new()),
                Some(Value::Object(entries)) => entries
                    .iter()
                    .map(|(name, value)| {
                        value
                            .as_str()
                            .map(|value| (name.clone(), value.to_owned()))
                            .ok_or_else(|| format!("{at}: \"{key}\" must map names to strings"))
                    })
                    .collect(),
                Some(_) => Err(format!("{at}: \"{key}\" must map names to strings")),
            }
        };
        let server_name = string("serverName")?.ok_or(format!("{at} needs a \"serverName\""))?;
        let valid_name = (1..=32).contains(&server_name.len())
            && server_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if !valid_name {
            return Err(format!(
                "{at}: \"serverName\" must match /^[A-Za-z0-9_-]{{1,32}}$/, not \"{server_name}\""
            ));
        }
        let transport = match transport {
            Some("stdio") => TransportConfig::Stdio(StdioParams {
                command: string("command")?.ok_or(format!("{at} needs a \"command\""))?,
                args: strings("args")?,
                env: dictionary("env")?,
                cwd: string("cwd")?.unwrap_or_default(),
                ..StdioParams::default()
            }),
            _ => TransportConfig::StreamableHttp(HttpParams {
                url: string("url")?.ok_or(format!("{at} needs a \"url\""))?,
                headers: dictionary("headers")?.into_iter().collect(),
            }),
        };
        let mut config = Self::new(server_name, transport);
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
        match map.get("maxInstructionBytes") {
            None | Some(Value::Null) => {}
            Some(value) => {
                config.max_instruction_bytes = value
                    .as_u64()
                    .filter(|bytes| *bytes >= 1)
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .ok_or(format!(
                        "{at}: \"maxInstructionBytes\" must be a whole number of at least 1"
                    ))?;
            }
        }
        config.reconnect = map
            .get("reconnect")
            .filter(|value| !value.is_null())
            .cloned();
        Ok(config)
    }
}

/// Why a server didn't start. (dsh: the errors `apply` throws.)
#[derive(Debug)]
pub struct McpStartError {
    pub message: String,
    pub cause: Option<Box<crate::McpError>>,
}

impl fmt::Display for McpStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        // dsh keeps the first attempt's error as the `cause`; Karaka says it.
        if let Some(cause) = &self.cause {
            write!(f, ": {cause}")?;
        }
        Ok(())
    }
}

impl std::error::Error for McpStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_ref()
            .map(|cause| cause.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// The port: the live servers' names per scope, and their log lines.
pub struct McpClients {
    state: Mutex<State>,
    on_log: Option<Log>,
}

#[derive(Default)]
struct State {
    reserved: Vec<(Option<ScopeKey>, String)>,
    lines: std::collections::VecDeque<(Level, String)>,
}

impl McpClients {
    pub fn new(on_log: Option<Log>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            on_log,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The servers' most recent lines, oldest first: what dsh's logger
    /// gets.
    pub fn lines(&self) -> Vec<(Level, String)> {
        self.lock().lines.iter().cloned().collect()
    }

    fn log(&self, level: Level, message: &str) {
        {
            let mut state = self.lock();
            if state.lines.len() == KEPT_LINES {
                state.lines.pop_front();
            }
            state.lines.push_back((level, message.to_owned()));
        }
        if let Some(on_log) = &self.on_log {
            on_log(level, message);
        }
    }

    /// Starts the server of `config` in `scope`, its tools registered there
    /// and its instructions a prompt section there, and waits for its first
    /// connection and tool sync. (dsh: `apply`.)
    ///
    /// # Errors
    ///
    /// In dsh's order and words: a bad `reconnect` config; a `serverName`
    /// already live in this scope; and with `fail_on_startup_error`, a
    /// failed first attempt, which also stops the server again.
    pub async fn start(
        self: &Arc<Self>,
        scope: Option<ScopeKey>,
        config: McpServerConfig,
        tools: Arc<ToolRegistry>,
        prompt: Arc<SystemPrompt>,
    ) -> Result<McpServer, McpStartError> {
        let factory = transport_factory(config.transport.clone());
        self.start_with(scope, config, factory, tools, prompt).await
    }

    /// As [`start`](Self::start), with each attempt's transport made by
    /// `factory` rather than from `config.transport`: the seam for a
    /// transport of the caller's own.
    ///
    /// # Errors
    ///
    /// As [`start`](Self::start).
    pub async fn start_with(
        self: &Arc<Self>,
        scope: Option<ScopeKey>,
        config: McpServerConfig,
        factory: TransportFactory,
        tools: Arc<ToolRegistry>,
        prompt: Arc<SystemPrompt>,
    ) -> Result<McpServer, McpStartError> {
        let name = config.server_name.clone();
        let label = format!("mcp-client({name})");
        let policy =
            resolve_reconnect_policy(config.reconnect.as_ref(), &format!("{label}: reconnect"))
                .map_err(|message| McpStartError {
                    message,
                    cause: None,
                })?;
        {
            let mut state = self.lock();
            if state
                .reserved
                .iter()
                .any(|(owner, held)| *owner == scope && *held == name)
            {
                return Err(McpStartError {
                    message: format!(
                        "mcp-client: serverName \"{name}\" is already in use by another mcp-client instance — pick a unique serverName in cordis.yml"
                    ),
                    cause: None,
                });
            }
            state.reserved.push((scope.clone(), name.clone()));
        }
        let me = Arc::downgrade(self);
        let log: Log = Arc::new(move |level, message| {
            if let Some(me) = me.upgrade() {
                me.log(level, message);
            }
        });
        let bridge = Arc::new(ToolBridge::new(
            name.clone(),
            scope.clone(),
            tools,
            Duration::from_millis(config.tool_call_timeout_ms),
            log,
        ));
        let connection = start_connection(
            ConnectionConfig {
                server_name: name.clone(),
                tool_call_timeout_ms: config.tool_call_timeout_ms,
                fail_on_startup_error: config.fail_on_startup_error,
                max_instruction_bytes: config.max_instruction_bytes,
            },
            policy,
            factory,
            Arc::clone(&bridge) as _,
        );
        // The server's instructions, literal, read at each assembly. (dsh:
        // `registerServerContext`.)
        let shown = connection.clone();
        let section = prompt.section(
            scope.as_ref(),
            Section {
                interpolate: false,
                ..Section::new(
                    format!("mcp:{name}"),
                    MCP_SERVERS_SECTION_ORDER,
                    Text::dynamic(move |_| shown.instructions()),
                )
            },
        );
        let server = McpServer {
            clients: Arc::clone(self),
            scope,
            name: name.clone(),
            connection,
            bridge,
            section: section.ok(),
        };
        let outcome = server.connection.ready().await;
        if let Err(cause) = outcome
            && config.fail_on_startup_error
        {
            server.dispose().await;
            return Err(McpStartError {
                message: format!("{label}: initial connection or tool synchronization failed"),
                cause: Some(Box::new(cause)),
            });
        }
        Ok(server)
    }
}

impl fmt::Debug for McpClients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpClients")
            .field("servers", &self.lock().reserved)
            .finish_non_exhaustive()
    }
}

/// One started server.
pub struct McpServer {
    clients: Arc<McpClients>,
    scope: Option<ScopeKey>,
    name: String,
    connection: ConnectionHandle,
    bridge: Arc<ToolBridge>,
    section: Option<Registration>,
}

impl McpServer {
    /// The supervised connection.
    pub fn connection(&self) -> &ConnectionHandle {
        &self.connection
    }

    /// The public names of the tools it has registered now.
    pub fn tools(&self) -> Vec<String> {
        self.bridge.registered()
    }

    /// Stops the server: disconnects, takes its tools and prompt section
    /// back, and frees its `serverName`. (dsh: the plugin's disposal.)
    pub async fn dispose(self) {
        self.connection.dispose().await;
        if let Some(section) = self.section {
            section.remove();
        }
        let mut state = self.clients.lock();
        state
            .reserved
            .retain(|(owner, held)| !(*owner == self.scope && *held == self.name));
    }
}

/// Provides [`McpClients`], and starts the global servers it was given, in
/// order, each waiting for its first connection. (dsh: the global
/// `mcp-client` rows.)
pub struct McpClientsPlugin {
    servers: Vec<McpServerConfig>,
    on_log: Option<Log>,
    started: Mutex<Vec<McpServer>>,
}

impl McpClientsPlugin {
    pub fn new() -> Self {
        Self {
            servers: Vec::new(),
            on_log: None,
            started: Mutex::new(Vec::new()),
        }
    }

    /// Adds a global server.
    #[must_use]
    pub fn server(mut self, config: McpServerConfig) -> Self {
        self.servers.push(config);
        self
    }

    /// Tells `on_log` each line the servers report, besides keeping it.
    #[must_use]
    pub fn on_log(mut self, on_log: Log) -> Self {
        self.on_log = Some(on_log);
        self
    }
}

impl Default for McpClientsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for McpClientsPlugin {
    fn name(&self) -> &str {
        "mcp-client"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<McpClients>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>(), PortId::of::<SystemPrompt>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let clients = Arc::new(McpClients::new(self.on_log.clone()));
        let tools = ctx.get::<ToolRegistry>();
        let prompt = ctx.get::<SystemPrompt>();
        for config in &self.servers {
            let server = clients
                .start(
                    None,
                    config.clone(),
                    Arc::clone(&tools),
                    Arc::clone(&prompt),
                )
                .await?;
            // For the runtime's life.
            self.started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(server);
        }
        ctx.provide(clients);
        Ok(())
    }
}

/// A server a preset mounts: its tools and instructions in the preset's
/// scope, so only the preset's agents see them. A failed start breaks the
/// preset. (dsh: an `mcp-client` row inside an agent preset.)
pub struct McpServerPresetPlugin {
    config: McpServerConfig,
    started: Mutex<Vec<McpServer>>,
}

impl McpServerPresetPlugin {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            started: Mutex::new(Vec::new()),
        }
    }
}

impl PresetPlugin for McpServerPresetPlugin {
    fn name(&self) -> &str {
        "mcp-client"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<McpClients>(),
            PortId::of::<ToolRegistry>(),
            PortId::of::<SystemPrompt>(),
        ]
    }

    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async move {
            let server = ctx
                .get::<McpClients>()
                .start(
                    Some(ctx.scope().clone()),
                    self.config.clone(),
                    ctx.get::<ToolRegistry>(),
                    ctx.get::<SystemPrompt>(),
                )
                .await?;
            self.started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(server);
            Ok(())
        })
    }
}
